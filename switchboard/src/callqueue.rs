use super::server::SWITCHBOARD_SERVICE;
use crate::{
    bridge::{Bridge, BridgeExtension},
    channel::{Channel, ChannelBridgeResult},
    switchboard::Switchboard,
};
use anyhow::{anyhow, Result};
use chrono::Utc;
use futures;
use futures::stream::{self, StreamExt};
use itertools::Itertools;
use nebula_db::{
    message::{ChannelEvent, Endpoint},
    models::{
        MusicOnHold, QueueGroup, QueueHint, QueueHintAudio, QueueRecordChange, User,
    },
};
use nebula_redis::REDIS;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::HashMap, sync::Arc};
use std::{collections::HashSet, str::FromStr};
use strum_macros;
use strum_macros::EnumString;
use tokio::sync::{Mutex, MutexGuard};
use tokio::{self, time::Instant};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(strum_macros::Display, EnumString, PartialEq, Eq, Clone, Debug, Copy)]
pub enum CallQueueResult {
    #[strum(serialize = "ok")]
    Ok,
    #[strum(serialize = "fail")]
    Fail,
    #[strum(serialize = "timeout")]
    Timeout,
    #[strum(serialize = "ring_timeout")]
    RingTimeout,
    #[strum(serialize = "max_waiting_reached")]
    MaxWaitingReached,
    #[strum(serialize = "shortcode")]
    Shortcode,
    #[strum(serialize = "no_registered")]
    NoRegistered,
    #[strum(serialize = "pickup")]
    Pickup,
}

#[derive(Clone)]
pub struct CallQueue {
    pub id: String,
    pub tenant_id: String,
    pub record_id: Uuid,
    pub switchboard: Switchboard,
    pub group: QueueGroup,
    pub start_time: SystemTime,
    pub start_time_unix: String,
    pub waiting_time: u64,
    pub ring_timeout: u64,
    pub max_waiting: u64,
    pub timeout_time: Option<SystemTime>,
    pub moh: Option<MusicOnHold>,
    pub hint: Option<QueueHint>,
    pub ring_audio: String,
    pub ringtone: String,
    pub announcement: String,
    pub queue_members: Arc<Mutex<Vec<BridgeExtension>>>,
    pub raw_members: Vec<serde_json::Value>,
    // the last updated time for the hunt group if the member only contains one
    // hunt group, and we'll use this time to decide when to refresh the members
    pub member_hunt_group_updated: Arc<Mutex<Option<SystemTime>>>,
    pub no_registered: String,
    pub shortcode: String,
    pub callback: String,
    pub in_callback: Arc<Mutex<bool>>,
    pub tried_members: Arc<Mutex<HashSet<String>>>,
    pub answer_at: Arc<Mutex<Option<SystemTime>>>,
    pub failed_users: Arc<Mutex<HashMap<String, usize>>>,
    pub failed_times: Arc<Mutex<usize>>,
}

