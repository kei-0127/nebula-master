use crate::bridge::Bridge;
use crate::server::SWITCHBOARD_SERVICE;
use anyhow::Result;
use itertools::Itertools;
use nebula_db::message::ChannelEvent;
use nebula_db::models::MusicOnHold;
use nebula_redis::{redis, REDIS};
use strum_macros;
use strum_macros::EnumString;
use tokio::time::Instant;

use crate::channel::Channel;

#[derive(strum_macros::Display, EnumString, PartialEq, Clone)]
pub enum ConferenceEvent {
    #[strum(serialize = "member_join")]
    MemberJoin,
    #[strum(serialize = "member_leave")]
    MemberLeave,
}

pub struct Conference {
    id: String,
    no_joinleave: bool,
    moh: Option<MusicOnHold>,
    needs_chairman: bool,
}

impl Conference {
    pub fn new(
        id: String,
        no_joinleave: bool,
        needs_chairman: bool,
        moh: Option<MusicOnHold>,
    ) -> Self {
        Self {
            id,
            no_joinleave,
            needs_chairman,
            moh,
        }
    }

    pub async fn wait(&self, channel: &Channel) -> Result<()> {
        println!("conference wait");
        loop {
            tokio::select! {
                _ = channel.receive_event("0", ChannelEvent::Hangup) => {
                    break;
                }
                result = self.receive_event("$", ConferenceEvent::MemberLeave) => {
                    if self.needs_chairman && !self.chairman_joined().await? {
                        if let Some(moh) = self.moh.as_ref() {
                            let stream_id = channel.get_audio_id().await?;
                            SWITCHBOARD_SERVICE
                                .media_rpc_client
                                .play_sound(
                                    moh.play_id.clone(),
                                    channel.id.clone(),
                                    stream_id,
                                    moh.sounds.clone(),
                                    moh.random,
                                    0,
                                    true,
                                )
                                .await?;
                        }
                        loop {
                            tokio::select! {
                                _ = channel.receive_event("0", ChannelEvent::Hangup) => {}
                                _ = self.receive_event("$", ConferenceEvent::MemberJoin) => {
                                    if self.chairman_joined().await? {
                                        if let Some(moh) = self.moh.as_ref() {
                                            channel.stop_sound(&moh.play_id).await?;
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    } else {
                        if !self.no_joinleave {
                            if let Ok((key_id, member_id)) = result {
                                channel.play_sound(
                                     None,
                                     vec![
                                         format!("redis://{}-name", &member_id),
                                         "tts://has left the conference".to_string(),
                                     ],
                                     false,
                                ).await?;
                            }
                        }
                        self.play_alone().await?;
                    }
                }
            }
        }
        self.leave(channel).await?;
        Ok(())
    }

    pub async fn leave(&self, channel: &Channel) -> Result<()> {
        REDIS
            .srem(
                &format!("nebula:conference:{}:members", &self.id),
                &channel.id,
            )
            .await?;
        self.new_event(ConferenceEvent::MemberLeave, &channel.id)
            .await?;

        let members: Vec<String> = REDIS
            .smembers(&format!("nebula:conference:{}:members", &self.id))
            .await?;
        for member in members {
            if &member != &channel.id {
                println!("check channel hangup {}", &member);
                let memeber_channel = Channel::new(member.clone());
                if memeber_channel.is_hangup().await {
                    println!("channel has hangup {}", &member);
                    REDIS
                        .srem(
                            &format!("nebula:conference:{}:members", &self.id),
                            &memeber_channel.id,
                        )
                        .await?;
                }

                let _ = Bridge::unbridge_media(&channel, &memeber_channel).await;
                let _ = Bridge::unbridge_media(&memeber_channel, &channel).await;
            }
        }

        let members: Vec<String> = REDIS
            .smembers(&format!("nebula:conference:{}:members", &self.id))
            .await?;
        if members.len() == 0 {
            REDIS
                .del(&format!("nebula:conference:{}:stream", &self.id))
                .await?;
        }

        Ok(())
    }

    async fn play_alone(&self) -> Result<()> {
        let members: Vec<String> = REDIS
            .smembers(&format!("nebula:conference:{}:members", &self.id))
            .await?;
        if members.len() == 1 {
            let channel = Channel::new(members[0].clone());
            let sound_id = nebula_utils::uuid();
            let sounds = vec![
                "tts://You are currently the only person in this conference."
                    .to_string(),
            ];
            tokio::select! {
                _ = channel.play_sound(Some(sound_id.clone()), sounds, true) => {}
                _ = channel.receive_event("0", ChannelEvent::Hangup) => {
                    return Ok(());
                }
                _ = self.receive_event("$", ConferenceEvent::MemberJoin) => {
                    channel.stop_sound(&sound_id).await?;
                    return Ok(());
                }
            }

            if let Some(moh) = self.moh.as_ref() {
                let stream_id = channel.get_audio_id().await?;
                SWITCHBOARD_SERVICE
                    .media_rpc_client
                    .play_sound(
                        moh.play_id.clone(),
                        channel.id.clone(),
                        stream_id,
                        moh.sounds.clone(),
                        moh.random,
                        0,
                        true,
                    )
                    .await?;
                tokio::select! {
                    _ = channel.receive_event("0", ChannelEvent::Hangup) => {}
                    _ = self.receive_event("$", ConferenceEvent::MemberJoin) => {
                        channel.stop_sound(&moh.play_id).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn receive_event(
        &self,
        key_id: &str,
        event_name: ConferenceEvent,
    ) -> Result<(String, String)> {
        let start = Instant::now();
        let event_name = event_name.to_string();
        let stream = format!("nebula:conference:{}:stream", &self.id);
        let mut key_id = key_id.to_string();
        loop {
            if let Ok((new_key, event, value)) =
                REDIS.xread_next_entry_timeout(&stream, &key_id, 1000).await
            {
                key_id = new_key;
                if event == event_name {
                    return Ok((key_id, value));
                }
            }
            if start.elapsed().as_secs() > 86400 {
                return Err(anyhow::anyhow!("can't receive the event name"));
            }
        }
    }

    async fn chairman_joined(&self) -> Result<bool> {
        let key = format!("nebula:conference:{}:chairmans", &self.id);
        for c in REDIS
            .smembers::<Vec<String>>(&key)
            .await
            .unwrap_or(Vec::new())
            .iter()
        {
            let c = Channel::new(c.to_string());
            if !c.is_hangup().await {
                return Ok(true);
            } else {
                REDIS.srem(&key, &c.id).await?;
            }
        }
        Ok(false)
    }

    pub async fn new_event(
        &self,
        event: ConferenceEvent,
        value: &str,
    ) -> Result<()> {
        REDIS
            .xadd_maxlen(
                &format!("nebula:conference:{}:stream", &self.id),
                &event.to_string(),
                value,
                100,
            )
            .await?;
        Ok(())
    }

    pub async fn join(&self, channel: &Channel, is_chairman: bool) -> Result<()> {
        println!("conference join");
        REDIS
            .sadd(
                &format!("nebula:conference:{}:members", &self.id),
                &channel.id,
            )
            .await?;
        if is_chairman {
            REDIS
                .sadd(
                    &format!("nebula:conference:{}:chairmans", &self.id),
                    &channel.id,
                )
                .await?;
        }
        self.new_event(ConferenceEvent::MemberJoin, &channel.id)
            .await?;
        let members: Vec<String> = REDIS
            .smembers(&format!("nebula:conference:{}:members", &self.id))
            .await?;
        let chairman_joined = self.chairman_joined().await?;

        if self.needs_chairman && !chairman_joined {
            if let Some(moh) = self.moh.as_ref() {
                let stream_id = channel.get_audio_id().await?;
                SWITCHBOARD_SERVICE
                    .media_rpc_client
                    .play_sound(
                        moh.play_id.clone(),
                        channel.id.clone(),
                        stream_id,
                        moh.sounds.clone(),
                        moh.random,
                        0,
                        true,
                    )
                    .await?;
            }
            loop {
                tokio::select! {
                    _ = channel.receive_event("0", ChannelEvent::Hangup) => {}
                    _ = self.receive_event("$", ConferenceEvent::MemberJoin) => {
                        if self.chairman_joined().await? {
                            if let Some(moh) = self.moh.as_ref() {
                                channel.stop_sound(&moh.play_id).await?;
                            }
                            break;
                        }
                    }
                }
            }
        } else {
            for member in members.iter() {
                if member != &channel.id {
                    if !self.no_joinleave {
                        let channel_id = channel.id.clone();
                        let member = member.to_string();
                        tokio::spawn(async move {
                            let _ = Channel::play_sound_to_channel(
                                &member,
                                None,
                                vec![
                                    format!("redis://{}-name", &channel_id),
                                    "tts://has entered the conference".to_string(),
                                ],
                                1,
                                false,
                            )
                            .await;
                        });
                    }
                }
            }
        }

        for member in members.iter() {
            if member != &channel.id {
                let member_channel = Channel::new(member.to_string());
                let _ = Bridge::bridge_media(&channel, &member_channel).await;
                let _ = Bridge::bridge_media(&member_channel, &channel).await;
            }
        }
        self.play_alone().await?;
        Ok(())
    }
}
