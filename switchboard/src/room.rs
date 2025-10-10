use std::{
    collections::HashMap,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use chrono::Utc;
use futures::StreamExt;
use jsonrpc_lite::{Error as RpcError, JsonRpc, Params};
use jwt::{SignWithKey, VerifyWithKey};
use nebula_db::{
    api::ALL_CHANNELS,
    message::{
        AuthRequest, AuthorizeMemberRequest, CallContext, CallDirection,
        ConvertToRoom, CreateTempRoom, Endpoint, InviteExtenRequest,
        JoinRoomRequest, JoinRoomResponse, RegisterRoomApi, RemoveMemberRequest,
        RoomMember, SendReactionNotification, SubscribeRoomRequest, UserAgentType,
        UserAuthClaim, VideoAllocation, WatchRoomRequest, JWT_CLAIMS_KEY,
    },
    models::{CdrItemChange, User},
};
use nebula_redis::{redis, REDIS};
use nebula_rpc::message::{
    RoomNotificationMethod, RoomRequestMethod, RoomRpcMessage,
};
use sdp::{
    payload::{PayloadType, Rtpmap},
    sdp::{EXTERNAL_AUDIO_ORDER, INTERNAL_AUDIO_ORDER},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sip::{message::CallerId, transport::TransportType};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    bridge::{Bridge, BridgeExtension, ExternalNumber},
    callflow::get_operator_code,
    channel::{external_caller_id, internal_caller_id, Channel},
    server::SWITCHBOARD_SERVICE,
};

pub const ALL_ROOMS: &str = "nebula:all_rooms";
pub const ROOM_AUDIO_MUTED: &str = "room_audio_muted";
pub const ROOM_AUDIO_MUTED_BY_ADMIN: &str = "room_audio_muted_by_admin";
pub const ROOM_VIDEO_MUTED: &str = "room_video_muted";
pub const ROOM_VIDEO_MUTED_BY_ADMIN: &str = "room_video_muted_by_admin";

#[derive(Clone)]
pub struct Room {
    id: String,
}

#[derive(Serialize, Deserialize)]
struct MemberInfo {
    name: String,
    is_admin: bool,
    addr: String,
    proxy_host: String,
    user: Option<String>,
}

impl Room {
    pub fn new(id: String) -> Self {
        Self { id }
    }

    pub async fn start() {
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        loop {
            ticker.tick().await;
            let _ = Self::tick().await;
        }
    }

    async fn tick() -> Result<()> {
        let mut cursor = 0;
        loop {
            let (new_cursor, rooms) = REDIS.sscan(ALL_ROOMS, cursor, 300).await?;
            for room in rooms {
                tokio::spawn(async move {
                    if REDIS
                        .scard(&format!("nebula:room:{}:members", room))
                        .await
                        .unwrap_or(0)
                        == 0
                    {
                        let _ = REDIS.srem(ALL_ROOMS, &room).await;
                        return;
                    }

                    let room = Room {
                        id: room.to_string(),
                    };

                    let local_room = room.clone();
                    tokio::spawn(async move {
                        let _ = local_room.room_active_speaker_tick().await;
                    });

                    tokio::spawn(async move {
                        let _ = room.room_members_tick().await;
                    });
                });
            }
            if new_cursor == 0 {
                return Ok(());
            }

            cursor = new_cursor;
        }
    }

    pub async fn members(&self) -> Vec<RoomMember> {
        let members = REDIS
            .smembers::<Vec<String>>(&format!("nebula:room:{}:members", self.id))
            .await
            .unwrap_or_default();
        futures::stream::iter(members)
            .filter_map(|m| async move {
                let member_channel = Channel::new(m.to_string());
                let context = member_channel.get_call_context().await.ok()?;
                let name = context.endpoint.name();
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                let mut member = RoomMember {
                    id: m.to_string(),
                    user: context.endpoint.user().map(|u| u.uuid.clone()),
                    name,
                    is_admin: false,
                    hand_raised: false,
                    audio_muted: false,
                    video_muted: false,
                    audio_muted_by_admin: false,
                    video_muted_by_admin: false,
                    is_sip: addr.is_empty(),
                    screen_share: false,
                };
                if let Ok(info) = REDIS
                    .hmget::<Vec<Option<String>>>(
                        &format!("nebula:channel:{}", &m),
                        &[
                            "room_hand_raised",
                            "room_audio_muted",
                            "room_video_muted",
                            ROOM_AUDIO_MUTED_BY_ADMIN,
                            ROOM_VIDEO_MUTED_BY_ADMIN,
                            "room_admin",
                            "room_screen_share",
                        ],
                    )
                    .await
                {
                    member.hand_raised =
                        info.get(0).map(|s| s.as_ref().map(|s| s.as_str()))
                            == Some(Some("yes"));
                    member.audio_muted =
                        info.get(1).map(|s| s.as_ref().map(|s| s.as_str()))
                            == Some(Some("yes"));
                    member.video_muted =
                        info.get(2).map(|s| s.as_ref().map(|s| s.as_str()))
                            == Some(Some("yes"));
                    member.audio_muted_by_admin =
                        info.get(3).map(|s| s.as_ref().map(|s| s.as_str()))
                            == Some(Some("yes"));
                    member.video_muted_by_admin =
                        info.get(4).map(|s| s.as_ref().map(|s| s.as_str()))
                            == Some(Some("yes"));
                    member.is_admin =
                        info.get(5).map(|s| s.as_ref().map(|s| s.as_str()))
                            == Some(Some("yes"));
                    member.screen_share =
                        info.get(6).map(|s| s.as_ref().map(|s| s.as_str()))
                            == Some(Some("yes"));
                }
                Some(member)
            })
            .collect()
            .await
    }

    async fn member_last_level(
        &self,
        member: &str,
        start: &str,
        end: &str,
    ) -> Result<usize> {
        let events: Vec<redis::Value> = REDIS
            .xrevrange_count(
                &format!("nebula:room:{}:members:{}:levels", self.id, member),
                start,
                end,
                1,
            )
            .await?;
        for value in events.iter() {
            let (_key, event): (String, Vec<String>) =
                redis::from_redis_value(value)?;
            if event[0] == "level" {
                if let Ok(level) = event[1].parse::<usize>() {
                    return Ok(level);
                }
            }
        }
        Err(anyhow!("no level"))
    }

    async fn member_last_chunk_levels(
        &self,
        member: &str,
        start: &str,
        end: &str,
    ) -> Result<Vec<usize>> {
        let events: Vec<redis::Value> = REDIS
            .xrevrange_count(
                &format!("nebula:room:{}:members:{}:levels", self.id, member),
                start,
                end,
                5,
            )
            .await?;
        let mut levels = Vec::new();
        for value in events.iter() {
            let (_key, event): (String, Vec<String>) =
                redis::from_redis_value(value)?;
            if event[0] == "level" {
                if let Ok(level) = event[1].parse::<usize>() {
                    levels.push(level);
                }
            }
        }
        Ok(levels)
    }

    async fn member_levels(&self, member: &str, start: &str) -> Result<Vec<usize>> {
        let events: Vec<redis::Value> = REDIS
            .xrange(
                &format!("nebula:room:{}:members:{}:levels", self.id, member),
                start,
                "+",
            )
            .await?;
        let mut levels = Vec::new();
        for value in events.iter() {
            let (_key, event): (String, Vec<String>) =
                redis::from_redis_value(value)?;
            if event[0] == "level" {
                if let Ok(level) = event[1].parse::<usize>() {
                    levels.push(level);
                }
            }
        }
        Ok(levels)
    }

    async fn room_members_tick(&self) -> Result<()> {
        if !REDIS
            .setpxnx(
                &format!("nebula:room:{}:members_tick", self.id),
                "yes",
                1000,
            )
            .await
        {
            return Ok(());
        }

        let members: Vec<String> = REDIS
            .smembers(&format!("nebula:room:{}:members", self.id))
            .await?;
        for member in members.iter() {
            let last_received_time = REDIS
                .hget::<u64>(
                    &format!("nebula:channel:{}", member),
                    "last_rtp_packet",
                )
                .await
                .unwrap_or(0);
            if let Ok(ctime) = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
            {
                if let Some(duration) = ctime.checked_sub(last_received_time) {
                    if duration > 10 {
                        info!(
                            room = self.id,
                            channel = member,
                            "channel leave room due to inactivity of audio"
                        );
                        let channel = Channel::new(member.to_string());
                        let _ = channel.leave_room(&self.id, false).await;
                    }
                }
            }
        }

        Ok(())
    }

    async fn room_active_speaker_tick(&self) -> Result<()> {
        const HIGH: usize = 70;
        const LOW: usize = 40;
        const CHUNK_SIZE: usize = 5;

        if !REDIS
            .setpxnx(
                &format!("nebula:room:{}:active_speaker_tick", self.id),
                "yes",
                300,
            )
            .await
        {
            return Ok(());
        }

        let now = SystemTime::now();
        let start = now
            .checked_sub(Duration::from_secs(1))
            .ok_or_else(|| anyhow::anyhow!("can't sub time"))?
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .to_string();
        let now = now.duration_since(UNIX_EPOCH)?.as_millis().to_string();

        let active = REDIS
            .hget(&format!("nebula:room:{}", self.id), "active")
            .await
            .unwrap_or_else(|_| "".to_string());

        if !active.is_empty() {
            if self
                .member_last_level(&active, &now, &start)
                .await
                .unwrap_or(0)
                >= LOW
            {
                return Ok(());
            }

            if self
                .member_last_chunk_levels(&active, &now, &start)
                .await
                .unwrap_or_else(|_| Vec::new())
                .iter()
                .any(|l| *l > HIGH)
            {
                return Ok(());
            }
        }

        let members: Vec<String> = REDIS
            .smembers(&format!("nebula:room:{}:members", self.id))
            .await?;

        let mut member_levels = Vec::new();
        for member in members.iter() {
            if self
                .member_last_level(member, &now, &start)
                .await
                .unwrap_or(0)
                <= LOW
            {
                continue;
            }

            if self
                .member_last_chunk_levels(member, &now, &start)
                .await
                .unwrap_or_else(|_| Vec::new())
                .iter()
                .any(|l| *l < HIGH)
            {
                continue;
            }

            if let Ok(levels) = self.member_levels(member, &start).await {
                let count = levels
                    .chunks_exact(CHUNK_SIZE)
                    .filter(|chunk| chunk.iter().all(|l| *l > HIGH))
                    .count();
                member_levels.push((member.to_string(), count));
            }
        }
        member_levels.sort_by_key(|(_, count)| *count);
        if let Some((member, _)) = member_levels.last() {
            if member != &active {
                let _ = REDIS
                    .hset(&format!("nebula:room:{}", self.id), "active", member)
                    .await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Self::notify_active_speaker(member, members, now).await;
                println!("new active speaker {member}");
                return Ok(());
            } else {
                return Ok(());
            }
        }

        if !active.is_empty() {
            let _ = REDIS
                .hdel(&format!("nebula:room:{}", self.id), "active")
                .await;
            Self::notify_clear_active_speaker(members).await;
            println!("no active speaker");
        }

        Ok(())
    }

    async fn notify_room_invite(
        &self,
        proxy_host: &str,
        call_id: &str,
        from_user: &str,
        auth_token: &str,
        addr: &str,
    ) {
        let _ = SWITCHBOARD_SERVICE
            .room_rpc
            .notify_room_invite(
                proxy_host, &self.id, call_id, from_user, auth_token, addr,
            )
            .await;
    }

    async fn notify_active_speaker(active: &str, members: Vec<String>, now: u64) {
        let active_video_id = Channel::new(active.to_string())
            .get_video_id()
            .await
            .ok()
            .and_then(|id| Uuid::from_str(&id).ok());

        if let Some(active_video_id) = active_video_id {
            let _ = REDIS
                .hsetex(
                    &format!("nebula:media_stream:{active_video_id}",),
                    "last_key_speaker",
                    &now.to_string(),
                )
                .await;
        }

        for member in members {
            let active = active.to_string();
            tokio::spawn(async move {
                let channel = Channel::new(member);
                let _ = channel
                    .notify_active_speaker(active, active_video_id, now)
                    .await;
            });
        }
    }

    async fn notify_clear_active_speaker(members: Vec<String>) {
        for member in members {
            tokio::spawn(async move {
                let channel = Channel::new(member);
                let _ = channel.notify_clear_active_speaker().await;
            });
        }
    }

    async fn still_waiting(addr: &str) -> Result<()> {
        let call_id: String =
            REDIS.hget(&format!("nebula:wss:{addr}"), "call_id").await?;
        let room: String = REDIS
            .hget(&format!("nebula:wss:{addr}"), "waiting_room")
            .await?;
        let room = Room::new(room);
        let info = room.get_member_info(&call_id).await?;
        room.notify_waiting(&call_id, &info.name, info.user).await?;
        Ok(())
    }

    async fn leave_room(addr: &str) -> Result<()> {
        let call_id: String =
            REDIS.hget(&format!("nebula:wss:{addr}"), "call_id").await?;

        let room: String = REDIS
            .hget(&format!("nebula:channel:{}", &call_id), "room")
            .await
            .unwrap_or_default();

        if room.is_empty() {
            let room: String = REDIS
                .hget(&format!("nebula:wss:{addr}"), "waiting_room")
                .await?;
            let _ = REDIS
                .srem(&format!("nebula:waiting_room:{room}:members"), &call_id)
                .await;
            let room = Room::new(room);
            for member in room.members().await {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &member.id), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &member.id), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                if !addr.is_empty() {
                    let _ = SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_leave_waiting_room(
                            &proxy_host,
                            call_id.clone(),
                            addr,
                        )
                        .await;
                }
            }
        } else {
            let channel = Channel::new(call_id);
            channel.leave_room(&room, false).await?;
        }
        Ok(())
    }

    async fn get_room_from_addr(addr: &str) -> Result<(Room, Channel, bool)> {
        let call_id: String =
            REDIS.hget(&format!("nebula:wss:{addr}"), "call_id").await?;
        let room: String = REDIS
            .hget(&format!("nebula:channel:{}", &call_id), "room")
            .await?;
        let is_admin = REDIS
            .hget(&format!("nebula:channel:{}", &call_id), "room_admin")
            .await
            .unwrap_or_else(|_| "".to_string())
            == "yes";
        Ok((Room { id: room }, Channel::new(call_id), is_admin))
    }

    async fn mute_audio(addr: &str) -> Result<()> {
        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &channel.id),
                "room_audio_muted",
                "yes",
            )
            .await;
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let member_channel = Channel::new(m.to_string());
                if let Ok(member_context) = member_channel.get_call_context().await {
                    let proxy_host = &member_context.proxy_host;
                    let addr = member_context.api_addr.as_deref().unwrap_or("");
                    if addr.is_empty() || proxy_host.is_empty() {
                        warn!("member room message lost: mute_audio room {} api_addr {addr} proxy_host {proxy_host}", room.id);
                    }
                    if !addr.is_empty() {
                        SWITCHBOARD_SERVICE
                            .room_rpc
                            .member_mute_audio(
                                proxy_host,
                                &channel.id,
                                addr.to_string(),
                                false,
                            )
                            .await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn unmute_audio(addr: &str) -> Result<()> {
        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &channel.id),
                "room_audio_muted",
                "no",
            )
            .await;
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let member_channel = Channel::new(m.to_string());
                if let Ok(member_context) = member_channel.get_call_context().await {
                    let proxy_host = &member_context.proxy_host;
                    let addr = member_context.api_addr.as_deref().unwrap_or("");
                    if addr.is_empty() || proxy_host.is_empty() {
                        warn!("member room message lost: unmute_audio room {} api_addr {addr} proxy_host {proxy_host}", room.id);
                    }
                    if !addr.is_empty() {
                        SWITCHBOARD_SERVICE
                            .room_rpc
                            .member_unmute_audio(
                                proxy_host,
                                &channel.id,
                                addr.to_string(),
                                false,
                            )
                            .await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn mute_video(addr: &str) -> Result<()> {
        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &channel.id),
                "room_video_muted",
                "yes",
            )
            .await;
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                if !addr.is_empty() {
                    SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_mute_video(&proxy_host, &channel.id, addr, false)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn unmute_video(addr: &str) -> Result<()> {
        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &channel.id),
                "room_video_muted",
                "no",
            )
            .await;
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_unmute_video(&proxy_host, &channel.id, addr, false)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn start_screen_share(addr: &str) -> Result<()> {
        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &channel.id),
                "room_screen_share",
                "yes",
            )
            .await;
        if let Ok(video_id) = channel.get_video_id().await {
            let _ = REDIS
                .hsetex(
                    &format!("nebula:media_stream:{video_id}"),
                    "screen_share",
                    "yes",
                )
                .await;
        }
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_start_screen_share(
                            &proxy_host,
                            channel.id.clone(),
                            addr,
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn stop_screen_share(addr: &str) -> Result<()> {
        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &channel.id),
                "room_screen_share",
                "no",
            )
            .await;
        if let Ok(video_id) = channel.get_video_id().await {
            let _ = REDIS
                .hsetex(
                    &format!("nebula:media_stream:{video_id}"),
                    "screen_share",
                    "no",
                )
                .await;
        }
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_stop_screen_share(
                            &proxy_host,
                            channel.id.clone(),
                            addr,
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn raise_hand(addr: &str) -> Result<()> {
        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &channel.id),
                "room_hand_raised",
                "yes",
            )
            .await;
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_raise_hand(&proxy_host, channel.id.clone(), addr)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn unraise_hand(addr: &str) -> Result<()> {
        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &channel.id),
                "room_hand_raised",
                "no",
            )
            .await;
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_unraise_hand(&proxy_host, channel.id.clone(), addr)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn send_reaction(addr: &str, params: Value) -> Result<()> {
        let notification: SendReactionNotification = serde_json::from_value(params)
            .map_err(|_e| anyhow!("invalid notification"))?;

        let (room, channel, _) = Self::get_room_from_addr(addr).await?;
        let members = room.room_members().await?;
        for m in members {
            if m != channel.id {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_send_reaction(
                            &proxy_host,
                            &channel.id,
                            addr,
                            &notification.reaction,
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn mute_member_audio(
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let (room, _channel, is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        if !is_admin {
            return Err(RpcError {
                code: 400,
                message: "only admin can mute other member".to_string(),
                data: None,
            });
        }
        let member =
            params
                .get("member")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError {
                    code: 400,
                    message: "no member in request".to_string(),
                    data: None,
                })?;
        if !room.is_member(member).await.unwrap_or(false) {
            return Err(RpcError {
                code: 400,
                message: "no such member in the room".to_string(),
                data: None,
            });
        }
        let is_muted_by_admin = REDIS
            .hget(
                &format!("nebula:channel:{member}"),
                ROOM_AUDIO_MUTED_BY_ADMIN,
            )
            .await
            .unwrap_or_else(|_| "".to_string())
            == "yes";
        if is_muted_by_admin {
            return Err(RpcError {
                code: 400,
                message: "member is already muted by an admin".to_string(),
                data: None,
            });
        }

        let _ = Channel::new(member.to_string()).mute_audio().await;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{member}"),
                ROOM_AUDIO_MUTED_BY_ADMIN,
                "yes",
            )
            .await;

        if let Ok(members) = room.room_members().await {
            for m in members {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                if !addr.is_empty() {
                    let _ = SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_mute_audio(&proxy_host, member, addr, true)
                        .await;
                }
            }
        }

        Ok(serde_json::json!({}))
    }

    async fn unmute_member_audio(
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let (room, _channel, is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        if !is_admin {
            return Err(RpcError {
                code: 400,
                message: "only admin can unmute other member".to_string(),
                data: None,
            });
        }
        let member =
            params
                .get("member")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError {
                    code: 400,
                    message: "no member in request".to_string(),
                    data: None,
                })?;
        if !room.is_member(member).await.unwrap_or(false) {
            return Err(RpcError {
                code: 400,
                message: "no such member in the room".to_string(),
                data: None,
            });
        }
        let is_muted_by_admin = REDIS
            .hget(
                &format!("nebula:channel:{member}"),
                ROOM_AUDIO_MUTED_BY_ADMIN,
            )
            .await
            .unwrap_or_else(|_| "".to_string())
            == "yes";
        if !is_muted_by_admin {
            return Err(RpcError {
                code: 400,
                message: "member is not muted by an admin".to_string(),
                data: None,
            });
        }

        let _ = Channel::new(member.to_string()).unmute_audio().await;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{member}"),
                ROOM_AUDIO_MUTED_BY_ADMIN,
                "no",
            )
            .await;

        if let Ok(members) = room.room_members().await {
            for m in members {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    let _ = SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_unmute_audio(&proxy_host, member, addr, true)
                        .await;
                }
            }
        }

        Ok(serde_json::json!({}))
    }

    async fn mute_member_video(
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let (room, _channel, is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        if !is_admin {
            return Err(RpcError {
                code: 400,
                message: "only admin can mute other member".to_string(),
                data: None,
            });
        }
        let member =
            params
                .get("member")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError {
                    code: 400,
                    message: "no member in request".to_string(),
                    data: None,
                })?;
        if !room.is_member(member).await.unwrap_or(false) {
            return Err(RpcError {
                code: 400,
                message: "no such member in the room".to_string(),
                data: None,
            });
        }
        let is_muted_by_admin = REDIS
            .hget(
                &format!("nebula:channel:{member}"),
                ROOM_VIDEO_MUTED_BY_ADMIN,
            )
            .await
            .unwrap_or_else(|_| "".to_string())
            == "yes";
        if is_muted_by_admin {
            return Err(RpcError {
                code: 400,
                message: "member is already muted by an admin".to_string(),
                data: None,
            });
        }

        let _ = Channel::new(member.to_string()).mute_video().await;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{member}"),
                ROOM_VIDEO_MUTED_BY_ADMIN,
                "yes",
            )
            .await;

        if let Ok(members) = room.room_members().await {
            for m in members {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    let _ = SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_mute_video(&proxy_host, member, addr, true)
                        .await;
                }
            }
        }

        Ok(serde_json::json!({}))
    }

    async fn unmute_member_video(
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let (room, _channel, is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        if !is_admin {
            return Err(RpcError {
                code: 400,
                message: "only admin can mute other member".to_string(),
                data: None,
            });
        }
        let member =
            params
                .get("member")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError {
                    code: 400,
                    message: "no member in request".to_string(),
                    data: None,
                })?;
        if !room.is_member(member).await.unwrap_or(false) {
            return Err(RpcError {
                code: 400,
                message: "no such member in the room".to_string(),
                data: None,
            });
        }
        let is_muted_by_admin = REDIS
            .hget(
                &format!("nebula:channel:{member}"),
                ROOM_VIDEO_MUTED_BY_ADMIN,
            )
            .await
            .unwrap_or_else(|_| "".to_string())
            == "yes";
        if !is_muted_by_admin {
            return Err(RpcError {
                code: 400,
                message: "member is not muted by an admin".to_string(),
                data: None,
            });
        }

        let _ = Channel::new(member.to_string()).unmute_video().await;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{member}"),
                ROOM_VIDEO_MUTED_BY_ADMIN,
                "no",
            )
            .await;

        if let Ok(members) = room.room_members().await {
            for m in members {
                let addr = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "api_addr")
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                    .await
                    .unwrap_or_else(|_| "".to_string());

                if !addr.is_empty() {
                    let _ = SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_unmute_video(&proxy_host, member, addr, true)
                        .await;
                }
            }
        }

        Ok(serde_json::json!({}))
    }

    async fn subscribe_room(params: Value, addr: &str) -> Result<serde_json::Value> {
        let request: SubscribeRoomRequest = serde_json::from_value(params)
            .map_err(|_e| anyhow!("invalid request"))?;

        let (_room, channel, _is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        let resp = channel.subscribe_room(request).await?;
        let resp = serde_json::to_value(resp)?;
        Ok(resp)
    }

    async fn join_room(
        proxy_host: &str,
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value> {
        let request: JoinRoomRequest = serde_json::from_value(params)
            .map_err(|_| anyhow!("invalid request"))?;
        let claims: UserAuthClaim = request
            .auth_token
            .verify_with_key(&*JWT_CLAIMS_KEY)
            .map_err(|_| anyhow!("authentication error"))?;

        let endpoint = if let Some(user) = claims.user.as_ref() {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", user)
                .await
                .ok_or_else(|| anyhow!("can't authenticate user"))?;
            user
        } else {
            User {
                display_name: Some(claims.name.clone()),
                tenant_id: claims.tenant_id.clone(),
                ..Default::default()
            }
        };

        let resp = Self::endpoint_join_room(
            proxy_host,
            claims.room,
            claims.call_id,
            endpoint,
            &request.sdp,
            request.audio_muted,
            request.video_muted,
            claims.is_admin,
            addr,
        )
        .await?;
        let resp = serde_json::to_value(resp)?;
        Ok(resp)
    }

    async fn get_room_members(
        _params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let (room, _channel, _is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        let members = room.members().await;
        let value = serde_json::to_value(members).map_err(|_| RpcError {
            code: 500,
            message: "internal error".to_string(),
            data: None,
        })?;
        Ok(value)
    }

    async fn get_waiting_room_members(
        _params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let (room, _channel, is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        if !is_admin {
            return Err(RpcError {
                code: 403,
                message: "only admin can get waiting room members".to_string(),
                data: None,
            });
        }

        let members = REDIS
            .smembers::<Vec<String>>(&format!(
                "nebula:waiting_room:{}:members",
                &room.id
            ))
            .await
            .unwrap_or_default();

        let room = &room;
        let members: Vec<Value> = futures::stream::iter(members)
            .filter_map(|m| async move {
                let info = room.get_member_info(&m).await.ok()?;
                Some(serde_json::json!({
                    "member": m,
                    "name": info.name,
                }))
            })
            .collect()
            .await;

        let value = serde_json::to_value(members).map_err(|_| RpcError {
            code: 500,
            message: "internal error".to_string(),
            data: None,
        })?;
        Ok(value)
    }

    async fn remove_member(
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let request: RemoveMemberRequest =
            serde_json::from_value(params).map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;

        let (room, _channel, is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        if !is_admin {
            return Err(RpcError {
                code: 403,
                message: "only admin can get waiting room members".to_string(),
                data: None,
            });
        }

        tokio::spawn(async move {
            info!(
                room = room.id,
                channel = request.member,
                "admin remove member {} from room",
                request.member
            );
            let _ = Channel::new(request.member)
                .leave_room(&room.id, true)
                .await;
        });

        Ok(serde_json::json!({}))
    }

    async fn reject_member(
        params: Value,
        _addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let request: AuthorizeMemberRequest = serde_json::from_value(params)
            .map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;
        let claims: UserAuthClaim = request
            .auth_token
            .verify_with_key(&*JWT_CLAIMS_KEY)
            .map_err(|_| RpcError::internal_error())?;
        if !claims.is_admin {
            return Err(RpcError {
                code: 403,
                message: "not authorized to reject another member in".to_string(),
                data: None,
            });
        }

        let room = Room { id: claims.room };
        if room.is_member(&request.member).await.unwrap_or(false) {
            return Err(RpcError {
                code: 404,
                message: "member already in room".to_string(),
                data: None,
            });
        }

        let info =
            room.get_member_info(&request.member)
                .await
                .map_err(|_| RpcError {
                    code: 404,
                    message: "member didn't authenticate".to_string(),
                    data: None,
                })?;

        let _ = SWITCHBOARD_SERVICE
            .room_rpc
            .notifiy_room_rejected(&info.proxy_host, &info.addr)
            .await;

        let _ = room.notify_member_rejected(&request.member).await;

        Ok(serde_json::json!({}))
    }

    async fn allow_member(
        params: Value,
        _addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let request: AuthorizeMemberRequest = serde_json::from_value(params)
            .map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;
        let claims: UserAuthClaim = request
            .auth_token
            .verify_with_key(&*JWT_CLAIMS_KEY)
            .map_err(|_| RpcError::internal_error())?;
        if !claims.is_admin {
            return Err(RpcError {
                code: 403,
                message: "not authorized to allow another member in".to_string(),
                data: None,
            });
        }

        let room = Room { id: claims.room };
        if room.is_member(&request.member).await.unwrap_or(false) {
            return Err(RpcError {
                code: 404,
                message: "member already in room".to_string(),
                data: None,
            });
        }

        let info =
            room.get_member_info(&request.member)
                .await
                .map_err(|_| RpcError {
                    code: 404,
                    message: "member didn't authenticate".to_string(),
                    data: None,
                })?;

        let claims = UserAuthClaim {
            room: room.id.clone(),
            tenant_id: claims.tenant_id.clone(),
            call_id: request.member.clone(),
            user: None,
            name: info.name.clone(),
            is_admin: false,
            time: std::time::SystemTime::now(),
        };
        let token: String = claims
            .sign_with_key(&*JWT_CLAIMS_KEY)
            .map_err(|_| RpcError::internal_error())?;

        let _ = SWITCHBOARD_SERVICE
            .room_rpc
            .notifiy_room_allowed(&info.proxy_host, &token, &info.addr)
            .await;

        let _ = room.notify_member_allowed(&request.member).await;

        Ok(serde_json::json!({}))
    }

    async fn is_authenticated(
        request: &AuthRequest,
        room_password: Option<&str>,
    ) -> (String, Option<User>, Option<bool>) {
        let user = if let Some(device) = request.device.as_ref() {
            let device_token = request.device_token.as_deref().unwrap_or("");
            SWITCHBOARD_SERVICE
                .db
                .get_user_by_device_token(device, device_token)
                .await
        } else if let Some(username) = request.username.as_ref() {
            let password = request.password.as_deref().unwrap_or("");
            let user = SWITCHBOARD_SERVICE.db.get_user_by_name(username).await;
            user.filter(|u| u.password == password)
        } else {
            None
        };

        if let Some(user) = user.clone() {
            let member = SWITCHBOARD_SERVICE
                .db
                .get_room_member(&request.room, &user.uuid)
                .await;
            if let Some(member) = member.as_ref() {
                return (
                    user.display_name.clone().unwrap_or_else(|| "".to_string()),
                    Some(user),
                    Some(member.admin),
                );
            }
        };

        let display_name = user
            .clone()
            .and_then(|u| u.display_name)
            .or_else(|| request.display_name.clone())
            .unwrap_or_else(|| "".to_string());

        if Self::room_password_auth(room_password, request.room_password.as_deref())
        {
            return (display_name, user, Some(false));
        }

        (display_name, user, None)
    }

    fn room_password_auth(expected: Option<&str>, password: Option<&str>) -> bool {
        let expected = expected.unwrap_or("");
        let password = password.unwrap_or("");
        !expected.is_empty() && expected == password
    }

    async fn register_room_api(
        proxy_host: &str,
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let request: RegisterRoomApi =
            serde_json::from_value(params).map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;

        let user = if let Some(device) = request.device.as_ref() {
            let device_token = request.device_token.as_deref().unwrap_or("");
            SWITCHBOARD_SERVICE
                .db
                .get_user_by_device_token(device, device_token)
                .await
        } else if let Some(username) = request.username.as_ref() {
            let password = request.password.as_deref().unwrap_or("");
            let user = SWITCHBOARD_SERVICE.db.get_user_by_name(username).await;
            user.filter(|u| u.password == password)
        } else {
            None
        };

        let user = user.ok_or_else(|| RpcError {
            code: 401,
            message: "Can't authenticate".to_string(),
            data: None,
        })?;

        let _ = REDIS
            .hmset(
                &format!("nebula:register_room_api:{addr}"),
                vec![("user", &user.uuid), ("proxy_host", proxy_host)],
            )
            .await;
        let _ = REDIS
            .sadd(
                &format!("nebula:user:{}:register_room_api", user.uuid),
                addr,
            )
            .await;

        Ok(serde_json::json!({
            "user": user.uuid,
        }))
    }

    async fn create_temp_room(
        proxy_host: &str,
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let request: CreateTempRoom =
            serde_json::from_value(params).map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;

        let user = if let Some(device) = request.device.as_ref() {
            let device_token = request.device_token.as_deref().unwrap_or("");
            SWITCHBOARD_SERVICE
                .db
                .get_user_by_device_token(device, device_token)
                .await
        } else if let Some(username) = request.username.as_ref() {
            let password = request.password.as_deref().unwrap_or("");
            let user = SWITCHBOARD_SERVICE.db.get_user_by_name(username).await;
            user.filter(|u| u.password == password)
        } else {
            None
        };

        let user = user.ok_or_else(|| RpcError {
            code: 401,
            message: "Can't authenticate".to_string(),
            data: None,
        })?;
        let tenant_id = user.tenant_id.clone();
        let room_id = format!("{tenant_id}:{}", uuid::Uuid::new_v4());
        let resp = Self::endpoint_join_room(
            proxy_host,
            room_id.clone(),
            request.call_id.clone(),
            user,
            &request.sdp,
            request.audio_muted,
            request.video_muted,
            true,
            addr,
        )
        .await
        .map_err(|e| RpcError {
            code: 500,
            message: e.to_string(),
            data: None,
        })?;

        let context = Channel::new(request.call_id.clone())
            .get_call_context()
            .await
            .map_err(|e| RpcError {
                code: 500,
                message: e.to_string(),
                data: None,
            })?;

        let room = Room::new(room_id.clone());
        let invitation_id = nebula_utils::uuid();

        let _ = REDIS
            .hset(
                &format!("nebula:room:{room_id}"),
                &format!("invitation:{invitation_id}:from"),
                &request.call_id,
            )
            .await;

        let mut invited = Vec::new();
        for exten in request.invited {
            let tenant_id = tenant_id.clone();
            let context = context.clone();
            if let Ok(local_invited) = room
                .invite_exten(context, invitation_id.clone(), tenant_id, exten)
                .await
            {
                invited.extend_from_slice(&local_invited)
            }
        }

        Ok(json!({
            "room_id": room_id,
            "sdp": resp.sdp,
            "invited": invited.into_iter().map(|(bridge_id, extension_id)| json!({
                "bridge_id": bridge_id,
                "destination": extension_id,
            })).collect::<Vec<_>>(),
        }))
    }

    async fn invite_exten_to_room(
        proxy_host: &str,
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let (room, channel, is_admin) =
            Self::get_room_from_addr(addr).await.map_err(|_| RpcError {
                code: 400,
                message: "not in a rom".to_string(),
                data: None,
            })?;
        if !is_admin {
            return Err(RpcError {
                code: 403,
                message: "only admin can invite".to_string(),
                data: None,
            });
        }

        let request: InviteExtenRequest =
            serde_json::from_value(params).map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;

        let context = channel.get_call_context().await.map_err(|_| RpcError {
            code: 500,
            message: "internal error".to_string(),
            data: None,
        })?;
        let user = context.endpoint.user().ok_or_else(|| RpcError {
            code: 400,
            message: "invite can only from a user".to_string(),
            data: None,
        })?;

        let invitation_id = nebula_utils::uuid();

        let _ = REDIS
            .hset(
                &format!("nebula:room:{}", room.id),
                &format!("invitation:{invitation_id}:from"),
                &channel.id,
            )
            .await;

        let mut invited = Vec::new();
        for exten in request.extens {
            let context = context.clone();
            let tenant_id = user.tenant_id.clone();
            let room = room.clone();
            if let Ok(local_invited) = room
                .invite_exten(context, invitation_id.clone(), tenant_id, exten)
                .await
            {
                invited.extend_from_slice(&local_invited)
            }
        }

        Ok(json!({
            "invited": invited.into_iter().map(|(bridge_id, extension_id)| json!({
                "bridge_id": bridge_id,
                "destination": extension_id,
            })).collect::<Vec<_>>(),
        }))
    }

    async fn watch_room(
        proxy_host: &str,
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let request: WatchRoomRequest =
            serde_json::from_value(params).map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;
        let claims: UserAuthClaim = request
            .auth_token
            .verify_with_key(&*JWT_CLAIMS_KEY)
            .map_err(|_| RpcError {
                code: 401,
                message: "authentication error".to_string(),
                data: None,
            })?;

        let _ = REDIS
            .hmset(
                &format!("nebula:wss:{}", addr),
                vec![("room", &claims.room), ("call_id", &claims.call_id)],
            )
            .await;
        let _ = REDIS
            .hmset(
                &format!("nebula:channel:{}", claims.call_id),
                vec![
                    ("api_addr", addr),
                    ("proxy_host", proxy_host),
                    ("room", &claims.room),
                ],
            )
            .await;
        Ok(json!({
            "room_id": claims.room,
        }))
    }

    async fn convert_to_room(
        proxy_host: &str,
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let request: ConvertToRoom =
            serde_json::from_value(params).map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;

        let channel = Channel::new(request.call_id.clone());

        let (user, context, self_channel, other_channel) =
            channel.convert_to_room().await.map_err(|e| RpcError {
                code: 500,
                message: e.to_string(),
                data: None,
            })?;

        let room_id = format!("{}:{}", user.tenant_id, nebula_utils::uuid());

        let _ = Channel::new(self_channel).sip_join_room(&room_id).await;
        let _ = Channel::new(other_channel.clone())
            .sip_join_room(&room_id)
            .await;

        let local_room = Room::new(room_id.clone());
        tokio::spawn(async move {
            let other_channel = Channel::new(other_channel);
            let context = other_channel.get_call_context().await?;
            if let Endpoint::User {
                user,
                user_agent_type,
                ..
            } = &context.endpoint
            {
                if user_agent_type == &Some(UserAgentType::DesktopApp)
                    || user_agent_type == &Some(UserAgentType::MobileApp)
                {
                    let sdp = other_channel.get_local_sdp().await?;

                    let claims = UserAuthClaim {
                        room: local_room.id.clone(),
                        tenant_id: user.tenant_id.clone(),
                        call_id: other_channel.id.clone(),
                        user: Some(user.uuid.clone()),
                        name: user
                            .display_name
                            .clone()
                            .unwrap_or_else(|| "".to_string()),
                        is_admin: false,
                        time: std::time::SystemTime::now(),
                    };
                    let token: String = claims.sign_with_key(&*JWT_CLAIMS_KEY)?;

                    SWITCHBOARD_SERVICE
                        .proxy_rpc
                        .convert_to_room(
                            &context.proxy_host,
                            other_channel.id,
                            sdp.to_string(),
                            local_room.id.clone(),
                            token,
                        )
                        .await?;
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        let room = Room::new(room_id.clone());
        let tenant_id = user.tenant_id;
        let invitation_id = nebula_utils::uuid();
        let _ = REDIS
            .hset(
                &format!("nebula:room:{room_id}"),
                &format!("invitation:{invitation_id}:from"),
                &channel.id,
            )
            .await;
        let mut invited = Vec::new();
        for exten in request.invited {
            let room = room.clone();
            let tenant_id = tenant_id.clone();
            let context = context.clone();
            if let Ok(local_invited) = room
                .invite_exten(context, invitation_id.clone(), tenant_id, exten)
                .await
            {
                invited.extend_from_slice(&local_invited)
            }
        }

        let _ = REDIS
            .hmset(
                &format!("nebula:wss:{}", addr),
                vec![("room", &room_id), ("call_id", &channel.id)],
            )
            .await;
        let _ = REDIS
            .hmset(
                &format!("nebula:channel:{}", channel.id),
                vec![
                    ("api_addr", addr),
                    ("proxy_host", proxy_host),
                    ("room", &room_id),
                    ("room_admin", "yes"),
                ],
            )
            .await;
        Ok(json!({
            "room_id": room_id,
            "invited": invited.into_iter().map(|(bridge_id, extension_id)| json!({
                "bridge_id": bridge_id,
                "destination": extension_id,
            })).collect::<Vec<_>>(),
        }))
    }

    async fn format_extension(
        context: &CallContext,
        cdr_uuid: &str,
        tenant_id: &str,
        exten: &str,
    ) -> Result<Vec<BridgeExtension>> {
        if let Ok(exten) = exten.parse::<i64>() {
            if let Some(extension) =
                SWITCHBOARD_SERVICE.db.get_extension(tenant_id, exten).await
            {
                match extension.discriminator.as_ref() {
                    "sipuser" => {
                        let user = SWITCHBOARD_SERVICE
                            .db
                            .get_user(tenant_id, &extension.parent_id)
                            .await
                            .ok_or_else(|| anyhow!("can't find user"))?;
                        return Ok(vec![BridgeExtension::User(user)]);
                    }
                    "huntgroup" => {
                        let _ = SWITCHBOARD_SERVICE
                            .db
                            .get_group(&extension.tenant_id, &extension.parent_id)
                            .await
                            .ok_or_else(|| anyhow!("no such hunt group"))?;
                        let users = SWITCHBOARD_SERVICE
                            .db
                            .get_group_users(
                                &extension.tenant_id,
                                &extension.parent_id,
                            )
                            .await
                            .ok_or_else(|| anyhow!("hunt group don't have users"))?;
                        return Ok(users
                            .into_iter()
                            .map(BridgeExtension::User)
                            .collect());
                    }
                    _ => {}
                }
            }
        }

        if let Some(user) = SWITCHBOARD_SERVICE.db.get_user_by_name(exten).await {
            if user.tenant_id == tenant_id {
                return Ok(vec![BridgeExtension::User(user)]);
            }
        }

        let cc = phonenumber::country::Id::from_str(&context.endpoint.cc())
            .unwrap_or(phonenumber::country::GB);
        let (operator_code, exten) = get_operator_code(cc, exten);
        let number = phonenumber::parse(Some(cc), exten)?;

        let e164 = number.format().mode(phonenumber::Mode::E164).to_string();

        let (is_trunk, user_uuid, user_agent_type) = match &context.endpoint {
            Endpoint::User {
                user,
                user_agent_type,
                ..
            } => (
                false,
                Some(user.uuid.as_str()),
                user_agent_type.as_ref().map(|t| t.to_string()),
            ),
            Endpoint::Trunk { .. } => (true, None, None),
            Endpoint::Provider { .. } => (false, None, None),
        };

        let internal_number =
            SWITCHBOARD_SERVICE.db.get_number("admin", &e164[1..]).await;

        let (callerid, anonymous) = if internal_number.is_some() {
            ("".to_string(), false)
        } else {
            let ext_caller_id = external_caller_id(context).await;
            (ext_caller_id.user, ext_caller_id.anonymous)
        };

        let request_json = serde_json::json!({
            "uuid": &cdr_uuid,
            "reseller_uuid": tenant_id,
            "operator_code": operator_code.map(|(c, _)| c.to_string()),
            "country_code": number.code().value(),
            "destination": number.national().value(),
            "e164": e164,
            "ip": "",
            "trunk": is_trunk,
            "callerid": Some(&callerid),
            "user_uuid": user_uuid,
            "remove_call_restrictions": context.remove_call_restrictions,
            "user_agent_type": user_agent_type,
        });

        if let Some(internal_number) = internal_number {
            let (code, _) = SWITCHBOARD_SERVICE
                .yaypi_client
                .check_outbound_restriction(request_json)
                .await?;
            if code != 200 {
                return Err(anyhow!("outbound call to {} rejected", exten));
            }

            return Ok(vec![BridgeExtension::Intra(internal_number, None)]);
        }

        let (code, resp) = SWITCHBOARD_SERVICE
            .yaypi_client
            .create_call(request_json)
            .await?;
        if code != 200 {
            return Err(anyhow!("outbound call to {} rejected", exten));
        }

        let callerid = resp
            .get("callerid")
            .and_then(|v| v.as_str())
            .unwrap_or(&callerid);
        let caller_id = CallerId {
            user: callerid.to_string(),
            e164: None,
            display_name: "".to_string(),
            anonymous,
            asserted_identity: resp
                .get("asserted_identity")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string()),
            original_number: None,
            to_number: None,
        };

        let backup_callerid = resp
            .get("backup_callerid")
            .and_then(|v| v.as_str())
            .unwrap_or(callerid);
        let backup_caller_id = CallerId {
            user: backup_callerid.to_string(),
            e164: None,
            display_name: "".to_string(),
            anonymous,
            asserted_identity: resp
                .get("asserted_identity")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string()),
            original_number: None,
            to_number: None,
        };

        let number = if operator_code.is_some() {
            resp.get("operator_number")
                .and_then(|v| v.as_str())
                .and_then(|s| {
                    let s = if !s.starts_with('+') && !s.starts_with('0') {
                        format!("+{}", s)
                    } else {
                        s.to_string()
                    };
                    phonenumber::parse(Some(cc), s).ok()
                })
                .unwrap_or(number)
        } else {
            number
        };

        let provider = resp["provider"]
            .as_str()
            .ok_or_else(|| anyhow!("no provider"))?
            .to_string();
        let backup_provider = resp
            .get("backup_provider")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
            .filter(|v| v != &provider);
        let number = ExternalNumber {
            tenant_id: tenant_id.to_string(),
            number,
            provider,
            backup_provider,
            caller_id: Some(caller_id),
            backup_caller_id: Some(backup_caller_id),
            operator_code: operator_code.map(|(c, s)| {
                (c.to_string(), s, resp.get("operator_number").is_some())
            }),
        };
        Ok(vec![BridgeExtension::ExternalNumber(number, None)])
    }

    async fn invite_exten(
        &self,
        context: CallContext,
        invitation_id: String,
        tenant_id: String,
        exten: String,
    ) -> Result<Vec<(String, String)>> {
        let cdr_uuid = SWITCHBOARD_SERVICE.db.create_cdr().await?.uuid.to_string();

        let extensions =
            Self::format_extension(&context, &cdr_uuid, &tenant_id, &exten).await?;
        let mut invited = Vec::new();
        for extension in extensions {
            let room = self.clone();
            let to_exten = exten.clone();
            let context = context.clone();
            let tenant_id = tenant_id.clone();
            let bridge_id = nebula_utils::uuid();
            invited.push((bridge_id.clone(), extension.id()));

            let _ = REDIS
                .hset(
                    &format!("nebula:bridge:{bridge_id}"),
                    "invitation",
                    &invitation_id,
                )
                .await;
            let _ = REDIS
                .hset(&format!("nebula:bridge:{bridge_id}"), "room", &self.id)
                .await;

            tokio::spawn(async move {
                let _ = room
                    .invite_extension(
                        bridge_id, context, tenant_id, extension, to_exten,
                    )
                    .await;
            });
        }
        Ok(invited)
    }

    async fn invite_extension(
        &self,
        bridge_id: String,
        context: CallContext,
        tenant_id: String,
        extension: BridgeExtension,
        to_exten: String,
    ) -> Result<()> {
        info!(
            room = self.id,
            bridge = bridge_id,
            "room invite extension {extension:?}"
        );
        let locations = extension
            .locations(true, to_exten, false)
            .await
            .ok_or_else(|| anyhow!("can't find locations for extension"))?;
        for (
            endpoint,
            mut location,
            backup_location,
            force_caller_id,
            backup_force_caller_id,
        ) in locations
        {
            let caller_id = match force_caller_id {
                Some(caller_id) => caller_id,
                None => match &endpoint {
                    Endpoint::User { user, group, .. } => {
                        internal_caller_id(&context, user.cc.clone(), group.clone())
                            .await
                    }
                    Endpoint::Trunk { .. } => external_caller_id(&context).await,
                    Endpoint::Provider { .. } => external_caller_id(&context).await,
                },
            };
            let backup_caller_id =
                backup_force_caller_id.unwrap_or_else(|| caller_id.clone());
            let mut rtpmaps: Vec<Rtpmap> =
                if location.intra || endpoint.is_internal_sdp() {
                    INTERNAL_AUDIO_ORDER
                        .iter()
                        .map(|pt| pt.get_rtpmap())
                        .collect()
                } else {
                    EXTERNAL_AUDIO_ORDER
                        .iter()
                        .map(|pt| pt.get_rtpmap())
                        .collect()
                };
            rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());
            let bridge_id = bridge_id.clone();
            let id = nebula_utils::uuid();

            if let Some(user) = endpoint.user() {
                if location.device.is_some()
                    || location.dst_uri.transport == TransportType::Wss
                    || location.dst_uri.transport == TransportType::Ws
                {
                    let call_id = nebula_utils::uuid();

                    let _ = REDIS
                        .sadd(
                            &format!("nebula:bridge:{}:channels", &bridge_id),
                            &call_id,
                        )
                        .await;

                    let mut context = CallContext::new(
                        Endpoint::User {
                            user: user.clone(),
                            group: None,
                            user_agent: None,
                            user_agent_type: None,
                        },
                        self.id.clone(),
                        CallDirection::Initiator,
                    );
                    context.room = self.id.clone();

                    let _ = REDIS
                        .hmset(
                            &format!("nebula:channel:{call_id}"),
                            vec![
                                ("bridge_id", &bridge_id),
                                ("call_context", &serde_json::to_string(&context)?),
                            ],
                        )
                        .await;

                    let claims = UserAuthClaim {
                        room: self.id.clone(),
                        tenant_id: user.tenant_id.clone(),
                        call_id: call_id.clone(),
                        user: context.endpoint.user().map(|u| u.uuid.clone()),
                        name: context
                            .endpoint
                            .user()
                            .and_then(|user| user.display_name.clone())
                            .unwrap_or_default(),
                        is_admin: false,
                        time: std::time::SystemTime::now(),
                    };
                    let token: String = claims.sign_with_key(&*JWT_CLAIMS_KEY)?;
                    location.room_auth_token = Some(token);
                }
            }

            let inbound_channel = id.clone();
            let _ = REDIS
                .sadd(&format!("nebula:bridge:{}:channels", &bridge_id), &id)
                .await;
            let room = self.id.clone();
            tokio::spawn(async move {
                let _ = Channel::new_outbound(
                    id,
                    caller_id,
                    endpoint,
                    location,
                    backup_location.map(|l| (l, backup_caller_id)),
                    inbound_channel,
                    bridge_id,
                    true,
                    rtpmaps,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Vec::new(),
                    room,
                    false,
                    true,
                )
                .await;
            });
        }

        tokio::spawn(async move {
            let bridge = Bridge::new(bridge_id);
            let _ = bridge.set_tenant_id(&tenant_id).await;
            let _ = bridge
                .handle_timeout(
                    tokio::time::Instant::now()
                        + tokio::time::Duration::from_secs(120),
                    false,
                )
                .await;
        });

        Ok(())
    }

    pub async fn notify_room_invite_rejection(
        &self,
        bridge_id: String,
        invitation_id: String,
    ) -> Result<()> {
        let channel: String = REDIS
            .hget(
                &format!("nebula:room:{}", self.id),
                &format!("invitation:{invitation_id}:from"),
            )
            .await?;

        let addr = REDIS
            .hget(&format!("nebula:channel:{channel}"), "api_addr")
            .await
            .unwrap_or_else(|_| "".to_string());
        let proxy_host = REDIS
            .hget(&format!("nebula:channel:{channel}"), "proxy_host")
            .await
            .unwrap_or_else(|_| "".to_string());

        if !addr.is_empty() && !proxy_host.is_empty() {
            SWITCHBOARD_SERVICE
                .room_rpc
                .reject_room_invite(&proxy_host, &self.id, &bridge_id, &addr)
                .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn endpoint_join_room(
        proxy_host: &str,
        room_id: String,
        call_id: String,
        endpoint: User,
        sdp: &str,
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
        is_admin: bool,
        addr: &str,
    ) -> Result<JoinRoomResponse> {
        let mut context = CallContext::new(
            Endpoint::User {
                user: endpoint.clone(),
                group: None,
                user_agent: None,
                user_agent_type: None,
            },
            room_id.clone(),
            CallDirection::Initiator,
        );
        context.room = room_id.clone();
        context.proxy_host = proxy_host.to_string();
        context.api_addr = Some(addr.to_string());
        let cdr = Some(SWITCHBOARD_SERVICE.db.create_cdr().await?.uuid);
        context.cdr = cdr;
        info!(
            room = room_id,
            channel = call_id,
            addr = addr,
            "endpoint join room"
        );

        let channel = Channel::new(call_id.clone());
        channel.set_call_context(&context).await?;
        channel.set_api_remote_addr(addr).await?;
        channel
            .update_cdr(CdrItemChange {
                tenant_id: Some(endpoint.tenant_id.clone()),
                answer: Some(Utc::now()),
                disposition: Some("answered".to_string()),
                call_type: Some("room".to_string()),
                from_uuid: Some(endpoint.uuid.clone()),
                from_exten: endpoint.extension.map(|e| e.to_string()),
                from_type: Some("sipuser".to_string()),
                from_user: Some(endpoint.name.clone()),
                from_name: Some(endpoint.display_name.unwrap_or_default()),
                ..Default::default()
            })
            .await?;

        if let Some(tenant_id) = context.endpoint.tenant_id() {
            let _ = REDIS
                .sadd(
                    &format!("nebula:channels_for_tenant:{tenant_id}"),
                    &channel.id,
                )
                .await;
        }

        let _ = REDIS.sadd(ALL_CHANNELS, &channel.id).await;
        let _ = REDIS
            .expire(&format!("nebula:channel:{}", &channel.id), 86400)
            .await;

        let audio_muted = audio_muted.unwrap_or(false);
        let video_muted = video_muted.unwrap_or(false);
        let room_audio_muted = if audio_muted { "yes" } else { "no" };
        let room_video_muted = if video_muted { "yes" } else { "no" };
        let _ = REDIS
            .srem(
                &format!("nebula:waiting_room:{room_id}:members"),
                &channel.id,
            )
            .await;
        let resp = channel
            .join_room(&context, &room_id, is_admin, sdp, audio_muted, video_muted)
            .await?;

        REDIS
            .hmset(
                &format!("nebula:wss:{}", addr),
                vec![("room", &room_id), ("call_id", &channel.id)],
            )
            .await?;
        REDIS
            .hmset(
                &format!("nebula:channel:{}", channel.id),
                vec![
                    ("api_addr", addr),
                    ("proxy_host", proxy_host),
                    ("room", &room_id),
                    ("room_admin", if is_admin { "yes" } else { "no" }),
                    ("room_audio_muted", room_audio_muted),
                    ("room_video_muted", room_video_muted),
                ],
            )
            .await?;
        Ok(resp)
    }

    async fn authenticate(
        proxy_host: &str,
        params: Value,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let request: AuthRequest =
            serde_json::from_value(params).map_err(|_| RpcError {
                code: 400,
                message: "invalid request".to_string(),
                data: None,
            })?;

        let room_model =
            SWITCHBOARD_SERVICE.db.get_room(&request.room).await.ok_or(
                RpcError {
                    code: 404,
                    message: "room doesn't exist".to_string(),
                    data: None,
                },
            )?;

        let room = Room {
            id: room_model.uuid.to_string(),
        };

        let (display_name, user, is_admin) = if room_model.open {
            let (display_name, user, status) =
                Self::is_authenticated(&request, room_model.password.as_deref())
                    .await;
            (display_name, user, status.unwrap_or(false))
        } else if room_model.force_password && room_model.force_waiting_room {
            if !Self::room_password_auth(
                room_model.password.as_deref(),
                request.room_password.as_deref(),
            ) {
                return Err(RpcError {
                    code: 401,
                    message: "You'll need the correct password to get in the room"
                        .to_string(),
                    data: None,
                });
            }

            let (display_name, user, status) =
                Self::is_authenticated(&request, room_model.password.as_deref())
                    .await;
            let is_admin = match status {
                Some(is_admin) => is_admin,
                None => {
                    room.waiting_room(
                        &request.call_id,
                        &display_name,
                        addr,
                        user.as_ref().map(|u| u.uuid.clone()),
                        proxy_host,
                    )
                    .await;
                    return Err(RpcError {
                        code: 403,
                        message: "not authenticated, waiting to be accepted"
                            .to_string(),
                        data: None,
                    });
                }
            };
            if !is_admin {
                room.waiting_room(
                    &request.call_id,
                    &display_name,
                    addr,
                    user.as_ref().map(|u| u.uuid.clone()),
                    proxy_host,
                )
                .await;
                return Err(RpcError {
                    code: 403,
                    message: "not admin, waiting to be accepted".to_string(),
                    data: None,
                });
            }
            (display_name, user, is_admin)
        } else if room_model.force_password {
            if !Self::room_password_auth(
                room_model.password.as_deref(),
                request.room_password.as_deref(),
            ) {
                return Err(RpcError {
                    code: 401,
                    message: "You'll need the correct password to get in the room"
                        .to_string(),
                    data: None,
                });
            }
            let (display_name, user, status) =
                Self::is_authenticated(&request, room_model.password.as_deref())
                    .await;
            (display_name, user, status.unwrap_or(false))
        } else if room_model.force_waiting_room {
            let (display_name, user, status) =
                Self::is_authenticated(&request, room_model.password.as_deref())
                    .await;
            let is_admin = match status {
                Some(is_admin) => is_admin,
                None => {
                    room.waiting_room(
                        &request.call_id,
                        &display_name,
                        addr,
                        user.as_ref().map(|u| u.uuid.clone()),
                        proxy_host,
                    )
                    .await;
                    return Err(RpcError {
                        code: 403,
                        message: "not authenticated, waiting to be accepted"
                            .to_string(),
                        data: None,
                    });
                }
            };
            if !is_admin {
                room.waiting_room(
                    &request.call_id,
                    &display_name,
                    addr,
                    user.as_ref().map(|u| u.uuid.clone()),
                    proxy_host,
                )
                .await;
                return Err(RpcError {
                    code: 403,
                    message: "not admin, waiting to be accepted".to_string(),
                    data: None,
                });
            }
            (display_name, user, is_admin)
        } else {
            let (display_name, user, status) =
                Self::is_authenticated(&request, room_model.password.as_deref())
                    .await;
            let is_admin = match status {
                Some(is_admin) => is_admin,
                None => {
                    room.waiting_room(
                        &request.call_id,
                        &display_name,
                        addr,
                        user.as_ref().map(|u| u.uuid.clone()),
                        proxy_host,
                    )
                    .await;
                    return Err(RpcError {
                        code: 403,
                        message: "not authenticated, waiting to be accepted"
                            .to_string(),
                        data: None,
                    });
                }
            };
            (display_name, user, is_admin)
        };

        let claims = UserAuthClaim {
            room: request.room.clone(),
            tenant_id: room_model.tenant_id.clone(),
            call_id: request.call_id.clone(),
            user: user.map(|u| u.uuid),
            name: display_name,
            is_admin,
            time: std::time::SystemTime::now(),
        };
        let _ = room
            .set_member_info(
                &request.call_id,
                &claims.name,
                addr,
                claims.user.as_deref(),
                is_admin,
                proxy_host,
            )
            .await;
        let token: String = claims
            .sign_with_key(&*JWT_CLAIMS_KEY)
            .map_err(|_| RpcError::internal_error())?;
        Ok(json!({
            "auth_token": token,
        }))
    }

    pub async fn handle_notification(
        method: &str,
        params: Params,
        addr: &str,
    ) -> Result<()> {
        let method = RoomNotificationMethod::from_str(method)
            .map_err(|_| anyhow!("unsupported method"))?;
        let params =
            serde_json::to_value(params).map_err(|_| anyhow!("invalid params"))?;
        match method {
            RoomNotificationMethod::StillWaiting => {
                Self::still_waiting(addr).await?
            }
            RoomNotificationMethod::LeaveRoom => Self::leave_room(addr).await?,
            RoomNotificationMethod::RaiseHand => Self::raise_hand(addr).await?,
            RoomNotificationMethod::UnraiseHand => Self::unraise_hand(addr).await?,
            RoomNotificationMethod::StartScreenShare => {
                Self::start_screen_share(addr).await?
            }
            RoomNotificationMethod::StopScreenShare => {
                Self::stop_screen_share(addr).await?
            }
            RoomNotificationMethod::SendReaction => {
                Self::send_reaction(addr, params).await?
            }
            RoomNotificationMethod::MuteAudio => Self::mute_audio(addr).await?,
            RoomNotificationMethod::UnmuteAudio => Self::unmute_audio(addr).await?,
            RoomNotificationMethod::MuteVideo => Self::mute_video(addr).await?,
            RoomNotificationMethod::UnmuteVideo => Self::unmute_video(addr).await?,
        }
        Ok(())
    }

    async fn waiting_room(
        &self,
        call_id: &str,
        name: &str,
        addr: &str,
        user: Option<String>,
        proxy_host: &str,
    ) {
        let _ = self
            .set_member_info(call_id, name, addr, user.as_deref(), false, proxy_host)
            .await;
        let _ = REDIS
            .hmset(
                &format!("nebula:wss:{}", addr),
                vec![("waiting_room", &self.id), ("call_id", call_id)],
            )
            .await;
        let _ = REDIS
            .sadd(&format!("nebula:waiting_room:{}:members", self.id), call_id)
            .await;
        let _ = self.notify_waiting(call_id, name, user).await;
    }

    pub async fn handle_request(
        proxy_host: &str,
        method: &str,
        params: Params,
        addr: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let method = RoomRequestMethod::from_str(method)
            .map_err(|_| RpcError::method_not_found())?;
        let params =
            serde_json::to_value(params).map_err(|_| RpcError::invalid_params())?;
        match method {
            RoomRequestMethod::RegisterRoomApi => {
                Self::register_room_api(proxy_host, params, addr).await
            }
            RoomRequestMethod::CreateTempRoom => {
                Self::create_temp_room(proxy_host, params, addr).await
            }
            RoomRequestMethod::WatchRoom => {
                Self::watch_room(proxy_host, params, addr).await
            }
            RoomRequestMethod::ConvertToRoom => {
                Self::convert_to_room(proxy_host, params, addr).await
            }
            RoomRequestMethod::InviteExten => {
                Self::invite_exten_to_room(proxy_host, params, addr).await
            }
            RoomRequestMethod::Authenticate => {
                Self::authenticate(proxy_host, params, addr).await
            }
            RoomRequestMethod::GetRoomMembers => {
                Self::get_room_members(params, addr).await
            }
            RoomRequestMethod::GetWaitingRoomMembers => {
                Self::get_waiting_room_members(params, addr).await
            }
            RoomRequestMethod::AllowMember => Self::allow_member(params, addr).await,
            RoomRequestMethod::RejectMember => {
                Self::reject_member(params, addr).await
            }
            RoomRequestMethod::RemoveMember => {
                Self::remove_member(params, addr).await
            }
            RoomRequestMethod::JoinRoom => Self::join_room(proxy_host, params, addr)
                .await
                .map_err(|e| RpcError {
                    code: 500,
                    message: e.to_string(),
                    data: None,
                }),
            RoomRequestMethod::SubscribeRoom => Self::subscribe_room(params, addr)
                .await
                .map_err(|e| RpcError {
                    code: 500,
                    message: e.to_string(),
                    data: None,
                }),
            RoomRequestMethod::MuteMemberAudio => {
                Self::mute_member_audio(params, addr).await
            }
            RoomRequestMethod::UnmuteMemberAudio => {
                Self::unmute_member_audio(params, addr).await
            }
            RoomRequestMethod::MuteMemberVideo => {
                Self::mute_member_video(params, addr).await
            }
            RoomRequestMethod::UnmuteMemberVideo => {
                Self::unmute_member_video(params, addr).await
            }
        }
    }

    pub async fn handle_available_rate(
        &self,
        member: String,
        rate: u64,
    ) -> Result<()> {
        let addr = REDIS
            .hget(&format!("nebula:channel:{member}"), "api_addr")
            .await
            .unwrap_or_else(|_| "".to_string());
        let proxy_host = REDIS
            .hget(&format!("nebula:channel:{member}"), "proxy_host")
            .await
            .unwrap_or_else(|_| "".to_string());

        if !addr.is_empty() {
            let _ = SWITCHBOARD_SERVICE
                .room_rpc
                .member_available_rate(&proxy_host, addr, rate)
                .await;
        }
        Ok(())
    }

    pub async fn handle_video_allocation(
        &self,
        member: String,
        allocations: HashMap<String, VideoAllocation>,
    ) -> Result<()> {
        let addr = REDIS
            .hget(&format!("nebula:channel:{member}"), "api_addr")
            .await
            .unwrap_or_else(|_| "".to_string());
        let proxy_host = REDIS
            .hget(&format!("nebula:channel:{member}"), "proxy_host")
            .await
            .unwrap_or_else(|_| "".to_string());

        if !addr.is_empty() {
            let _ = SWITCHBOARD_SERVICE
                .room_rpc
                .member_video_allocations(&proxy_host, addr, allocations)
                .await;
        }
        Ok(())
    }

    pub async fn handle_audio_level(&self, member: String, level: u8) -> Result<()> {
        let members = self.room_members().await?;
        for m in members {
            let addr = REDIS
                .hget(&format!("nebula:channel:{}", &m), "api_addr")
                .await
                .unwrap_or_else(|_| "".to_string());
            let proxy_host = REDIS
                .hget(&format!("nebula:channel:{}", &m), "proxy_host")
                .await
                .unwrap_or_else(|_| "".to_string());

            if !addr.is_empty() {
                let _ = SWITCHBOARD_SERVICE
                    .room_rpc
                    .member_audio_level(&proxy_host, &member, addr, level)
                    .await;
            }
        }
        Ok(())
    }

    pub async fn handle_rpc(proxy_host: String, msg: RoomRpcMessage) -> Result<()> {
        match &msg.rpc {
            JsonRpc::Request(_) => {
                let id = msg.rpc.get_id().ok_or_else(|| anyhow!("no id"))?;
                let method =
                    msg.rpc.get_method().ok_or_else(|| anyhow!("no method"))?;
                let params =
                    msg.rpc.get_params().ok_or_else(|| anyhow!("no params"))?;
                let resp = match Self::handle_request(
                    &proxy_host,
                    method,
                    params,
                    &msg.addr,
                )
                .await
                {
                    Ok(resp) => JsonRpc::success(id, &resp),
                    Err(e) => JsonRpc::error(id, e),
                };
                SWITCHBOARD_SERVICE
                    .room_rpc
                    .room_rpc(&proxy_host, resp, msg.addr.clone())
                    .await?;
            }
            JsonRpc::Notification(_) => {
                let method =
                    msg.rpc.get_method().ok_or_else(|| anyhow!("no method"))?;
                let params =
                    msg.rpc.get_params().ok_or_else(|| anyhow!("no params"))?;
                Self::handle_notification(method, params, &msg.addr).await?;
            }
            JsonRpc::Success(_) => {}
            JsonRpc::Error(_) => {}
        }

        Ok(())
    }

    async fn notify_member_allowed(&self, waiting_member: &str) -> Result<()> {
        let members = self.room_members().await?;
        for member in members.iter() {
            if let Ok(info) = self.get_member_info(member).await {
                let _ = SWITCHBOARD_SERVICE
                    .room_rpc
                    .notifiy_member_allowed(
                        &info.proxy_host,
                        waiting_member,
                        &info.addr,
                    )
                    .await;
            }
        }
        Ok(())
    }

    async fn notify_member_rejected(&self, waiting_member: &str) -> Result<()> {
        let members = self.room_members().await?;
        for member in members.iter() {
            if let Ok(info) = self.get_member_info(member).await {
                let _ = SWITCHBOARD_SERVICE
                    .room_rpc
                    .notifiy_member_rejected(
                        &info.proxy_host,
                        waiting_member,
                        &info.addr,
                    )
                    .await;
            }
        }
        Ok(())
    }

    async fn is_member_admin(member: &str) -> bool {
        REDIS
            .hget(&format!("nebula:channel:{member}"), "room_admin")
            .await
            .unwrap_or_else(|_| "".to_string())
            == "yes"
    }

    async fn notify_waiting(
        &self,
        waiting_member: &str,
        name: &str,
        user: Option<String>,
    ) -> Result<()> {
        let members = self.room_members().await?;
        for member in members.iter() {
            if Self::is_member_admin(member).await {
                let _ = self
                    .notify_waiting_member(
                        member,
                        waiting_member,
                        name,
                        user.clone(),
                    )
                    .await;
            }
        }
        Ok(())
    }

    async fn notify_waiting_member(
        &self,
        member: &str,
        waiting_member: &str,
        name: &str,
        user: Option<String>,
    ) -> Result<()> {
        let info = self.get_member_info(member).await?;
        // if info.is_admin {
        let _ = SWITCHBOARD_SERVICE
            .room_rpc
            .notifiy_member_waiting(
                &info.proxy_host,
                waiting_member,
                name,
                user,
                &info.addr,
            )
            .await;
        // }
        Ok(())
    }

    async fn get_member_info(&self, member: &str) -> Result<MemberInfo> {
        let info: String = REDIS
            .get(&format!("nebula:room:{}:member:{}", self.id, member))
            .await?;
        let info: MemberInfo = serde_json::from_str(&info)?;
        Ok(info)
    }

    async fn set_member_info(
        &self,
        member: &str,
        name: &str,
        addr: &str,
        user: Option<&str>,
        is_admin: bool,
        proxy_host: &str,
    ) -> Result<()> {
        REDIS
            .set(
                &format!("nebula:room:{}:member:{}", &self.id, member),
                &serde_json::to_string(&MemberInfo {
                    name: name.to_string(),
                    is_admin,
                    addr: addr.to_string(),
                    user: user.map(|u| u.to_string()),
                    proxy_host: proxy_host.to_string(),
                })
                .unwrap(),
            )
            .await?;
        Ok(())
    }

    async fn is_member(&self, member: &str) -> Result<bool> {
        REDIS
            .sismember(&format!("nebula:room:{}:members", self.id), member)
            .await
    }

    async fn room_members(&self) -> Result<Vec<String>> {
        let members: Vec<String> = REDIS
            .smembers(&format!("nebula:room:{}:members", self.id))
            .await?;
        Ok(members)
    }
}