impl CallQueue {
    pub async fn run(&self) -> Result<CallQueueResult> {
        println!("now run call queue");
        let result = self.wait_position().await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                info!(
                    channel = self.switchboard.channel.id,
                    queue = self.id,
                    "channel {} in queue {} wait got error {e:#}",
                    self.switchboard.channel.id,
                    self.id,
                );
                CallQueueResult::Fail
            }
        };
        info!(
            "channel {} in queue {} wait done, result is {result}",
            self.switchboard.channel.id, self.id,
        );
        self.switchboard
            .channel
            .new_event(ChannelEvent::CallQueueWaitDone, &result.to_string())
            .await?;
        println!("wait position result {:?}", result);
        if result != CallQueueResult::Ok {
            return Ok(result);
        }

        let record_id = self.record_id;
        tokio::spawn(async move {
            let _ = SWITCHBOARD_SERVICE
                .db
                .update_queue_record(
                    record_id,
                    QueueRecordChange {
                        ready: Some(Utc::now()),
                        disposition: Some("ringing".to_string()),
                        ..Default::default()
                    },
                )
                .await;
        });

        if !self.ring_audio.is_empty() {
            info!(
                channel = self.switchboard.channel.id,
                queue = self.id,
                "channel {} in queue {} start to play ring audio {}",
                self.switchboard.channel.id,
                self.id,
                self.ring_audio,
            );
            self.switchboard
                .channel
                .play_sound(None, vec![self.ring_audio.clone()], true)
                .await?;
        }

        info!(
            channel = self.switchboard.channel.id,
            queue = self.id,
            "channel {} in queue {} start to bridge users",
            self.switchboard.channel.id,
            self.id
        );

        let ring_dealine = if self.ring_timeout > 0 {
            Some(Instant::now() + Duration::from_secs(self.ring_timeout))
        } else {
            None
        };

        loop {
            if self.switchboard.channel.is_hangup().await {
                return Ok(CallQueueResult::Fail);
            }

            let result = self.bridge(ring_dealine.as_ref()).await;
            let (bridge_result, member_ids) = match result {
                Ok(result) => result,
                Err(e) => {
                    warn!(
                        channel = self.switchboard.channel.id,
                        queue = self.id,
                        "channel {} in queue {} got error: {e}",
                        self.switchboard.channel.id,
                        self.id
                    );
                    (ChannelBridgeResult::Noanswer, Vec::new())
                }
            };

            match bridge_result {
                ChannelBridgeResult::Noanswer
                | ChannelBridgeResult::NoanswerWithCompletedElsewhere
                | ChannelBridgeResult::Busy => {
                    {
                        *self.failed_times.lock().await += 1;
                    }
                    let mut failed_users = self.failed_users.lock().await;
                    for m in &member_ids {
                        if !failed_users.contains_key(m) {
                            failed_users.insert(m.to_string(), 0);
                        }
                        *failed_users.get_mut(m).unwrap() += 1;
                    }
                }
                ChannelBridgeResult::Ok => return Ok(CallQueueResult::Ok),
                ChannelBridgeResult::Unavailable => {
                    return Ok(CallQueueResult::NoRegistered)
                }
                ChannelBridgeResult::Cancelled => {
                    return Ok(CallQueueResult::RingTimeout);
                }
            }
            if let Some(deadline) = ring_dealine.as_ref() {
                if deadline < &Instant::now() {
                    return Ok(CallQueueResult::RingTimeout);
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn is_timeout(&self) -> bool {
        self.timeout_time
            .map(|t| SystemTime::now() >= t)
            .unwrap_or(false)
    }

    async fn play_hint(&self, sound_id: &str, audio: &QueueHintAudio) -> Result<()> {
        let sounds: Vec<String> = stream::iter(&audio.files)
            .filter_map(|f| async move {
                Some(match f.as_ref() {
                    "position" => {
                        format!("tts://{}", self.position().await.unwrap_or(0) + 1)
                    }
                    "people_before" => {
                        format!("tts://{}", self.position().await.unwrap_or(0))
                    }
                    "average_waiting_time" => {
                        let average_waiting_time =
                            self.average_waiting_time().await.unwrap_or(0);
                        if average_waiting_time < 60 {
                            "tts://less than one minute".to_string()
                        } else {
                            let minutes = average_waiting_time / 60;
                            format!(
                                "tts://{} {}",
                                minutes,
                                if minutes > 1 { "minutes" } else { "minute" }
                            )
                        }
                    }
                    _ => f.clone(),
                })
            })
            .collect()
            .await;
        println!("play hint {:?} {:?}", &audio.files, &sounds);
        self.switchboard
            .channel
            .play_sound(Some(sound_id.to_string()), sounds, true)
            .await?;
        Ok(())
    }

    pub async fn wait_audio(&self) -> Result<()> {
        if let Some(moh) = self.moh.as_ref() {
            self.switchboard.channel.play_moh(moh).await?;
        }

        if let Some(hint) = self.hint.as_ref() {
            let hint_audios: Vec<QueueHintAudio> =
                serde_json::from_str(&hint.audios)?;
            println!("hint is {}, {:?}", &hint.audios, &hint_audios);

            if !hint_audios.is_empty() && hint_audios[0].count.is_some() {
                self.wait_audio_new(hint_audios).await?;
                return Ok(());
            }

            if !hint_audios.is_empty() && hint.frequency > 0 {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(hint.frequency as u64)) => {
                        }
                        _ = self.switchboard.channel
                            .receive_event(&self.start_time_unix, ChannelEvent::CallQueueWaitDone) => {
                                self.switchboard.channel.stop_moh().await?;
                                return Ok(());
                            }
                    };

                    // check in callback before play hint audio
                    if *self.in_callback.lock().await {
                        continue;
                    }

                    // now we play hint audio

                    let waiting_time =
                        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
                            - self.start_time.duration_since(UNIX_EPOCH)?.as_secs();
                    let mut hint_audio = &hint_audios[hint_audios.len() - 1];
                    for a in &hint_audios {
                        if waiting_time > a.waiting_time.unwrap_or(60)
                            && !a.files.is_empty()
                        {
                            hint_audio = a;
                        }
                    }

                    self.switchboard.channel.stop_moh().await?;

                    let sound_id = nebula_utils::uuid();
                    tokio::select! {
                        _ = self.play_hint(&sound_id, hint_audio) => {
                        }
                        _ = self.switchboard.channel
                            .receive_event(&self.start_time_unix, ChannelEvent::CallQueueWaitDone) => {
                                self.switchboard.channel.stop_sound(&sound_id).await?;
                                return Ok(());
                            }
                    }

                    // check in callback before play moh again
                    if *self.in_callback.lock().await {
                        continue;
                    }

                    if let Some(moh) = self.moh.as_ref() {
                        self.switchboard.channel.play_moh(moh).await?;
                    }
                }
            }
        }

        self.switchboard
            .channel
            .receive_event(&self.start_time_unix, ChannelEvent::CallQueueWaitDone)
            .await?;
        println!("wait for call queue done");
        self.switchboard.channel.stop_moh().await?;
        Ok(())
    }

    async fn wait_audio_new(&self, hint_audios: Vec<QueueHintAudio>) -> Result<()> {
        println!("wait audio new");
        for audio in hint_audios {
            let mut i = 0;
            let count = audio.count.unwrap_or(0);
            while count <= 0 || i < count {
                let delay = if i == 0 {
                    audio.delay.unwrap_or(60)
                } else {
                    audio.frequency.unwrap_or(60)
                };
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                    _ = self.switchboard.channel
                        .receive_event(&self.start_time_unix, ChannelEvent::CallQueueWaitDone) => {
                            self.switchboard.channel.stop_moh().await?;
                            return Ok(());
                        }
                };

                // check in callback before play hint audio
                if *self.in_callback.lock().await {
                    continue;
                }

                self.switchboard.channel.stop_moh().await?;
                let sound_id = nebula_utils::uuid();
                tokio::select! {
                    _ = self.switchboard
                        .channel
                        .play_sound(Some(sound_id.clone()), audio.files.clone(), true) => {}
                    _ = self.switchboard.channel
                        .receive_event(&self.start_time_unix, ChannelEvent::CallQueueWaitDone) => {
                            self.switchboard.channel.stop_sound(&sound_id).await?;
                            return Ok(());
                        }
                };

                // check in callback before play moh again
                if *self.in_callback.lock().await {
                    continue;
                }

                if let Some(moh) = self.moh.as_ref() {
                    self.switchboard.channel.play_moh(moh).await?;
                }
                i += 1
            }
        }
        self.switchboard
            .channel
            .receive_event(&self.start_time_unix, ChannelEvent::CallQueueWaitDone)
            .await?;
        self.switchboard.channel.stop_moh().await?;
        Ok(())
    }

    async fn queue_answered(&self, answer_channel: &str) -> Result<()> {
        let _ = self.add_waiting_time().await;
        self.remove_call().await;
        *self.answer_at.lock().await = Some(SystemTime::now());

        let channel = Channel::new(answer_channel.to_string());
        let context = channel.get_call_context().await?;
        let answer_by = match &context.endpoint {
            Endpoint::User { user, .. } => Some(user.uuid.clone()),
            _ => None,
        };

        let record_id = self.record_id;
        tokio::spawn(async move {
            if let Ok(queue_record) = SWITCHBOARD_SERVICE
                .db
                .update_queue_record(
                    record_id,
                    QueueRecordChange {
                        answer: Some(Utc::now()),
                        answer_by,
                        disposition: Some("answered".to_string()),
                        ..Default::default()
                    },
                )
                .await
            {
                if let Some(ready) = queue_record.ready {
                    if let Some(answer) = queue_record.answer {
                        let _ = SWITCHBOARD_SERVICE
                            .db
                            .update_queue_record(
                                record_id,
                                QueueRecordChange {
                                    ring_duration: Some(
                                        answer
                                            .signed_duration_since(ready)
                                            .num_seconds(),
                                    ),
                                    ..Default::default()
                                },
                            )
                            .await;
                    }
                }
            }
        });

        let _ = self.incr_successful_calls().await;
        Ok(())
    }

    async fn bridge_result(&self, bridge: &Bridge) -> Result<ChannelBridgeResult> {
        let mut answer_channel = "".to_string();
        let mut stream_key = "0".to_string();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(86400)) => {
                    return Err(anyhow!("never received bridge hangup"));
                }
                (key_id, event_name, event_value) = bridge.next_event(&stream_key) => {
                    stream_key = key_id;
                    let event = ChannelEvent::from_str(&event_name)?;
                    match event {
                        ChannelEvent::Hangup => {
                            let channel = Channel::new(event_value);
                            if let Ok(context) = channel.get_call_context().await {
                                if let Endpoint::User { user, .. } = &context.endpoint {
                                    println!("user hangup {:?}", user.extension);
                                    if let Ok(result) = bridge
                                        .endpoint_hangup_result(&context.endpoint)
                                        .await
                                    {
                                        self.set_waiting(user, &result).await?;
                                    }
                                }
                            }
                        }
                        ChannelEvent::BridgeAnswer => {
                            answer_channel = event_value.clone();
                            let _ = self.queue_answered(&event_value).await;
                        }
                        ChannelEvent::BridgeHangup => {
                            println!("bridge hangup");
                            if self.group.duration > 0 {
                                let expire = self.group.duration as u64;
                                let queue_id = self.id.clone();
                                let members = self.members().await.to_owned();
                                let answer_at = { *self.answer_at.lock().await };
                                tokio::spawn(Self::call_end(
                                    queue_id,
                                    members,
                                    answer_at,
                                    answer_channel,
                                    expire,
                                ));
                            }
                            return Ok(ChannelBridgeResult::from_str(&event_value)?);
                        }
                        _ => (),
                    };
                }
            }
        }
    }

    async fn members(&self) -> MutexGuard<Vec<BridgeExtension>> {
        self.refresh_members().await;
        self.queue_members.lock().await
    }

    // if the members only contains one hunt group, check the hunt group updated_at,
    // and decide if we re-parse the members from raw_members
    async fn refresh_members(&self) -> Option<()> {
        if self.raw_members.len() == 1
            && self.raw_members[0].get("type")?.as_str()?.to_lowercase()
                == "huntgroup"
        {
            let group_uuid = self.raw_members[0].get("uuid")?.as_str()?;
            let hunt_group =
                SWITCHBOARD_SERVICE.db.get_hunt_group(group_uuid).await?;
            if hunt_group.updated_at != *self.member_hunt_group_updated.lock().await
            {
                *self.member_hunt_group_updated.lock().await = hunt_group.updated_at;
                let queue_members = stream::iter(&self.raw_members)
                    .filter_map(|m| async move {
                        format_queue_member(&self.tenant_id, m).await
                    })
                    .collect::<Vec<Vec<BridgeExtension>>>()
                    .await
                    .into_iter()
                    .flatten()
                    .unique_by(|e| e.id())
                    .collect::<Vec<_>>();
                *self.queue_members.lock().await = queue_members;
            }
        }

        Some(())
    }

    async fn bridge(
        &self,
        ring_deadline: Option<&Instant>,
    ) -> Result<(ChannelBridgeResult, Vec<String>)> {
        if !self.is_available().await {
            if let Some(moh) = self.moh.as_ref() {
                self.switchboard.channel.play_moh(moh).await?;
            }
            return Ok((ChannelBridgeResult::Noanswer, Vec::new()));
        }
        self.switchboard.channel.stop_moh().await?;
        let members = match self.group.strategy.to_lowercase().as_ref() {
            "sequential" => self.sequential(false).await,
            "sequential_from_first" => self.sequential(true).await,
            "longestidle" => self.longest_idle().await,
            "fewestcalls" => self.fewest_calls().await,
            "leasttalkingtime" => self.least_talking_time().await,
            _ => self.available_members().await,
        };

        if members.is_empty() {
            self.tried_members.lock().await.clear();
            return Ok((ChannelBridgeResult::Noanswer, Vec::new()));
        }
        let member_ids: Vec<String> = members.iter().map(|m| m.id()).collect();
        let members = if self.group.ring_progressively
            && self.group.strategy.to_lowercase().as_str() != "ringall"
        {
            let tried_members = self.tried_members.lock().await;
            let current_members = self.members().await.to_owned();
            [
                members,
                current_members
                    .into_iter()
                    .filter(|m| tried_members.contains(&m.id()))
                    .collect::<Vec<_>>(),
            ]
            .concat()
        } else {
            members
        };

        let ring_timeout = if self.group.ring_timeout != 0 {
            self.group.ring_timeout as u64
        } else {
            10
        };

        let bridge = self
            .switchboard
            .channel
            .bridge_extensions(
                &self.tenant_id,
                members.iter().collect(),
                &self.ringtone,
                &self.announcement,
                ring_timeout,
                None,
                false,
                self.group.hide_missed_calls == Some(true),
            )
            .await?;

        if let Some(deadline) = ring_deadline.copied() {
            let local_bridge = bridge.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                let _ = local_bridge.cancel().await;
            });
        }

        for member in members.iter() {
            match member {
                BridgeExtension::User(u) => {
                    let key = format!(
                        "nebula:callqueue:{}:user:{}:waiting_lock",
                        &self.id, u.uuid
                    );
                    REDIS.setex(&key, 86400, "1").await?;
                    let key = format!(
                        "nebula:callqueue:user:{}:global_waiting_lock",
                        u.uuid
                    );
                    REDIS.setex(&key, 86400, "1").await?;
                }
                BridgeExtension::Group(g) => {
                    for u in g.members.iter() {
                        let key = format!(
                            "nebula:callqueue:{}:user:{}:waiting_lock",
                            &self.id, u.uuid
                        );
                        REDIS.setex(&key, 86400, "1").await?;
                        let key = format!(
                            "nebula:callqueue:user:{}:global_waiting_lock",
                            u.uuid
                        );
                        REDIS.setex(&key, 86400, "1").await?;
                    }
                }
                _ => (),
            }
        }

        let result = match self.bridge_result(&bridge).await {
            Ok(result) => result,
            Err(e) => {
                println!("bridge got error {}", e);
                bridge.hangup(ChannelBridgeResult::Noanswer).await?;
                ChannelBridgeResult::Noanswer
            }
        };
        let _ = REDIS
            .xadd::<String>(
                &format!("nebula:bridge:{}:stream", &bridge.id),
                &ChannelEvent::BridgeHangupAck.to_string(),
                "ack",
            )
            .await;

        for member_id in &member_ids {
            self.tried_members.lock().await.insert(member_id.clone());
        }

        let mut failed_users = HashSet::new();
        for member in members {
            match member {
                BridgeExtension::User(user) => {
                    failed_users.insert(user.uuid.clone());
                }
                BridgeExtension::Group(group) => {
                    for user in &group.members {
                        failed_users.insert(user.uuid.clone());
                    }
                }
                BridgeExtension::Trunk(_, _, _, _) => {}
                BridgeExtension::ExternalNumber(_, _) => {}
                BridgeExtension::Intra(_, _) => {}
                BridgeExtension::InterTenant { .. } => {}
            }
        }
        Ok((result, failed_users.into_iter().collect()))
    }

    async fn call_end(
        queue_id: String,
        members: Vec<BridgeExtension>,
        answer_at: Option<SystemTime>,
        answer_channel: String,
        expire: u64,
    ) -> Result<()> {
        let answer_at = answer_at.ok_or_else(|| anyhow!("call wasn't answered"))?;
        let now = SystemTime::now();
        let answer_channel = Channel::new(answer_channel);
        let context = answer_channel.get_call_context().await?;
        let user = context
            .endpoint
            .user()
            .map(|u| u.name.to_lowercase())
            .unwrap_or_else(|| "".to_string());

        for member in &members {
            if member.has_user(&user) {
                let call_key = &format!(
                    "nebula:callqueue:{}:member:{}:call:{}",
                    &queue_id,
                    &member.id(),
                    answer_channel.id
                );
                REDIS
                    .hmset(
                        call_key,
                        vec![
                            (
                                "end",
                                &now.duration_since(UNIX_EPOCH)?
                                    .as_secs()
                                    .to_string(),
                            ),
                            (
                                "duration",
                                &answer_at.elapsed()?.as_secs().to_string(),
                            ),
                        ],
                    )
                    .await?;
                REDIS.expire(call_key, expire).await?;
                let calls_key = &format!(
                    "nebula:callqueue:{}:member:{}:calls",
                    &queue_id,
                    &member.id()
                );
                REDIS.rpush(calls_key, &answer_channel.id).await?;
                REDIS.expire(calls_key, expire).await?;
                return Ok(());
            }
        }
        Ok(())
    }

    async fn member_talking_time(&self, member: &BridgeExtension) -> Result<usize> {
        let calls: Vec<String> = REDIS
            .lrange(
                &format!(
                    "nebula:callqueue:{}:member:{}:calls",
                    &self.id,
                    member.id()
                ),
                0,
                -1,
            )
            .await?;
        Ok(stream::iter(calls)
            .fold(0, |acc, call| async move {
                let duration = REDIS
                    .hget(
                        &format!(
                            "nebula:callqueue:{}:member:{}:call:{}",
                            &self.id,
                            member.id(),
                            call
                        ),
                        "duration",
                    )
                    .await
                    .unwrap_or(0);
                duration + acc
            })
            .await)
    }

    async fn member_num_calls(&self, member: &BridgeExtension) -> Result<usize> {
        let calls: Vec<String> = REDIS
            .lrange(
                &format!(
                    "nebula:callqueue:{}:member:{}:calls",
                    &self.id,
                    member.id()
                ),
                0,
                -1,
            )
            .await?;
        Ok(stream::iter(calls)
            .fold(0, |acc, call| async move {
                let duration = REDIS
                    .hget(
                        &format!(
                            "nebula:callqueue:{}:member:{}:call:{}",
                            &self.id,
                            member.id(),
                            call
                        ),
                        "duration",
                    )
                    .await
                    .unwrap_or(0);
                if duration > 0 {
                    acc + 1
                } else {
                    acc
                }
            })
            .await)
    }

    async fn member_last_call_ended(
        &self,
        member: &BridgeExtension,
    ) -> Result<usize> {
        let calls: Vec<String> = REDIS
            .lrange(
                &format!(
                    "nebula:callqueue:{}:member:{}:calls",
                    &self.id,
                    member.id()
                ),
                -1,
                -1,
            )
            .await?;
        if calls.is_empty() {
            return Err(anyhow!("now call"));
        }
        let end: usize = REDIS
            .hget(
                &format!(
                    "nebula:callqueue:{}:member:{}:call:{}",
                    &self.id,
                    member.id(),
                    &calls[0]
                ),
                "end",
            )
            .await?;
        Ok(end)
    }

    async fn available_members(&self) -> Vec<BridgeExtension> {
        let members = self.members().await.to_owned();
        stream::iter(members)
            .filter_map(|m| async move {
                if self.is_member_available(&m).await.unwrap_or(false) {
                    Some(m)
                } else {
                    None
                }
            })
            .collect()
            .await
    }

    async fn longest_idle(&self) -> Vec<BridgeExtension> {
        let mut earliest = 0;
        let mut selected = None;
        for member in self.available_members().await {
            if self.is_member_tried(&member).await {
                continue;
            }
            if let Ok(last_call_ended) = self.member_last_call_ended(&member).await {
                if earliest == 0 || last_call_ended < earliest {
                    earliest = last_call_ended;
                    selected = Some(member);
                }
            } else {
                return vec![member];
            }
        }
        if let Some(selected) = selected {
            vec![selected]
        } else {
            vec![]
        }
    }

    async fn least_talking_time(&self) -> Vec<BridgeExtension> {
        let mut least = 0;
        let mut selected = None;
        for user in self.available_members().await {
            if self.is_member_tried(&user).await {
                continue;
            }
            let talking_time = self.member_talking_time(&user).await.unwrap_or(0);
            if talking_time == 0 {
                return vec![user];
            }
            if least == 0 || talking_time < least {
                least = talking_time;
                selected = Some(user);
            }
        }
        if let Some(selected) = selected {
            vec![selected]
        } else {
            vec![]
        }
    }

    async fn fewest_calls(&self) -> Vec<BridgeExtension> {
        let mut fewest = 0;
        let mut selected = None;
        for user in self.available_members().await {
            if self.is_member_tried(&user).await {
                continue;
            }
            let num_calls = self.member_num_calls(&user).await.unwrap_or(0);
            if num_calls == 0 {
                return vec![user];
            }
            if fewest == 0 || num_calls < fewest {
                fewest = num_calls;
                selected = Some(user);
            }
        }
        if let Some(selected) = selected {
            vec![selected]
        } else {
            vec![]
        }
    }

    async fn sequential(&self, from_first: bool) -> Vec<BridgeExtension> {
        let members = self.members().await.to_owned();

        let n = if from_first {
            0
        } else {
            let n = self.successful_calls().await.unwrap_or(0);
            n % members.len()
        };
        for i in [
            (n..members.len()).collect::<Vec<usize>>(),
            (0..n).collect::<Vec<usize>>(),
        ]
        .concat()
        {
            let member = members[i].clone();
            if self.is_member_available(&member).await.unwrap_or(false)
                && !self.is_member_tried(&member).await
            {
                return vec![member];
            }
        }
        Vec::new()
    }

    async fn successful_calls(&self) -> Result<usize> {
        let n: usize = REDIS
            .get(&format!(
                "nebula:callqueue:{}:{}",
                &self.id, "successful_calls"
            ))
            .await?;
        Ok(n)
    }

    async fn incr_successful_calls(&self) -> Result<()> {
        let key = &format!("nebula:callqueue:{}:{}", &self.id, "successful_calls");
        let n = REDIS.incr(key).await?;
        if n == self.members().await.len() as i64 {
            let _ = REDIS.set(key, "0").await?;
        }
        let _ = REDIS.expire(key, 86400).await;
        Ok(())
    }

    fn check_callback(&self, pressed: Arc<Mutex<bool>>) {
        let channel = self.switchboard.channel.clone();
        if !self.callback.is_empty() {
            let callback = self.callback.clone();
            let start_time_unix = self.start_time_unix.clone();
            tokio::spawn(async move {
                let mut key = start_time_unix;
                loop {
                    if let Ok((key_id, event, value)) =
                        channel.next_event(&key).await
                    {
                        key = key_id;
                        if let Ok(event) = ChannelEvent::from_str(&event) {
                            match event {
                                ChannelEvent::Dtmf => {
                                    if value == callback {
                                        *pressed.lock().await = true;
                                        continue;
                                    }
                                }
                                ChannelEvent::Hangup
                                | ChannelEvent::CallQueueWaitDone
                                | ChannelEvent::SwitchCallflow => return,
                                _ => continue,
                            }
                        }
                    }
                }
            });
        }
    }

    fn check_shortcode(&self, pressed: Arc<Mutex<bool>>) {
        let channel = self.switchboard.channel.clone();
        if !self.shortcode.is_empty() {
            let shortcode = self.shortcode.clone();
            let start_time_unix = self.start_time_unix.clone();
            tokio::spawn(async move {
                let mut key = start_time_unix;
                loop {
                    if let Ok((key_id, event, value)) =
                        channel.next_event(&key).await
                    {
                        key = key_id;
                        if let Ok(event) = ChannelEvent::from_str(&event) {
                            match event {
                                ChannelEvent::Dtmf => {
                                    if value == shortcode {
                                        *pressed.lock().await = true;
                                        return;
                                    }
                                }
                                ChannelEvent::Hangup
                                | ChannelEvent::CallQueueWaitDone
                                | ChannelEvent::SwitchCallflow => return,
                                _ => continue,
                            }
                        }
                    }
                }
            });
        }
    }

    async fn wait_position(&self) -> Result<CallQueueResult> {
        if !self.no_registered.is_empty() && self.no_registered_member().await {
            return Ok(CallQueueResult::NoRegistered);
        }

        let shortcode_pressed = Arc::new(Mutex::new(false));
        if !self.shortcode.is_empty() {
            self.check_shortcode(shortcode_pressed.clone());
        }

        let callback_pressed = Arc::new(Mutex::new(false));
        if !self.callback.is_empty() {
            self.check_callback(callback_pressed.clone());
        }

        let mut position = self.position().await?;

        if self.max_waiting > 0 && position as u64 >= self.max_waiting {
            return Ok(CallQueueResult::MaxWaitingReached);
        }

        let picked_up = Arc::new(Mutex::new(false));

        if position > 0 || !self.is_available().await {
            let call_queue = self.clone();
            tokio::spawn(async move {
                let result = call_queue.wait_audio().await;
                println!("wait audio result {:?}", result);
            });

            let picked_up = picked_up.clone();
            let channel = self.switchboard.channel.clone();
            let start_time_unix = self.start_time_unix.clone();
            tokio::spawn(async move {
                if channel
                    .receive_event(&start_time_unix, ChannelEvent::CallQueueWaitDone)
                    .await
                    .is_ok()
                {
                    *picked_up.lock().await = true;
                }
            });
        }

        while position > 0 || !self.is_available().await {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let picked_up = { *picked_up.lock().await };
            if picked_up {
                if let Ok(bridge) = self.switchboard.channel.get_bridge().await {
                    let record_id = self.record_id;
                    tokio::spawn(async move {
                        let _ = SWITCHBOARD_SERVICE
                            .db
                            .update_queue_record(
                                record_id,
                                QueueRecordChange {
                                    ready: Some(Utc::now()),
                                    disposition: Some("ringing".to_string()),
                                    ..Default::default()
                                },
                            )
                            .await;
                    });
                    if let Ok(answer_channel) = bridge.answer_channel().await {
                        let _ = self.queue_answered(&answer_channel).await;
                    }
                    let _ = bridge.wait().await;
                }
                return Ok(CallQueueResult::Pickup);
            }

            if self.switchboard.channel.is_hangup().await {
                info!(
                    channel = self.switchboard.channel.id,
                    queue = self.id,
                    "channel {} in queue {} return Fail because channel is hangup",
                    self.switchboard.channel.id,
                    self.id,
                );
                return Ok(CallQueueResult::Fail);
            }
            if self.is_timeout() {
                return Ok(CallQueueResult::Timeout);
            }
            if !self.shortcode.is_empty() && *shortcode_pressed.lock().await {
                return Ok(CallQueueResult::Shortcode);
            }
            if !self.callback.is_empty() && *callback_pressed.lock().await {
                *self.in_callback.lock().await = true;
                self.switchboard
                    .channel
                    .register_callback(self.moh.clone())
                    .await?;
                *callback_pressed.lock().await = false;
                *shortcode_pressed.lock().await = false;
                *self.in_callback.lock().await = false;
            }
            position = self.position().await?;
        }

        Ok(CallQueueResult::Ok)
    }

    pub async fn add_call(&self) -> Result<()> {
        let score = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        REDIS
            .zadd(
                &format!("nebula:callqueue:{}:calls", &self.id),
                score as i64,
                &self.switchboard.channel.id,
            )
            .await?;

        let queue_id = self.id.clone();
        let channel = self.switchboard.channel.clone();
        let key = format!("nebula:callqueue:{}:calls", &self.id);
        tokio::spawn(async move {
            loop {
                if channel.is_hangup().await {
                    return;
                }
                if REDIS.zrank(&key, &channel.id).await.is_err() {
                    return;
                }
                let _ = REDIS
                    .setex(
                        &format!(
                            "nebula:channel:{}:still_in_queue:{queue_id}",
                            &channel.id
                        ),
                        15,
                        "yes",
                    )
                    .await;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
        Ok(())
    }

    pub async fn remove_call(&self) {
        info!(
            channel = self.switchboard.channel.id,
            queue = self.id,
            "remove channel {} from queue {}",
            self.switchboard.channel.id,
            self.id
        );
        let _ = REDIS
            .zrem(
                &format!("nebula:callqueue:{}:calls", &self.id),
                &self.switchboard.channel.id,
            )
            .await;
    }

    async fn average_waiting_time(&self) -> Result<usize> {
        let calls: Vec<String> = REDIS
            .lrange(
                &format!("nebula:callqueue:{}:waiting_times", &self.id),
                0,
                -1,
            )
            .await?;
        let waiting_times: Vec<usize> = stream::iter(calls)
            .filter_map(|call| async move {
                let key =
                    &format!("nebula:callqueue:{}:{}:waiting_time", &self.id, call);
                if let Ok(waiting_time) = REDIS.get::<usize>(key).await {
                    Some(waiting_time)
                } else {
                    let _ = REDIS.lrem(key, 0, &call).await;
                    None
                }
            })
            .collect()
            .await;
        if waiting_times.is_empty() {
            return Ok(0);
        }
        Ok(waiting_times.iter().sum::<usize>() / waiting_times.len())
    }

    async fn add_waiting_time(&self) -> Result<()> {
        let elapsed = self.start_time.elapsed()?.as_secs();
        REDIS
            .setex(
                &format!(
                    "nebula:callqueue:{}:{}:waiting_time",
                    &self.id, &self.switchboard.channel.id
                ),
                elapsed,
                &elapsed.to_string(),
            )
            .await?;

        let key = &format!("nebula:callqueue:{}:waiting_times", &self.id);
        REDIS.rpush(key, &self.switchboard.channel.id).await?;
        REDIS.expire(key, self.waiting_time).await?;
        Ok(())
    }

    pub async fn query_position(&self) -> Result<usize> {
        let key = format!("nebula:callqueue:{}:calls", &self.id);
        let pos = REDIS.zrank(&key, &self.switchboard.channel.id).await?;
        Ok(pos)
    }

    async fn position(&self) -> Result<usize> {
        let pos = self.query_position().await?;

        let queue_id = self.id.clone();
        let key = format!("nebula:callqueue:{}:calls", &self.id);
        tokio::spawn(async move {
            // Every second, we do a full check on all the channels in the queue
            // And remove the ones that has hangup
            // This redis lock here is to make sure it's only checked every second,
            // because this method is called by all the channels in the queue
            if REDIS
                .setexnx(
                    &format!("nebula:callqueue:{queue_id}:calls:check_lock"),
                    "yes",
                    1,
                )
                .await
            {
                if let Ok(channels) = REDIS.zrange(&key, 0, -1).await {
                    for channel in channels {
                        let channel = Channel::new(channel);
                        if channel.is_hangup().await {
                            let _ = REDIS.zrem(&key, &channel.id).await;
                        } else {
                            let in_queue_key = &format!(
                                "nebula:channel:{}:still_in_queue:{queue_id}",
                                &channel.id
                            );
                            // if there's no in_queue_key, it means the call queue
                            // running thread for this channel crashed.
                            // So we hangup this call and removed it from queue, so that
                            // it doesn't stuck.
                            if !REDIS.exists(in_queue_key).await.unwrap_or(false) {
                                let _ = channel.hangup(None).await;
                                let _ = REDIS.zrem(&key, &channel.id).await;
                            }
                        }
                    }
                }
            }
        });
        Ok(pos)
    }

    async fn no_registered_member(&self) -> bool {
        let members = self.members().await;
        for member in members.iter() {
            match member {
                BridgeExtension::User(u) => {
                    if u.is_available().await.unwrap_or(false)
                        && u.has_locations().await
                        && u.is_aux_available().await
                        && u.is_queue_login().await
                        && u.is_individual_queue_login(&self.id).await
                    {
                        return false;
                    }
                }
                BridgeExtension::Group(g) => {
                    for u in &g.members {
                        if u.is_available().await.unwrap_or(false)
                            && u.has_locations().await
                            && u.is_aux_available().await
                            && u.is_queue_login().await
                            && u.is_individual_queue_login(&self.id).await
                        {
                            return false;
                        }
                    }
                }
                _ => return false,
            };
        }
        true
    }

    async fn is_available(&self) -> bool {
        for member in self.members().await.iter() {
            if self.is_member_available(member).await.unwrap_or(false) {
                return true;
            }
        }
        false
    }

    async fn set_waiting(
        &self,
        user: &User,
        bridge_result: &ChannelBridgeResult,
    ) -> Result<()> {
        println!("set waiting {}", bridge_result);
        let (wait_type, (wait_timeout, global)) = match bridge_result {
            ChannelBridgeResult::Ok => (
                "answer_wait",
                if user.queue_answer_wait.unwrap_or(0) > 0 {
                    (user.queue_answer_wait.unwrap_or(0), true)
                } else {
                    (self.group.answer_wait, false)
                },
            ),
            ChannelBridgeResult::Noanswer
            | ChannelBridgeResult::NoanswerWithCompletedElsewhere
            | ChannelBridgeResult::Cancelled
            | ChannelBridgeResult::Unavailable => (
                "no_answer_wait",
                if user.queue_no_answer_wait.unwrap_or(0) > 0 {
                    (user.queue_no_answer_wait.unwrap_or(0), true)
                } else {
                    (self.group.no_answer_wait, false)
                },
            ),
            ChannelBridgeResult::Busy => (
                "reject_wait",
                if user.queue_reject_wait.unwrap_or(0) > 0 {
                    (user.queue_reject_wait.unwrap_or(0), true)
                } else {
                    (self.group.reject_wait, false)
                },
            ),
        };
        if wait_timeout > 0 {
            REDIS
                .setex(
                    &format!(
                        "nebula:callqueue:{}:user:{}:{}",
                        if global { "global" } else { &self.id },
                        &user.uuid,
                        wait_type
                    ),
                    wait_timeout as u64,
                    "1",
                )
                .await?;
        }
        REDIS
            .del(&format!(
                "nebula:callqueue:{}:user:{}:waiting_lock",
                &self.id, user.uuid
            ))
            .await?;
        REDIS
            .del(&format!(
                "nebula:callqueue:user:{}:global_waiting_lock",
                user.uuid
            ))
            .await?;
        Ok(())
    }

    async fn is_waiting(&self, user: &User) -> Result<bool> {
        {
            let base_key = format!("nebula:callqueue:global:user:{}", user.uuid);
            let noanswer_wait = format!("{}:{}", base_key, "no_answer_wait");
            let answer_wait = format!("{}:{}", base_key, "answer_wait");
            let reject_wait = format!("{}:{}", base_key, "reject_wait");
            if REDIS.exists(&noanswer_wait).await?
                || REDIS.exists(&answer_wait).await?
                || REDIS.exists(&reject_wait).await?
            {
                return Ok(true);
            }
        }

        {
            let base_key =
                format!("nebula:callqueue:{}:user:{}", &self.id, user.uuid);
            let noanswer_wait = format!("{}:{}", base_key, "no_answer_wait");
            let answer_wait = format!("{}:{}", base_key, "answer_wait");
            let reject_wait = format!("{}:{}", base_key, "reject_wait");
            if REDIS.exists(&noanswer_wait).await?
                || REDIS.exists(&answer_wait).await?
                || REDIS.exists(&reject_wait).await?
            {
                return Ok(true);
            }
        }

        let lock_key = format!(
            "nebula:callqueue:{}:user:{}:waiting_lock",
            &self.id, user.uuid
        );
        let waiting_lock = REDIS.exists(&lock_key).await?;
        if waiting_lock {
            println!("in waiting lock");
            let ttl = REDIS.ttl(&lock_key).await?;
            if ttl > 10 || ttl == -1 {
                REDIS.expire(&lock_key, 10).await?;
            }
        }

        let global_lock_key =
            format!("nebula:callqueue:user:{}:global_waiting_lock", user.uuid);
        let global_waiting_lock = REDIS.exists(&global_lock_key).await?;
        if global_waiting_lock {
            println!("in global waiting lock");
            let ttl = REDIS.ttl(&global_lock_key).await?;
            if ttl > 10 || ttl == -1 {
                REDIS.expire(&global_lock_key, 10).await?;
            }
        }

        Ok(waiting_lock || global_waiting_lock)
    }

    async fn is_member_tried(&self, member: &BridgeExtension) -> bool {
        self.tried_members.lock().await.get(&member.id()).is_some()
    }

    async fn is_member_available(&self, member: &BridgeExtension) -> Result<bool> {
        match member {
            BridgeExtension::User(u) => self.is_user_available(u).await,
            BridgeExtension::Group(g) => {
                for u in &g.members {
                    if self.is_user_available(u).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => Ok(true),
        }
    }

    async fn user_in_call(&self, user: &User) -> Result<bool> {
        for channel in user.channels().await? {
            let channel = Channel::new(channel);
            if !channel.is_hangup().await {
                return Ok(true);
            }
            REDIS
                .srem(
                    &format!("nebula:sipuser:{}:channels", user.uuid),
                    &channel.id,
                )
                .await?;
        }
        Ok(false)
    }

    async fn is_user_available(&self, user: &User) -> Result<bool> {
        if self.user_in_call(user).await? {
            return Ok(false);
        }
        if !user.is_available().await? {
            return Ok(false);
        }
        if !user.is_aux_available().await {
            return Ok(false);
        }
        if !user.is_queue_login().await {
            return Ok(false);
        }
        if !user.is_individual_queue_login(&self.id).await {
            return Ok(false);
        }
        if self.is_waiting(user).await? {
            return Ok(false);
        }
        let has_locations = user.has_locations().await;

        if has_locations {
            let key = &format!("nebula:sipuser:{}:queue_available_lock", user.uuid);
            Ok(REDIS.setexnx_or_eq(key, &self.id, 2).await.unwrap_or(false))
        } else {
            Ok(false)
        }
    }
}

pub async fn format_queue_member(
    tenant_uuid: &str,
    extension: &serde_json::Value,
) -> Option<Vec<BridgeExtension>> {
    let extension_type = extension.get("type")?.as_str()?.to_lowercase();
    match extension_type.as_ref() {
        "sipuser" => {
            let sipuser_uuid = extension.get("uuid")?.as_str()?;
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user(tenant_uuid, sipuser_uuid)
                .await?;
            let tenant_id = user.tenant_id.clone();
            let mut bridge_users = vec![BridgeExtension::User(user)];

            if let Some(users) =
                SWITCHBOARD_SERVICE.db.get_login_users(sipuser_uuid).await
            {
                for user_uuid in users {
                    if user_uuid != sipuser_uuid {
                        if let Some(user) = SWITCHBOARD_SERVICE
                            .db
                            .get_user(&tenant_id, &user_uuid)
                            .await
                        {
                            bridge_users.push(BridgeExtension::User(user));
                        }
                    }
                }
            }

            Some(bridge_users)
        }
        "huntgroup" => {
            let group_uuid = extension.get("uuid")?.as_str()?;
            let group = SWITCHBOARD_SERVICE
                .db
                .get_group(tenant_uuid, group_uuid)
                .await?;

            if let Some(order) = extension.get("order") {
                if order.is_null() {
                    Some(vec![BridgeExtension::Group(group)])
                } else {
                    let mut members = SWITCHBOARD_SERVICE
                        .db
                        .get_group_users(tenant_uuid, group_uuid)
                        .await
                        .unwrap_or_default();
                    for user in members.clone() {
                        if let Some(users) =
                            SWITCHBOARD_SERVICE.db.get_login_users(&user.uuid).await
                        {
                            for user_uuid in users {
                                if user_uuid != user.uuid {
                                    if let Some(user) = SWITCHBOARD_SERVICE
                                        .db
                                        .get_user(tenant_uuid, &user_uuid)
                                        .await
                                    {
                                        members.push(user);
                                    }
                                }
                            }
                        }
                    }
                    let _ = sort_queue_group_members(&mut members, order);
                    Some(members.into_iter().map(BridgeExtension::User).collect())
                }
            } else {
                Some(vec![BridgeExtension::Group(group)])
            }
        }
        _ => None,
    }
}

fn sort_queue_group_members(
    members: &mut Vec<User>,
    order: &serde_json::Value,
) -> Result<()> {
    match order {
        serde_json::Value::String(order) => match order.to_lowercase().as_str() {
            "display_name_az" => {
                members.sort_by_key(|m| m.display_name.clone());
            }
            "display_name_za" => {
                members.sort_by_key(|m| m.display_name.clone());
                members.reverse();
            }
            "extension_asc" => {
                members.sort_by_key(|m| m.extension);
            }
            "extension_desc" => {
                members.sort_by_key(|m| m.extension);
                members.reverse();
            }
            _ => {}
        },
        serde_json::Value::Array(_) => {
            let order: Vec<String> = serde_json::from_value(order.clone())?;
            members.sort_by_key(|m| {
                order
                    .iter()
                    .position(|uuid| uuid == &m.uuid)
                    .unwrap_or(usize::MAX)
            });
        }
        _ => {}
    }
    Ok(())
}
