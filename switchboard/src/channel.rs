use super::server::SWITCHBOARD_SERVICE;
use super::switchboard::Switchboard;
use crate::bridge::{Bridge, BridgeExtension, ExternalNumber};
use crate::callflow::get_operator_code;
use crate::room::{ALL_ROOMS, ROOM_AUDIO_MUTED_BY_ADMIN, ROOM_VIDEO_MUTED_BY_ADMIN};
use crate::server::is_ip_global;
use crate::switchboard::{format_national_number, get_phone_number};
use anyhow::{anyhow, Context, Error, Result};
use async_recursion::async_recursion;
use bigdecimal::BigDecimal;
use chrono::{Datelike, Timelike, Utc};
use chrono_tz::Tz;
use codec::InitT38Params;
use futures::stream::{self, StreamExt};
use nebula_db::api::{
    is_channel_answered, is_channel_hangup, user_dialogs, ALL_CHANNELS, DB,
};
use nebula_db::message::{
    CallConfirmation, CallContext, CallDirection, CallInvitation, ChannelEvent,
    DialogState, Endpoint, InviteRequest, JoinRoomResponse,
    NewInboundChannelRequest, RecordingContext, ReferContext, ReferRequest, ReferTo,
    Response, RoomMember, SubscribeRoomMember, SubscribeRoomRequest,
    SubscribeRoomResponse, TransferContext, TransferPeer, TransferTarget,
};
use nebula_db::models::{
    AlertInfoPatterns, CdrItem, CdrItemChange, MusicOnHold, NewCdrEvent, Number,
    Provider, ShortCode, User, VoicemailUser,
};
use nebula_db::usrloc::Usrloc;
use nebula_redis::{DistributedMutex, REDIS};
use nebula_rpc::media::HasSound;
use nebula_rpc::{
    client::notify_dialog,
    message::{CreateCall, VideoSourceInfo},
};
use nebula_utils::uuid_v5;
use sdp::payload::{PayloadType, Rtpmap};
use sdp::sdp::{
    MediaDescKind, MediaDescription, MediaMode, MediaStreamTrack, MediaType,
    SdpType, SessionDescKind, SessionDescription, DUMMY_RTP_PORT,
    EXTERNAL_AUDIO_ORDER, INTERNAL_AUDIO_ORDER, LOCAL_RTP_PEER_PORT,
    PEER_CONNECTION_PORT,
};
use serde::Deserialize;
use serde_json::{self, Value};
use sip::message::{HangupReason, TeamsReferCx, Uri};
use sip::{
    message::{CallerId, Location},
    transport::TransportType,
};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::str::FromStr;
use std::string::ToString;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use strum_macros;
use strum_macros::EnumString;
use tracing::{error, info, warn};
use uuid::Uuid;
use yaypi::api::YaypiClient;

pub enum Direction {
    Inbound,
    Outbound,
}

#[derive(strum_macros::Display, EnumString, PartialEq, Clone, Debug)]
pub enum ChannelBridgeResult {
    #[strum(serialize = "ok")]
    Ok,
    #[strum(serialize = "cancelled")]
    Cancelled,
    #[strum(serialize = "noanswer")]
    Noanswer,
    #[strum(serialize = "noanswer_with_completed_elsewhere")]
    // this will make it sends "call completed elsewhere"
    // when we send the cancel to the sip client in the bridge
    // the use case for it is for call queue to hide missed calls when going
    // to the next round
    NoanswerWithCompletedElsewhere,
    #[strum(serialize = "unavailable")]
    Unavailable,
    #[strum(serialize = "busy")]
    Busy,
}

#[derive(strum_macros::Display, EnumString, PartialEq, Clone)]
pub enum ChannelHangupDirection {
    #[strum(serialize = "server")]
    Server,
    #[strum(serialize = "client")]
    Client,
}

pub struct ForwardInfo {
    pub callflow_id: String,
    pub callflow_name: String,
    pub from_number: String,
    pub to_number: String,
    pub tenant_id: String,
}

#[derive(Deserialize, Clone)]
pub struct CreateOutboundCall {
    pub tenant_id: String,
    pub destination: String,
    pub callerid: String,
    pub callflow: String,
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub id: String,
}

#[derive(strum_macros::Display, EnumString, PartialEq, Clone)]
pub enum CallMonitoring {
    #[strum(serialize = "listen")]
    Listen,
    #[strum(serialize = "whisper")]
    Whisper,
    #[strum(serialize = "barge")]
    Barge,
}

#[derive(PartialEq, Eq, Debug)]
enum RecordingPause {
    Pause,
    Resume,
    Unitialised,
}

impl HasSound for RecordingPause {
    fn get_sound(&self) -> String {
        match self {
            RecordingPause::Pause => "e25cbd22-7d8f-4c24-9cef-633dd7ff1da9",
            RecordingPause::Resume => "741d84ad-500b-42bb-a5b9-2c3b51b8f9d5",
            RecordingPause::Unitialised => "741d84ad-500b-42bb-a5b9-2c3b51b8f9d5",
        }
        .to_string()
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum AuthPinResult {
    Accepted,
    Failed,
    Timeout,
}

impl Channel {
    pub async fn new_inbound(req: &NewInboundChannelRequest) -> Result<Channel> {
        let cdr = SWITCHBOARD_SERVICE.db.create_cdr().await?.uuid;
        info!(
            channel = req.id,
            cdr = cdr.to_string(),
            "new inbound {} {req:?}",
            req.id
        );
        let channel = Self::new(req.id.clone());

        // let cdr = if req.redirect.is_some() {
        //     None
        // } else {
        //     Some(SWITCHBOARD_SERVICE.db.create_cdr().await?)
        // };

        let context = CallContext {
            endpoint: req.from.clone(),
            proxy_host: req.proxy_host.clone(),
            api_addr: None,
            location: None,
            inter_tenant: req.inter_tenant.clone(),
            to: req.to.clone(),
            to_internal_number: None,
            direction: CallDirection::Initiator,
            caller_id: None,
            set_caller_id: None,
            answer_caller_id: None,
            call_queue_id: None,
            call_queue_name: None,
            call_confirmation: None,
            call_flow_id: None,
            call_flow_name: None,
            call_flow_show_original_callerid: None,
            call_flow_moh: None,
            call_flow_custom_callerid: None,
            parent_call_flow_name: None,
            parent_call_flow_show_original_callerid: None,
            parent_call_flow_moh: None,
            parent_call_flow_custom_callerid: None,
            intra_peer: req.intra.clone(),
            intra_chain: req.intra_chain.clone(),
            redirect_peer: req.redirect.clone(),
            refer: req.refer.clone(),
            transfer: req.transfer.clone(),
            reply_to: req.reply_to.clone(),
            cdr: Some(cdr),
            started_at: Utc::now(),
            answred_at: None,
            forward: false,
            backup: None,
            backup_invite: None,
            recording: None,
            click_to_dial: None,
            callback: None,
            callback_peer: None,
            callflow: None,
            room: "".to_string(),
            remove_call_restrictions: false,
            ziron_tag: None,
            diversion: None,
            pai: None,
            remote_ip: req.remote_ip.clone(),
        };
        channel.set_call_context(&context).await?;
        let _ = REDIS.sadd(ALL_CHANNELS, &channel.id).await;
        let _ = REDIS
            .expire(&format!("nebula:channel:{}", &channel.id), 86400)
            .await;
        if let Some(tenant_id) = context.endpoint.tenant_id() {
            let _ = REDIS
                .sadd(
                    &format!("nebula:channels_for_tenant:{tenant_id}"),
                    &channel.id,
                )
                .await;
        }

        if context.refer.is_none() {
            let mut sdp = SessionDescription::from_str(&req.sdp)?;
            let is_ip_global = Ipv4Addr::from_str(&sdp.connection.addr)
                .map(|ip| is_ip_global(&ip))
                .unwrap_or(false);
            println!("invite is_ip_gloal {} {is_ip_global}", sdp.connection.addr);
            if !is_ip_global {
                if let Some(remote_ip) = &req.remote_ip {
                    println!("invite set sdp to {remote_ip}");
                    sdp.connection.addr = remote_ip.clone();
                }
            }
            channel.set_remote_sdp(&sdp, SdpType::Offer).await?;
            if context.redirect_peer.is_none() {
                match context.endpoint {
                    Endpoint::User { user, .. } => {
                        REDIS
                            .sadd(
                                &format!("nebula:sipuser:{}:channels", user.uuid),
                                &channel.id,
                            )
                            .await?;
                        notify_user_presence(&user.uuid);
                    }
                    _ => (),
                }
            }
        }
        Ok(channel)
    }

    pub fn new(id: String) -> Channel {
        Channel { id }
    }

    pub async fn create_outbound_call(call: CreateOutboundCall) -> Result<()> {
        let (operator_code, exten) =
            get_operator_code(phonenumber::country::GB, &call.destination);
        let number = phonenumber::parse(Some(phonenumber::country::GB), exten)?;
        if operator_code
            .as_ref()
            .map(|(_, short_operator_code)| *short_operator_code)
            .unwrap_or(false)
        {
            println!("short operator code, we don't need to check if valid");
        } else if !number.is_valid() {
            return Err(anyhow!("invalid number"));
        }

        let (callerid, anonymous) = if call.callerid.starts_with("anonymous") {
            let e164 = call.callerid[10..call.callerid.len() - 1].to_string();
            (e164, true)
        } else {
            (call.callerid.clone(), false)
        };

        let cdr = SWITCHBOARD_SERVICE.db.create_cdr().await?;
        let cdr_uuid = cdr.uuid;

        {
            let call = call.clone();
            tokio::spawn(async move {
                let _ = SWITCHBOARD_SERVICE
                    .db
                    .update_cdr(
                        cdr_uuid,
                        CdrItemChange {
                            tenant_id: Some(call.tenant_id.clone()),
                            from_type: Some("number".to_string()),
                            from_exten: Some(call.callerid.clone()),
                            to_type: Some("number".to_string()),
                            to_exten: Some(call.destination.clone()),
                            call_type: Some("outbound".to_string()),
                            ..Default::default()
                        },
                    )
                    .await;
            });
        }

        let e164 = number.format().mode(phonenumber::Mode::E164).to_string();

        let request_json = serde_json::json!({
            "uuid": &cdr_uuid,
            "reseller_uuid": &call.tenant_id,
            "operator_code": operator_code.map(|(c, _)| c.to_string()),
            "country_code": number.code().value(),
            "destination": number.national().value(),
            "e164": e164,
            "ip": "",
            "trunk": false,
            "callerid": Some(&callerid),
            "user_uuid": None::<Option<String>>,
            "remove_call_restrictions": false,
        });

        let extension = if let Some(internal_number) = SWITCHBOARD_SERVICE
            .db
            .get_number("admin", &e164[1..].to_string())
            .await
        {
            let (code, resp) = SWITCHBOARD_SERVICE
                .yaypi_client
                .check_outbound_restriction(request_json)
                .await?;
            if code != 200 {
                return Err(anyhow!("can't authenticate outbound call {}", resp));
            }
            BridgeExtension::Intra(internal_number, None)
        } else {
            let (code, resp) = SWITCHBOARD_SERVICE
                .yaypi_client
                .create_call(request_json)
                .await?;
            if code != 200 {
                return Err(anyhow!("can't authenticate outbound call {}", resp));
            }

            let provider = resp["provider"]
                .as_str()
                .ok_or_else(|| anyhow!("no provider in call creation"))?
                .to_string();
            let backup_provider = resp
                .get("backup_provider")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string())
                .filter(|v| v != &provider);

            let number = if operator_code.is_some() {
                resp.get("operator_number")
                    .and_then(|v| v.as_str())
                    .and_then(|s| {
                        let s = if !s.starts_with('+') && !s.starts_with('0') {
                            format!("+{}", s)
                        } else {
                            s.to_string()
                        };
                        phonenumber::parse(Some(phonenumber::country::GB), s).ok()
                    })
                    .unwrap_or(number)
            } else {
                number
            };

            let number = ExternalNumber {
                tenant_id: call.tenant_id.clone(),
                number,
                provider,
                backup_provider,
                caller_id: None,
                backup_caller_id: None,
                operator_code: operator_code.map(|(c, s)| {
                    (c.to_string(), s, resp.get("operator_number").is_some())
                }),
            };
            BridgeExtension::ExternalNumber(number, None)
        };

        let bridge_id = nebula_utils::uuid();
        let caller_id = CallerId {
            user: callerid,
            e164: None,
            display_name: "".to_string(),
            anonymous,
            asserted_identity: None,
            original_number: None,
            to_number: None,
        };
        let mut rtpmaps: Vec<Rtpmap> = EXTERNAL_AUDIO_ORDER
            .iter()
            .map(|pt| pt.get_rtpmap())
            .collect();
        rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());

        if let Some(locations) =
            extension.locations(false, "".to_string(), true).await
        {
            for (
                endpoint,
                location,
                backup_location,
                _force_caller_id,
                _force_backup_caller_id,
            ) in locations
            {
                let id = nebula_utils::uuid();
                REDIS
                    .sadd(&format!("nebula:bridge:{}:channels", &bridge_id), &id)
                    .await?;

                let caller_id = caller_id.clone();
                let bridge_id = bridge_id.clone();
                let tenant_id = call.tenant_id.clone();
                let rtpmaps = rtpmaps.clone();
                let inbound_channel = id.clone();
                let callflow = call.callflow.clone();
                let backup_location =
                    backup_location.map(|l| (l, caller_id.clone()));
                tokio::spawn(async move {
                    let _ = Channel::new_outbound(
                        id,
                        caller_id,
                        endpoint,
                        location,
                        backup_location,
                        inbound_channel,
                        bridge_id,
                        true,
                        rtpmaps,
                        None,
                        None,
                        None,
                        None,
                        Some((tenant_id, callflow, cdr_uuid)),
                        Vec::new(),
                        "".to_string(),
                        false,
                        true,
                    )
                    .await;
                });
            }
        }

        let tenant_id = call.tenant_id.clone();
        tokio::spawn(async move {
            let bridge = Bridge::new(bridge_id);
            let _ = bridge.set_ringtone("").await;
            let _ = bridge.set_announcement("").await;
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

    async fn get_transfer_target(
        user: &User,
        target: &str,
    ) -> Option<TransferTarget> {
        if let Some(uuid) = target.strip_prefix("app:") {
            let apps = SWITCHBOARD_SERVICE
                .db
                .get_mobile_apps(user)
                .await
                .unwrap_or_default();
            for app in &apps {
                if app.uuid.to_string() == uuid {
                    let uri = Uri {
                        user: Some(user.name.clone()),
                        host: "10.132.0.57".to_string(),
                        port: Some(5190),
                        transport: TransportType::Tcp,
                        ..Default::default()
                    };
                    return Some(TransferTarget::Location(
                        Location {
                            user: user.name.clone(),
                            proxy_host: SWITCHBOARD_SERVICE
                                .config
                                .london_proxy_host()
                                .to_string(),
                            from_host: user
                                .domain
                                .clone()
                                .unwrap_or_else(|| "talk.yay.com".to_string()),
                            to_host: user
                                .domain
                                .clone()
                                .unwrap_or_else(|| "talk.yay.com".to_string()),
                            display_name: user.display_name.clone(),
                            req_uri: uri.clone(),
                            dst_uri: uri,
                            route: Some(Uri {
                                host: "23.251.134.254".to_string(),
                                transport: TransportType::Tcp,
                                ..Default::default()
                            }),
                            intra: false,
                            inter_tenant: None,
                            encryption: false,
                            device: Some(app.device.clone()),
                            device_token: Some(app.device_token.clone()),
                            dnd: false,
                            dnd_allow_internal: false,
                            provider: None,
                            is_registration: None,
                            user_agent: Some(format!("yay {}", app.device)),
                            tenant_id: Some(user.tenant_id.clone()),
                            teams_refer_cx: None,
                            ziron_tag: None,
                            diversion: None,
                            pai: None,
                            global_dnd: false,
                            global_forward: None,
                            room_auth_token: None,
                        },
                        Endpoint::User {
                            user: user.clone(),
                            group: None,
                            user_agent: None,
                            user_agent_type: None,
                        },
                    ));
                }
            }
        } else if let Some(callid) = target.strip_prefix("registration:") {
            let hashed_callid = nebula_utils::sha256(callid);
            let username = &user.name.to_lowercase();
            let location_key = &Usrloc::location_key(username);
            if let Some(location) = Usrloc::lookup_one(
                user,
                &hashed_callid,
                location_key,
                &mut HashSet::new(),
            )
            .await
            {
                return Some(TransferTarget::Location(
                    location,
                    Endpoint::User {
                        user: user.clone(),
                        group: None,
                        user_agent: None,
                        user_agent_type: None,
                    },
                ));
            }
        } else if let Some(number) = target.strip_prefix("number:") {
            return Some(TransferTarget::Number(number.to_string()));
        }
        None
    }

    pub async fn raw_transfer(
        &self,
        target: &str,
        auto_answer: bool,
        empty_sdp: bool,
        destination: &str,
    ) -> Result<()> {
        self.ring("".to_string()).await?;
        let transfer_channel = self
            .transfer_call(target, auto_answer, empty_sdp, destination)
            .await?;
        let transfer_channel = Channel::new(transfer_channel);
        tokio::select! {
            _ = transfer_channel.receive_event("0", ChannelEvent::Answer) => {
            }
            _ = transfer_channel.receive_event("0", ChannelEvent::Hangup) => {
                return Err(anyhow!("the receipient channel hangup, can't continue"));
            }
        }

        if self.is_hangup().await {
            return Err(anyhow!("channel hangup, can't continue"));
        }

        self.answer().await?;

        Ok(())
    }

    pub async fn transfer_call(
        &self,
        target: &str,
        auto_answer: bool,
        empty_sdp: bool,
        destination: &str,
    ) -> Result<String> {
        let mut context = self.get_call_context().await?;
        let user = context
            .endpoint
            .user()
            .ok_or_else(|| anyhow!("not a call from a user"))?;

        if target.trim().is_empty() {
            if let Some(transfer) = &context.transfer {
                if let TransferPeer::Initiator { to_channel, .. } = &transfer.peer {
                    let _ = SWITCHBOARD_SERVICE
                        .switchboard_rpc
                        .hangup(
                            &SWITCHBOARD_SERVICE.config.switchboard_host,
                            to_channel.to_string(),
                            false,
                            None,
                            None,
                            None,
                        )
                        .await;
                    if let Some(peer) = self.get_peer().await {
                        if let Some(other_peer) =
                            Channel::new(to_channel.to_string()).get_peer().await
                        {
                            let _ = Bridge::unbridge_media(&peer, &other_peer).await;
                            let _ = Bridge::unbridge_media(&other_peer, &peer).await;
                        }
                    }
                }
            }
            context.transfer = None;
            self.set_call_context(&context).await?;
            if let Some(peer) = self.get_peer().await {
                let _ = Bridge::bridge_media(&peer, self).await;
                let _ = Bridge::bridge_media(self, &peer).await;
            }
            return Err(anyhow!("no target"));
        }

        let target = Self::get_transfer_target(user, target)
            .await
            .ok_or_else(|| anyhow!("can't find target"))?;

        if let Some(transfer) = &context.transfer {
            if let TransferPeer::Initiator { to_channel, .. } = &transfer.peer {
                let _ = SWITCHBOARD_SERVICE
                    .switchboard_rpc
                    .hangup(
                        &SWITCHBOARD_SERVICE.config.switchboard_host,
                        to_channel.to_string(),
                        false,
                        None,
                        None,
                        None,
                    )
                    .await;
                if let Some(peer) = self.get_peer().await {
                    if let Some(other_peer) =
                        Channel::new(to_channel.to_string()).get_peer().await
                    {
                        let _ = Bridge::unbridge_media(&peer, &other_peer).await;
                        let _ = Bridge::unbridge_media(&other_peer, &peer).await;
                    }
                }
            }
        }

        let transfer_channel_id = nebula_utils::uuid();
        context.transfer = Some(TransferContext {
            peer: TransferPeer::Initiator {
                to_channel: transfer_channel_id.clone(),
                empty_sdp,
            },
        });
        self.set_call_context(&context).await?;

        if let Some(peer) = self.get_peer().await {
            let _ = Bridge::unbridge_media(&peer, self).await;
            let _ = Bridge::unbridge_media(self, &peer).await;
        }

        let to = match &target {
            TransferTarget::Location(_, _) => {
                user.extension.unwrap_or(0).to_string()
            }
            TransferTarget::Number(n) => n.to_string(),
        };

        {
            let mut rtpmaps: Vec<Rtpmap> = INTERNAL_AUDIO_ORDER
                .iter()
                .map(|pt| pt.get_rtpmap())
                .collect();
            rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());
            SWITCHBOARD_SERVICE
                .switchboard_rpc
                .new_inbound_channel(
                    &SWITCHBOARD_SERVICE.config.switchboard_host,
                    transfer_channel_id.clone(),
                    &context.endpoint,
                    "",
                    &to,
                    &SessionDescription::create_basic(
                        SWITCHBOARD_SERVICE.config.media_ip,
                        DUMMY_RTP_PORT,
                        None,
                        rtpmaps,
                    )
                    .to_string(),
                    false,
                    None,
                    None,
                    None,
                    Vec::new(),
                    None,
                    None,
                    None,
                    Some(TransferContext {
                        peer: TransferPeer::Recipient {
                            from_channel: self.id.clone(),
                            target,
                            auto_answer,
                            destination: destination.to_string(),
                        },
                    }),
                    None,
                    None,
                )
                .await?;
        }
        Ok(transfer_channel_id)
    }

    pub async fn create_call(call: CreateCall) -> Result<()> {
        let user = if let Some(user_uuid) = call.user_uuid {
            let user = SWITCHBOARD_SERVICE.db.get_user("admin", &user_uuid).await;
            user
        } else {
            None
        };

        let tenant_id = call
            .tenant_id
            .or_else(|| user.clone().map(|u| u.tenant_id.clone()))
            .ok_or(anyhow!("dont' have tenant id"))?;
        let tenant_id = &tenant_id;
        let mut extensions: Vec<BridgeExtension> =
            stream::iter(call.targets.unwrap_or(Vec::new()))
                .filter_map(|t| async move {
                    match t.get("type")?.as_str()?.to_lowercase().as_str() {
                        "sipuser" => {
                            let sipuser_uuid = t.get("uuid")?.as_str()?;
                            let user = SWITCHBOARD_SERVICE
                                .db
                                .get_user(tenant_id, &sipuser_uuid)
                                .await?;
                            Some(BridgeExtension::User(user))
                        }
                        "huntgroup" => {
                            let group_uuid = t.get("uuid")?.as_str()?;
                            let group = SWITCHBOARD_SERVICE
                                .db
                                .get_group(tenant_id, &group_uuid)
                                .await?;
                            Some(BridgeExtension::Group(group))
                        }
                        _ => None,
                    }
                })
                .collect()
                .await;
        if let Some(user) = user {
            extensions.push(BridgeExtension::User(user));
        }

        let bridge_id = nebula_utils::uuid();
        let caller_id = CallerId {
            user: call.destination.clone(),
            e164: None,
            display_name: call.display_name.unwrap_or("".to_string()),
            anonymous: false,
            asserted_identity: None,
            original_number: None,
            to_number: None,
        };
        let mut rtpmaps: Vec<Rtpmap> = INTERNAL_AUDIO_ORDER
            .iter()
            .map(|pt| pt.get_rtpmap())
            .collect();
        rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());

        let inbound_channel = nebula_utils::uuid();
        for extension in &extensions {
            if let Some(locations) =
                extension.locations(true, "".to_string(), true).await
            {
                for (
                    endpoint,
                    location,
                    backup_location,
                    _force_caller_id,
                    _force_backup_caller_id,
                ) in locations
                {
                    let id = nebula_utils::uuid();
                    REDIS
                        .sadd(&format!("nebula:bridge:{}:channels", &bridge_id), &id)
                        .await?;

                    let caller_id = caller_id.clone();
                    let bridge_id = bridge_id.clone();

                    let mut rtpmaps = rtpmaps.clone();
                    if location.device.is_none()
                        && location.dst_uri.transport != TransportType::Wss
                        && location.dst_uri.transport != TransportType::Ws
                    {
                        let rtpmap = Rtpmap {
                            name: PayloadType::TelephoneEvent,
                            rate: 48000,
                            type_number: 102,
                            fmtp: "0-15".to_string(),
                            ..Default::default()
                        };
                        rtpmaps.push(rtpmap);
                    }

                    let proxy_host = call.proxy_host.clone().unwrap_or_default();
                    let destination = call.destination.clone();
                    let inbound_channel = inbound_channel.clone();
                    let backup_location =
                        backup_location.map(|l| (l, caller_id.clone()));
                    if let Some(tenant_id) = endpoint.tenant_id() {
                        let _ = REDIS
                            .sadd(
                                &format!("nebula:channels_for_tenant:{tenant_id}"),
                                &id,
                            )
                            .await;
                    }
                    tokio::spawn(async move {
                        let _ = Channel::new_outbound(
                            id,
                            caller_id,
                            endpoint,
                            location,
                            backup_location,
                            inbound_channel,
                            bridge_id,
                            true,
                            rtpmaps,
                            None,
                            None,
                            Some((destination, proxy_host)),
                            None,
                            None,
                            Vec::new(),
                            "".to_string(),
                            false,
                            true,
                        )
                        .await;
                    });
                }
            }
        }
        let bridge = Bridge::new(bridge_id.clone());
        bridge.set_ringtone("").await?;
        bridge.set_announcement("").await?;
        bridge.set_tenant_id(tenant_id).await?;
        let _ = bridge
            .handle_timeout(
                tokio::time::Instant::now() + tokio::time::Duration::from_secs(60),
                false,
            )
            .await;

        Ok(())
    }

    pub async fn switch(&self, req: NewInboundChannelRequest) -> Result<()> {
        let switchboard = Switchboard::new(self.clone());
        if let Err(e) = switchboard.switch(req).await {
            warn!(channel = self.id, "switchboard got error {e:?}");
        }
        tracing::info!(channel = self.id, "switchboard finished");
        self.hangup(None).await?;
        Ok(())
    }

    async fn check_reinvite_crypto(
        &self,
        reinvite_sdp: &SessionDescription,
    ) -> Result<()> {
        let reinvite_audio =
            reinvite_sdp.audio().ok_or_else(|| anyhow!("no audio"))?;
        if reinvite_audio.cryptos().is_empty() {
            return Ok(());
        }
        let local_sdp = self.get_local_sdp().await?;
        let local_audio = local_sdp.audio().ok_or_else(|| anyhow!("no audio"))?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .reinvite_crypto(
                local_audio.uuid(&self.id).to_string(),
                local_audio.to_string(),
                reinvite_audio.to_string(),
            )
            .await?;

        Ok(())
    }

    pub async fn reinvite(&self, req: InviteRequest) -> Result<()> {
        println!("got reinvite");
        let context = self.get_call_context().await?;
        if context.refer.is_some() {
            return Ok(());
        }
        if req.body.is_empty() {
            let sdp = self
                .reinvite_sdp(MediaMode::Sendrecv)
                .await
                .map(|s| s.to_string())
                .unwrap_or_else(|_| "".to_string());
            SWITCHBOARD_SERVICE
                .proxy_rpc
                .answer(&context.proxy_host, self.id.clone(), sdp, None, None)
                .await?;
            return Ok(());
        }
        let incoming_sdp = SessionDescription::from_str(&req.body)?;
        let mode = match incoming_sdp.audio() {
            Some(audio) => match audio.mode() {
                MediaMode::Sendrecv => {
                    if context.endpoint.provider().is_none() {
                        let _ = self.unhold().await;
                        let _ = self.check_reinvite_crypto(&incoming_sdp).await;
                    } else {
                        // let _ = self.check_reinvite_sdp(&incoming_sdp).await;
                    }

                    MediaMode::Sendrecv
                }
                MediaMode::Sendonly => {
                    if context.endpoint.provider().is_none() {
                        let _ = self.hold().await;
                        self.set_media_mode(MediaMode::Recvonly).await?;
                    }
                    MediaMode::Recvonly
                }
                MediaMode::Inactive => {
                    if context.endpoint.provider().is_none() {
                        let _ = self.hold().await;
                        self.set_media_mode(MediaMode::Inactive).await?;
                    }
                    MediaMode::Inactive
                }
                MediaMode::Recvonly => MediaMode::Sendonly,
            },
            None => {
                if let Some(image) = incoming_sdp.image() {
                    if let Some(peer) = self.get_peer().await {
                        let mut reinvite_sdp = peer.get_local_sdp().await?;
                        let mut reinvite_image = image.clone();
                        if let Some(port) = reinvite_sdp.audio().map(|a| a.port) {
                            reinvite_image.port = port;
                        }
                        reinvite_sdp.media_descriptions = vec![reinvite_image];

                        {
                            let audio_id = self.get_audio_id().await?;
                            let peer_audio_id = peer.get_audio_id().await?;

                            check_peer_addr(
                                &audio_id,
                                &incoming_sdp.connection.addr,
                                image.port,
                            )
                            .await;

                            let _ = REDIS
                                .hset(
                                    &format!("nebula:media_stream:{audio_id}"),
                                    "t38_passthrough",
                                    "yes",
                                )
                                .await;
                            let _ = REDIS
                                .hset(
                                    &format!("nebula:media_stream:{peer_audio_id}"),
                                    "t38_passthrough",
                                    "yes",
                                )
                                .await;

                            SWITCHBOARD_SERVICE
                                .media_rpc_client
                                .set_t38_passthrough(audio_id)
                                .await?;
                            SWITCHBOARD_SERVICE
                                .media_rpc_client
                                .set_t38_passthrough(peer_audio_id)
                                .await?;
                        }

                        SWITCHBOARD_SERVICE
                            .proxy_rpc
                            .reinvite(
                                &context.proxy_host,
                                peer.id.clone(),
                                reinvite_sdp.to_string(),
                                SWITCHBOARD_SERVICE.config.switchboard_host.clone(),
                            )
                            .await?;
                        return Ok(());
                    }

                    let mut answer_sdp = self.get_local_sdp().await?;
                    let mut answer_image = image.clone();
                    if let Some(port) = answer_sdp.audio().map(|a| a.port) {
                        answer_image.port = port;
                    }
                    answer_sdp.media_descriptions = vec![answer_image];
                    SWITCHBOARD_SERVICE
                        .proxy_rpc
                        .answer(
                            &context.proxy_host,
                            self.id.clone(),
                            answer_sdp.to_string(),
                            None,
                            None,
                        )
                        .await?;
                    let t38_params = InitT38Params {
                        version: 0,
                        max_bit_rate: 14400,
                        max_buffer: 2000,
                        max_datagram: 400,
                        fill_bit_removal: 1,
                        transcoding_mmr: 0,
                        transcoding_jbig: 0,
                        rate_management: "transferredTCF".to_string(),
                    };
                    // for attr in image.attributes.iter() {
                    //     if let Some(value) = attr.value.as_ref() {
                    //         match attr.key.to_lowercase().as_str() {
                    //             "t38faxversion" => {
                    //                 if let Ok(version) = value.parse::<usize>() {
                    //                     t38_params.version = version;
                    //                 }
                    //             }
                    //             "t38maxbitrate" => {
                    //                 if let Ok(value) = value.parse::<usize>() {
                    //                     t38_params.max_bit_rate = value;
                    //                 }
                    //             }
                    //             "t38faxmaxbuffer" => {
                    //                 if let Ok(value) = value.parse::<usize>() {
                    //                     t38_params.max_buffer = value;
                    //                 }
                    //             }
                    //             "t38faxmaxdatagram" => {
                    //                 if let Ok(value) = value.parse::<usize>() {
                    //                     t38_params.max_datagram = value;
                    //                 }
                    //             }
                    //             "t38faxfillbitremoval" => {
                    //                 if let Ok(value) = value.parse::<usize>() {
                    //                     t38_params.fill_bit_removal = value;
                    //                 }
                    //             }
                    //             "t38faxtranscodingmmr" => {
                    //                 if let Ok(value) = value.parse::<usize>() {
                    //                     t38_params.transcoding_mmr = value;
                    //                 }
                    //             }
                    //             "t38faxtranscodingjbig" => {
                    //                 if let Ok(value) = value.parse::<usize>() {
                    //                     t38_params.transcoding_jbig = value;
                    //                 }
                    //             }
                    //             "t38faxratemanagement" => {
                    //                 t38_params.rate_management = value.to_string();
                    //             }
                    //             _ => {}
                    //         }
                    //     }
                    // }
                    let audio_id = self.get_audio_id().await?;
                    info!(
                        channel = self.id,
                        "[{}] init t38 {:?}", self.id, t38_params
                    );
                    SWITCHBOARD_SERVICE
                        .media_rpc_client
                        .init_t38(audio_id, t38_params)
                        .await?;
                }
                return Ok(());
            }
        };
        let mut rtpmap = self
            .negotiate_audio_rtpmap()
            .await
            .ok_or(anyhow!("don't have negotiated rtpmap"))?;

        let changed_rtpmap_type_number = incoming_sdp.audio().and_then(|audio| {
            audio.get_rtpmap(&rtpmap.name).and_then(|incoming_rtpmap| {
                if incoming_rtpmap.type_number != rtpmap.type_number {
                    Some(incoming_rtpmap.type_number)
                } else {
                    None
                }
            })
        });
        if let Some(new_type_number) = changed_rtpmap_type_number {
            // if rtpmap type number changed, we need to update it as well
            rtpmap.type_number = new_type_number;
        }

        let mut answer_sdp = self.get_local_sdp().await?;
        answer_sdp.audio_mut().map(|audio| {
            let dtmf_rtpmap = audio.dtmf_rtpmap(None);

            audio.clear_rtpmap();
            audio.insert_rtpmap(&rtpmap);
            if let Some(dtmf_rtpmap) = dtmf_rtpmap {
                audio.insert_rtpmap(&dtmf_rtpmap);
            }
            audio.set_mode(mode);
            for attribute in &mut audio.attributes {
                if attribute.key == "setup" {
                    // make sure setup=active because we're answering
                    attribute.value = Some("active".to_string());
                }
            }
        });

        if changed_rtpmap_type_number.is_some() {
            self.set_remote_sdp(&incoming_sdp, SdpType::Offer).await?;
            self.set_local_sdp(&answer_sdp, SdpType::Answer).await?;
            let audio_id = self.get_audio_id().await?;
            let _ = SWITCHBOARD_SERVICE
                .media_rpc_client
                .stop_media_stream(audio_id, "codec change".to_string(), false)
                .await;
            tracing::info!(
                channel = self.id,
                "reinvite had a new rtpmap type number, so we're updating it"
            );
        }

        SWITCHBOARD_SERVICE
            .proxy_rpc
            .answer(
                &context.proxy_host,
                self.id.clone(),
                answer_sdp.to_string(),
                None,
                None,
            )
            .await?;
        Ok(())
    }

    async fn reinvite_sdp(&self, mode: MediaMode) -> Result<SessionDescription> {
        let rtpmap = self
            .negotiate_audio_rtpmap()
            .await
            .ok_or(anyhow!("don't have negotiated rtpmap"))?;
        let mut answer_sdp = self.get_local_sdp().await?;
        if let Some(audio) = answer_sdp.audio_mut() {
            let dtmf_rtpmap = audio.dtmf_rtpmap(None);

            audio.clear_rtpmap();
            audio.insert_rtpmap(&rtpmap);
            if let Some(dtmf_rtpmap) = dtmf_rtpmap {
                audio.insert_rtpmap(&dtmf_rtpmap);
            }
            audio.set_mode(mode);
        }
        Ok(answer_sdp)
    }

    async fn get_bridge_channel(&self) -> Option<Channel> {
        let inbound_channel_id: String = REDIS
            .hget(&format!("nebula:channel:{}", &self.id), "inbound_channel")
            .await
            .unwrap_or("".to_string());
        if inbound_channel_id == "" {
            return Some(self.clone());
        }

        Some(Channel::new(inbound_channel_id))
    }

    async fn update_callerid(&self, from_channel: &Channel) -> Result<()> {
        let from_context = from_channel.get_call_context().await?;
        let context = self.get_call_context().await?;
        let user = context
            .endpoint
            .user()
            .ok_or(anyhow!("this channel isn't sipuser"))?
            .clone();
        if let Some(location) = from_context.location.as_ref() {
            if location.device.is_some() {
                return Ok(());
            }
        }

        let rtpmap = self
            .negotiate_audio_rtpmap()
            .await
            .ok_or(anyhow!("don't have negotiated rtpmap"))?;
        let mut answer_sdp = self.get_local_sdp().await?;
        answer_sdp.audio_mut().map(|audio| {
            let dtmf_rtpmap = audio.dtmf_rtpmap(None);

            audio.clear_rtpmap();
            audio.insert_rtpmap(&rtpmap);
            if let Some(dtmf_rtpmap) = dtmf_rtpmap {
                audio.insert_rtpmap(&dtmf_rtpmap);
            }
        });

        SWITCHBOARD_SERVICE
            .proxy_rpc
            .update_caller_id(
                &context.proxy_host,
                self.id.clone(),
                internal_caller_id(&from_context, user.cc.clone(), None).await,
                answer_sdp.to_string(),
            )
            .await?;
        Ok(())
    }

    async fn blind_transfer(&self, exten: String) -> Result<()> {
        let mut context = self.get_call_context().await?;

        let from_user = match &context.endpoint {
            Endpoint::User { user, .. } => user.clone(),
            _ => return Err(anyhow!("can't refer from this type of endpoint")),
        };
        let from = Endpoint::User {
            user: from_user.clone(),
            group: None,
            user_agent: None,
            user_agent_type: None,
        };
        let _ = self.unhold().await;

        let mut no_recording = false;
        if from_user
            .disable_external_transfer_recording
            .unwrap_or(false)
        {
            if let Some(peer) = self.get_peer().await {
                if let Ok(peer_context) = peer.get_call_context().await {
                    if peer_context.endpoint.provider().is_some()
                        && Self::get_extension(&from_user.tenant_id, &exten)
                            .await
                            .is_none()
                    {
                        no_recording = true;
                        if let Ok(audio_id) = self.get_audio_id().await {
                            let _ = SWITCHBOARD_SERVICE
                                .media_rpc_client
                                .pause_recording(audio_id)
                                .await;
                        }
                        if let Ok(audio_id) = peer.get_audio_id().await {
                            let _ = SWITCHBOARD_SERVICE
                                .media_rpc_client
                                .pause_recording(audio_id)
                                .await;
                        }
                    }
                }
            }
        }

        let refer_channel_id = nebula_utils::uuid();
        context.refer = Some(ReferContext {
            direction: CallDirection::Initiator,
            peer: refer_channel_id.clone(),
            slot: None,
            no_recording,
        });
        context.transfer = None;
        self.set_call_context(&context).await?;
        // SWITCHBOARD_SERVICE
        //     .proxy_rpc_client
        //     .notify_refer_success(self.id.clone())
        //     .await?;
        info!(
            from_channel = &self.id,
            to_channel = &refer_channel_id,
            "blind transfer"
        );
        SWITCHBOARD_SERVICE
            .proxy_rpc
            .hangup(
                &context.proxy_host,
                self.id.clone(),
                None,
                None,
                None,
                self.is_answered().await,
            )
            .await?;
        self.delete_channel_user().await?;
        let _ = self.remove_in_wait_lock().await;
        self.reset_refer_sdp(&Channel::new(refer_channel_id.clone()))
            .await?;

        let refer_context = ReferContext {
            direction: CallDirection::Recipient,
            peer: self.id.clone(),
            slot: None,
            no_recording,
        };
        let req = NewInboundChannelRequest {
            id: refer_channel_id.clone(),
            from,
            to: exten,
            proxy_host: "".to_string(),
            sdp: "".to_string(),
            refer: Some(refer_context),
            transfer: None,
            raw_transfer: None,
            existing: false,
            intra: None,
            intra_chain: Vec::new(),
            inter_tenant: None,
            redirect: None,
            reply_to: None,
            replaces: None,
            callflow: None,
            teams_refer_cx: None,
            remote_ip: None,
        };
        let refer_channel = Channel::new_inbound(&req).await?;

        if let Some(inbound_channel) = self.get_bridge_channel().await {
            inbound_channel
                .update_cdr(CdrItemChange {
                    child: refer_channel
                        .get_cdr()
                        .await
                        .map(|cdr| cdr.uuid.to_string()),
                    ..Default::default()
                })
                .await?;
            refer_channel
                .update_cdr(CdrItemChange {
                    parent: inbound_channel
                        .get_cdr()
                        .await
                        .map(|cdr| cdr.uuid.to_string()),
                    ..Default::default()
                })
                .await?;
        }

        refer_channel.answer().await?;
        refer_channel.switch(req).await?;

        Ok(())
    }

    #[async_recursion::async_recursion]
    pub async fn refer(&self, req: ReferRequest) -> Result<()> {
        if let Some(peer) = self.get_peer().await {
            if let Ok(peer_context) = peer.get_call_context().await {
                if let Some(transfer) = peer_context.transfer.as_ref() {
                    match &transfer.peer {
                        TransferPeer::Initiator { .. } => {}
                        TransferPeer::Recipient { from_channel, .. } => {
                            let from_channel = Channel::new(from_channel.clone());
                            return from_channel.refer(req).await;
                        }
                    }
                }
            }
        }

        let mutex =
            DistributedMutex::new(format!("nebula:channel:{}:refer:lock", &self.id));
        mutex.lock().await;

        let mut context = self.get_call_context().await?;
        if context.refer.is_some() {
            return Ok(());
        }

        if let Some(transfer) = context.transfer.as_ref() {
            match &transfer.peer {
                TransferPeer::Initiator { to_channel, .. } => {
                    let to_channel = Channel::new(to_channel.clone());
                    let mut to_channel_context =
                        to_channel.get_call_context().await?;
                    to_channel_context.transfer = None;
                    to_channel.set_call_context(&to_channel_context).await?;
                    let _ = SWITCHBOARD_SERVICE
                        .switchboard_rpc
                        .hangup(
                            &SWITCHBOARD_SERVICE.config.switchboard_host,
                            to_channel.id.clone(),
                            false,
                            None,
                            None,
                            None,
                        )
                        .await;
                    if let Some(peer) = self.get_peer().await {
                        if let Some(other_peer) = to_channel.get_peer().await {
                            let _ = Bridge::unbridge_media(&peer, &other_peer).await;
                            let _ = Bridge::unbridge_media(&other_peer, &peer).await;
                        }
                    }
                    if let Some(peer) = self.get_peer().await {
                        let _ = Bridge::bridge_media(&peer, self).await;
                        let _ = Bridge::bridge_media(self, &peer).await;
                    }
                }
                TransferPeer::Recipient { .. } => {}
            }
        }

        info!(channel = self.id, "now refer {req:?}");
        match req.to {
            ReferTo::Exten(exten) => {
                self.blind_transfer(exten).await?;
            }
            ReferTo::Channel(refer_channel_id) => {
                let from_user = match &context.endpoint {
                    Endpoint::User { user, .. } => user.clone(),
                    _ => {
                        return Err(anyhow!(
                            "can't refer from this type of endpoint"
                        ))
                    }
                };

                let refer_channel = Channel::new(refer_channel_id.clone());
                let mut refer_channel_context =
                    refer_channel.get_call_context().await?;

                // Case where attended transfer to mailbox via `**extension`
                // When confirming the transfer, we create a new blind transfer to
                //   the endpoint and hangup the new leg the attended transfer made
                if refer_channel_context.to.starts_with("**") {
                    tokio::spawn(async move {
                        let _ = refer_channel.hangup(None).await;
                    });
                    return self.blind_transfer(refer_channel_context.to).await;
                }

                context.refer = Some(ReferContext {
                    direction: CallDirection::Initiator,
                    peer: refer_channel_id.clone(),
                    slot: None,
                    no_recording: false,
                });
                context.transfer = None;
                self.set_call_context(&context).await?;
                refer_channel_context.refer = Some(ReferContext {
                    direction: CallDirection::Recipient,
                    peer: self.id.clone(),
                    slot: None,
                    no_recording: false,
                });
                refer_channel
                    .set_call_context(&refer_channel_context)
                    .await?;

                // SWITCHBOARD_SERVICE
                //     .proxy_rpc_client
                //     .notify_refer_success(self.id.clone())
                //     .await?;
                SWITCHBOARD_SERVICE
                    .proxy_rpc
                    .hangup(
                        &context.proxy_host,
                        self.id.clone(),
                        None,
                        None,
                        None,
                        self.is_answered().await,
                    )
                    .await?;
                SWITCHBOARD_SERVICE
                    .proxy_rpc
                    .hangup(
                        &context.proxy_host,
                        refer_channel_id.clone(),
                        None,
                        None,
                        None,
                        is_channel_answered(&refer_channel_id).await,
                    )
                    .await?;
                self.delete_channel_user().await?;
                let _ = self.remove_in_wait_lock().await;
                refer_channel.delete_channel_user().await?;
                let _ = refer_channel.remove_in_wait_lock().await;

                self.reset_refer_sdp(&refer_channel).await?;

                if let Some(inbound_channel) = self.get_bridge_channel().await {
                    if let Some(refer_inbound_channel) =
                        refer_channel.get_bridge_channel().await
                    {
                        inbound_channel
                            .update_cdr(CdrItemChange {
                                child: refer_inbound_channel
                                    .get_cdr()
                                    .await
                                    .map(|cdr| cdr.uuid.to_string()),
                                ..Default::default()
                            })
                            .await?;
                        refer_inbound_channel
                            .update_cdr(CdrItemChange {
                                parent: inbound_channel
                                    .get_cdr()
                                    .await
                                    .map(|cdr| cdr.uuid.to_string()),
                                ..Default::default()
                            })
                            .await?;
                    }
                }

                if from_user
                    .disable_external_transfer_recording
                    .unwrap_or(false)
                {
                    if let Some(peer) = self.get_peer().await {
                        if let Ok(peer_context) = peer.get_call_context().await {
                            if peer_context.endpoint.provider().is_some() {
                                let refer_channel =
                                    Channel::new(refer_channel_id.clone());
                                if let Some(refer_peer) =
                                    refer_channel.get_peer().await
                                {
                                    if let Ok(refer_peer_context) =
                                        refer_peer.get_call_context().await
                                    {
                                        if refer_peer_context
                                            .endpoint
                                            .provider()
                                            .is_some()
                                        {
                                            if let Ok(audio_id) =
                                                self.get_audio_id().await
                                            {
                                                let _ = SWITCHBOARD_SERVICE
                                                    .media_rpc_client
                                                    .pause_recording(audio_id)
                                                    .await;
                                            }
                                            if let Ok(audio_id) =
                                                peer.get_audio_id().await
                                            {
                                                let _ = SWITCHBOARD_SERVICE
                                                    .media_rpc_client
                                                    .pause_recording(audio_id)
                                                    .await;
                                            }
                                            if let Ok(audio_id) =
                                                refer_channel.get_audio_id().await
                                            {
                                                let _ = SWITCHBOARD_SERVICE
                                                    .media_rpc_client
                                                    .pause_recording(audio_id)
                                                    .await;
                                            }
                                            if let Ok(audio_id) =
                                                refer_peer.get_audio_id().await
                                            {
                                                let _ = SWITCHBOARD_SERVICE
                                                    .media_rpc_client
                                                    .pause_recording(audio_id)
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                let _ = self.unhold().await;
                let _ = refer_channel.unhold().await;
                if let Some(peer) = refer_channel.get_peer().await {
                    if let Some(self_peer) = self.get_peer().await {
                        let _ = peer.update_callerid(&self_peer).await;
                    }
                }
                println!("attended transfer done");
            }
            ReferTo::Teams { refer_to, refer_by } => {
                let from = match &context.endpoint {
                    Endpoint::User { user, .. } => Endpoint::User {
                        user: user.clone(),
                        group: None,
                        user_agent: None,
                        user_agent_type: None,
                    },
                    _ => {
                        return Err(anyhow!(
                            "can't refer from this type of endpoint"
                        ))
                    }
                };
                let _ = self.unhold().await;

                let refer_channel_id = nebula_utils::uuid();
                context.refer = Some(ReferContext {
                    direction: CallDirection::Initiator,
                    peer: refer_channel_id.clone(),
                    slot: None,
                    no_recording: false,
                });
                context.transfer = None;
                self.set_call_context(&context).await?;

                println!("teams transfer");
                self.delete_channel_user().await?;
                let _ = self.remove_in_wait_lock().await;
                self.reset_refer_sdp(&Channel::new(refer_channel_id.clone()))
                    .await?;

                let refer_context = ReferContext {
                    direction: CallDirection::Recipient,
                    peer: self.id.clone(),
                    slot: None,
                    no_recording: false,
                };
                let req = NewInboundChannelRequest {
                    id: refer_channel_id.clone(),
                    from,
                    to: "".to_string(),
                    proxy_host: "".to_string(),
                    sdp: "".to_string(),
                    refer: Some(refer_context),
                    transfer: None,
                    raw_transfer: None,
                    existing: false,
                    intra: None,
                    intra_chain: Vec::new(),
                    inter_tenant: None,
                    redirect: None,
                    reply_to: None,
                    replaces: None,
                    callflow: None,
                    teams_refer_cx: Some(TeamsReferCx {
                        channel_id: self.id.clone(),
                        refer_to,
                        refer_by,
                    }),
                    remote_ip: None,
                };
                let refer_channel = Channel::new_inbound(&req).await?;

                if let Some(inbound_channel) = self.get_bridge_channel().await {
                    inbound_channel
                        .update_cdr(CdrItemChange {
                            child: refer_channel
                                .get_cdr()
                                .await
                                .map(|cdr| cdr.uuid.to_string()),
                            ..Default::default()
                        })
                        .await?;
                    refer_channel
                        .update_cdr(CdrItemChange {
                            parent: inbound_channel
                                .get_cdr()
                                .await
                                .map(|cdr| cdr.uuid.to_string()),
                            ..Default::default()
                        })
                        .await?;
                }

                refer_channel.answer().await?;
                refer_channel.switch(req).await?;
            }
        }
        Ok(())
    }

    pub async fn monitor_call(
        &self,
        tenant_id: &str,
        kind: CallMonitoring,
        exten: i64,
    ) -> Result<()> {
        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or(anyhow!("not coming from a user"))?;

        let extension = SWITCHBOARD_SERVICE
            .db
            .get_extension(tenant_id, exten)
            .await
            .ok_or(anyhow!("no extension"))?;
        if extension.discriminator != "sipuser" {
            return Err(anyhow!("can't listen to a non sipuser"));
        }
        let to_user = SWITCHBOARD_SERVICE
            .db
            .get_user(tenant_id, &extension.parent_id)
            .await
            .ok_or(anyhow!("can'd find sipuser"))?;
        if !SWITCHBOARD_SERVICE
            .db
            .check_sipuser_permission(
                &from_user.uuid,
                &to_user.uuid,
                &kind.to_string(),
            )
            .await
        {
            match kind {
                CallMonitoring::Listen => {
                    if !from_user.listen.unwrap_or(false) {
                        return Err(anyhow!("user isn't allowed to listen"));
                    }
                    if !to_user.be_listened.unwrap_or(false) {
                        return Err(anyhow!("user isn't allowed to be listened"));
                    }
                }
                CallMonitoring::Whisper => {
                    if !from_user.whisper.unwrap_or(false) {
                        return Err(anyhow!("user isn't allowed to whipser"));
                    }
                    if !to_user.be_whispered.unwrap_or(false) {
                        return Err(anyhow!("user isn't allowed to be whisperd"));
                    }
                }
                CallMonitoring::Barge => {
                    if !from_user.barge.unwrap_or(false) {
                        return Err(anyhow!("user isn't allowed to barge"));
                    }
                    if !to_user.be_barged.unwrap_or(false) {
                        return Err(anyhow!("user isn't allowed to be barged"));
                    }
                }
            };
        }

        REDIS
            .sadd(
                &format!("nebula:monitor:{}:user:{}", kind, to_user.uuid),
                &self.id,
            )
            .await?;

        self.answer().await?;
        let channels: Vec<String> = REDIS
            .smembers(&format!("nebula:sipuser:{}:channels", to_user.uuid))
            .await
            .unwrap_or(Vec::new());
        for channel in channels {
            let channel = Channel::new(channel);
            let _ = self.monitor_channel(&kind, &channel).await;
        }

        self.receive_event("0", ChannelEvent::Hangup).await?;
        REDIS
            .srem(
                &format!("nebula:monitor:{}:user:{}", kind, to_user.uuid),
                &self.id,
            )
            .await?;
        Ok(())
    }

    pub async fn monitor_channel(
        &self,
        kind: &CallMonitoring,
        channel: &Channel,
    ) -> Result<()> {
        Bridge::bridge_media(channel, self).await?;
        if let Some(peer) = channel.get_peer().await {
            Bridge::bridge_media(&peer, self).await?;
        }
        if kind != &CallMonitoring::Listen {
            Bridge::bridge_media(self, channel).await?;
        }

        if kind == &CallMonitoring::Barge {
            if let Some(peer) = channel.get_peer().await {
                Bridge::bridge_media(self, &peer).await?;
            }
        }
        Ok(())
    }

    async fn hold(&self) -> Result<()> {
        self.new_event(ChannelEvent::Hold, "hold").await?;
        let peer = self
            .get_peer()
            .await
            .ok_or(anyhow!("channel doesn't have peer"))?;

        let peer_context = peer.get_call_context().await?;
        let context = self.get_call_context().await?;
        let moh = if let Some(callflow_moh) = peer_context.call_flow_moh.as_ref() {
            callflow_moh
        } else if let Some(callflow_moh) = context.call_flow_moh.as_ref() {
            callflow_moh
        } else {
            match &context.endpoint {
                Endpoint::User { user, .. } => {
                    user.music_on_hold.as_deref().unwrap_or("")
                }
                _ => "",
            }
        };
        if let Some(moh) = SWITCHBOARD_SERVICE.db.get_music_on_hold(moh).await {
            peer.play_moh(&moh).await?;
        } else {
            let moh = MusicOnHold {
                play_id: nebula_utils::uuid(),
                random: false,
                sounds: vec![],
            };
            peer.play_moh(&moh).await?;
        }
        Ok(())
    }

    async fn unhold(&self) -> Result<()> {
        self.new_event(ChannelEvent::Unhold, "unhold").await?;
        self.set_media_mode(MediaMode::Sendrecv).await?;
        let peer = self
            .get_peer()
            .await
            .ok_or(anyhow!("channel doesn't have peer"))?;
        peer.stop_moh().await?;
        Ok(())
    }

    pub async fn set_api_remote_addr(&self, addr: &str) -> Result<()> {
        REDIS
            .hset(&format!("nebula:channel:{}", &self.id), "api_addr", addr)
            .await?;
        Ok(())
    }

    pub async fn get_api_remote_addr(&self) -> Result<String> {
        REDIS
            .hget(&format!("nebula:channel:{}", &self.id), "api_addr")
            .await
    }

    pub async fn notify_active_speaker(
        &self,
        active: String,
        active_video_id: Option<Uuid>,
        now: u64,
    ) -> Result<()> {
        let addr = REDIS
            .hget(&format!("nebula:channel:{}", &self.id), "api_addr")
            .await
            .unwrap_or_else(|_| "".to_string());
        let context = self.get_call_context().await?;
        let proxy_host = &context.proxy_host;
        if !addr.is_empty() {
            let _ = SWITCHBOARD_SERVICE
                .room_rpc
                .room_active_speaker(proxy_host, active, addr)
                .await;
        }
        if let Ok(video_id) = self.get_video_id().await {
            if let Some(active_video_id) = active_video_id {
                let _ = SWITCHBOARD_SERVICE
                    .media_rpc_client
                    .set_active_speaker(video_id, active_video_id, now)
                    .await;
            }
        }
        Ok(())
    }

    pub async fn notify_clear_active_speaker(&self) -> Result<()> {
        let addr = REDIS
            .hget(&format!("nebula:channel:{}", &self.id), "api_addr")
            .await
            .unwrap_or_else(|_| "".to_string());
        let context = self.get_call_context().await?;
        let proxy_host = &context.proxy_host;
        if !addr.is_empty() {
            SWITCHBOARD_SERVICE
                .room_rpc
                .room_clear_active_speaker(proxy_host, addr)
                .await?;
        }
        Ok(())
    }

    pub async fn leave_room(&self, room: &str, by_adimin: bool) -> Result<()> {
        info!(room = room, channel = self.id, "endpoint leave room");
        let context = self.get_call_context().await?;
        let self_api_addr = context.api_addr.unwrap_or_default();
        if self_api_addr.is_empty() {
            // this is a sip client, so we need to hangup the call
            let _ = self.hangup(None).await;
        } else {
            let _ = SWITCHBOARD_SERVICE
                .switchboard_rpc
                .hangup(
                    &SWITCHBOARD_SERVICE.config.switchboard_host,
                    self.id.clone(),
                    true,
                    None,
                    None,
                    None,
                )
                .await;
        }

        if !REDIS
            .sismember(&format!("nebula:room:{}:members", room), &self.id)
            .await?
        {
            return Ok(());
        }

        let _ = REDIS
            .srem(&format!("nebula:room:{}:members", room), &self.id)
            .await;

        if REDIS
            .scard(&format!("nebula:room:{}:members", room))
            .await
            .unwrap_or(0)
            == 0
        {
            let _ = REDIS.srem(ALL_ROOMS, room).await;
        }

        if by_adimin {
            let proxy_host = context.proxy_host;
            if !self_api_addr.is_empty() {
                let _ = SWITCHBOARD_SERVICE
                    .room_rpc
                    .member_leave_room(
                        &proxy_host,
                        self.id.clone(),
                        self_api_addr,
                        by_adimin,
                    )
                    .await;
            }
        }

        let members: Vec<String> = REDIS
            .smembers(&format!("nebula:room:{}:members", room))
            .await?;
        for m in &members {
            let member_channel = Channel::new(m.to_string());
            if let Ok(member_context) = member_channel.get_call_context().await {
                let proxy_host = &member_context.proxy_host;
                let addr = member_context.api_addr.as_deref().unwrap_or("");
                if !addr.is_empty() {
                    let _ = SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_leave_room(
                            proxy_host,
                            self.id.clone(),
                            addr.to_string(),
                            by_adimin,
                        )
                        .await;
                }
            }
        }

        Ok(())
    }

    async fn create_video_media(
        &self,
        mid: String,
        remote_rtpmaps: &[Rtpmap],
    ) -> Option<MediaDescription> {
        let local_sdp = self.get_local_sdp().await.ok()?;
        let remote_sdp = self.get_remote_sdp().await.ok()?;
        let tracks = remote_sdp.media_stream_tracks();
        let mut local_rtpmaps = local_sdp.get_rtpmaps(&MediaType::Video)?;
        for local_rtpmap in local_rtpmaps.iter_mut() {
            for remote_rtpmap in remote_rtpmaps {
                if remote_rtpmap.name == local_rtpmap.name {
                    local_rtpmap.type_number = remote_rtpmap.type_number;
                    if local_rtpmap.name == PayloadType::Rtx {
                        local_rtpmap.fmtp = format!(
                            "apt={}",
                            local_rtpmap.type_number.saturating_sub(1)
                        );
                    }
                    break;
                }
            }
        }
        for (_, track) in tracks {
            if track.media_type == MediaType::Video {
                let ssrc = {
                    let id = format!("{}:{}", self.id, track.mid);
                    let ssrc = uuid_v5(&id).as_fields().0;
                    ssrc
                };

                let video = MediaDescription::create_video(
                    local_rtpmaps,
                    ssrc,
                    mid,
                    self.id.clone(),
                );
                return Some(video);
            }
        }
        None
    }

    async fn get_video_track(&self) -> Option<MediaStreamTrack> {
        let remote_sdp = self.get_remote_sdp().await.ok()?;
        let tracks = remote_sdp.media_stream_tracks();
        for (_mid, track) in tracks {
            if track.media_type == MediaType::Video {
                return Some(track);
            }
        }
        None
    }

    pub async fn subscribe_room(
        &self,
        req: SubscribeRoomRequest,
    ) -> Result<SubscribeRoomResponse> {
        let room = REDIS
            .hget(&format!("nebula:channel:{}", &self.id), "room")
            .await
            .unwrap_or("".to_string());
        if room == "" {
            return Err(anyhow!("call doens't have room"));
        }

        let video_id = self.get_video_id().await?;
        let current_sources = REDIS
            .smembers::<Vec<String>>(&format!(
                "nebula:media_stream:{}:sources",
                video_id
            ))
            .await
            .unwrap_or(Vec::new());

        let mids_map: HashMap<String, String> = req
            .members
            .iter()
            .map(|m| (m.video_mid.to_string(), m.id.to_string()))
            .collect();

        let new_sources: HashMap<String, SubscribeRoomMember> =
            stream::iter(req.members)
                .filter_map(|member| async move {
                    if member.id == self.id {
                        return None;
                    }
                    let video_id = Channel::new(member.id.to_string())
                        .get_video_id()
                        .await
                        .ok()?;

                    Some((video_id, member))
                })
                .collect()
                .await;

        for s in &current_sources {
            if !new_sources.contains_key(s) {
                let _ = REDIS
                    .srem(&format!("nebula:media_stream:{}:sources", video_id), s)
                    .await;
                let _ = REDIS
                    .srem(
                        &format!("nebula:media_stream:{}:destinations", s),
                        &video_id,
                    )
                    .await;
                let _ = REDIS
                    .hdel(
                        &format!("nebula:media_stream:{}:destination_layer", s),
                        &video_id,
                    )
                    .await;
                SWITCHBOARD_SERVICE
                    .media_rpc_client
                    .update_media_destinations(s.to_string())
                    .await?;
            }
        }

        println!("old {:?}, new {:?}", current_sources, new_sources);

        for (s, member) in &new_sources {
            if !current_sources.contains(s) {
                let _ = REDIS
                    .sadd(&format!("nebula:media_stream:{}:sources", video_id), s)
                    .await;
                let _ = REDIS
                    .sadd(
                        &format!("nebula:media_stream:{}:destinations", s),
                        &video_id,
                    )
                    .await;
                SWITCHBOARD_SERVICE
                    .media_rpc_client
                    .update_media_destinations(s.to_string())
                    .await?;
            }
        }

        let new_source_info: HashMap<String, VideoSourceInfo> = new_sources
            .into_iter()
            .filter_map(|(stream_id, member)| {
                Some((
                    stream_id,
                    VideoSourceInfo {
                        width: member.width?,
                        height: member.height?,
                    },
                ))
            })
            .collect();
        if let Ok(info) = serde_json::to_string(&new_source_info) {
            let _ = REDIS
                .hsetex(
                    &format!("nebula:media_stream:{video_id}"),
                    "source_info",
                    &info,
                )
                .await;
        }

        SWITCHBOARD_SERVICE
            .media_rpc_client
            .update_video_sources(video_id)
            .await?;

        let remote_sdp = SessionDescription::from_str(&req.sdp)?;

        let local_sdp = self.get_local_sdp().await?;
        let dtls = local_sdp.dtls();
        let ice = local_sdp.ice();

        let mut videos = Vec::new();

        for (i, media) in remote_sdp.medias()[2..].iter().enumerate() {
            let mid = media.mid().unwrap_or_else(|| "".to_string());
            let rtpmaps = media.get_rtpmaps();
            let video = async {
                let member = mids_map.get(&mid)?;
                let member_live = REDIS
                    .sismember(&format!("nebula:room:{}:members", &room), &member)
                    .await
                    .unwrap_or(false);
                if !member_live {
                    return None;
                }
                let member_channel = Channel::new(member.clone());
                let mut video = member_channel
                    .create_video_media(mid.clone(), &rtpmaps)
                    .await?;
                video.port = 0;
                video.insert_attribute("bundle-only".to_string(), None);
                if let Some(dtls) = dtls.as_ref() {
                    video.insert_dtls(dtls);
                }
                if let Some(ice) = ice.as_ref() {
                    video.insert_ice(ice);
                }
                video.insert_attribute("sendonly".to_string(), None);
                Some(video)
            }
            .await;

            videos.push(video.unwrap_or_else(|| {
                let mut video = MediaDescription {
                    media_type: MediaType::Video,
                    port: 0,
                    num_ports: 0,
                    proto: "UDP/TLS/RTP/SAVPF".to_string(),
                    payloads: Vec::new(),
                    attributes: Vec::new(),
                };
                for rtpmap in &rtpmaps {
                    if rtpmap.name == PayloadType::VP8 {
                        video.insert_rtpmap(rtpmap);
                        video.insert_rtpmap(&Rtpmap {
                            name: PayloadType::Rtx,
                            rate: 90000,
                            type_number: rtpmap.type_number + 1,
                            fmtp: format!("apt={}", rtpmap.type_number),
                            ..Default::default()
                        });
                    }
                }
                video.insert_mid(mid.clone());
                video.insert_attribute("extmap".to_string(), Some("3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01".to_string()));
                video.insert_attribute("rtcp-mux".to_string(), None);
                let track_id = nebula_utils::uuid();
                let id = format!("{}:{}", self.id, mid);
                let ssrc = uuid_v5(&id).as_fields().0;
                video.insert_msid("-".to_string(), track_id.clone());
                video.insert_ssrc_group(
                    ssrc,
                    ssrc.wrapping_add(1),
                    "-".to_string(),
                    track_id,
                );
                video.insert_attribute("bundle-only".to_string(), None);
                if let Some(dtls) = dtls.as_ref() {
                    video.insert_dtls(dtls);
                }
                if let Some(ice) = ice.as_ref() {
                    video.insert_ice(ice);
                }
                video.insert_attribute("inactive".to_string(), None);
                video
            }));
        }

        let mut medias = Vec::new();
        for media in local_sdp.medias() {
            medias.push(media.clone());
        }

        for media in videos.iter_mut() {
            medias.push(media.clone());
        }

        let mut new_sdp =
            SessionDescription::new(SWITCHBOARD_SERVICE.config.media_ip);
        new_sdp.media_descriptions = medias;
        if let Some(bundle) = remote_sdp.attribute("group") {
            new_sdp.insert_attribute("group".to_string(), Some(bundle));
        }
        // if remote_sdp.has_attribute("extmap-allow-mixed") {
        //     new_sdp.insert_attribute("extmap-allow-mixed".to_string(), None);
        // }
        // if let Some(semantic) = remote_sdp.attribute("msid-semantic") {
        //     new_sdp.insert_attribute("msid-semantic".to_string(), Some(semantic));
        // }

        let resp = SubscribeRoomResponse {
            sdp: new_sdp.to_string(),
        };

        Ok(resp)
    }

    pub async fn mute_audio(&self) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .mute_stream(audio_id)
            .await?;
        Ok(())
    }

    pub async fn unmute_audio(&self) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .unmute_stream(audio_id)
            .await?;
        Ok(())
    }

    pub async fn mute_video(&self) -> Result<()> {
        let video_id = self.get_video_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .mute_stream(video_id)
            .await?;
        Ok(())
    }

    pub async fn unmute_video(&self) -> Result<()> {
        let video_id = self.get_video_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .unmute_stream(video_id)
            .await?;
        Ok(())
    }

    pub async fn sip_join_room(&self, room: &str) -> Result<()> {
        let _ = REDIS
            .sadd(&format!("nebula:room:{room}:members"), &self.id)
            .await;
        let _ = REDIS.sadd(ALL_ROOMS, room).await;

        let context = self.get_call_context().await?;
        let self_member = &RoomMember {
            id: self.id.clone(),
            user: context.endpoint.user().map(|u| u.uuid.clone()),
            name: context.endpoint.name(),
            is_admin: false,
            hand_raised: false,
            audio_muted: false,
            video_muted: false,
            audio_muted_by_admin: false,
            video_muted_by_admin: false,
            is_sip: true,
            screen_share: false,
        };

        let members = REDIS
            .smembers::<Vec<String>>(&format!("nebula:room:{}:members", room))
            .await
            .unwrap_or_default();
        for member in members {
            if member != self.id {
                let member_channel = Channel::new(member);
                let _ = Bridge::bridge_media(self, &member_channel).await;
                let _ = Bridge::bridge_media(&member_channel, self).await;

                let addr = REDIS
                    .hget(
                        &format!("nebula:channel:{}", member_channel.id),
                        "api_addr",
                    )
                    .await
                    .unwrap_or_else(|_| "".to_string());
                let proxy_host = REDIS
                    .hget(
                        &format!("nebula:channel:{}", member_channel.id),
                        "proxy_host",
                    )
                    .await
                    .unwrap_or_else(|_| "".to_string());
                if !addr.is_empty() && !proxy_host.is_empty() {
                    let _ = SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_join_room(
                            &proxy_host,
                            self_member.clone(),
                            addr.to_string(),
                        )
                        .await;
                }
            }
        }

        Ok(())
    }

    pub async fn join_room(
        &self,
        context: &CallContext,
        room: &str,
        is_admin: bool,
        sdp: &str,
        audio_muted: bool,
        video_muted: bool,
    ) -> Result<JoinRoomResponse> {
        let sdp = SessionDescription::from_str(sdp)?;
        self.set_remote_sdp(&sdp, SdpType::Offer).await?;

        if let Ok(bridge) = self.get_bridge().await {
            let _ = bridge.answer(self).await;
        }

        let answer_sdp = self
            .create_answer_sdp(true)
            .await
            .context("failed when trying to create answer sdp")?;
        self.set_answered().await?;

        REDIS
            .sadd(&format!("nebula:room:{}:members", room), &self.id)
            .await?;
        let _ = REDIS.sadd(ALL_ROOMS, room).await;

        let audio_id = &self.get_audio_id().await?;

        let self_member = &RoomMember {
            id: self.id.clone(),
            user: context.endpoint.user().map(|u| u.uuid.clone()),
            name: context.endpoint.name(),
            is_admin,
            hand_raised: false,
            audio_muted,
            video_muted,
            audio_muted_by_admin: false,
            video_muted_by_admin: false,
            is_sip: false,
            screen_share: false,
        };

        let members = REDIS
            .smembers::<Vec<String>>(&format!("nebula:room:{}:members", room))
            .await
            .unwrap_or_default();
        println!("room members {:?}", members);
        let members: Vec<RoomMember> = stream::iter(members)
            .filter_map(|m| async move {
                if m == self.id {
                    return None;
                }
                let member_channel = Channel::new(m.to_string());
                let member_context = member_channel.get_call_context().await.ok()?;
                let name = member_context.endpoint.name();
                let member_audio_id = member_channel.get_audio_id().await.ok()?;

                let _ = REDIS
                    .sadd(
                        &format!(
                            "nebula:media_stream:{}:destinations",
                            member_audio_id
                        ),
                        audio_id,
                    )
                    .await;
                let _ = REDIS
                    .sadd(
                        &format!("nebula:media_stream:{}:destinations", audio_id),
                        &member_audio_id,
                    )
                    .await;
                let _ = REDIS
                    .sadd(
                        &format!("nebula:media_stream:{}:sources", member_audio_id),
                        audio_id,
                    )
                    .await;
                let _ = REDIS
                    .sadd(
                        &format!("nebula:media_stream:{}:sources", audio_id),
                        &member_audio_id,
                    )
                    .await;
                let _ = SWITCHBOARD_SERVICE
                    .media_rpc_client
                    .update_media_destinations(member_audio_id)
                    .await;

                let proxy_host = &member_context.proxy_host;
                let addr = member_context.api_addr.as_deref().unwrap_or("");
                if addr.is_empty() || proxy_host.is_empty() {
                    warn!("member room message lost: join_room room {room} api_addr {addr} proxy_host {proxy_host}");
                }
                if !addr.is_empty() {
                    let _ = SWITCHBOARD_SERVICE
                        .room_rpc
                        .member_join_room(
                            proxy_host,
                            self_member.clone(),
                            addr.to_string(),
                        )
                        .await;
                }

                let mut member = RoomMember {
                    id: m.to_string(),
                    user: member_context.endpoint.user().map(|u| u.uuid.clone()),
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
            .await;

        let _ = SWITCHBOARD_SERVICE
            .media_rpc_client
            .update_media_destinations(audio_id.to_string())
            .await;

        Ok(JoinRoomResponse {
            call_id: self.id.clone(),
            sdp: answer_sdp.to_string(),
            members,
            is_admin,
        })
    }

    pub async fn convert_to_room(
        &self,
    ) -> Result<(User, CallContext, String, String)> {
        let context = self
            .get_call_context()
            .await
            .map_err(|e| anyhow!("can't get call context: {}", e))?;
        let user = context
            .endpoint
            .user()
            .ok_or_else(|| anyhow!("only user can convert a call to a room"))?;
        let bridge = self
            .get_bridge()
            .await
            .map_err(|e| anyhow!("can't get bridge: {}", e))?;
        if !bridge.answered().await {
            return Err(anyhow!("the call isn't answered"));
        }

        let inbound_channel = bridge
            .get_inbound()
            .await
            .map_err(|e| anyhow!("bridge can't get inbound channel: {}", e))?;
        let answer_channel = bridge
            .answer_channel()
            .await
            .map_err(|e| anyhow!("bridge can't get answered channel: {}", e))?;

        let _ = REDIS
            .hdel(&format!("nebula:channel:{answer_channel}"), "bridge_id")
            .await;
        let _ = REDIS
            .srem(
                &format!("nebula:bridge:{}:channels", &bridge.id),
                &answer_channel,
            )
            .await;
        let _ = REDIS
            .srem(
                &format!("nebula:bridge:{}:channels", &bridge.id),
                &inbound_channel.id,
            )
            .await;

        let (self_channel, other_channel) = if self.id == inbound_channel.id {
            (inbound_channel.id, answer_channel)
        } else {
            (answer_channel, inbound_channel.id)
        };

        Ok((user.clone(), context, self_channel, other_channel))
    }

    pub async fn handle_cancel(&self) -> Result<()> {
        println!("handle cancel");
        if self.is_answered().await {
            return Err(anyhow!("can't cancel a channel that has been answered"));
        }
        self.handle_hangup(true, None, None).await
    }

    pub async fn pickup_user(&self, from_user: &User, to_user: &User) -> Result<()> {
        if !from_user.pickup.unwrap_or(false)
            || !to_user.be_picked_up.unwrap_or(false)
        {
            let from_groups = SWITCHBOARD_SERVICE
                .db
                .get_user_groups(&from_user.uuid)
                .await
                .unwrap_or_default();
            let to_groups = SWITCHBOARD_SERVICE
                .db
                .get_user_groups(&to_user.uuid)
                .await
                .unwrap_or_default();
            if !SWITCHBOARD_SERVICE
                .db
                .check_user_permission(
                    &from_user.tenant_id,
                    &from_user.uuid,
                    from_groups,
                    &to_user.uuid,
                    to_groups,
                    "pickup",
                )
                .await
            {
                return Err(anyhow!("user isn't allowed to pick up"));
            }
        }

        for channel in to_user.channels().await.unwrap_or_default() {
            let channel = Channel::new(channel);
            if let Ok(bridge) = channel.get_bridge().await {
                let inbound = bridge
                    .get_inbound()
                    .await
                    .unwrap_or_else(|_| Channel::new("".to_string()));
                if !inbound.id.is_empty()
                    && inbound.id != channel.id
                    && !bridge.is_hangup().await.unwrap_or(true)
                    && !bridge.answered().await
                {
                    bridge.add_channel(&self.id).await?;

                    if let Ok(inbound_context) = inbound.get_call_context().await {
                        let mut context = self.get_call_context().await?;
                        context.answer_caller_id = Some(
                            internal_caller_id(
                                &inbound_context,
                                from_user.cc.clone(),
                                None,
                            )
                            .await,
                        );
                        self.set_call_context(&context).await?;
                    }

                    self.answer().await?;
                    bridge.answer(self).await?;
                    self.receive_event("0", ChannelEvent::Hangup).await?;
                    return Ok(());
                }
            }
        }

        Err(anyhow!("user doens't have any call"))
    }

    pub async fn toggle_queue_login(
        &self,
        from_user: &User,
        to_user: &User,
        check_for_self: bool,
    ) -> Result<()> {
        if check_for_self || from_user.uuid != to_user.uuid {
            let from_groups = SWITCHBOARD_SERVICE
                .db
                .get_user_groups(&from_user.uuid)
                .await
                .unwrap_or_default();
            let to_groups = SWITCHBOARD_SERVICE
                .db
                .get_user_groups(&to_user.uuid)
                .await
                .unwrap_or_default();
            if !SWITCHBOARD_SERVICE
                .db
                .check_user_permission(
                    &from_user.tenant_id,
                    &from_user.uuid,
                    from_groups,
                    &to_user.uuid,
                    to_groups,
                    "queue_login",
                )
                .await
            {
                self.play_sound(
                    None,
                    vec![format!(
                        "tts://You are not allowed to login to queues for extension {}",
                        &to_user.extension.unwrap_or(0)
                    )],
                    true,
                )
                .await?;
                return Err(anyhow!("user isn't allowed to login queues"));
            }
        }
        let is_login = to_user.is_queue_login().await;
        SWITCHBOARD_SERVICE
            .db
            .insert_sipuser_queue_login(
                to_user.uuid.clone(),
                Utc::now(),
                format!("user:{}", from_user.uuid),
                None,
                !is_login,
            )
            .await?;

        // now clear cache
        let _ = reqwest::Client::new()
            .put(format!(
                "http://{}/clear_queue_login_log_cache/{}/None",
                SWITCHBOARD_SERVICE.config.proxy_host, to_user.uuid
            ))
            .send()
            .await;

        // Reset Aux code if exists
        if to_user
            .current_aux_code()
            .await
            .map(|code| !code.code.to_lowercase().eq("available"))
            .is_some()
        {
            let _ = SWITCHBOARD_SERVICE
                .db
                .insert_sipuser_aux_log(
                    to_user.uuid.clone(),
                    Utc::now(),
                    "Available".to_string(),
                    "Queue Login".to_string(),
                )
                .await;
            let _ = reqwest::Client::new()
                .put(format!(
                    "http://{}/clear_aux_log_cache/{}",
                    SWITCHBOARD_SERVICE.config.proxy_host, to_user.uuid
                ))
                .send()
                .await;
        }

        let _ = notify_dialog(
            SWITCHBOARD_SERVICE
                .config
                .all_proxies()
                .map(|p| p.to_vec())
                .unwrap_or_else(|| {
                    vec![SWITCHBOARD_SERVICE.config.proxy_host.clone()]
                }),
            to_user.uuid.clone(),
        )
        .await;
        let _ = notify_dialog(
            SWITCHBOARD_SERVICE
                .config
                .all_proxies()
                .map(|p| p.to_vec())
                .unwrap_or_else(|| {
                    vec![SWITCHBOARD_SERVICE.config.proxy_host.clone()]
                }),
            format!("{}:queue_login_logout", to_user.uuid),
        )
        .await;

        self.answer().await?;
        let tts_text = format!(
            "tts://{} is now logged {} to queues",
            if from_user.uuid != to_user.uuid {
                "Extension".to_string() + &to_user.extension.unwrap_or(0).to_string()
            } else {
                "Your sip user".to_string()
            },
            if !is_login { "in" } else { "out" }
        );
        self.play_sound(None, vec![tts_text], true).await?;

        Ok(())
    }

    pub async fn delete_channel_user(&self) -> Result<()> {
        let context = self.get_call_context().await?;
        if let Endpoint::User { user, .. } = &context.endpoint {
            let _ = REDIS
                .srem(&format!("nebula:sipuser:{}:channels", user.uuid), &self.id)
                .await;
            notify_user_presence(&user.uuid);
        }
        Ok(())
    }

    async fn by_redirect(&self) -> Result<bool> {
        let bridge = self.get_bridge().await?;
        let inbound = bridge.get_inbound().await?;
        let context = inbound.get_call_context().await?;
        Ok(context.redirect_peer.is_some())
    }

    pub async fn handle_hangup(
        &self,
        from_proxy: bool,
        code: Option<usize>,
        redirect_exten: Option<String>,
    ) -> Result<()> {
        let mutex = DistributedMutex::new(format!(
            "nebula:channel:{}:hangup:lock",
            &self.id
        ));
        mutex.lock().await;

        if self.is_hangup().await {
            return Ok(());
        }

        info!(
            channel = self.id,
            "now handle channel hangup {} {} {:?} {:?}",
            self.id,
            from_proxy,
            code,
            redirect_exten
        );

        let local_channel = self.clone();
        tokio::spawn(async move {
            let _ = local_channel.complete_yaypi_call().await;
            let _ = local_channel
                .complete_cdr(ChannelHangupDirection::Client)
                .await;
        });

        {
            let channel = self.clone();
            tokio::spawn(async move {
                let _ = channel.add_cdr_event(ChannelHangupDirection::Client).await;
            });
        }

        let context = self.get_call_context().await.ok();
        if let Some(context) = &context {
            if context.refer.is_some() && from_proxy {
                return Ok(());
            }

            if context.callback.is_some() && from_proxy {
                return Ok(());
            }

            if let Some(transfer) = &context.transfer {
                if !from_proxy {
                    let _ = SWITCHBOARD_SERVICE
                        .proxy_rpc
                        .hangup(
                            &context.proxy_host,
                            self.id.clone(),
                            None,
                            None,
                            None,
                            self.is_answered().await,
                        )
                        .await;
                } else {
                    let peer = match &transfer.peer {
                        TransferPeer::Initiator { to_channel, .. } => to_channel,
                        TransferPeer::Recipient { from_channel, .. } => from_channel,
                    };
                    let _ = SWITCHBOARD_SERVICE
                        .switchboard_rpc
                        .hangup(
                            &SWITCHBOARD_SERVICE.config.switchboard_host,
                            peer.to_string(),
                            false,
                            None,
                            None,
                            None,
                        )
                        .await;
                }
            }
        }

        println!("now channel handle hangup {}", self.id);

        self.new_event(ChannelEvent::Hangup, "hangup").await?;
        self.set_hangup(ChannelHangupDirection::Client).await?;

        if let Ok(bridge) = self.get_bridge().await {
            let mut had_backup = false;
            if let Some(code) = code {
                if code == 302 {
                    if let Some(redirect_exten) = redirect_exten {
                        if !self.by_redirect().await.unwrap_or(false) {
                            if let Ok(context) = self.get_call_context().await {
                                if let Some(_user) = context.endpoint.user() {
                                    let new_channel =
                                        Channel::new(nebula_utils::uuid());
                                    let new_context = CallContext {
                                        endpoint: context.endpoint.clone(),
                                        proxy_host: context.proxy_host.clone(),
                                        api_addr: None,
                                        inter_tenant: None,
                                        location: None,
                                        to: redirect_exten.clone(),
                                        to_internal_number: None,
                                        direction: CallDirection::Initiator,
                                        call_confirmation: None,
                                        set_caller_id: None,
                                        caller_id: None,
                                        answer_caller_id: None,
                                        call_queue_id: None,
                                        call_queue_name: None,
                                        call_flow_id: None,
                                        call_flow_name: None,
                                        call_flow_show_original_callerid: None,
                                        call_flow_custom_callerid: None,
                                        call_flow_moh: None,
                                        parent_call_flow_name: None,
                                        parent_call_flow_show_original_callerid:
                                            None,
                                        parent_call_flow_moh: None,
                                        parent_call_flow_custom_callerid: None,
                                        intra_peer: None,
                                        intra_chain: Vec::new(),
                                        redirect_peer: None,
                                        reply_to: None,
                                        refer: None,
                                        transfer: None,
                                        cdr: None,
                                        started_at: Utc::now(),
                                        answred_at: None,
                                        forward: false,
                                        backup: None,
                                        backup_invite: None,
                                        recording: None,
                                        click_to_dial: None,
                                        callflow: None,
                                        callback: None,
                                        callback_peer: None,
                                        room: "".to_string(),
                                        remove_call_restrictions: false,
                                        ziron_tag: None,
                                        diversion: None,
                                        pai: None,
                                        remote_ip: None,
                                    };
                                    new_channel
                                        .set_call_context(&new_context)
                                        .await?;
                                    REDIS
                                        .hset(
                                            &format!(
                                                "nebula:channel:{}",
                                                new_channel.id
                                            ),
                                            "bridge_id",
                                            &bridge.id,
                                        )
                                        .await?;
                                    REDIS
                                        .sadd(
                                            &format!(
                                                "nebula:bridge:{}:channels",
                                                &bridge.id
                                            ),
                                            &new_channel.id,
                                        )
                                        .await?;

                                    let reply_id = bridge.get_inbound().await?.id;
                                    let mut rtpmaps =
                                        self.outbound_audio_rtpmaps(true).await;
                                    rtpmaps.push(
                                        PayloadType::TelephoneEvent.get_rtpmap(),
                                    );
                                    tokio::spawn(async move {
                                        let _ = new_channel
                                            .new_redirect(
                                                redirect_exten,
                                                reply_id,
                                                rtpmaps,
                                            )
                                            .await;
                                    });

                                    had_backup = true;
                                }
                            }
                        }
                    }
                } else if code != 404 && code != 486 {
                    if let Ok(contex) = self.get_call_context().await {
                        if !bridge.answered().await
                            && !bridge.is_hangup().await.unwrap_or(false)
                        {
                            if let Some(invite) = contex.backup_invite {
                                let id = nebula_utils::uuid();
                                REDIS
                                    .sadd(
                                        &format!(
                                            "nebula:bridge:{}:channels",
                                            &invite.bridge_id
                                        ),
                                        &id,
                                    )
                                    .await?;
                                let _ = Channel::new_outbound(
                                    id,
                                    invite.caller_id,
                                    invite.endpoint,
                                    invite.location,
                                    None,
                                    invite.inbound_channel,
                                    invite.bridge_id,
                                    invite.internal_call,
                                    invite.rtpmaps,
                                    None,
                                    invite.call_confirmation,
                                    None,
                                    None,
                                    None,
                                    Vec::new(),
                                    "".to_string(),
                                    false,
                                    true,
                                )
                                .await;
                                had_backup = true;
                            }
                        }
                    }
                }
            }
            let _ = bridge.channel_hangup(&self.id).await;
            if had_backup {
                let _ = REDIS
                    .srem(
                        &format!("nebula:bridge:{}:hangup_channels", &bridge.id),
                        &self.id,
                    )
                    .await;
            }
        }

        self.hangup_cleanup().await?;

        if let Some(context) = &context {
            if !context.room.is_empty() && context.answred_at.is_some() {
                let channel = self.clone();
                let room = context.room.clone();
                tokio::spawn(async move {
                    let _ = channel.leave_room(&room, false).await;
                });
            }
        }

        Ok(())
    }

    async fn check_endpoint_switchboard_host(&self) -> Result<()> {
        if let Some(proxies) = SWITCHBOARD_SERVICE.config.all_proxies() {
            let context = self.get_call_context().await?;
            if context.direction != CallDirection::Initiator {
                return Ok(());
            }

            if let Some(tenant_id) = context.endpoint.tenant_id() {
                let key = &format!("nebula:channels_for_tenant:{tenant_id}");
                let _ = REDIS.srem(key, &self.id).await;
                if let Ok(channels) = REDIS.smembers::<Vec<String>>(key).await {
                    for channel in channels {
                        if is_channel_hangup(&channel).await {
                            let _ = REDIS.srem(key, &channel).await;
                        }
                    }
                }
                let n = REDIS.scard(key).await?;
                if n > 0 {
                    return Ok(());
                }

                if REDIS
                    .exists(&format!("nebula:queue_for_tenant:{tenant_id}"))
                    .await
                    .unwrap_or(false)
                {
                    return Ok(());
                }

                if REDIS
                    .exists(&format!("nebula:inbound_limit_for_tenant:{tenant_id}"))
                    .await
                    .unwrap_or(false)
                {
                    return Ok(());
                }

                for host in proxies {
                    let host = host.to_string();
                    let tenant_id = tenant_id.to_string();
                    tokio::spawn(async move {
                        let url =
                            format!("http://{host}/clear_tenant_switchboard_host_if_match/{tenant_id}/{}", SWITCHBOARD_SERVICE.config.switchboard_host);
                        let _ = reqwest::Client::new().put(url).send().await;
                    });
                }
            }
        }
        Ok(())
    }

    async fn hangup_cleanup(&self) -> Result<()> {
        let _ = self.delete_channel_user().await;
        let _ = self.stop_media(true).await;
        let _ = REDIS
            .expire(&format!("nebula:channel:{}", &self.id), 60)
            .await;
        let _ = REDIS
            .expire(&format!("nebula:channel:{}:stream", &self.id), 60)
            .await;
        {
            let channel = self.clone();
            tokio::spawn(async move {
                if let Ok(context) = channel.get_call_context().await {
                    let host = &context.proxy_host;
                    let url = format!(
                        "http://{host}/channel_hangup_cleanup/{}",
                        channel.id
                    );
                    let _ = reqwest::Client::new().put(url).send().await;

                    if let Some(callback_peer) = context.callback_peer {
                        let _ = SWITCHBOARD_SERVICE
                            .switchboard_rpc
                            .hangup(
                                &SWITCHBOARD_SERVICE.config.switchboard_host,
                                callback_peer,
                                false,
                                None,
                                None,
                                None,
                            )
                            .await;
                    }
                }
            });
        }

        let _ = REDIS.srem(ALL_CHANNELS, &self.id).await;
        {
            let channel = self.clone();
            tokio::spawn(async move {
                let _ = channel.check_endpoint_switchboard_host().await;
            });
        }
        Ok(())
    }

    pub async fn set_call_context(&self, context: &CallContext) -> Result<()> {
        REDIS
            .hset(
                &format!("nebula:channel:{}", &self.id),
                "call_context",
                &serde_json::to_string(context)?,
            )
            .await?;
        Ok(())
    }

    pub async fn get_in_channel_shortcodes(&self) -> Option<Vec<ShortCode>> {
        let context = self.get_call_context().await.ok()?;
        match context.endpoint {
            Endpoint::User { user, .. } => {
                SWITCHBOARD_SERVICE
                    .db
                    .get_in_channel_shortcodes(&user.tenant_id)
                    .await
            }
            _ => None,
        }
    }

    pub async fn get_cdr(&self) -> Option<CdrItem> {
        let context = self.get_call_context().await.ok()?;
        let cdr_uuid = context.cdr.as_ref()?;
        SWITCHBOARD_SERVICE.db.get_cdr(*cdr_uuid).await
    }

    pub async fn update_cdr(&self, change: CdrItemChange) -> Result<()> {
        let context = self.get_call_context().await?;
        if let Some(cdr_uuid) = context.cdr {
            tokio::spawn(async move {
                let cdr = SWITCHBOARD_SERVICE
                    .db
                    .update_cdr(cdr_uuid, change.clone())
                    .await?;
                if change.to_type.is_some() {
                    let _ = SWITCHBOARD_SERVICE
                        .yaypi_client
                        .create_cdr(&cdr.tenant_id, &cdr.uuid.to_string())
                        .await;
                }
                if change.answer_type.is_some() {
                    let _ = SWITCHBOARD_SERVICE
                        .yaypi_client
                        .answer_cdr(&cdr.tenant_id, &cdr.uuid.to_string())
                        .await;
                }
                if change.end.is_some() {
                    let _ = SWITCHBOARD_SERVICE
                        .yaypi_client
                        .complete_cdr(&cdr.tenant_id, &cdr.uuid.to_string())
                        .await;
                }
                Ok::<(), anyhow::Error>(())
            });
        }
        Ok(())
    }

    pub async fn get_call_context(&self) -> Result<CallContext> {
        let s: String = REDIS
            .hget(&format!("nebula:channel:{}", &self.id), "call_context")
            .await?;
        serde_json::from_str(&s).map_err(|e| anyhow!(e))
    }

    pub async fn get_peer(&self) -> Option<Channel> {
        let bridge = self.get_bridge().await.ok()?;
        let answer_channel = bridge.answer_channel().await.ok()?;
        let bleg = if answer_channel == "" {
            let channels = bridge.channels().await.ok()?;
            if channels.len() == 1 {
                channels[0].id.clone()
            } else {
                return None;
            }
        } else {
            answer_channel
        };

        let inbound = bridge.get_inbound().await.ok()?;
        if self.id == inbound.id {
            return Some(Channel::new(bleg));
        }
        if self.id == bleg {
            return Some(inbound);
        }
        None
    }

    pub async fn get_bridge(&self) -> Result<Bridge> {
        let bridge_id: String = REDIS.hget(&self.redis_key(), "bridge_id").await?;
        Ok(Bridge::new(bridge_id))
    }

    async fn outbound_audio_rtpmaps(&self, internal_sdp: bool) -> Vec<Rtpmap> {
        let remote_rtpmaps = self.remote_audio_rtpmaps().await.unwrap_or(Vec::new());
        let negotiated = self.negotiate_audio_rtpmap().await;
        let order = if internal_sdp {
            INTERNAL_AUDIO_ORDER
        } else {
            EXTERNAL_AUDIO_ORDER
        };
        let order = if let Some(rtpmap) = negotiated {
            let mut types = Vec::new();
            types.push(rtpmap.name.clone());

            let other_types = order
                .iter()
                .filter_map(|t| {
                    if t != &rtpmap.name {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<PayloadType>>();
            types.extend_from_slice(&other_types);
            types
        } else {
            let mut types = order
                .iter()
                .filter_map(|t| {
                    if remote_rtpmaps.iter().any(|r| &r.name == t) {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<PayloadType>>();
            let unsupported_types = order
                .iter()
                .filter_map(|t| {
                    if !remote_rtpmaps.iter().any(|r| &r.name == t) {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<PayloadType>>();
            types.extend_from_slice(&unsupported_types);
            types
        };
        order.iter().map(|rtp_type| rtp_type.get_rtpmap()).collect()
    }

    pub async fn bridge_teams_refer(
        &self,
        cx: &TeamsReferCx,
        timeout: u64,
    ) -> Result<Bridge> {
        if self.is_hangup().await {
            return Err(anyhow!("channel has hangup"));
        }
        let original_context = Channel::new(cx.channel_id.clone())
            .get_call_context()
            .await?;
        SWITCHBOARD_SERVICE
            .proxy_rpc
            .notify_refer_trying(&original_context.proxy_host, cx.channel_id.clone())
            .await?;

        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or_else(|| anyhow!("not from a user"))?;
        let bridge_id = nebula_utils::uuid();

        let outbound_id = nebula_utils::uuid();
        {
            let inbound_channel = self.id.clone();
            let bridge_id = bridge_id.clone();
            let internal_call = context.endpoint.is_internal_call();
            let host = "sip.pstnhub.microsoft.com";
            let req_uri = cx.refer_to.uri.clone();
            let dst_uri = Uri {
                scheme: "sip".to_string(),
                host: host.to_string(),
                transport: TransportType::Tls,
                ..Default::default()
            };
            let location = Location {
                user: cx.refer_to.uri.user.clone().unwrap_or_default(),
                proxy_host: SWITCHBOARD_SERVICE
                    .config
                    .london_proxy_host()
                    .to_string(),
                display_name: None,
                from_host: from_user
                    .msteam_domain
                    .clone()
                    .unwrap_or_else(|| "talk.yay.com".to_string()),
                to_host: from_user
                    .msteam_domain
                    .clone()
                    .unwrap_or_else(|| "talk.yay.com".to_string()),
                req_uri,
                dst_uri,
                intra: false,
                inter_tenant: None,
                encryption: true,
                route: None,
                device: None,
                device_token: None,
                dnd: false,
                dnd_allow_internal: false,
                provider: None,
                user_agent: None,
                is_registration: None,
                tenant_id: Some(from_user.tenant_id.clone()),
                teams_refer_cx: Some(cx.clone()),
                ziron_tag: None,
                diversion: None,
                pai: None,
                global_dnd: false,
                global_forward: None,
                room_auth_token: None,
            };

            let location = location.clone();
            let endpoint = context.endpoint.clone();
            let caller_id = CallerId {
                user: from_user
                    .msteam
                    .clone()
                    .unwrap_or_else(|| from_user.extension.unwrap_or(0).to_string()),
                e164: None,
                display_name: "".to_string(),
                anonymous: false,
                asserted_identity: None,
                original_number: None,
                to_number: None,
            };

            let internal_audio_rtpmaps = self.outbound_audio_rtpmaps(true).await;
            let external_audio_rtpmaps = self.outbound_audio_rtpmaps(false).await;
            let mut rtpmaps = if location.intra || endpoint.is_internal_sdp() {
                internal_audio_rtpmaps.clone()
            } else {
                external_audio_rtpmaps
            };
            rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());

            REDIS
                .sadd(
                    &format!("nebula:bridge:{}:channels", &bridge_id),
                    &outbound_id,
                )
                .await?;
            let outbound_id = outbound_id.clone();
            tokio::spawn(async move {
                let _ = Channel::new_outbound(
                    outbound_id,
                    caller_id,
                    endpoint,
                    location,
                    None,
                    inbound_channel,
                    bridge_id,
                    internal_call,
                    rtpmaps,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Vec::new(),
                    "".to_string(),
                    false,
                    false,
                )
                .await;
            });
        }

        REDIS
            .hset(&self.redis_key(), "bridge_id", &bridge_id)
            .await?;
        let bridge = Bridge::new(bridge_id.clone());
        bridge.set_inbound(&self.id).await?;

        let local_bridge = bridge.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now()
                + tokio::time::Duration::from_secs(timeout);
            let _ = local_bridge.handle_timeout(deadline, false).await;
        });

        let outbound_channel = Channel::new(outbound_id);
        let mut success = false;
        tokio::select! {
            _ = outbound_channel.receive_event("0", ChannelEvent::Ring) => {
                success = true;
        }
            _ = outbound_channel.receive_event("0", ChannelEvent::Answer) => {
                success = true;
            }
            _ = outbound_channel.receive_event("0", ChannelEvent::Hangup) => {
            }
        }

        if success {
            let channel_id = cx.channel_id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = SWITCHBOARD_SERVICE
                    .proxy_rpc
                    .notify_refer_success(&original_context.proxy_host, channel_id)
                    .await;
            });
        }

        Ok(bridge)
    }

    pub async fn bridge_transfer_location(
        &self,
        endpoint: &Endpoint,
        location: &Location,
        auto_answer: bool,
        timeout: u64,
        destination: &str,
    ) -> Result<Bridge> {
        if self.is_hangup().await {
            return Err(anyhow!("channel has hangup"));
        }
        let context = self.get_call_context().await?;
        let bridge_id = nebula_utils::uuid();

        {
            let inbound_channel = self.id.clone();
            let bridge_id = bridge_id.clone();
            let internal_call = context.endpoint.is_internal_call();
            let location = location.clone();
            let endpoint = endpoint.clone();
            let caller_id = if destination.is_empty() {
                internal_caller_id(&context, None, None).await
            } else {
                CallerId {
                    user: destination.to_string(),
                    e164: None,
                    display_name: "Smart Dialler Call".to_string(),
                    anonymous: false,
                    asserted_identity: None,
                    original_number: None,
                    to_number: None,
                }
            };

            let internal_audio_rtpmaps = self.outbound_audio_rtpmaps(true).await;
            let external_audio_rtpmaps = self.outbound_audio_rtpmaps(false).await;
            let mut rtpmaps = if location.intra || endpoint.is_internal_sdp() {
                internal_audio_rtpmaps.clone()
            } else {
                external_audio_rtpmaps
            };
            rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());
            if location.device.is_none()
                && location.dst_uri.transport != TransportType::Wss
                && location.dst_uri.transport != TransportType::Ws
            {
                let rtpmap = Rtpmap {
                    name: PayloadType::TelephoneEvent,
                    rate: 48000,
                    type_number: 102,
                    fmtp: "0-15".to_string(),
                    ..Default::default()
                };
                rtpmaps.push(rtpmap);
            }

            let id = nebula_utils::uuid();
            REDIS
                .sadd(&format!("nebula:bridge:{}:channels", &bridge_id), &id)
                .await?;
            tokio::spawn(async move {
                let _ = Channel::new_outbound(
                    id,
                    caller_id,
                    endpoint,
                    location,
                    None,
                    inbound_channel,
                    bridge_id,
                    internal_call,
                    rtpmaps,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Vec::new(),
                    "".to_string(),
                    auto_answer,
                    true,
                )
                .await;
            });
        }

        REDIS
            .hset(&self.redis_key(), "bridge_id", &bridge_id)
            .await?;
        let bridge = Bridge::new(bridge_id.clone());
        bridge.set_inbound(&self.id).await?;

        let local_bridge = bridge.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now()
                + tokio::time::Duration::from_secs(timeout);
            let _ = local_bridge.handle_timeout(deadline, false).await;
        });

        Ok(bridge)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn bridge_extensions(
        &self,
        tenant_id: &str,
        extensions: Vec<&BridgeExtension>,
        ringtone: &str,
        announcement: &str,
        timeout: u64,
        override_caller_id: Option<CallerId>,
        include_login_as: bool,
        send_completed_elsewhere_for_timeout: bool,
    ) -> Result<Bridge> {
        if self.is_hangup().await {
            return Err(anyhow!("channel has hangup"));
        }
        let context = self.get_call_context().await?;
        let bridge_id = nebula_utils::uuid();
        let mut outbound_channels = 0;

        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout);

        let remote_sdp = self.get_remote_sdp().await?;
        let internal_call = context.endpoint.is_internal_call();
        let internal_audio_rtpmaps = self.outbound_audio_rtpmaps(true).await;
        let external_audio_rtpmaps = self.outbound_audio_rtpmaps(false).await;
        let mut endpoints_has_user = false;
        let mut user_ring_notifications = Vec::new();
        let mut last_caller_id: Option<CallerId> = None;
        for extension in &extensions {
            if let Some(locations) = extension
                .locations(internal_call, context.to.clone(), include_login_as)
                .await
            {
                let confirmation = extension.call_confirmation();
                for (
                    endpoint,
                    location,
                    backup_location,
                    force_caller_id,
                    backup_force_caller_id,
                ) in locations
                {
                    let caller_id = match force_caller_id
                        .or(override_caller_id.clone())
                    {
                        Some(caller_id) => caller_id,
                        None => {
                            let redirect_context = redirect_context(&context).await;
                            let callerid_context =
                                redirect_context.as_ref().unwrap_or(&context);
                            match &endpoint {
                                Endpoint::User { user, group, .. } => {
                                    internal_caller_id(
                                        callerid_context,
                                        user.cc.clone(),
                                        group.clone(),
                                    )
                                    .await
                                }
                                Endpoint::Trunk { trunk, .. } => {
                                    let mut caller_id =
                                        external_caller_id(callerid_context).await;
                                    if trunk.show_pai == Some(true) {
                                        let pai = if let Some(number) =
                                            caller_id.original_number.clone()
                                        {
                                            number
                                        } else {
                                            caller_id.user.clone()
                                        };
                                        caller_id.asserted_identity = Some(pai);
                                    }
                                    if let Some(cc) = trunk.caller_id.as_ref() {
                                        if let Some(cc) = cc.strip_prefix('%') {
                                            let country =
                                                phonenumber::country::Id::from_str(
                                                    &cc.to_uppercase(),
                                                );
                                            if let Ok(country) = country {
                                                caller_id.user = get_phone_number(
                                                    &caller_id.user,
                                                )
                                                .map(|n| {
                                                    let cc = n.country().id();
                                                    if cc == Some(country) {
                                                        format_national_number(&n)
                                                    } else {
                                                        n.format()
                                                            .mode(phonenumber::Mode::E164)
                                                            .to_string()
                                                    }
                                                })
                                                .unwrap_or_else(|| {
                                                    caller_id.user.clone()
                                                });
                                            }
                                        }
                                    }
                                    caller_id
                                }
                                Endpoint::Provider { .. } => {
                                    external_caller_id(callerid_context).await
                                }
                            }
                        }
                    };
                    if endpoint.user().is_some() {
                        endpoints_has_user = true;
                    }
                    last_caller_id = Some(caller_id.clone());
                    let inbound_channel = self.id.clone();
                    let bridge_id = bridge_id.clone();
                    let caller_id = caller_id.clone();
                    let backup_caller_id =
                        backup_force_caller_id.unwrap_or_else(|| caller_id.clone());
                    let mut rtpmaps = if location.intra || endpoint.is_internal_sdp()
                    {
                        internal_audio_rtpmaps.clone()
                    } else {
                        external_audio_rtpmaps.clone()
                    };

                    let video_rtpmaps = if let Some(to_user) = endpoint.user() {
                        if let Some(from_user) = context.endpoint.user() {
                            if from_user.video == Some(true)
                                && to_user.video == Some(true)
                                && remote_sdp.video().is_some()
                            {
                                Some(vec![PayloadType::H264.get_rtpmap()])
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    if let Some(trunk) = endpoint.trunk() {
                        if let Some(codecs) =
                            trunk.codecs.as_ref().and_then(|codecs| {
                                serde_json::from_str::<Vec<PayloadType>>(codecs).ok()
                            })
                        {
                            let codecs: Vec<Rtpmap> = codecs
                                .into_iter()
                                .map(|codec| codec.get_rtpmap())
                                .collect();
                            if !codecs.is_empty() {
                                rtpmaps = codecs;
                            }
                        }
                    }

                    rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());
                    if internal_call
                        && location.device.is_none()
                        && location.dst_uri.transport != TransportType::Wss
                        && location.dst_uri.transport != TransportType::Ws
                    {
                        let rtpmap = Rtpmap {
                            name: PayloadType::TelephoneEvent,
                            rate: 48000,
                            type_number: 102,
                            fmtp: "0-15".to_string(),
                            ..Default::default()
                        };
                        rtpmaps.push(rtpmap);
                    }

                    outbound_channels += 1;
                    let id = nebula_utils::uuid();
                    REDIS
                        .sadd(&format!("nebula:bridge:{}:channels", &bridge_id), &id)
                        .await?;
                    let confirmation = confirmation.clone();
                    let forward_info = match &context.endpoint {
                        Endpoint::Provider { .. } => match &endpoint {
                            Endpoint::Provider {
                                number: to_number, ..
                            } => Some(ForwardInfo {
                                from_number: get_phone_number(&context.to)
                                    .map(|n| {
                                        n.format()
                                            .mode(phonenumber::Mode::E164)
                                            .to_string()
                                    })
                                    .unwrap_or(context.to.clone()),
                                to_number: to_number.to_string(),
                                tenant_id: tenant_id.to_string(),
                                callflow_id: context
                                    .call_flow_id
                                    .clone()
                                    .unwrap_or("".to_string()),
                                callflow_name: context
                                    .call_flow_name
                                    .clone()
                                    .unwrap_or("".to_string()),
                            }),
                            _ => None,
                        },
                        _ => None,
                    };
                    let intra_chain = context.intra_chain.clone();

                    if let Some(cdr_id) = context.cdr.as_ref() {
                        let first_time = REDIS
                            .hsetnx(
                                &format!("nebula:channel:{}", &self.id),
                                &format!("user_rang:{}", endpoint.id()),
                                "1",
                            )
                            .await
                            .unwrap_or(false);
                        user_ring_notifications.push(serde_json::json!({
                            "to": endpoint.id(),
                            "agent": location.user_agent,
                            "first_time": first_time,
                            "from": caller_id.user,
                        }));
                    }

                    info!(
                        channel = inbound_channel,
                        outbound_channel = id,
                        bridge = bridge_id,
                        "now bridge extension {} {location:?}",
                        endpoint.id()
                    );
                    tokio::spawn(async move {
                        let _ = Channel::new_outbound(
                            id,
                            caller_id,
                            endpoint,
                            location,
                            backup_location.map(|l| (l, backup_caller_id)),
                            inbound_channel,
                            bridge_id,
                            internal_call,
                            rtpmaps,
                            video_rtpmaps,
                            confirmation,
                            None,
                            forward_info,
                            None,
                            intra_chain,
                            "".to_string(),
                            false,
                            true,
                        )
                        .await;
                    });
                }
            }
        }
        if let Some(cdr_id) = context.cdr.as_ref() {
            let value = serde_json::json!({
                "cdr_id": cdr_id,
                "number": context.to,
                "bridge_id": bridge_id,
                "tenant_id": tenant_id,
                "internal_call": internal_call,
                "user_agents": user_ring_notifications,
            });
            tokio::spawn(async move {
                let _ = SWITCHBOARD_SERVICE
                    .yaypi_client
                    .create_cdr_event(value)
                    .await;
            });
            if let Some(caller_id) = last_caller_id {
                let cdr_id = *cdr_id;
                tokio::spawn(async move {
                    let _ = SWITCHBOARD_SERVICE
                        .db
                        .update_cdr(
                            cdr_id,
                            CdrItemChange {
                                caller_id: Some(caller_id.user),
                                caller_id_name: Some(caller_id.display_name),
                                ..Default::default()
                            },
                        )
                        .await;
                });
            }
        }

        REDIS
            .hset(&self.redis_key(), "bridge_id", &bridge_id)
            .await?;
        let bridge = Bridge::new(bridge_id.clone());
        bridge.set_inbound(&self.id).await?;
        bridge.set_ringtone(ringtone).await?;
        bridge.set_announcement(announcement).await?;
        bridge.set_tenant_id(tenant_id).await?;
        if context.endpoint.user().is_none() && context.endpoint.trunk().is_none() {
            // for calls not from a sip user or trunk, ring straight away
            self.ring(ringtone.to_string())
                .await
                .context("trying to ring")?;
            REDIS
                .hset(&format!("nebula:bridge:{}", &bridge_id), "ringing", "183")
                .await?;
        }
        if endpoints_has_user {
            let _ = REDIS
                .lpush(
                    &format!("nebula:tenant:{}:incoming_bridge", tenant_id),
                    &bridge_id,
                )
                .await;
            info!(
                channel = self.id,
                bridge = bridge_id,
                "[{}] add {} to {}'s incoming_bridge",
                &self.id,
                &bridge_id,
                tenant_id
            );
        }

        let local_bridge = bridge.clone();
        tokio::spawn(async move {
            let _ = local_bridge
                .handle_timeout(deadline, send_completed_elsewhere_for_timeout)
                .await;
        });

        Ok(bridge)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_outbound(
        id: String,
        caller_id: CallerId,
        endpoint: Endpoint,
        location: Location,
        backup_location: Option<(Location, CallerId)>,
        inbound_channel: String,
        bridge_id: String,
        internal_call: bool,
        rtpmaps: Vec<Rtpmap>,
        video_rtpmaps: Option<Vec<Rtpmap>>,
        call_confirmation: Option<CallConfirmation>,
        click_to_dial: Option<(String, String)>,
        forward_info: Option<ForwardInfo>,
        callflow: Option<(String, String, Uuid)>,
        intra_chain: Vec<String>,
        room: String,
        auto_answer: bool,
        add_to_user_channels: bool,
    ) -> Result<()> {
        if !room.is_empty() {
            info!(
                channel = id,
                room = room,
                "room create new outbound channel"
            );
        }
        let cdr = if let Some(forward_info) = forward_info {
            println!("got forward info");
            if let Some(cdr) = SWITCHBOARD_SERVICE.db.create_cdr().await.ok() {
                let cdr_uuid = cdr.uuid;
                tokio::spawn(async move {
                    let _ = SWITCHBOARD_SERVICE
                        .db
                        .update_cdr(
                            cdr_uuid,
                            CdrItemChange {
                                tenant_id: Some(forward_info.tenant_id),
                                callflow_uuid: Some(forward_info.callflow_id),
                                callflow_name: Some(forward_info.callflow_name),
                                from_type: Some("number".to_string()),
                                from_exten: Some(forward_info.from_number),
                                to_type: Some("number".to_string()),
                                to_exten: Some(forward_info.to_number),
                                call_type: Some("outbound".to_string()),
                                ..Default::default()
                            },
                        )
                        .await;
                });
                Some(cdr_uuid)
            } else {
                None
            }
        } else if let Some((_, _, cdr)) = callflow.as_ref() {
            Some(*cdr)
        } else {
            None
        };
        let context = CallContext {
            endpoint: endpoint.clone(),
            location: Some(location.clone()),
            inter_tenant: None,
            proxy_host: location.proxy_host.clone(),
            api_addr: None,
            to: caller_id.user.clone(),
            to_internal_number: None,
            direction: CallDirection::Recipient,
            caller_id: Some(caller_id.clone()),
            set_caller_id: None,
            answer_caller_id: None,
            call_queue_id: None,
            call_queue_name: None,
            call_flow_id: None,
            call_flow_name: None,
            call_flow_show_original_callerid: None,
            call_flow_moh: None,
            call_flow_custom_callerid: None,
            parent_call_flow_name: None,
            parent_call_flow_show_original_callerid: None,
            parent_call_flow_moh: None,
            parent_call_flow_custom_callerid: None,
            intra_peer: None,
            intra_chain,
            redirect_peer: None,
            reply_to: Some(inbound_channel.clone()),
            refer: None,
            transfer: None,
            cdr,
            started_at: Utc::now(),
            answred_at: None,
            backup: None,
            backup_invite: backup_location.map(|(l, backup_caller_id)| {
                CallInvitation {
                    location: l,
                    caller_id: backup_caller_id,
                    endpoint: endpoint.clone(),
                    inbound_channel: inbound_channel.clone(),
                    bridge_id: bridge_id.clone(),
                    internal_call,
                    call_confirmation: call_confirmation.clone(),
                    rtpmaps: rtpmaps.clone(),
                }
            }),
            forward: false,
            recording: None,
            call_confirmation,
            click_to_dial: click_to_dial.clone(),
            callflow: callflow.map(|(t, c, _)| (t, c)),
            callback: None,
            callback_peer: None,
            room: room.clone(),
            remove_call_restrictions: false,
            ziron_tag: None,
            diversion: None,
            pai: None,
            remote_ip: None,
        };

        REDIS
            .hmset(
                &format!("nebula:channel:{}", &id),
                vec![
                    ("bridge_id", &bridge_id),
                    ("inbound_channel", &inbound_channel),
                    ("user", &location.user),
                    ("call_context", &serde_json::to_string(&context)?),
                ],
            )
            .await?;
        let _ = REDIS
            .expire(&format!("nebula:channel:{}", &id), 86400)
            .await;

        let mut alert_info_header = None;
        if let Endpoint::User { user, .. } = &context.endpoint {
            if add_to_user_channels {
                REDIS
                    .sadd(&format!("nebula:sipuser:{}:channels", user.uuid), &id)
                    .await?;
                notify_user_presence(&user.uuid);
            }

            if let Some(patterns) = user.alert_info_patterns.as_ref() {
                if let Ok(patterns) =
                    serde_json::from_str::<Vec<AlertInfoPatterns>>(patterns)
                {
                    alert_info_header =
                        get_alert_info_header(&patterns, &caller_id, internal_call);
                }
            }
        }
        if auto_answer {
            alert_info_header = Some("Answer-After = 0".to_string());
        }

        info!(channel = id, "new outbound channel");
        REDIS.sadd(ALL_CHANNELS, &id).await?;

        let mut reply_to = inbound_channel.clone();
        let inbound_channel = Channel::new(inbound_channel.clone());
        let mut inbound_cdr = None;
        if let Ok(context) = inbound_channel.get_call_context().await {
            inbound_cdr = context.cdr.map(|cdr| cdr.to_string());
            if let Some(r) = context.reply_to {
                reply_to = r;
            }
        }

        if !location.intra {
            Channel::new(id)
                .invite(
                    caller_id,
                    location,
                    internal_call,
                    reply_to,
                    inbound_cdr,
                    rtpmaps,
                    video_rtpmaps,
                    alert_info_header,
                    click_to_dial.is_some(),
                    if room.is_empty() {
                        None
                    } else if endpoint.user().is_some() {
                        Some(room)
                    } else {
                        None
                    },
                )
                .await?;
        } else {
            tokio::spawn(async move {
                let _ = Channel::new(id)
                    .new_intra(caller_id, location, reply_to, rtpmaps)
                    .await;
            });
        }
        Ok(())
    }

    async fn new_intra(
        &self,
        caller_id: CallerId,
        location: Location,
        reply_to: String,
        rtpmaps: Vec<Rtpmap>,
    ) -> Result<()> {
        let intra_id = uuid_v5(&format!("{}:intra", &self.id)).to_string();
        println!("new intra channel from {} {}", self.id, intra_id);
        let mut context = self.get_call_context().await?;
        context.intra_peer = Some(intra_id.clone());
        let mut intra_chain = context.intra_chain.clone();
        intra_chain.push(location.user.clone());
        self.set_call_context(&context).await?;
        let sdp = SessionDescription::create_offer(
            SWITCHBOARD_SERVICE.config.media_ip,
            &self.id,
            false,
            rtpmaps,
            None,
            location.encryption,
        )
        .await?;
        self.set_local_sdp(&sdp, SdpType::Offer).await?;
        // self.set_peer_channel(&intra_id, &sdp).await?;
        let endpoint = Endpoint::Provider {
            provider: Provider {
                id: 0,
                name: "intra".to_string(),
                host: Some("intra".to_string()),
                ..Default::default()
            },
            number: if caller_id.anonymous {
                "anonymous".to_string()
            } else {
                caller_id.user
            },
            anonymous: caller_id.anonymous,
            tenant_id: location.tenant_id.clone(),
            diversion: None,
            pai: None,
        };
        let _ = SWITCHBOARD_SERVICE
            .proxy_rpc
            .new_intra(
                &location.proxy_host.clone(),
                intra_id,
                endpoint,
                location,
                reply_to,
                sdp.to_string(),
                self.id.clone(),
                intra_chain,
            )
            .await;
        // let req = NewInboundChannelRequest {
        //     id: intra_id,
        //     from: endpoint,
        //     proxy_host: "".to_string(),
        //     to: location.user,
        //     sdp: sdp.to_string(),
        //     existing: false,
        //     refer: None,
        //     intra: Some(self.id.clone()),
        //     intra_chain,
        //     redirect: None,
        //     reply_to: Some(reply_to),
        //     replaces: None,
        //     callflow: None,
        // };
        // let intra_channel = Self::new_inbound(&req).await?;
        // intra_channel.switch(req).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn invite(
        &self,
        caller_id: CallerId,
        location: Location,
        internal_call: bool,
        reply_to: String,
        inbound_cdr: Option<String>,
        rtpmaps: Vec<Rtpmap>,
        video_rtpmaps: Option<Vec<Rtpmap>>,
        alert_info_header: Option<String>,
        click_to_dial: bool,
        room: Option<String>,
    ) -> Result<()> {
        let webrtc = location.dst_uri.transport == TransportType::Wss
            || location.device.is_some();
        let sdp = SessionDescription::create_offer(
            SWITCHBOARD_SERVICE.config.media_ip,
            &self.id,
            webrtc,
            rtpmaps,
            video_rtpmaps,
            location.encryption,
        )
        .await?;
        self.set_local_sdp(&sdp, SdpType::Offer).await?;
        let _ = SWITCHBOARD_SERVICE
            .proxy_rpc
            .invite(
                &location.proxy_host,
                self.id.clone(),
                reply_to,
                inbound_cdr,
                caller_id,
                location.clone(),
                internal_call,
                alert_info_header,
                sdp.to_string(),
                click_to_dial,
                room,
                SWITCHBOARD_SERVICE.config.switchboard_host.clone(),
            )
            .await;
        Ok(())
    }

    pub async fn replace_channel(&self, refer_channel: &Channel) -> Result<()> {
        info!(channel = self.id, "replace channel to {}", refer_channel.id);
        self.answer().await?;

        let mut context = self.get_call_context().await?;
        context.transfer = Some(TransferContext {
            peer: TransferPeer::Initiator {
                to_channel: refer_channel.id.clone(),
                empty_sdp: false,
            },
        });
        self.set_call_context(&context).await?;

        let mut refer_channel_context = refer_channel.get_call_context().await?;
        refer_channel_context.transfer = Some(TransferContext {
            peer: TransferPeer::Recipient {
                from_channel: self.id.clone(),
                target: TransferTarget::Number("".to_string()),
                auto_answer: false,
                destination: "".to_string(),
            },
        });
        refer_channel
            .set_call_context(&refer_channel_context)
            .await?;

        SWITCHBOARD_SERVICE
            .proxy_rpc
            .hangup(
                &refer_channel_context.proxy_host,
                refer_channel.id.clone(),
                None,
                None,
                None,
                is_channel_answered(&refer_channel.id).await,
            )
            .await?;

        let _ = refer_channel.unhold().await;

        if let Some(peer) = refer_channel.get_peer().await {
            let _ = Bridge::unbridge_media(&peer, refer_channel).await;
            let _ = Bridge::unbridge_media(refer_channel, &peer).await;

            let _ = Bridge::bridge_media(&peer, self).await;
            let _ = Bridge::bridge_media(self, &peer).await;
        }

        self.receive_event("0", ChannelEvent::Hangup).await?;
        Ok(())
    }

    pub async fn pickup_channel(&self, channel: &Channel) -> Result<()> {
        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or(anyhow!("not coming from a user"))?;
        if let Ok(bridge) = channel.get_bridge().await {
            let inbound = bridge
                .get_inbound()
                .await
                .unwrap_or(Channel::new("".to_string()));
            if inbound.id != "" && inbound.id != channel.id {
                if !bridge.is_hangup().await.unwrap_or(true)
                    && !bridge.answered().await
                {
                    bridge.add_channel(&self.id).await?;

                    if let Ok(inbound_context) = inbound.get_call_context().await {
                        let mut context = self.get_call_context().await?;
                        context.answer_caller_id = Some(
                            internal_caller_id(
                                &inbound_context,
                                from_user.cc.clone(),
                                None,
                            )
                            .await,
                        );
                        self.set_call_context(&context).await?;
                    }

                    self.answer().await?;
                    bridge.answer(&self).await?;
                    self.receive_event("0", ChannelEvent::Hangup).await?;
                }
            }
        }
        Ok(())
    }

    async fn new_redirect(
        &self,
        exten: String,
        reply_to: String,
        rtpmaps: Vec<Rtpmap>,
    ) -> Result<()> {
        let redirect_id = uuid_v5(&format!("{}:redirect", &self.id)).to_string();
        let mut context = self.get_call_context().await?;
        context.redirect_peer = Some(redirect_id.clone());
        self.set_call_context(&context).await?;

        let sdp = SessionDescription::create_basic(
            SWITCHBOARD_SERVICE.config.media_ip,
            LOCAL_RTP_PEER_PORT,
            None,
            rtpmaps,
        );
        self.set_local_sdp(&sdp, SdpType::Offer).await?;

        let req = NewInboundChannelRequest {
            id: redirect_id,
            from: context.endpoint,
            proxy_host: context.proxy_host.clone(),
            to: exten,
            sdp: sdp.to_string(),
            existing: false,
            refer: None,
            transfer: None,
            raw_transfer: None,
            intra: None,
            intra_chain: Vec::new(),
            inter_tenant: None,
            redirect: Some(self.id.clone()),
            reply_to: Some(reply_to),
            replaces: None,
            callflow: None,
            teams_refer_cx: None,
            remote_ip: None,
        };
        let redirect_channel = Self::new_inbound(&req).await?;
        redirect_channel.switch(req).await?;
        Ok(())
    }

    pub async fn get_hangup(&self) -> Result<ChannelHangupDirection> {
        if !REDIS
            .exists(&format!("nebula:channel:{}", &self.id))
            .await
            .unwrap_or(false)
        {
            return Err(anyhow!("can't find hangup direction"));
        }
        let hangup_string: String = REDIS
            .hget(&format!("nebula:channel:{}", &self.id), "hangup")
            .await?;
        Ok(ChannelHangupDirection::from_str(&hangup_string)?)
    }

    pub async fn set_hangup(&self, direction: ChannelHangupDirection) -> Result<()> {
        REDIS
            .hset(&self.redis_key(), "hangup", &direction.to_string())
            .await?;
        Ok(())
    }

    pub async fn is_hangup(&self) -> bool {
        is_channel_hangup(&self.id).await
    }

    pub async fn notify_voicemail(mailbox: &VoicemailUser) -> Result<()> {
        let _ = notify_dialog(
            SWITCHBOARD_SERVICE
                .config
                .all_proxies()
                .map(|p| p.to_vec())
                .unwrap_or_else(|| {
                    vec![SWITCHBOARD_SERVICE.config.proxy_host.clone()]
                }),
            format!("vm:{}", mailbox.uuid),
        )
        .await;

        let users = SWITCHBOARD_SERVICE
            .db
            .get_user_by_voicemail(&mailbox.tenant_id, &mailbox.uuid)
            .await
            .ok_or_else(|| anyhow!("can't find users that use this mailbox"))?;
        let msgs = SWITCHBOARD_SERVICE
            .db
            .get_voicemail_messages(&mailbox.uuid)
            .await
            .ok_or_else(|| anyhow!("can't get messages in the mailbox"))?;
        let new = msgs.iter().filter(|m| m.folder == "new").count();
        let old = msgs.iter().filter(|m| m.folder == "saved").count();
        for user in users.iter() {
            let locations = Usrloc::lookup(user).await.unwrap_or_default();
            for location in locations {
                if location.device.is_none() {
                    let _ = SWITCHBOARD_SERVICE
                        .proxy_rpc
                        .notify_voicemail(location, new, old)
                        .await;
                }
            }
        }
        Ok(())
    }

    async fn complete_yaypi_call(&self) -> Result<()> {
        if !self.is_answered().await {
            return Ok(());
        }
        let answer_context = self.get_call_context().await?;
        if let Endpoint::Provider {
            provider, number, ..
        } = &answer_context.endpoint
        {
            if provider.name != "intra"
                && answer_context.direction == CallDirection::Recipient
            {
                let inbound = self
                    .get_inbound_channel()
                    .await
                    .unwrap_or_else(|| self.clone());

                let answer_context = answer_context.clone();
                let number = number.clone();
                let now = Utc::now();
                let id = self.id.clone();
                let provider_name = provider.name.clone();
                tokio::spawn(async move {
                    let inbound_context = inbound.get_call_context().await?;
                    let (inbound, inbound_context) = if let Some(redirect) =
                        redirect_inbound(&inbound_context).await
                    {
                        let redirect_context = redirect.get_call_context().await?;
                        (redirect, redirect_context)
                    } else {
                        (inbound, inbound_context)
                    };
                    let number = phonenumber::parse(None, number)?;
                    let e164 =
                        number.format().mode(phonenumber::Mode::E164).to_string();
                    let country_code = number.country().value();
                    let national_number = number.national().value();
                    let cdr = inbound_context
                        .cdr
                        .ok_or_else(|| anyhow!("don't have cdr on channel"))?;
                    let answered_at = answer_context
                        .answred_at
                        .ok_or_else(|| anyhow!("not answered"))?;
                    let start = answer_context.started_at.timestamp() as u64;
                    let duration = now
                        .signed_duration_since(answered_at)
                        .num_seconds() as usize;

                    let yaypi_client = YaypiClient::new();
                    let (code, value) = match yaypi_client
                        .complete_call(
                            &cdr.to_string(),
                            duration,
                            start,
                            country_code,
                            national_number,
                            &e164,
                            provider_name,
                        )
                        .await
                    {
                        Ok((code, value)) => (code, value),
                        Err(e) => {
                            error!(
                                cdr = cdr.to_string(),
                                channel = id,
                                "complete outbound call {cdr} error: {e}"
                            );
                            return Err(e);
                        }
                    };

                    let cost = value
                        .get("cost")
                        .ok_or_else(|| anyhow!("no cost"))?
                        .as_f64()
                        .ok_or_else(|| anyhow!("cost is not f64"))?;
                    let cost = BigDecimal::from_str(&cost.to_string())?;

                    let parent_cost =
                        value.get("parent_cost").and_then(|v| v.as_f64()).and_then(
                            |cost| BigDecimal::from_str(&cost.to_string()).ok(),
                        );
                    info!(
                        cdr = cdr.to_string(),
                        channel = id,
                        "complete outbound call {cdr} duration {} {} {}",
                        duration,
                        code,
                        value
                    );
                    inbound
                        .update_cdr(CdrItemChange {
                            cost: Some(cost),
                            parent_cost,
                            ..Default::default()
                        })
                        .await?;
                    Ok::<(), anyhow::Error>(())
                });
            }
        }
        Ok(())
    }

    async fn add_cdr_event(&self, direction: ChannelHangupDirection) -> Result<()> {
        let now = Utc::now();
        let inbound_channel = self
            .get_inbound_channel()
            .await
            .ok_or_else(|| anyhow!("no inbound channel"))?;
        let bridge = self.get_bridge().await?;
        let context = self.get_call_context().await?;

        let cdr = inbound_channel
            .get_cdr()
            .await
            .ok_or_else(|| anyhow!("no cdr"))?;

        let (user_id, to_type, user_agent) = match &context.endpoint {
            Endpoint::User { user, .. } => (
                user.uuid.clone(),
                "user",
                context
                    .location
                    .as_ref()
                    .and_then(|location| location.user_agent.clone()),
            ),
            Endpoint::Trunk { trunk, .. } => (trunk.uuid.clone(), "trunk", None),
            Endpoint::Provider {
                provider, number, ..
            } => (number.to_string(), "outbound", Some(provider.name.clone())),
        };

        let new_cdr_event = NewCdrEvent {
            cdr_id: cdr.uuid,
            event_type: "bridge".to_string(),
            start: context.started_at,
            answer: context.answred_at,
            end: Some(now),
            user_id: Some(user_id),
            to_type: Some(to_type.to_string()),
            duration: Some(
                now.signed_duration_since(context.started_at)
                    .num_seconds()
                    .max(0),
            ),
            billsec: Some(
                context
                    .answred_at
                    .map(|answered_at| {
                        now.signed_duration_since(answered_at).num_seconds().max(0)
                    })
                    .unwrap_or(0),
            ),
            user_agent,
            bridge_id: Some(bridge.id),
            hangup_direction: Some(direction.to_string()),
            caller_id: context.caller_id.as_ref().map(|c| c.user.clone()),
            caller_id_name: context
                .caller_id
                .as_ref()
                .map(|c| c.display_name.clone()),
        };
        SWITCHBOARD_SERVICE
            .db
            .create_cdr_event(new_cdr_event)
            .await?;

        Ok(())
    }

    async fn complete_cdr(&self, direction: ChannelHangupDirection) -> Result<()> {
        let end = Utc::now();
        let cdr = self.get_cdr().await.ok_or_else(|| anyhow!("no cdr"))?;
        let mut change = CdrItemChange {
            end: Some(end),
            duration: Some(
                end.signed_duration_since(cdr.start).num_seconds().max(0),
            ),
            hangup_direction: Some(direction.to_string()),
            ..Default::default()
        };
        if let Some(answer) = cdr.answer {
            let billsec = end.signed_duration_since(answer).num_seconds().max(0);
            change.billsec = Some(billsec);
        }
        self.update_cdr(change.clone()).await?;

        Ok(())
    }

    pub async fn hangup(&self, reason: Option<HangupReason>) -> Result<(), Error> {
        let mutex = DistributedMutex::new(format!(
            "nebula:channel:{}:hangup:lock",
            &self.id
        ));
        mutex.lock().await;

        if self.is_hangup().await {
            return Ok(());
        }

        info!(
            channel = self.id,
            "now send hangup to channel, reason: {reason:?}"
        );

        let local_channel = self.clone();
        tokio::spawn(async move {
            let _ = local_channel.complete_yaypi_call().await;
            let _ = local_channel
                .complete_cdr(ChannelHangupDirection::Server)
                .await;
        });

        {
            let channel = self.clone();
            tokio::spawn(async move {
                let _ = channel.add_cdr_event(ChannelHangupDirection::Server).await;
            });
        }

        self.new_event(ChannelEvent::Hangup, "hangup").await?;
        self.set_hangup(ChannelHangupDirection::Server).await?;

        self.hangup_endpoint(reason).await?;

        let _ = self.hangup_cleanup().await;
        Ok(())
    }

    pub async fn hangup_endpoint(
        &self,
        reason: Option<HangupReason>,
    ) -> Result<(), Error> {
        let context = self.get_call_context().await?;

        if let Some(intra) = context.intra_peer {
            let _ = SWITCHBOARD_SERVICE
                .proxy_rpc
                .hangup_intra(&context.proxy_host, intra)
                .await;
        } else if let Some(redirect) = context.redirect_peer {
            // let redirect_chanenl = Channel::new(redirect);
            // redirect_chanenl.handle_hangup(false, None, None).await;
            let _ = SWITCHBOARD_SERVICE
                .switchboard_rpc
                .hangup(
                    &SWITCHBOARD_SERVICE.config.switchboard_host,
                    redirect,
                    false,
                    None,
                    None,
                    None,
                )
                .await;
        } else if let Some(refer) = context.refer {
            if let Some(slot) = refer.slot {
                let park_uuid: String = REDIS
                    .hget(&format!("nebula:park:{}:details", slot), "uuid")
                    .await
                    .unwrap_or("".to_string());
                let _ = REDIS.del(&format!("nebula:park:{}", slot)).await;
                let _ = REDIS.del(&format!("nebula:park:{}:details", slot)).await;
                let _ = self.publish_park(park_uuid, slot, false).await;
            }
            // let refer_chanenl = Channel::new(refer.peer);
            // refer_chanenl.handle_hangup(false, None, None).await;
            let _ = SWITCHBOARD_SERVICE
                .switchboard_rpc
                .hangup(
                    &SWITCHBOARD_SERVICE.config.switchboard_host,
                    refer.peer,
                    false,
                    None,
                    None,
                    None,
                )
                .await;
        } else if let Some(transfer) = context.transfer {
            let peer = match transfer.peer {
                TransferPeer::Initiator { to_channel, .. } => {
                    let _ = SWITCHBOARD_SERVICE
                        .proxy_rpc
                        .hangup(
                            &context.proxy_host,
                            self.id.clone(),
                            None,
                            None,
                            None,
                            self.is_answered().await,
                        )
                        .await;
                    to_channel
                }
                TransferPeer::Recipient { from_channel, .. } => from_channel,
            };
            let _ = SWITCHBOARD_SERVICE
                .switchboard_rpc
                .hangup(
                    &SWITCHBOARD_SERVICE.config.switchboard_host,
                    peer,
                    false,
                    None,
                    None,
                    None,
                )
                .await;
        } else {
            let is_answered = self.is_answered().await;

            let _ = SWITCHBOARD_SERVICE
                .proxy_rpc
                .hangup(
                    &context.proxy_host,
                    self.id.clone(),
                    None,
                    None,
                    reason,
                    is_answered,
                )
                .await;
            println!("send rpc hangup channel {}", &self.id);
            if !is_answered {
                if let Some(user) = context.endpoint.user() {
                    if let Some(location) = context.location.as_ref() {
                        if let Some(device) = location.device.as_ref() {
                            if let Some(device_token) =
                                location.device_token.as_ref()
                            {
                                let ringing = REDIS
                                    .hget(
                                        &format!("nebula:channel:{}", &self.id),
                                        "ringing",
                                    )
                                    .await
                                    .unwrap_or(0);
                                if ringing < 180
                                    && Utc::now()
                                        .signed_duration_since(context.started_at)
                                        .num_seconds()
                                        > 25
                                {
                                    user.update_app_status(
                                        device,
                                        device_token,
                                        "unavailable",
                                    )
                                    .await;
                                    let _ = SWITCHBOARD_SERVICE
                                        .proxy_rpc
                                        .options_mobile_app(
                                            &context.proxy_host,
                                            location.clone(),
                                        )
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn pin_auth(
        &self,
        pin: &str,
        sounds: Vec<String>,
        wrong_pin_sound: Vec<String>,
        retry: usize,
        pin_timeout: Option<u64>,
    ) -> Result<AuthPinResult> {
        let mut n = 0;
        loop {
            let digits = self
                .play_sound_and_get_dtmf(
                    sounds.clone(),
                    pin_timeout.unwrap_or(5000),
                    5000,
                    pin.len(),
                )
                .await;
            println!("digits {:?}", &digits);
            if digits.len() == 0 {
                return Ok(AuthPinResult::Timeout);
            }

            if digits.join("") == pin {
                return Ok(AuthPinResult::Accepted);
            }
            println!("invalid pin");
            n += 1;
            if n > retry {
                return Ok(AuthPinResult::Failed);
            }
            self.play_sound(None, wrong_pin_sound.clone(), true).await?;
            println!("invalid pin played done");
        }
    }

    pub async fn start_voicemail(&self, voicemail_id: String) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .start_voicemail(audio_id, voicemail_id)
            .await?;
        Ok(())
    }

    pub async fn update_recording_from_user(
        &self,
        from_user: Option<&User>,
        to_user: Option<&User>,
        is_refer: bool,
    ) -> Result<()> {
        if from_user.is_none() && to_user.is_none() {
            return Ok(());
        }

        let to_rec = to_user.as_ref().map(|u| {
            (
                u.tenant_id.as_str(),
                u.recording.unwrap_or(false),
                u.unlimited_recording.unwrap_or(false),
            )
        });
        let from_rec = from_user.map(|u| {
            (
                u.tenant_id.as_str(),
                u.recording.unwrap_or(false),
                u.unlimited_recording.unwrap_or(false),
            )
        });

        if let Some((tenant_id, explicit, unlimited)) = match (from_rec, to_rec) {
            (None, None) => None,
            (None, Some(to)) => Some(to),
            (Some(from), None) => {
                if is_refer {
                    None
                } else {
                    Some(from)
                }
            }
            (Some(from), Some(to)) => {
                if is_refer {
                    Some(to)
                } else {
                    Some((to.0, to.1 && from.1, to.2 && from.2))
                }
            }
        } {
            return self.update_recording(tenant_id, explicit, unlimited).await;
        };

        Ok(())
    }

    async fn start_recording(&self) -> Result<()> {
        let answered = self.is_answered().await;
        if !answered {
            return Ok(());
        }

        let mut context = self.get_call_context().await?;

        let recording = match context.recording.as_mut() {
            Some(r) => Ok(r),
            None => {
                let self_clone = self.clone();
                tokio::spawn(async move {
                    let _ = Self::toggle_parent_recordings(self_clone, true).await;
                });
                Err(anyhow!("recording not enabled"))
            }
        }?;

        if recording.started {
            return Ok(());
        }

        let self_clone = self.clone();

        tokio::spawn(async move {
            let _ = Self::toggle_parent_recordings(self_clone, false).await;
        });

        if let Some(transfer) = &context.transfer {
            if let TransferPeer::Initiator { to_channel, .. } = &transfer.peer {
                if let Some(cdr) = context.cdr {
                    if let Some(peer) =
                        Channel::new(to_channel.to_string()).get_peer().await
                    {
                        let audio_id = peer.get_audio_id().await?;
                        SWITCHBOARD_SERVICE
                            .media_rpc_client
                            .start_recording(
                                audio_id,
                                cdr.to_string(),
                                recording.expires,
                            )
                            .await?;
                        self.update_cdr(CdrItemChange {
                            recording: Some(format!(
                                "{}/{cdr}.mp3",
                                recording.expires,
                            )),
                            ..Default::default()
                        })
                        .await?;
                        recording.started = true;
                        self.set_call_context(&context).await?;
                    }
                }
            }
        } else if let Some(cdr) = context.cdr {
            let audio_id = self.get_audio_id().await?;
            SWITCHBOARD_SERVICE
                .media_rpc_client
                .start_recording(audio_id, cdr.to_string(), recording.expires)
                .await?;
            self.update_cdr(CdrItemChange {
                recording: Some(format!("{}/{cdr}.mp3", recording.expires,)),
                ..Default::default()
            })
            .await?;
            recording.started = true;
            self.set_call_context(&context).await?;
        }

        Ok(())
    }

    async fn toggle_recording(&self) -> Result<()> {
        let inbound = self.get_inbound_channel().await.unwrap_or(self.clone());
        let context = inbound.get_call_context().await?;

        info!(channel = self.id, "Now toggle recording");
        let pause_reason =
            Self::do_recording_pause(&inbound, context.clone()).await?;

        if pause_reason == RecordingPause::Unitialised {
            self.do_initialise_recording(inbound.clone()).await?;
        }

        info!(
            channel = self.id,
            "Now play record pause sound {pause_reason:?}"
        );
        self.play_sound(None, vec![pause_reason.get_sound()], true)
            .await?;

        tokio::spawn(async move {
            let is_pause = context.recording.as_ref().is_some_and(|rc| !rc.paused);
            let _ = Self::toggle_parent_recordings(inbound, is_pause).await;
        });

        Ok(())
    }

    // Check if there is a peer that links to this leg
    // Need to check the direct refer as a recipient for the initialising leg
    // And for reciepient peer on the receiver leg, only accept receiver leg
    //   if it has `slot` to know that it is from a park / retrieve chain
    async fn get_toggle_parent(&self) -> Result<Option<String>> {
        // Check current channel for initiator peer
        if let Some(ctx_refer) = self
            .get_call_context()
            .await?
            .refer
            .filter(|r| r.direction == CallDirection::Recipient)
        {
            return Ok(Some(ctx_refer.peer));
        }

        // Check peer channel for parking refer
        if let Some(peer) = self.get_peer().await {
            let peer_ctx = peer.get_call_context().await?;

            if let Some(peer_refer) = peer_ctx.refer.filter(|r| {
                r.slot.is_some() && r.direction == CallDirection::Recipient
            }) {
                return Ok(Some(peer_refer.peer));
            }
        }

        Ok(None)
    }

    async fn toggle_parent_recordings(mut channel: Self, pause: bool) -> Result<()> {
        while let Some(peer_channel_id) = channel.get_toggle_parent().await? {
            channel = Channel {
                id: peer_channel_id,
            };

            let inbound = channel
                .get_inbound_channel()
                .await
                .unwrap_or(channel.clone());

            let context = inbound.get_call_context().await?;

            let recording_paused =
                context.recording.as_ref().map_or(true, |rec| rec.paused);

            if recording_paused != pause {
                Self::do_recording_pause(&inbound, context).await?;
            }
        }

        Ok(())
    }

    async fn do_recording_pause(
        inbound: &Channel,
        mut context: CallContext,
    ) -> Result<RecordingPause> {
        let Some(recording) = context.recording.as_mut() else {
            return Ok(RecordingPause::Unitialised);
        };

        let audio_id = inbound.get_audio_id().await?;
        recording.paused = !recording.paused;
        let pause_reason = if recording.paused {
            SWITCHBOARD_SERVICE
                .media_rpc_client
                .pause_recording(audio_id)
                .await?;
            RecordingPause::Pause
        } else {
            SWITCHBOARD_SERVICE
                .media_rpc_client
                .resume_recording(audio_id)
                .await?;
            RecordingPause::Resume
        };
        inbound.set_call_context(&context).await?;

        return Ok(pause_reason);
    }

    /// This is called when a user use `ToggleRecording` but there wasn't recording started
    /// for the call.
    /// So it takes the recording information from the channel's user and update recording
    /// on the inbound channel
    async fn do_initialise_recording(&self, inbound: Channel) -> Result<()> {
        info!(channel = self.id, "Now initialise recording");
        let user_context = self.get_call_context().await?;
        let user = user_context.endpoint.user().ok_or(anyhow!("not a user"))?;
        inbound
            .update_recording(
                &user.tenant_id,
                true,
                user.unlimited_recording.unwrap_or(false),
            )
            .await?;

        Ok(())
    }

    pub async fn update_recording(
        &self,
        tenant_id: &str,
        explicit: bool,
        unlimited: bool,
    ) -> Result<()> {
        let mut context = self.get_call_context().await?;

        if let Some(refer) = &context.refer {
            if refer.no_recording {
                return Ok(());
            }
        }

        if context.recording.is_none() {
            let recording = SWITCHBOARD_SERVICE.db.get_recording(tenant_id).await;
            let start = match &recording {
                Some(recording) => match recording.global_enabled {
                    Some(global) => {
                        if !global {
                            return Ok(());
                        } else {
                            true
                        }
                    }
                    None => explicit,
                },
                None => explicit,
            };
            if !start {
                return Ok(());
            }

            let expires = if unlimited {
                0
            } else if let Some(recording) = recording {
                recording.expires_in as usize
            } else {
                30
            };

            context.recording = Some(RecordingContext {
                started: false,
                paused: false,
                expires,
            });
            self.set_call_context(&context).await?;
        }

        self.start_recording().await?;

        Ok(())
    }

    #[async_recursion::async_recursion]
    async fn send_dtmf(&self, dtmf: String) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .send_dtmf(audio_id, dtmf.clone())
            .await?;

        let context = self.get_call_context().await?;

        if let Some(transfer) = context.transfer {
            match transfer.peer {
                TransferPeer::Initiator { to_channel, .. } => {
                    let _ = Channel::new(to_channel).send_dtmf_to_peer(dtmf).await;
                }
                TransferPeer::Recipient { from_channel, .. } => {
                    let _ = Channel::new(from_channel).send_dtmf_to_peer(dtmf).await;
                }
            }
        }

        Ok(())
    }

    async fn send_dtmf_to_peer(&self, dtmf: String) -> Result<()> {
        println!("send dtmf {} to peer", dtmf);
        if let Some(peer) = self.get_peer().await {
            peer.send_dtmf(dtmf).await?;
        }
        Ok(())
    }

    pub async fn receive_dtmf(&self, dtmf: String) -> Result<()> {
        info!(channel = self.id, "channel receive dtmf {}", dtmf);

        if let Some(peer) = self.get_peer().await {
            if let Ok(context) = peer.get_call_context().await {
                if let Some(transfer) = context.transfer {
                    if let TransferPeer::Recipient { from_channel, .. } =
                        transfer.peer
                    {
                        let _ = REDIS
                            .xadd::<String>(
                                &format!("nebula:channel:{from_channel}:stream"),
                                "dtmf",
                                &dtmf,
                            )
                            .await;
                    }
                }
            }
        }

        if let Some(in_channel_shortcodes) = self.get_in_channel_shortcodes().await {
            if !in_channel_shortcodes.is_empty() {
                let mutex = DistributedMutex::new(format!(
                    "nebula:channel:{}:receive_dtmf:lock",
                    &self.id
                ));
                mutex.lock().await;

                let pending_shortocdes: String = REDIS
                    .hget(&self.redis_key(), "pending_shortcodes")
                    .await
                    .unwrap_or("".to_string());

                if dtmf == "*" {
                    self.set_pending_shortcode("*").await;
                    if !pending_shortocdes.is_empty() {
                        for dtmf in pending_shortocdes.chars() {
                            self.send_dtmf_to_peer(dtmf.to_string()).await?;
                        }
                    }
                    return Ok(());
                } else {
                    if pending_shortocdes.len() > 0 {
                        let current = format!("{}{}", pending_shortocdes, dtmf);
                        for shortcode in in_channel_shortcodes {
                            let code = format!("*{}", shortcode.shortcode);
                            if code == current {
                                self.clear_pending_shortcode().await;
                                if let Err(e) = self
                                    .process_shortcode(shortcode.clone(), true)
                                    .await
                                {
                                    warn!(
                                        "process shortcode {:?} error {}",
                                        shortcode, e
                                    );
                                }
                                return Ok(());
                            } else if code.starts_with(&current) {
                                self.set_pending_shortcode(&current).await;
                                return Ok(());
                            }
                        }
                        self.clear_pending_shortcode().await;
                        // no match, sending dtmf to peer
                        for dtmf in current.chars() {
                            self.send_dtmf_to_peer(dtmf.to_string()).await?;
                        }
                        return Ok(());
                    }
                }
            }
        }
        self.send_dtmf_to_peer(dtmf).await?;

        Ok(())
    }

    async fn set_pending_shortcode(&self, pending: &str) {
        let key = self.redis_key();
        let version = nebula_utils::uuid();
        let _ = REDIS
            .hmset(
                &key,
                vec![
                    ("pending_shortcodes", pending),
                    ("pending_shortcodes_version", &version),
                ],
            )
            .await;

        let local_channel = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let pending_shortocdes: String = REDIS
                .hget(&key, "pending_shortcodes")
                .await
                .unwrap_or("".to_string());
            let current_version: String = REDIS
                .hget(&key, "pending_shortcodes_version")
                .await
                .unwrap_or("".to_string());
            if version != current_version {
                println!("pending shortcodes get updated, quit");
                return;
            }
            local_channel.clear_pending_shortcode().await;
            for dtmf in pending_shortocdes.chars() {
                let _ = local_channel.send_dtmf_to_peer(dtmf.to_string()).await;
            }
        });
    }

    async fn clear_pending_shortcode(&self) -> String {
        let version = nebula_utils::uuid();
        let _ = REDIS.hdel(&self.redis_key(), "pending_shortcodes").await;
        let _ = REDIS
            .hdel(&self.redis_key(), "pending_shortcodes_version")
            .await;
        version
    }

    pub async fn process_shortcode(
        &self,
        shortcode: ShortCode,
        in_channel: bool,
    ) -> Result<()> {
        let value: Value = serde_json::from_str(&shortcode.feature)?;
        let feature = value
            .get("feature")
            .ok_or(anyhow!("shortcode doesn't have feature"))?
            .as_str()
            .ok_or(anyhow!("shortcode feature is not string"))?;
        match feature.to_lowercase().as_str() {
            "parkretrieve" => {
                let slot = value
                    .get("slot")
                    .ok_or(anyhow!("shortcode doesn't have feature"))?;
                let slot = format!("{}_{}", slot, shortcode.tenant_id);
                if in_channel {
                    let park_uuid = self.park(slot.clone(), false).await?;
                    self.publish_park(park_uuid, slot, true).await?;
                } else {
                    self.retrieve(slot.clone()).await?;
                }
            }
            "playsound" => {
                let sound = value
                    .get("sound")
                    .ok_or(anyhow!("playsound doens't have sound"))?
                    .as_str()
                    .ok_or(anyhow!("playsound sound is not str"))?
                    .to_string();
                let peer = self
                    .get_peer()
                    .await
                    .ok_or(anyhow!("channel doesn't have peer"))?;
                if !self.is_answered().await {
                    return Ok(());
                }
                if !peer.is_answered().await {
                    return Ok(());
                }
                let local_sound = sound.clone();
                tokio::spawn(async move {
                    let _ = peer.play_sound(None, vec![local_sound], true).await;
                });
                self.play_sound(None, vec![sound], true).await?;
            }
            "togglerecording" => {
                self.toggle_recording().await?;
            }
            _ => {}
        }
        Ok(())
    }

    // async fn set_peer_channel(
    //     &self,
    //     peer_channel_id: &str,
    //     sdp: &SessionDescription,
    // ) -> Result<()> {
    //     let mut conn = REDIS_POOL.get().await?;
    //     for media in sdp.medias() {
    //         let mid = media.mid().ok_or(anyhow!("no mid"))?;
    //         let id = format!("{}:{}", &self.id, mid);
    //         let peer_id = format!("{}:{}", peer_channel_id, mid);
    //         conn.hset(&format!("nebula:media_stream:{}", &id), "peer", &peer_id)
    //             .await?;
    //         conn.hset(&format!("nebula:media_stream:{}", &id), "channel", &self.id)
    //             .await?;
    //         conn.hset(&format!("nebula:media_stream:{}", &peer_id), "peer", &id)
    //             .await?;
    //         conn.hset(
    //             &format!("nebula:media_stream:{}", &peer_id),
    //             "channel",
    //             peer_channel_id,
    //         )
    //         .await?;
    //     }
    //     Ok(())
    // }

    async fn reset_refer_sdp(&self, peer_channel: &Channel) -> Result<()> {
        let local_sdp = self.get_local_sdp().await?;
        let remote_sdp = self.get_remote_sdp().await?;
        let local_rtpmaps = local_sdp
            .get_rtpmaps(&MediaType::Audio)
            .ok_or(anyhow!("no audio rtpmaps"))?;
        let remote_rtpmaps = remote_sdp
            .get_rtpmaps(&MediaType::Audio)
            .ok_or(anyhow!("no audio rtpmaps"))?;
        let local_audio = local_sdp
            .audio()
            .ok_or(anyhow!("local sdp doens't have audio"))?;
        let local_sdp = SessionDescription::create_basic(
            SWITCHBOARD_SERVICE.config.media_ip,
            local_audio.port,
            local_audio.mid(),
            local_rtpmaps.clone(),
        );
        self.set_local_sdp(&local_sdp, SdpType::Offer).await?;
        let remote_sdp = SessionDescription::create_basic(
            SWITCHBOARD_SERVICE.config.media_ip,
            LOCAL_RTP_PEER_PORT,
            local_audio.mid(),
            remote_rtpmaps.clone(),
        );
        self.set_remote_sdp(&remote_sdp, SdpType::Answer).await?;

        let peer_local_sdp = peer_channel.get_local_sdp().await.ok();
        let peer_local_audio_port = peer_local_sdp
            .as_ref()
            .and_then(|sdp| sdp.audio())
            .map(|a| a.port)
            .unwrap_or(LOCAL_RTP_PEER_PORT);
        let peer_local_audio_mid = peer_local_sdp
            .as_ref()
            .and_then(|sdp| sdp.audio())
            .and_then(|a| a.mid());
        let peer_remote_sdp = SessionDescription::create_basic(
            SWITCHBOARD_SERVICE.config.media_ip,
            LOCAL_RTP_PEER_PORT,
            peer_local_audio_mid.clone(),
            local_rtpmaps.clone(),
        );
        peer_channel
            .set_remote_sdp(&peer_remote_sdp, SdpType::Offer)
            .await?;
        let peer_local_sdp = SessionDescription::create_basic(
            SWITCHBOARD_SERVICE.config.media_ip,
            peer_local_audio_port,
            peer_local_audio_mid.clone(),
            remote_rtpmaps.clone(),
        );
        let peer_local_audio = peer_local_sdp
            .audio()
            .ok_or(anyhow!("peer local sdp doens't have audio"))?;
        peer_channel
            .set_local_sdp(&peer_local_sdp, SdpType::Answer)
            .await?;

        let audio_id = local_audio.uuid(&self.id).to_string();
        let peer_audio_id = peer_local_audio.uuid(&peer_channel.id).to_string();

        REDIS
            .hset(
                &format!("nebula:media_stream:{}", &audio_id),
                "peer",
                &peer_audio_id,
            )
            .await?;
        REDIS
            .hset(
                &format!("nebula:media_stream:{}", &peer_audio_id),
                "peer",
                &audio_id,
            )
            .await?;

        SWITCHBOARD_SERVICE
            .media_rpc_client
            .set_peer_addr(
                audio_id.clone(),
                Ipv4Addr::new(127, 0, 0, 1),
                LOCAL_RTP_PEER_PORT,
            )
            .await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .set_peer_addr(
                peer_audio_id.clone(),
                Ipv4Addr::new(127, 0, 0, 1),
                LOCAL_RTP_PEER_PORT,
            )
            .await?;

        Ok(())
    }

    pub async fn publish_park(
        &self,
        park_uuid: String,
        slot: String,
        parked: bool,
    ) -> Result<()> {
        let context = self.get_call_context().await?;

        let _ = notify_dialog(
            SWITCHBOARD_SERVICE
                .config
                .all_proxies()
                .map(|p| p.to_vec())
                .unwrap_or_else(|| {
                    vec![SWITCHBOARD_SERVICE.config.proxy_host.clone()]
                }),
            slot.clone(),
        )
        .await;
        let to = match context.direction {
            CallDirection::Initiator => context.to.to_string(),
            CallDirection::Recipient => context.endpoint.extension(),
        };
        let from = match context.direction {
            CallDirection::Initiator => context.endpoint.clone(),
            CallDirection::Recipient => {
                let peer =
                    self.get_peer().await.ok_or_else(|| anyhow!("no peer"))?;
                peer.get_call_context().await?.endpoint
            }
        };

        let from = from.extension();
        let _ = SWITCHBOARD_SERVICE
            .proxy_rpc
            .publish_park(
                &context.proxy_host,
                park_uuid,
                slot.clone(),
                parked,
                context.direction.clone(),
                from,
                to,
            )
            .await;
        tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .get(&format!(
                    "http://10.132.0.36:8123/v1/notify_parking/{}",
                    &slot
                ))
                .timeout(Duration::from_secs(3))
                .send()
                .await;
        });

        Ok(())
    }

    pub async fn register_callback(&self, moh: Option<MusicOnHold>) -> Result<bool> {
        let _ = self.stop_moh().await;
        let mut context = self.get_call_context().await?;
        let exten = context.endpoint.extension();

        let mut exten = exten;
        loop {
            let is_anonymous = exten == "anonymous";

            let tts = if is_anonymous {
                "tts://you're calling from a withheld number, you can press 1 to input a different number, or press 2 to stay in the queue".to_string()
            } else {
                format!("tts://we'll call you back on <say-as interpret-as=\"telephone\">{exten}</say-as>, you can press 1 to confirm, press 2 to input a different number, or press 3 to stay in the queue")
            };
            let digits =
                self.play_sound_and_get_dtmf(vec![tts], 5000, 5000, 1).await;
            let confirmation = digits.join("");
            if is_anonymous {
                if confirmation == "1" {
                    // do nothing so that it continues down to the input number bit
                } else {
                    // go back to queue by default
                    if let Some(moh) = moh.as_ref() {
                        let _ = self.play_moh(moh).await;
                    }
                    return Ok(false);
                }
            } else {
                // not anonymous
                if confirmation == "1" {
                    context.callback = Some(exten.clone());
                    self.set_call_context(&context).await?;
                    break;
                } else if confirmation == "2" {
                    // do nothing so that it continues down to the input number bit
                } else {
                    // go back to queue by default
                    if let Some(moh) = moh.as_ref() {
                        let _ = self.play_moh(moh).await;
                    }
                    return Ok(false);
                }
            }

            if self.is_hangup().await {
                return Ok(false);
            }

            let digits = self
                .play_sound_and_get_dtmf(
                    vec![format!("tts://you can input your new number now")],
                    5000,
                    5000,
                    100,
                )
                .await;
            exten = digits.join("");

            if self.is_hangup().await {
                return Ok(false);
            }
        }

        // set media to inactive
        // so that the media server doesn't send rtp packets out
        let _ = self.set_media_mode(MediaMode::Inactive).await;
        self.hangup_endpoint(None).await?;
        self.delete_channel_user().await?;

        Ok(true)
    }

    #[async_recursion::async_recursion]
    pub async fn park(&self, slot: String, from_refer: bool) -> Result<String> {
        if let Some(peer) = self.get_peer().await {
            if let Ok(peer_context) = peer.get_call_context().await {
                if let Some(transfer) = peer_context.transfer.as_ref() {
                    match &transfer.peer {
                        TransferPeer::Initiator { .. } => {}
                        TransferPeer::Recipient { from_channel, .. } => {
                            let from_channel = Channel::new(from_channel.clone());
                            return from_channel.park(slot, from_refer).await;
                        }
                    }
                }
            }
        }

        if !REDIS
            .setnx(&format!("nebula:park:{}", slot), &self.id)
            .await?
        {
            if !from_refer {
                self.play_sound(
                    None,
                    vec!["tts://parking slot already taken".to_string()],
                    false,
                )
                .await?;
            }
            return Err(anyhow!("parking slot already taken"));
        }

        let context = self.get_call_context().await?;

        let from = context.endpoint.extension();
        let park_uuid = nebula_utils::uuid();

        let display_name = if context.direction == CallDirection::Recipient {
            context
                .caller_id
                .as_ref()
                .map(|cli| cli.display_name.clone())
        } else {
            None
        };

        REDIS
            .hmset(
                &format!("nebula:park:{}:details", slot),
                vec![
                    ("time", &Utc::now().to_rfc3339()),
                    ("direction", &context.direction.to_string()),
                    ("from", &from),
                    ("to", &context.to),
                    ("uuid", &park_uuid),
                    ("display_name", display_name.as_deref().unwrap_or("")),
                ],
            )
            .await?;

        self.hold().await?;
        self.set_media_mode(MediaMode::Recvonly).await?;

        let mut context = self.get_call_context().await?;
        if let Some(transfer) = context.transfer.as_ref() {
            match &transfer.peer {
                TransferPeer::Initiator { to_channel, .. } => {
                    let to_channel = Channel::new(to_channel.clone());
                    let mut to_channel_context =
                        to_channel.get_call_context().await?;
                    to_channel_context.transfer = None;
                    to_channel.set_call_context(&to_channel_context).await?;
                    let _ = SWITCHBOARD_SERVICE
                        .switchboard_rpc
                        .hangup(
                            &SWITCHBOARD_SERVICE.config.switchboard_host,
                            to_channel.id.clone(),
                            false,
                            None,
                            None,
                            None,
                        )
                        .await;
                    if let Some(peer) = self.get_peer().await {
                        if let Some(other_peer) = to_channel.get_peer().await {
                            let _ = Bridge::unbridge_media(&peer, &other_peer).await;
                            let _ = Bridge::unbridge_media(&other_peer, &peer).await;
                        }
                    }
                    if let Some(peer) = self.get_peer().await {
                        let _ = Bridge::bridge_media(&peer, self).await;
                        let _ = Bridge::bridge_media(self, &peer).await;
                    }
                }
                TransferPeer::Recipient { .. } => {}
            }
        }
        context.refer = Some(ReferContext {
            direction: CallDirection::Initiator,
            peer: "".to_string(),
            slot: Some(slot),
            no_recording: false,
        });
        context.transfer = None;
        self.set_call_context(&context).await?;
        SWITCHBOARD_SERVICE
            .proxy_rpc
            .hangup(
                &context.proxy_host,
                self.id.clone(),
                None,
                None,
                None,
                self.is_answered().await,
            )
            .await?;
        self.delete_channel_user().await?;
        let _ = self.remove_in_wait_lock().await;
        Ok(park_uuid)
    }

    /**
    Here, we have now created a new channel to handle the user dialing
        the retrive short code.
    We now create a dummy channel to link the new call to the previously
        parked call.



    */
    pub async fn retrieve(&self, slot: String) -> Result<()> {
        let park_channel_id: String = REDIS
            .get(&format!("nebula:park:{}", slot))
            .await
            .unwrap_or_else(|_| "".to_string());
        if park_channel_id.is_empty() {
            info!(
                channel = self.id,
                "[{}] start to play slot not found", self.id
            );
            let _ = self
                .play_sound(None, vec!["tts://slot not found ".to_string()], true)
                .await;
            return Err(anyhow!("slot doesn't have call"));
        }
        let park_uuid: String = REDIS
            .hget(&format!("nebula:park:{}:details", slot), "uuid")
            .await
            .unwrap_or("".to_string());
        REDIS.del(&format!("nebula:park:{}", slot)).await?;
        REDIS.del(&format!("nebula:park:{}:details", slot)).await?;

        let _ = self.publish_park(park_uuid, slot.clone(), false).await;

        let bridge_id = nebula_utils::uuid();
        REDIS
            .hset(&self.redis_key(), "bridge_id", &bridge_id)
            .await?;
        let bridge = Bridge::new(bridge_id);
        bridge.set_inbound(&self.id).await?;

        let park_channel = Channel::new(park_channel_id);
        let mut park_channel_context = park_channel.get_call_context().await?;
        park_channel.unhold().await?;

        if let Some(peer) = park_channel.get_peer().await {
            if let Ok(peer_contex) = peer.get_call_context().await {
                let mut context = self.get_call_context().await?;
                context.answer_caller_id = Some(
                    internal_caller_id(
                        &peer_contex,
                        context.endpoint.user().and_then(|u| u.cc.clone()),
                        None,
                    )
                    .await,
                );
                self.set_call_context(&context).await?;
            }
        }

        let new_channel = Channel::new(nebula_utils::uuid());
        let new_context = CallContext {
            endpoint: park_channel_context.endpoint.clone(),
            proxy_host: "".to_string(),
            inter_tenant: None,
            api_addr: None,
            location: None,
            to: "".to_string(),
            to_internal_number: None,
            direction: CallDirection::Recipient,
            set_caller_id: None,
            answer_caller_id: None,
            caller_id: None,
            call_queue_id: None,
            call_queue_name: None,
            call_confirmation: None,
            call_flow_id: None,
            call_flow_name: None,
            call_flow_show_original_callerid: None,
            call_flow_moh: None,
            call_flow_custom_callerid: None,
            parent_call_flow_name: None,
            parent_call_flow_show_original_callerid: None,
            parent_call_flow_moh: None,
            parent_call_flow_custom_callerid: None,
            intra_peer: None,
            intra_chain: Vec::new(),
            redirect_peer: None,
            reply_to: None,
            started_at: Utc::now(),
            answred_at: None,
            refer: Some(ReferContext {
                direction: CallDirection::Recipient,
                peer: park_channel.id.clone(),
                slot: Some(slot.clone()),
                no_recording: false,
            }),
            transfer: None,
            cdr: None,
            backup: None,
            backup_invite: None,
            recording: None,
            forward: false,
            click_to_dial: None,
            callflow: None,
            callback: None,
            callback_peer: None,
            room: "".to_string(),
            remove_call_restrictions: false,
            ziron_tag: None,
            diversion: None,
            pai: None,
            remote_ip: None,
        };
        REDIS
            .hmset(
                &format!("nebula:channel:{}", &new_channel.id),
                vec![
                    ("bridge_id", &bridge.id),
                    ("inbound_channel", &self.id),
                    ("call_context", &serde_json::to_string(&new_context)?),
                ],
            )
            .await?;
        REDIS
            .sadd(
                &format!("nebula:bridge:{}:channels", &bridge.id),
                &new_channel.id,
            )
            .await?;

        park_channel_context.refer = Some(ReferContext {
            direction: CallDirection::Initiator,
            peer: new_channel.id.clone(),
            slot: Some(slot.clone()),
            no_recording: false,
        });
        park_channel.set_call_context(&park_channel_context).await?;
        park_channel.reset_refer_sdp(&new_channel).await?;

        let sdp = new_channel.get_remote_sdp().await?;
        new_channel.handle_answer(&sdp).await?;

        if let Some(park_inbound_channel) = park_channel.get_bridge_channel().await {
            self.update_cdr(CdrItemChange {
                parent: park_inbound_channel
                    .get_cdr()
                    .await
                    .map(|cdr| cdr.uuid.to_string()),
                ..Default::default()
            })
            .await?;
            park_inbound_channel
                .update_cdr(CdrItemChange {
                    child: self.get_cdr().await.map(|cdr| cdr.uuid.to_string()),
                    ..Default::default()
                })
                .await?;
        }

        let context = self.get_call_context().await?;

        // Need the check against the user of the current channel
        if let Some(user) = context.endpoint.user() {
            // set the recording context here based on the recording setting
            //  on the user
            //  so that when channel do `start_recording`,
            //  it will have the correct context.recording
            //
            // The reason we need to do it here for call parking,
            //  and didn't need to do it for blind transfer,
            //  is because for blind transfer, the new channel for the
            //  blind transfer is A Leg and it will go through the switchboard.switch(),
            //  and it will provide the correct context.recording
            //
            // But for the call parking here, the new channel is
            //  B Leg without going through the switchboard.switch(),
            //  so we need to "manually" set the recording context.
            let _ = self
                .update_recording_from_user(Some(user), None, false)
                .await;
        }

        if let Some(peer) = park_channel.get_peer().await {
            self.update_callerid(&peer).await?;
        }

        bridge.wait().await?;
        Ok(())
    }

    pub async fn set_media_mode(&self, mode: MediaMode) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .set_session_media_mode(audio_id, mode.to_string())
            .await?;

        REDIS
            .hsetex(
                &format!("nebula:channel:{}", &self.id),
                "mode",
                &mode.to_string(),
            )
            .await?;
        REDIS
            .hsetex(
                &format!("nebula:channel:{}", &self.id),
                "last_mode_change",
                &std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)?
                    .as_secs()
                    .to_string(),
            )
            .await?;
        Ok(())
    }

    pub async fn ack(&self) -> Result<(), Error> {
        let context = self.get_call_context().await?;
        SWITCHBOARD_SERVICE
            .proxy_rpc
            .ack(&context.proxy_host, self.id.clone())
            .await?;
        Ok(())
    }

    pub async fn play_sound_and_get_dtmf(
        &self,
        sounds: Vec<String>,
        timeout: u64,
        digit_timeout: u64,
        max: usize,
    ) -> Vec<String> {
        let _ = self.answer().await;
        let mut event_key = "$".to_string();
        let mut digits = Vec::new();
        let sound_id = nebula_utils::uuid();
        let mut sound_ended = self
            .async_play_sound(Some(sound_id.clone()), sounds, true)
            .await
            .is_err();
        loop {
            tokio::select! {
                   Ok((new_key, dtmf)) = self.receive_event(&event_key, ChannelEvent::Dtmf) => {
            let _ =            self.stop_sound(&sound_id).await;
                       event_key = new_key;
                       if dtmf == "#" {
                           return digits;
                       }
                       println!("event_key is {}", event_key);
                       digits.push(dtmf);
                       if digits.len() >= max {
                           return digits;
                       }
                       println!("stop sound");
                       break;
                   }
                   Ok((_, end_sound_id)) = self.receive_event("$", ChannelEvent::SoundEnd), if !sound_ended => {
                       if end_sound_id == sound_id {
                           sound_ended = true;
                       }
                   }
                   _ = tokio::time::sleep(Duration::from_millis(timeout)), if sound_ended => {
                       println!("sound play timeout");
                       return digits;
                   }
                   _ = self.receive_event("0", ChannelEvent::Hangup) => {
                       return digits;
                   }
               }
        }

        loop {
            tokio::select! {
                result = self.receive_event(&event_key, ChannelEvent::Dtmf) => {
                    println!("result is {:?}", result);
                    if let Ok((new_key, dtmf)) = result {
                        println!("get dtmf {}", dtmf);
                        event_key = new_key;
                        if dtmf == "#" {
                            return digits;
                        }
                        digits.push(dtmf);
                        if digits.len() >= max {
                            return digits;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(digit_timeout)) => {
                    println!("digit timeout");
                    return digits;
                }
                _ = self.receive_event("0", ChannelEvent::Hangup) => {
                    return digits;
                }
            }
        }
    }

    pub async fn play_moh(&self, moh: &MusicOnHold) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .play_moh(self.id.clone(), audio_id, moh.clone())
            .await?;
        Ok(())
    }

    pub async fn stop_moh(&self) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .stop_moh(audio_id)
            .await?;
        Ok(())
    }

    pub async fn stop_sound(&self, sound_id: &str) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .stop_sound(sound_id, audio_id)
            .await?;
        Ok(())
    }

    pub async fn stop_record_to_file(&self, uuid: String) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .stop_record_to_file(audio_id, uuid)
            .await?;
        Ok(())
    }

    pub async fn record_to_file(&self, uuid: String) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .record_to_file(audio_id, uuid)
            .await?;
        Ok(())
    }

    pub async fn upload_record_to_file(&self, uuid: String) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .upload_record_to_file(audio_id, uuid)
            .await?;
        Ok(())
    }

    pub async fn record_to_redis(
        &self,
        key: String,
        limit: usize,
        expire: u64,
    ) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .record_to_redis(audio_id, key, limit, expire)
            .await?;
        Ok(())
    }

    pub async fn async_play_sound(
        &self,
        sound_id: Option<String>,
        sounds: Vec<String>,
        block: bool,
    ) -> Result<String, Error> {
        self.answer().await?;
        Self::async_play_sound_to_channel(&self.id, sound_id, sounds, 1, block).await
    }

    pub async fn play_sound(
        &self,
        sound_id: Option<String>,
        sounds: Vec<String>,
        block: bool,
    ) -> Result<String, Error> {
        self.answer().await?;
        Self::play_sound_to_channel(&self.id, sound_id, sounds, 1, block).await
    }

    pub async fn async_play_sound_to_channel(
        channel: &str,
        sound_id: Option<String>,
        sounds: Vec<String>,
        repeat: u32,
        block: bool,
    ) -> Result<String, Error> {
        let channel = Channel::new(channel.to_string());
        let audio_id = channel.get_audio_id().await?;
        let sound_id = sound_id.unwrap_or(nebula_utils::uuid());
        channel
            .new_event(ChannelEvent::PlaySound, &sound_id)
            .await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .play_sound(
                sound_id.clone(),
                channel.id.clone(),
                audio_id,
                sounds,
                false,
                repeat,
                block,
            )
            .await?;
        Ok(sound_id)
    }

    pub async fn play_sound_to_channel(
        channel: &str,
        sound_id: Option<String>,
        sounds: Vec<String>,
        repeat: u32,
        block: bool,
    ) -> Result<String, Error> {
        let sound_id = Self::async_play_sound_to_channel(
            channel, sound_id, sounds, repeat, block,
        )
        .await?;
        let channel = Channel::new(channel.to_string());
        let mut event_key = "0".to_string();
        loop {
            tokio::select! {
                Ok((new_key, end_sound_id)) = channel.receive_event(&event_key, ChannelEvent::SoundEnd) => {
                    event_key = new_key;
                    if end_sound_id == sound_id {
                        break;
                    }
                }
                _ = channel.receive_event("0", ChannelEvent::Hangup) => {
                    break;
                }
            }
        }
        println!("play sound done");
        Ok(sound_id)
    }

    pub async fn set_session_rtpmap(
        &self,
        port: u16,
        rtpmap: &Rtpmap,
    ) -> Result<(), Error> {
        let key = format!("nebula:session:{}:rtpmap", port);
        REDIS.set(&key, &serde_json::to_string(rtpmap)?).await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .update_sessoin(port)
            .await?;
        Ok(())
    }
    pub async fn next_event(
        &self,
        key_id: &str,
    ) -> Result<(String, String, String)> {
        let stream = format!("nebula:channel:{}:stream", &self.id);
        let key_id = if key_id == "$" {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_millis()
                .to_string()
        } else {
            key_id.to_string()
        };
        REDIS.xread_next_entry_timeout(&stream, &key_id, 1000).await
    }

    pub async fn stream_start_send(&self) -> Result<()> {
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .stream_start_sending(audio_id)
            .await?;
        Ok(())
    }

    pub async fn receive_event_ignore_hangup(
        &self,
        key_id: &str,
        event_name: ChannelEvent,
    ) -> Result<(String, String)> {
        let event_name = event_name.to_string();
        let mut key_id = key_id.to_string();
        loop {
            if let Ok((new_key, event, value)) = self.next_event(&key_id).await {
                key_id = new_key;
                if event == event_name {
                    return Ok((key_id, value));
                }
            }
        }
    }

    pub async fn receive_event(
        &self,
        key_id: &str,
        event_name: ChannelEvent,
    ) -> Result<(String, String)> {
        let event_name = event_name.to_string();
        let mut key_id = key_id.to_string();
        loop {
            if let Ok((new_key, event, value)) = self.next_event(&key_id).await {
                key_id = new_key;
                if event == event_name {
                    return Ok((key_id, value));
                }
            } else if self.is_hangup().await {
                // check if the channel is hangup if there's no event during the last second
                return Err(anyhow!("channel is hangup"));
            }
        }
    }

    pub async fn channel_new_event(
        channel_id: &str,
        event: ChannelEvent,
        value: &str,
    ) -> Result<(), Error> {
        REDIS
            .xadd(
                &format!("nebula:channel:{}:stream", channel_id),
                &event.to_string(),
                value,
            )
            .await?;
        Ok(())
    }

    pub async fn new_event(
        &self,
        event: ChannelEvent,
        value: &str,
    ) -> Result<(), Error> {
        Self::channel_new_event(&self.id, event, value).await
    }

    pub fn redis_key(&self) -> String {
        format!("nebula:channel:{}", &self.id)
    }

    fn session_redis_key(&self, port: u16) -> String {
        format!("nebula:session:{}", port)
    }

    pub async fn set_local_sdp(
        &self,
        sdp: &SessionDescription,
        sdp_type: SdpType,
    ) -> Result<()> {
        self.set_sdp(SessionDescKind::Local, sdp, sdp_type).await
    }

    pub async fn set_remote_sdp(
        &self,
        sdp: &SessionDescription,
        sdp_type: SdpType,
    ) -> Result<()> {
        self.set_sdp(SessionDescKind::Remote, sdp, sdp_type).await
    }

    pub async fn set_sdp(
        &self,
        kind: SessionDescKind,
        sdp: &SessionDescription,
        sdp_type: SdpType,
    ) -> Result<()> {
        REDIS
            .hset(&self.redis_key(), &kind.to_string(), &sdp.to_string())
            .await?;
        REDIS
            .hset(&self.redis_key(), &sdp_type.to_string(), &sdp.to_string())
            .await?;

        if sdp_type == SdpType::Answer {
            let other_kind = match kind {
                SessionDescKind::Local => SessionDescKind::Remote,
                SessionDescKind::Remote => SessionDescKind::Local,
            };
            let other_sdp = self.get_sdp(other_kind).await?;
            let (local_sdp, remote_sdp) = match kind {
                SessionDescKind::Local => (sdp, &other_sdp),
                SessionDescKind::Remote => (&other_sdp, sdp),
            };

            let (is_user, room) = if let Ok(context) = self.get_call_context().await
            {
                (context.endpoint.user().is_some(), context.room.clone())
            } else {
                (false, "".to_string())
            };

            for (i, media) in local_sdp.medias().iter().enumerate() {
                let id = media.uuid(&self.id).to_string();
                let key = format!("nebula:media_stream:{}", id);
                REDIS
                    .hmset(
                        &key,
                        vec![
                            (&MediaDescKind::Local.to_string(), &media.to_string()),
                            (
                                &(if kind == SessionDescKind::Local {
                                    SdpType::Answer
                                } else {
                                    SdpType::Offer
                                })
                                .to_string(),
                                &media.to_string(),
                            ),
                            ("channel", &self.id),
                            ("is_user", if is_user { "yes" } else { "no" }),
                            ("room", &room),
                        ],
                    )
                    .await?;
                let _ = REDIS.expire(&key, 86400).await;
                println!("set {} {}", &id, &MediaDescKind::Local);

                if let Some(remote_media) = if let Some(mid) = media.mid() {
                    remote_sdp.mid(mid)
                } else {
                    remote_sdp.media(i)
                } {
                    let addr = format!(
                        "{}:{}",
                        remote_sdp.connection.addr, remote_media.port
                    );
                    REDIS
                        .hset(
                            &key,
                            &MediaDescKind::Remote.to_string(),
                            &remote_media.to_string(),
                        )
                        .await?;
                    REDIS
                        .hset(
                            &key,
                            &(if kind == SessionDescKind::Remote {
                                SdpType::Answer
                            } else {
                                SdpType::Offer
                            })
                            .to_string(),
                            &remote_media.to_string(),
                        )
                        .await?;
                    REDIS.hsetnx(&key, "peer_addr", &addr).await?;
                    println!("set {} {}", &id, &MediaDescKind::Remote);
                }
            }

            let local_tracks = local_sdp.media_stream_tracks();
            let remote_tracks = remote_sdp.media_stream_tracks();
            for (mid, remote_track) in remote_tracks {
                if let Some(local_track) = local_tracks.get(&mid) {
                    let id = format!("{}:{}", self.id, mid);
                    let uuid = uuid_v5(&id);
                    let id = uuid.to_string();
                    let key = format!("nebula:media_stream:{}", &id);
                    let ssrc = if remote_track.ssrc == 0 {
                        uuid.as_fields().0
                    } else {
                        remote_track.ssrc
                    };
                    println!(
                        "set ssrc {ssrc} for stream {id}, remote track ssrc {}",
                        remote_track.ssrc
                    );
                    let _ = REDIS.hset(&key, "ssrc", &ssrc.to_string()).await;
                    let _ = REDIS
                        .hset(&key, "local_ssrc", &local_track.ssrc.to_string())
                        .await;
                }
            }

            let local_ice = local_sdp.ice();
            let remote_ice = remote_sdp.ice();

            if let (Some(local_ice), Some(remote_ice)) = (local_ice, remote_ice) {
                let ice_username =
                    format!("{}:{}", &local_ice.ufrag, &remote_ice.ufrag);
                let key = format!("nebula:ice:{}", ice_username);
                REDIS.set(&key, &self.id).await?;
            }
        }

        Ok(())
    }

    fn local_sdp_lock(&self) -> DistributedMutex {
        DistributedMutex::new(format!("nebula:channel:{}:local_sdp_lock", &self.id))
    }

    pub async fn get_local_sdp(&self) -> Result<SessionDescription> {
        self.get_sdp(SessionDescKind::Local).await
    }

    pub async fn get_remote_sdp(&self) -> Result<SessionDescription> {
        self.get_sdp(SessionDescKind::Remote).await
    }

    pub async fn get_sdp(
        &self,
        kind: SessionDescKind,
    ) -> Result<SessionDescription> {
        let str: String = REDIS
            .hget(&self.redis_key(), &kind.to_string())
            .await
            .context("Failed to get sdp")?;
        Ok(SessionDescription::from_str(&str).context("Failed to get sdp")?)
    }

    pub async fn get_answer_sdp(&self) -> Result<SessionDescription> {
        self.get_sdp_by_directon(SdpType::Answer).await
    }

    pub async fn get_offer_sdp(&self) -> Result<SessionDescription> {
        self.get_sdp_by_directon(SdpType::Offer).await
    }

    pub async fn get_sdp_by_directon(
        &self,
        sdp_type: SdpType,
    ) -> Result<SessionDescription> {
        let str: String = REDIS
            .hget(&self.redis_key(), &sdp_type.to_string())
            .await
            .context("Failed to get sdp")?;
        SessionDescription::from_str(&str).context("Failed to parse sdp")
    }

    async fn stop_ice(&self) -> Result<()> {
        let local_sdp = self.get_local_sdp().await?;
        let remote_sdp = self.get_remote_sdp().await?;
        let local_ice = local_sdp.ice().ok_or(anyhow!("no ice"))?;
        let remote_ice = remote_sdp.ice().ok_or(anyhow!("no ice"))?;
        let ice_id =
            format!("{}:{}:{}", &self.id, &local_ice.ufrag, &remote_ice.ufrag);
        let _ = SWITCHBOARD_SERVICE.media_rpc_client.stop_ice(ice_id).await;
        let ice_username = format!("{}:{}", &local_ice.ufrag, &remote_ice.ufrag);
        REDIS
            .del_if_value(&format!("nebula:ice:{}", ice_username), &self.id)
            .await?;
        Ok(())
    }

    async fn stop_media(&self, hangup: bool) -> Result<()> {
        let sdp = self.get_local_sdp().await?;
        for media in sdp.medias() {
            let id = media.uuid(&self.id).to_string();
            let _ = REDIS.del(&format!("nebula:media_stream:{}", &id)).await;
            SWITCHBOARD_SERVICE
                .media_rpc_client
                .stop_media_stream(id.clone(), "channel hangup".to_string(), hangup)
                .await?;
            if media.port != PEER_CONNECTION_PORT {
                REDIS.del(&format!("nebula:session:{}", media.port)).await?;
            }
            if let Ok(sources) = REDIS
                .smembers::<Vec<String>>(&format!(
                    "nebula:media_stream:{}:sources",
                    &id
                ))
                .await
            {
                for source in sources {
                    let _ = REDIS
                        .srem(
                            &format!("nebula:media_stream:{}:destinations", source),
                            &id,
                        )
                        .await;
                    let _ = SWITCHBOARD_SERVICE
                        .media_rpc_client
                        .update_video_destination_layer(
                            source.clone(),
                            id.clone(),
                            None,
                        )
                        .await;
                }
            }
            let _ = REDIS
                .del(&format!("nebula:media_stream:{}:sources", &id))
                .await;
            let _ = REDIS
                .del(&format!("nebula:media_stream:{}:destinations", &id))
                .await;
        }
        let _ = self.stop_ice().await;
        Ok(())
    }

    pub async fn get_video_id(&self) -> Result<String> {
        let sdp = self.get_local_sdp().await?;
        let id = sdp
            .video()
            .ok_or(anyhow!("no video in sdp"))?
            .uuid(&self.id)
            .to_string();
        Ok(id)
    }

    pub async fn get_audio_id(&self) -> Result<String> {
        let sdp = self.get_local_sdp().await?;
        let id = sdp
            .audio()
            .ok_or(anyhow!("no audio in sdp"))?
            .uuid(&self.id)
            .to_string();
        Ok(id)
    }

    #[async_recursion]
    pub async fn ring(&self, ringtone: String) -> Result<()> {
        println!("now ring {}", ringtone);
        let context = self.get_call_context().await?;

        if let Some(TransferPeer::Initiator { to_channel, .. }) =
            context.transfer.map(|t| t.peer)
        {
            let mut peer_has_183 = false;
            if let Some(other_peer) =
                Channel::new(to_channel.to_string()).get_peer().await
            {
                if let Some(peer) = self.get_peer().await {
                    let ringing = REDIS
                        .hget(&format!("nebula:channel:{}", peer.id), "ringing")
                        .await
                        .unwrap_or(0);
                    peer_has_183 = ringing == 183;
                    if peer_has_183 {
                        let _ = Bridge::bridge_media(&peer, &other_peer).await;
                    }
                }

                if !peer_has_183 {
                    other_peer.ring("".to_string()).await?;
                }
            }
            return Ok(());
        }

        if ringtone.is_empty() {
            if let Endpoint::User { .. } = &context.endpoint {
                if context.refer.is_none()
                    && context.redirect_peer.is_none()
                    && !self.is_answered().await
                {
                    SWITCHBOARD_SERVICE
                        .proxy_rpc
                        .set_response(
                            &context.proxy_host,
                            self.id.clone(),
                            180,
                            "Ringing".to_string(),
                            "".to_string(),
                        )
                        .await?;
                    return Ok(());
                }
            }
        }

        if self.is_answered().await {
        } else {
            let answer_sdp = if let Ok(sdp) = self.get_local_sdp().await {
                sdp
            } else {
                self.create_answer_sdp(false).await?
            };
            if let Some(intra) = context.intra_peer {
                let _ = SWITCHBOARD_SERVICE
                    .proxy_rpc
                    .intra_session_progress(
                        &context.proxy_host,
                        intra,
                        answer_sdp.to_string(),
                    )
                    .await;
            } else if let Some(redirect) = context.redirect_peer {
                let redirect_channel = Channel::new(redirect);
                redirect_channel
                    .handle_response(&Response {
                        id: "".to_string(),
                        code: 180,
                        status: "Ringing".to_string(),
                        body: answer_sdp.to_string(),
                        remote_ip: None,
                    })
                    .await?;
            } else {
                SWITCHBOARD_SERVICE
                    .proxy_rpc
                    .set_response(
                        &context.proxy_host,
                        self.id.clone(),
                        183,
                        "Session Progress".to_string(),
                        answer_sdp.to_string(),
                    )
                    .await?;
            }
        }

        let (sounds, random) = self.ringtone_sounds(&ringtone).await;
        let audio_id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .play_ringtone(
                ringtone.clone(),
                self.id.clone(),
                audio_id,
                sounds,
                random,
            )
            .await?;

        Ok(())
    }

    async fn ringtone_sounds(&self, ringtone: &str) -> (Vec<String>, bool) {
        if ringtone.is_empty() {
            return (
                vec![
                    "tone://400,200,400,450".to_string(),
                    "tone://400,2000,400,450".to_string(),
                ],
                false,
            );
        }
        let sound = SWITCHBOARD_SERVICE.db.get_sound(ringtone).await;
        match sound {
            Some(sound) => {
                return (vec![sound.uuid.clone()], false);
            }
            None => {
                let moh = SWITCHBOARD_SERVICE.db.get_music_on_hold(ringtone).await;
                if let Some(moh) = moh {
                    return (moh.sounds, moh.random);
                }
            }
        }

        (vec![], false)
    }

    async fn remote_audio_rtpmaps(&self) -> Result<Vec<Rtpmap>> {
        let remote_sdp = self.get_remote_sdp().await?;
        let remote_audio = remote_sdp.audio().ok_or(anyhow!("no audio"))?;
        let mut rtpmaps = Vec::new();
        for remote_pt in &remote_audio.payloads {
            if let Some(remote_rtpmap) = remote_audio.pt_rtpmap(remote_pt) {
                if remote_rtpmap.name != PayloadType::TelephoneEvent {
                    rtpmaps.push(remote_rtpmap);
                }
            }
        }
        println!("remote rtpmaps are {:?}", rtpmaps);
        Ok(rtpmaps)
    }

    async fn negotiate_audio_rtpmap(&self) -> Option<Rtpmap> {
        let offer_sdp = self.get_offer_sdp().await.ok()?;
        let answer_sdp = self.get_answer_sdp().await.ok()?;
        offer_sdp.audio()?.negotiate_rtpmap(answer_sdp.audio()?)
    }

    pub async fn session_progress(&self, src_channel: &Channel) -> Result<()> {
        let context = self.get_call_context().await?;
        match &context.endpoint {
            Endpoint::User { .. } | Endpoint::Trunk { .. } => {
                if context.refer.is_some() || self.is_answered().await {
                    self.ring("".to_string()).await?;
                    return Ok(());
                }
                let answer_sdp = if let Ok(sdp) = self.get_local_sdp().await {
                    sdp
                } else {
                    self.create_answer_sdp(false).await?
                };
                Bridge::bridge_media(src_channel, self).await?;
                SWITCHBOARD_SERVICE
                    .proxy_rpc
                    .set_response(
                        &context.proxy_host,
                        self.id.clone(),
                        183,
                        "Session Progress".to_string(),
                        answer_sdp.to_string(),
                    )
                    .await?;
            }
            _ => {
                self.ring("".to_string()).await?;
            }
        }
        Ok(())
    }

    pub async fn set_answered(&self) -> Result<()> {
        REDIS
            .hset(
                &self.redis_key(),
                "last_rtp_packet",
                &std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)?
                    .as_secs()
                    .to_string(),
            )
            .await?;
        REDIS.hset(&self.redis_key(), "answered", "yes").await?;
        self.new_event(ChannelEvent::Answer, "answer").await?;

        let _ = self.start_recording().await;

        let mut context = self.get_call_context().await?;

        context.answred_at = Some(Utc::now());
        self.set_call_context(&context).await?;

        if let Endpoint::User { user, .. } = &context.endpoint {
            notify_user_presence(&user.uuid);
        }

        Ok(())
    }

    pub async fn is_answered(&self) -> bool {
        is_channel_answered(&self.id).await
    }

    async fn get_peer_remote_sdp(&self) -> Option<SessionDescription> {
        let peer = self.get_peer().await?;
        peer.get_remote_sdp().await.ok()
    }

    pub async fn create_answer_sdp(
        &self,
        is_room: bool,
    ) -> Result<SessionDescription> {
        let context = self.get_call_context().await;
        let internal_sdp = context
            .as_ref()
            .map(|c| c.endpoint.is_internal_sdp())
            .unwrap_or(true);
        let remote_sdp = self.get_remote_sdp().await?;
        let peer_remote_sdp = self.get_peer_remote_sdp().await;

        let answer_sdp = remote_sdp
            .create_answer(
                SWITCHBOARD_SERVICE.config.media_ip,
                &self.id,
                peer_remote_sdp.as_ref(),
                internal_sdp,
                is_room,
            )
            .await?;
        if let Some(intra_peer) =
            context.as_ref().ok().and_then(|c| c.intra_peer.clone())
        {
            self.set_local_rtp_peer(&intra_peer, &answer_sdp, &remote_sdp)
                .await?;
        }
        if let Some(redirect_peer) =
            context.ok().and_then(|c| c.redirect_peer.clone())
        {
            self.set_local_rtp_peer(&redirect_peer, &answer_sdp, &remote_sdp)
                .await?;
        }
        self.set_local_sdp(&answer_sdp, SdpType::Answer)
            .await
            .context("error in set local sdp")?;

        Ok(answer_sdp)
    }

    async fn set_local_rtp_peer(
        &self,
        peer_channel_id: &str,
        local_sdp: &SessionDescription,
        peer_local_sdp: &SessionDescription,
    ) -> Result<()> {
        let local_audio = local_sdp
            .audio()
            .ok_or(anyhow!("local sdp doesn't have audio"))?;
        let peer_audio = peer_local_sdp
            .audio()
            .ok_or(anyhow!("peer local sdp doesn't have audio"))?;
        let audio_id = local_audio.uuid(&self.id).to_string();
        let peer_audio_id = peer_audio.uuid(peer_channel_id).to_string();

        REDIS
            .hmset(
                &format!("nebula:media_stream:{}", &audio_id),
                vec![("peer", &peer_audio_id), ("channel", &self.id)],
            )
            .await?;
        REDIS
            .hmset(
                &format!("nebula:media_stream:{}", &peer_audio_id),
                vec![("peer", &audio_id), ("channel", peer_channel_id)],
            )
            .await?;
        Ok(())
    }

    pub async fn stop_ringtone(&self) -> Result<()> {
        let id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .stop_ringtone(id)
            .await?;
        Ok(())
    }

    pub async fn set_fax(&self) -> Result<()> {
        let id = self.get_audio_id().await?;
        SWITCHBOARD_SERVICE.media_rpc_client.set_fax(id).await?;
        Ok(())
    }

    pub async fn answer(&self) -> Result<()> {
        if self.is_answered().await {
            return Ok(());
        }

        let answer_sdp = if let Ok(sdp) = self.get_local_sdp().await {
            sdp
        } else {
            self.create_answer_sdp(false).await?
        };

        self.update_cdr(CdrItemChange {
            answer: Some(Utc::now()),
            disposition: Some("answered".to_string()),
            ..Default::default()
        })
        .await?;
        info!(channel = self.id, "channel answer");
        let context = self.get_call_context().await?;
        if let Some(intra) = context.intra_peer {
            // let intra_channel = Channel::new(intra);
            // intra_channel.handle_answer(&answer_sdp).await?;
            let _ = SWITCHBOARD_SERVICE
                .proxy_rpc
                .answer_intra(&context.proxy_host, intra, answer_sdp.to_string())
                .await;
        } else if let Some(redirect) = context.redirect_peer {
            // let redirect_channel = Channel::new(redirect);
            // redirect_channel.handle_answer(&answer_sdp).await?;
            let _ = SWITCHBOARD_SERVICE
                .switchboard_rpc
                .answer(
                    &SWITCHBOARD_SERVICE.config.switchboard_host,
                    redirect,
                    answer_sdp.to_string(),
                    None,
                    None,
                    None,
                )
                .await;
        } else if let Some(refer) = context.refer {
            let refer_channel = Channel::new(refer.peer.clone());
            refer_channel
                .set_remote_sdp(&answer_sdp, SdpType::Answer)
                .await?;
        } else if let Some(TransferPeer::Recipient { .. }) =
            context.transfer.as_ref().map(|t| &t.peer)
        {
            // Channel::new(from_channel)
            //     .set_remote_sdp(&answer_sdp, SdpType::Answer)
            //     .await?;
        } else {
            let _ = REDIS.hsetex(&self.redis_key(), "expect_ack", "yes").await;

            let answer_sdp = if context
                .transfer
                .as_ref()
                .map(|t| {
                    if let TransferPeer::Initiator { empty_sdp, .. } = &t.peer {
                        *empty_sdp
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
            {
                "".to_string()
            } else {
                answer_sdp.to_string()
            };

            SWITCHBOARD_SERVICE
                .proxy_rpc
                .answer(
                    &context.proxy_host,
                    self.id.clone(),
                    answer_sdp.clone(),
                    context.answer_caller_id.clone(),
                    context.cdr.map(|cdr| cdr.to_string()),
                )
                .await?;

            let channel = self.clone();
            let answer_caller_id = context.answer_caller_id.clone();
            let answer_cdr_uuid = context.cdr.map(|cdr| cdr.to_string());

            let proxy_host = context.proxy_host.clone();
            tokio::spawn(async move {
                let mut retries = 0;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {
                            let _ = SWITCHBOARD_SERVICE
                                .proxy_rpc
                                .answer(
                                    &proxy_host,
                                    channel.id.clone(),
                                    answer_sdp.clone(),
                                    answer_caller_id.clone(),
                                    answer_cdr_uuid.clone(),
                                )
                                .await;
                            retries += 1;
                            if retries > 10 {
                                return;
                            }
                        }
                        _ = channel.receive_event("0", ChannelEvent::AnswerAck) => {
                            return;
                        }
                        _ = channel.receive_event("0", ChannelEvent::Hangup) => {
                            return;
                        }
                    }
                }
            });
        }

        self.set_answered().await?;

        if let Some(number) = context.to_internal_number.as_ref() {
            if number.limit > 0 {
                let channel = self.clone();
                let number = number.to_owned();
                tokio::spawn(async move {
                    channel.monitor_inbound_limit(&number).await;
                });
            }
        }

        Ok(())
    }

    pub async fn get_inbound_channel(&self) -> Option<Channel> {
        let inbound_channel_id: String = REDIS
            .hget(&format!("nebula:channel:{}", &self.id), "inbound_channel")
            .await
            .unwrap_or("".to_string());
        if inbound_channel_id.is_empty() {
            None
        } else {
            Some(Channel {
                id: inbound_channel_id,
            })
        }
    }

    pub async fn handle_response(&self, resp: &Response) -> Result<()> {
        if resp.code == 180 || resp.code == 183 {
            self.new_event(ChannelEvent::Ring, "ring").await?;
        }
        let mut resp = resp.clone();
        if resp.code == 183 && resp.body.is_empty() {
            // if resp code is 183 but no body, we treat it as 180
            resp.code = 180;
        }

        if resp.code == 180 {
            // set ringing code to 180 if it's 180 ringing
            let _ = REDIS
                .hsetex(
                    &format!("nebula:channel:{}", &self.id),
                    "ringing",
                    &resp.code.to_string(),
                )
                .await;
        }
        if resp.code == 183 {
            if self.get_remote_sdp().await.is_err() {
                let mut sdp = SessionDescription::from_str(&resp.body)?;
                let is_ip_global = Ipv4Addr::from_str(&sdp.connection.addr)
                    .map(|ip| is_ip_global(&ip))
                    .unwrap_or(false);
                if !is_ip_global {
                    if let Some(remote_ip) = &resp.remote_ip {
                        sdp.connection.addr = remote_ip.clone();
                    }
                }
                self.set_remote_sdp(&sdp, SdpType::Answer).await?;
            }
            // only set ringing code to 183 if it's got sdp
            let _ = REDIS
                .hsetex(
                    &format!("nebula:channel:{}", &self.id),
                    "ringing",
                    &resp.code.to_string(),
                )
                .await;
        }
        let bridge = self.get_bridge().await?;
        bridge.handle_response(self.clone(), &resp).await?;
        Ok(())
    }

    async fn monitor_inbound_limit(&self, number: &Number) {
        let key = format!("nebula:number:inbound_limit:{}", number.e164);
        let _ = REDIS.setexnx(&key, "0", 86400).await;
        let mut last_tick = std::time::Instant::now();

        let mut ticker = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let millis = REDIS.incr_by(&key, last_tick.elapsed().as_millis() as usize).await.unwrap_or(0);
                    last_tick = std::time::Instant::now();
                    if millis >= number.limit * 1000 {
                        self.notify_inbound_limit(number, millis).await;
                        let _ = self.hangup(None).await;
                        return;
                    }
                }
                _ = self.receive_event("0", ChannelEvent::Hangup) => {
                    let _ = REDIS.incr_by(&key, last_tick.elapsed().as_millis() as usize).await;
                    return;
                }
            }
        }
    }

    pub async fn notify_inbound_limit(&self, number: &Number, millis: i64) {
        info!(channel = self.id, "call end due to inbound limit, limit is {}, {} seconds answered in the past 24 hours", number.limit, millis/1000);
        let _ = SWITCHBOARD_SERVICE
            .yaypi_client
            .notify_inbound_limit(&number.uuid)
            .await;
    }

    pub async fn monitor_outbound(&self, number: &str) -> Result<()> {
        let inbound = self
            .get_inbound_channel()
            .await
            .unwrap_or_else(|| self.clone());

        let number = phonenumber::parse(None, number)?;
        let e164 = number.format().mode(phonenumber::Mode::E164).to_string();
        let country_code = number.country().value();
        let national_number = number.national().value();
        let yaypi_client = YaypiClient::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(60));

        let inbound_context = inbound.get_call_context().await?;
        let cdr_uuid =
            if let Some(redirect) = redirect_context(&inbound_context).await {
                redirect.cdr.as_ref().ok_or(anyhow!("no cdr"))?.to_string()
            } else {
                inbound_context
                    .cdr
                    .as_ref()
                    .ok_or(anyhow!("no cdr"))?
                    .to_string()
            };

        let context = self.get_call_context().await?;
        let answered_at = context.answred_at.ok_or(anyhow!("not answered"))?;
        let start = context.started_at.timestamp() as u64;

        loop {
            ticker.tick().await;
            if self.is_hangup().await {
                return Ok(());
            }
            let duration =
                Utc::now().signed_duration_since(answered_at).num_seconds() as usize;
            let (code, resp) = yaypi_client
                .update_call(
                    &cdr_uuid,
                    duration,
                    start,
                    country_code,
                    national_number,
                    &e164,
                )
                .await
                .unwrap_or((500, serde_json::Value::Null));
            println!(
                "update duration {} outbound call {} {}",
                duration, code, resp
            );
            if let Some(cost) = resp.get("cost").and_then(|c| c.as_f64()) {
                if let Ok(cost) = BigDecimal::from_str(&cost.to_string()) {
                    let _ = inbound
                        .update_cdr(CdrItemChange {
                            cost: Some(cost),
                            ..Default::default()
                        })
                        .await;
                }
            }
            if code != 200 {
                info!(
                    channel = self.id,
                    "[{}] outbound in call fail {} {}", &self.id, code, resp
                );
                self.hangup(None).await?;
                let bridge = self.get_bridge().await?;
                bridge.channel_hangup(&self.id).await?;
                return Ok(());
            }
            let msg = resp
                .get("message")
                .and_then(|m| m.as_str())
                .map(|m| m.to_string())
                .unwrap_or_default();
            if !msg.is_empty() {
                let _ = Self::async_play_sound_to_channel(
                    &self.id,
                    None,
                    vec![format!("tts://{}", &msg)],
                    1,
                    true,
                )
                .await;
            }
        }
    }

    pub async fn check_call_monitoring(&self) -> Result<()> {
        let context = self.get_call_context().await?;
        if context.refer.is_some() {
            return Ok(());
        }
        match context.endpoint {
            Endpoint::User { user, .. } => {
                for kind in &[
                    CallMonitoring::Listen,
                    CallMonitoring::Whisper,
                    CallMonitoring::Barge,
                ] {
                    let key = &format!("nebula:monitor:{}:user:{}", kind, user.uuid);
                    let channels: Vec<String> =
                        REDIS.smembers(key).await.unwrap_or(Vec::new());
                    for channel in channels {
                        let channel = Channel::new(channel);
                        if channel.is_hangup().await {
                            let _ = REDIS.srem(key, &channel.id).await;
                        } else {
                            let _ = channel.monitor_channel(kind, self).await;
                        }
                    }
                }
            }
            _ => return Ok(()),
        }
        Ok(())
    }

    pub async fn handle_answer(&self, sdp: &SessionDescription) -> Result<()> {
        let _ = self.ack().await;

        if self.is_answered().await {
            if let Some(image) = sdp.image() {
                if let Some(peer) = self.get_peer().await {
                    let audio_id = self.get_audio_id().await?;
                    check_peer_addr(&audio_id, &sdp.connection.addr, image.port)
                        .await;

                    let context = peer.get_call_context().await?;
                    let mut answer_sdp = peer.get_local_sdp().await?;
                    let mut answer_image = image.clone();
                    if let Some(port) = answer_sdp.audio().map(|a| a.port) {
                        answer_image.port = port;
                    }
                    answer_sdp.media_descriptions = vec![answer_image];
                    SWITCHBOARD_SERVICE
                        .proxy_rpc
                        .answer(
                            &context.proxy_host,
                            peer.id.clone(),
                            answer_sdp.to_string(),
                            None,
                            None,
                        )
                        .await?;
                }
            }

            return Ok(());
        }

        if let Ok(remote_sdp) = self.get_remote_sdp().await {
            if let Some(new_audio) = sdp.audio() {
                if let Some(old_audio) = remote_sdp.audio() {
                    if new_audio.payloads != old_audio.payloads {
                        self.set_remote_sdp(sdp, SdpType::Answer).await?;
                        if let Ok(audio_id) = self.get_audio_id().await {
                            let _ = SWITCHBOARD_SERVICE
                                .media_rpc_client
                                .stop_media_stream(
                                    audio_id,
                                    "codec change".to_string(),
                                    false,
                                )
                                .await;
                        }
                    }
                }
            }
        } else {
            self.set_remote_sdp(&sdp, SdpType::Answer).await?;
        }

        self.set_answered().await?;

        println!("handle answer");
        let context = self.get_call_context().await?;
        if let Endpoint::Provider {
            provider, number, ..
        } = context.endpoint
        {
            if provider.name != "intra" {
                println!("monitor outbound");
                let channel = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = channel.monitor_outbound(&number).await {
                        println!("monitor outbound error: {e}");
                    }
                });
            }
        }

        if let Ok(bridge) = self.get_bridge().await {
            bridge.answer(self).await?;
        }
        Ok(())
    }

    pub async fn log_out_all(&self) -> Result<()> {
        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or(anyhow!("not coming from a user"))?;
        let _ = SWITCHBOARD_SERVICE
            .db
            .clear_login_users(&from_user.uuid)
            .await;
        self.play_sound(
            None,
            vec!["tts://you've successfully logged out all users".to_string()],
            true,
        )
        .await?;

        Ok(())
    }

    pub async fn login_as_user(&self, exten: i64) -> Result<()> {
        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or(anyhow!("not coming from a user"))?;
        let extension = SWITCHBOARD_SERVICE
            .db
            .get_extension(&from_user.tenant_id, exten)
            .await
            .ok_or(anyhow!("no extension"))?;
        if extension.discriminator != "sipuser" {
            return Err(anyhow!("can't listen to a non sipuser"));
        }
        let to_user = SWITCHBOARD_SERVICE
            .db
            .get_user(&from_user.tenant_id, &extension.parent_id)
            .await
            .ok_or(anyhow!("can'd find sipuser"))?;
        if from_user.uuid == to_user.uuid {
            return Err(anyhow!("can't login as yourself"));
        }

        if !from_user.login.unwrap_or(false) || !to_user.be_login.unwrap_or(false) {
            let from_groups = SWITCHBOARD_SERVICE
                .db
                .get_user_groups(&from_user.uuid)
                .await
                .unwrap_or_default();
            let to_groups = SWITCHBOARD_SERVICE
                .db
                .get_user_groups(&to_user.uuid)
                .await
                .unwrap_or_default();
            if !SWITCHBOARD_SERVICE
                .db
                .check_user_permission(
                    &from_user.tenant_id,
                    &from_user.uuid,
                    from_groups,
                    &to_user.uuid,
                    to_groups,
                    "login",
                )
                .await
            {
                return Err(anyhow!("user isn't allowed to login"));
            }
        }

        let logged_in = SWITCHBOARD_SERVICE
            .db
            .get_login_users(&to_user.uuid)
            .await
            .map(|users| users.contains(&from_user.uuid))
            .unwrap_or(false);
        if logged_in {
            SWITCHBOARD_SERVICE
                .db
                .delete_login_user(&from_user.uuid, &to_user.uuid)
                .await?;
            self.play_sound(
                None,
                vec![format!(
                    "tts://you've successfully logged out as {}",
                    to_user.display_name.unwrap_or_default()
                )],
                true,
            )
            .await?;
        } else {
            SWITCHBOARD_SERVICE
                .db
                .create_login_user(&from_user.uuid, &to_user.uuid)
                .await?;
            self.play_sound(
                None,
                vec![format!(
                    "tts://you've successfully logged in as {}",
                    to_user.display_name.unwrap_or_default()
                )],
                true,
            )
            .await?;
        }

        Ok(())
    }

    async fn get_hot_desk_extension(&self, from_user: &User) -> Option<User> {
        let digits = self
            .play_sound_and_get_dtmf(
                vec!["tts://please input the extension number".to_string()],
                3000,
                3000,
                100,
            )
            .await;
        let extension = digits.join("").parse::<i64>().ok()?;
        let extension = SWITCHBOARD_SERVICE
            .db
            .get_extension(&from_user.tenant_id, extension)
            .await?;
        if extension.discriminator != "sipuser" {
            return None;
        }
        let to_user = SWITCHBOARD_SERVICE
            .db
            .get_user(&from_user.tenant_id, &extension.parent_id)
            .await?;
        let to_user_pin = to_user.pin.clone().unwrap_or_default();
        if to_user_pin.is_empty() {
            return None;
        }
        Some(to_user)
    }

    pub async fn hot_desk_logout(&self, s: &str) -> Result<()> {
        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or(anyhow!("not coming from a user"))?;

        let mut to_user = None;
        let mut parts = s.split('#');
        if s.is_empty() {
            loop {
                let input_user = self.get_hot_desk_extension(from_user).await;
                match input_user {
                    Some(user) => {
                        to_user = Some(user);
                        break;
                    }
                    None => {
                        self.play_sound(
                            None,
                            vec!["tts://invalid extension".to_string()],
                            true,
                        )
                        .await?;
                    }
                }
                if self.is_hangup().await {
                    return Err(anyhow!("channel hangup"));
                }
            }
        } else {
            let exten = parts
                .next()
                .ok_or_else(|| anyhow!("no parts"))?
                .parse::<i64>()?;
            let extension = SWITCHBOARD_SERVICE
                .db
                .get_extension(&from_user.tenant_id, exten)
                .await
                .ok_or(anyhow!("no extension"))?;
            if extension.discriminator != "sipuser" {
                return Err(anyhow!("can't hot desk to a non sipuser"));
            }
            let extension_user = SWITCHBOARD_SERVICE
                .db
                .get_user(&from_user.tenant_id, &extension.parent_id)
                .await
                .ok_or(anyhow!("can'd find sipuser"))?;
            let to_user_pin = extension_user.pin.clone().unwrap_or_default();
            if to_user_pin.is_empty() {
                return Err(anyhow!("user doesn't have a pin"));
            }
            to_user = Some(extension_user);
        }

        let mut pin_matched = false;
        let to_user = to_user.unwrap();
        let to_user_pin = to_user.pin.unwrap_or_default();
        if to_user_pin == "#" {
            pin_matched = true;
        } else if let Some(input_pin) = parts.next() {
            if !input_pin.is_empty() {
                if input_pin != to_user_pin {
                    self.play_sound(
                        None,
                        vec!["tts://pin incorrect".to_string()],
                        true,
                    )
                    .await?;
                } else {
                    pin_matched = true;
                }
            }
        }

        if !pin_matched {
            loop {
                let digits = self
                    .play_sound_and_get_dtmf(
                        vec!["tts://please input the pin".to_string()],
                        3000,
                        3000,
                        to_user_pin.len(),
                    )
                    .await;
                let pin = digits.join("");
                if pin == to_user_pin {
                    break;
                } else {
                    self.play_sound(
                        None,
                        vec!["tts://pin incorrect".to_string()],
                        true,
                    )
                    .await?;
                }
                if self.is_hangup().await {
                    return Err(anyhow!("channel hangup"));
                }
            }
        }

        let user_agent = if let Endpoint::User { user_agent, .. } = &context.endpoint
        {
            user_agent.as_ref().map(|u| u.as_str())
        } else {
            None
        };
        SWITCHBOARD_SERVICE
            .yaypi_client
            .hot_desk_logout(&to_user.uuid, user_agent)
            .await?;
        self.play_sound(
            None,
            vec!["tts://hot desk log out successful".to_string()],
            true,
        )
        .await?;

        Ok(())
    }

    pub async fn hot_desk(&self, s: &str) -> Result<()> {
        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or(anyhow!("not coming from a user"))?;

        let mut to_user = None;
        let mut parts = s.split('#');
        if s.is_empty() {
            loop {
                let input_user = self.get_hot_desk_extension(from_user).await;
                // let input_user = match input_user {
                //     Some(user) => {
                //         if user.uuid == from_user.uuid {
                //             None
                //         } else {
                //             Some(user)
                //         }
                //     }
                //     None => None,
                // };
                match input_user {
                    Some(user) => {
                        to_user = Some(user);
                        break;
                    }
                    None => {
                        self.play_sound(
                            None,
                            vec!["tts://invalid extension".to_string()],
                            true,
                        )
                        .await?;
                    }
                }
                if self.is_hangup().await {
                    return Err(anyhow!("channel hangup"));
                }
            }
        } else {
            let exten = parts
                .next()
                .ok_or_else(|| anyhow!("no parts"))?
                .parse::<i64>()?;
            let extension = SWITCHBOARD_SERVICE
                .db
                .get_extension(&from_user.tenant_id, exten)
                .await
                .ok_or(anyhow!("no extension"))?;
            if extension.discriminator != "sipuser" {
                return Err(anyhow!("can't hot desk to a non sipuser"));
            }
            let extension_user = SWITCHBOARD_SERVICE
                .db
                .get_user(&from_user.tenant_id, &extension.parent_id)
                .await
                .ok_or(anyhow!("can'd find sipuser"))?;
            if from_user.uuid == extension_user.uuid {
                return Err(anyhow!("can't hot desk as yourself"));
            }
            let to_user_pin = extension_user.pin.clone().unwrap_or_default();
            if to_user_pin.is_empty() {
                return Err(anyhow!("user doesn't have a pin"));
            }
            to_user = Some(extension_user);
        }

        let mut pin_matched = false;
        let to_user = to_user.unwrap();
        let to_user_pin = to_user.pin.unwrap_or_default();
        if to_user_pin == "#" {
            pin_matched = true;
        } else if let Some(input_pin) = parts.next() {
            if !input_pin.is_empty() {
                if input_pin != to_user_pin {
                    self.play_sound(
                        None,
                        vec!["tts://pin incorrect".to_string()],
                        true,
                    )
                    .await?;
                } else {
                    pin_matched = true;
                }
            }
        }

        if !pin_matched {
            loop {
                let digits = self
                    .play_sound_and_get_dtmf(
                        vec!["tts://please input the pin".to_string()],
                        5000,
                        5000,
                        to_user_pin.len(),
                    )
                    .await;
                let pin = digits.join("");
                if pin == to_user_pin {
                    break;
                } else {
                    self.play_sound(
                        None,
                        vec!["tts://pin incorrect".to_string()],
                        true,
                    )
                    .await?;
                }
                if self.is_hangup().await {
                    return Err(anyhow!("channel hangup"));
                }
            }
        }

        let user_agent = if let Endpoint::User { user_agent, .. } = &context.endpoint
        {
            user_agent.as_ref().map(|u| u.as_str())
        } else {
            None
        };
        let login = SWITCHBOARD_SERVICE
            .yaypi_client
            .new_hot_desk(&from_user.uuid, &to_user.uuid, user_agent)
            .await?;
        self.play_sound(
            None,
            vec![format!(
                "tts://hot desking{} successful",
                if login { "" } else { " log out" }
            )],
            true,
        )
        .await?;

        Ok(())
    }

    pub async fn paging(&self, s: &str) -> Result<()> {
        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or_else(|| anyhow!("not coming from a user"))?;

        let exten = s.parse::<i64>()?;
        let extension = SWITCHBOARD_SERVICE
            .db
            .get_extension(&from_user.tenant_id, exten)
            .await
            .ok_or_else(|| anyhow!("no extension"))?;

        let to_users = match extension.discriminator.as_str() {
            "sipuser" => {
                let to_user = SWITCHBOARD_SERVICE
                    .db
                    .get_user(&from_user.tenant_id, &extension.parent_id)
                    .await
                    .ok_or_else(|| anyhow!("can'd find sipuser"))?;
                vec![to_user]
            }
            "huntgroup" => SWITCHBOARD_SERVICE
                .db
                .get_group_users(&from_user.tenant_id, &extension.parent_id)
                .await
                .ok_or_else(|| anyhow!("can't get group users"))?,
            _ => Vec::new(),
        };

        let mut paging_channels = Vec::new();

        for to_user in to_users {
            if to_user.is_talking().await {
                continue;
            }

            let mut locations = Usrloc::lookup(&to_user).await.unwrap_or_default();
            locations.retain(|l| l.dst_uri.transport != TransportType::Wss);

            let caller_id =
                internal_caller_id(&context, to_user.cc.clone(), None).await;
            let mut rtpmaps: Vec<Rtpmap> = INTERNAL_AUDIO_ORDER
                .iter()
                .map(|pt| pt.get_rtpmap())
                .collect();
            rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());
            let endpoint = Endpoint::User {
                user: to_user,
                group: None,
                user_agent: None,
                user_agent_type: None,
            };
            for location in locations {
                let id = nebula_utils::uuid();
                paging_channels.push(id.clone());
                let endpoint = endpoint.clone();
                let rtpmaps = rtpmaps.clone();
                let caller_id = caller_id.clone();
                let inbound_channel = self.id.clone();
                tokio::spawn(async move {
                    let channel = Channel::new(id.clone());
                    Channel::new_outbound(
                        id,
                        caller_id,
                        endpoint,
                        location,
                        None,
                        inbound_channel.clone(),
                        "".to_string(),
                        true,
                        rtpmaps,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Vec::new(),
                        "".to_string(),
                        true,
                        true,
                    )
                    .await?;
                    tokio::select! {
                        _ = channel.receive_event("0", ChannelEvent::Answer) => {
                            Bridge::bridge_media(&Channel::new(inbound_channel), &channel).await?;
                        }
                        _ = channel.receive_event("0", ChannelEvent::Hangup) => {
                            return Ok(());
                        }
                    }
                    let result: Result<()> = Ok(());
                    result
                });
            }
        }

        self.answer().await?;
        self.receive_event("0", ChannelEvent::Hangup).await?;

        for id in paging_channels {
            let _ = Channel::new(id).hangup(None).await;
        }

        Ok(())
    }

    async fn get_extension(
        tenant_id: &str,
        exten: &str,
    ) -> Option<nebula_db::models::Extension> {
        if let Ok(exten) = exten.parse::<i64>() {
            if let Some(extension) =
                SWITCHBOARD_SERVICE.db.get_extension(tenant_id, exten).await
            {
                return Some(extension);
            }
        }
        None
    }

    pub async fn check_last_call(&self) -> Result<()> {
        let context = self.get_call_context().await?;
        let user = context
            .endpoint
            .user()
            .ok_or_else(|| anyhow!("only sip user can check last call"))?;
        let event = SWITCHBOARD_SERVICE
            .db
            .get_user_last_cdr_event(user.uuid.to_string())
            .await
            .ok_or_else(|| anyhow!("sip user don't have cdr event"))?;
        let cdr = SWITCHBOARD_SERVICE
            .db
            .get_cdr(event.cdr_id)
            .await
            .ok_or_else(|| anyhow!("sip user don't have cdr item"))?;

        let tz = user
            .timezone
            .as_ref()
            .and_then(|tz| tz.parse::<Tz>().ok())
            .unwrap_or(Tz::Europe__London);
        let time = event.start.with_timezone(&tz);
        let date = if time.date() == Utc::now().with_timezone(&tz).date() {
            "today".to_string()
        } else {
            format!("on {}-{}-{}", time.year(), time.month(), time.day())
        };

        let time = format!("{}:{}", time.hour(), time.minute());

        println!("date is {date}, time is {time}");

        let sound = format!("tts://You were called {date} at {time} by caller <say-as interpret-as=\"telephone\">{}</say-as>. Press 3 to return the call. ", cdr.from_exten.clone().unwrap_or_default());
        let dtmf = self
            .play_sound_and_get_dtmf(vec![sound], 5000, 5000, 1)
            .await
            .join("");
        if dtmf == "3" {
            let _ = SWITCHBOARD_SERVICE
                .switchboard_rpc
                .new_inbound_channel(
                    &SWITCHBOARD_SERVICE.config.switchboard_host,
                    self.id.clone(),
                    &context.endpoint,
                    &context.proxy_host,
                    &cdr.from_exten.clone().unwrap_or_default(),
                    "",
                    true,
                    None,
                    None,
                    None,
                    Vec::new(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await;
            self.receive_event("0", ChannelEvent::Hangup).await?;
        }
        Ok(())
    }

    async fn remove_in_wait_lock(&self) -> Result<()> {
        let peer = self
            .get_peer()
            .await
            .ok_or_else(|| anyhow!("can't find peer"))?;
        let peer_context = peer.get_call_context().await?;
        let callqueue = peer_context
            .call_queue_id
            .ok_or_else(|| anyhow!("not in a callqueu"))?;
        let context = self.get_call_context().await?;
        let user = context
            .endpoint
            .user()
            .ok_or_else(|| anyhow!("channel isn't a user"))?;

        let _ = REDIS
            .del(&format!(
                "nebula:callqueue:{callqueue}:user:{}:waiting_lock",
                user.uuid
            ))
            .await;
        let _ = REDIS
            .del(&format!(
                "nebula:callqueue:user:{}:global_waiting_lock",
                user.uuid
            ))
            .await;

        Ok(())
    }

    pub async fn toggle_group_member(&self, exten: &str) -> Result<()> {
        let context = self.get_call_context().await?;
        let from_user = context
            .endpoint
            .user()
            .ok_or_else(|| anyhow!("not coming from a user"))?;

        let exten = exten.parse::<i64>()?;
        let Some(extension) = SWITCHBOARD_SERVICE
            .db
            .get_extension(&from_user.tenant_id, exten)
            .await
        else {
            self.play_sound(
                None,
                vec!["tts://can't find the extension number".to_string()],
                true,
            )
            .await?;
            return Err(anyhow!("can't find exten number"));
        };
        if extension.discriminator != "huntgroup" {
            self.play_sound(
                None,
                vec!["tts://the extension number isn't a group".to_string()],
                true,
            )
            .await?;
            return Err(anyhow!("not hunt group"));
        }
        let Some(group) = SWITCHBOARD_SERVICE
            .db
            .get_hunt_group(&extension.parent_id)
            .await
        else {
            self.play_sound(
                None,
                vec!["tts://can't find the group".to_string()],
                true,
            )
            .await?;
            return Err(anyhow!("can't find hunt group"));
        };

        let (joined, err_tts) = SWITCHBOARD_SERVICE
            .yaypi_client
            .toggle_hunt_group_member(&from_user.uuid, &group.uuid)
            .await?;

        let text = match err_tts {
            Some(err_tts) => format!("tts://{}", err_tts),
            None => format!(
                "tts://You've successfully logged {} the group",
                if joined { "into" } else { "out of" }
            ),
        };

        self.play_sound(None, vec![text], true).await?;

        Ok(())
    }

    pub async fn request_keyframe(&self) -> Result<()> {
        let video_id = self.get_video_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .request_keyframe(video_id)
            .await?;
        Ok(())
    }
}

#[async_recursion]
pub async fn external_caller_id(context: &CallContext) -> CallerId {
    match &context.endpoint {
        Endpoint::User { user, .. } => {
            if let Some(refer) = &context.refer {
                if !user.transfer_show_sipuser {
                    let peer = Channel::new(refer.peer.clone());
                    if let Some(peer) = peer.get_peer().await {
                        if let Ok(peer_context) = peer.get_call_context().await {
                            return external_caller_id(&peer_context).await;
                        }
                    }
                }
            }
            let callerid = match context.set_caller_id.clone() {
                Some(callerid) => callerid,
                None => user.caller_id.clone().unwrap_or_else(|| "".to_string()),
            };
            let (user, anonymous) = if callerid.starts_with("anonymous") {
                let e164 = callerid[10..callerid.len() - 1].to_string();
                (e164, true)
            } else {
                (callerid, false)
            };

            let user = get_phone_number(&user)
                .map(|number| {
                    number.format().mode(phonenumber::Mode::E164).to_string()
                })
                .unwrap_or(user);

            CallerId {
                user,
                e164: None,
                display_name: "".to_string(),
                anonymous,
                asserted_identity: None,
                original_number: None,
                to_number: None,
            }
        }
        Endpoint::Trunk {
            number, anonymous, ..
        } => {
            let user = match &context.set_caller_id {
                Some(callerid) => callerid.to_string(),
                None => number.to_string(),
            };
            CallerId {
                user,
                e164: None,
                display_name: "".to_string(),
                anonymous: *anonymous,
                asserted_identity: None,
                original_number: None,
                to_number: None,
            }
        }
        Endpoint::Provider {
            number, anonymous, ..
        } => {
            let show_original_callerid =
                context.call_flow_show_original_callerid.unwrap_or(true);

            let user: String = match &context.set_caller_id {
                Some(cli) => cli.to_string(),
                None => {
                    if let Some(custom_callerid) =
                        context.call_flow_custom_callerid.as_ref()
                    {
                        custom_callerid.to_string()
                    } else if !show_original_callerid {
                        context.to.clone()
                    } else {
                        number.to_string()
                    }
                }
            };

            let user = get_phone_number(&user)
                .map(|number| {
                    number.format().mode(phonenumber::Mode::E164).to_string()
                })
                .unwrap_or(user);

            CallerId {
                user,
                e164: None,
                display_name: "".to_string(),
                anonymous: if !show_original_callerid {
                    false
                } else {
                    *anonymous
                },
                asserted_identity: None,
                original_number: Some(number.to_string()),
                to_number: Some(context.to.clone()),
            }
        }
    }
}

#[async_recursion]
pub async fn internal_caller_id(
    context: &CallContext,
    cc: Option<String>,
    group: Option<String>,
) -> CallerId {
    match &context.endpoint {
        Endpoint::User { user, .. } => {
            if let Some(refer) = &context.refer {
                if !user.transfer_show_sipuser {
                    let peer = Channel::new(refer.peer.clone());
                    if let Some(peer) = peer.get_peer().await {
                        if let Ok(peer_context) = peer.get_call_context().await {
                            return internal_caller_id(&peer_context, cc, group)
                                .await;
                        }
                    }
                }
            }
            let display_name =
                if let Some(call_flow_name) = context.call_flow_name.as_ref() {
                    if let Some(call_queue_name) = context.call_queue_name.as_ref() {
                        format!("{call_flow_name} > {call_queue_name}")
                    } else if let Some(g) = group {
                        format!("{} > {}", call_flow_name, g)
                    } else {
                        call_flow_name.clone()
                    }
                } else {
                    user.display_name.clone().unwrap_or("".to_string())
                };
            CallerId {
                user: user.extension.unwrap_or(0).to_string(),
                e164: None,
                display_name,
                anonymous: false,
                asserted_identity: None,
                original_number: None,
                to_number: None,
            }
        }
        Endpoint::Trunk {
            number, anonymous, ..
        } => {
            let user = number.to_string();
            let display_name =
                if let Some(call_flow_name) = context.call_flow_name.as_ref() {
                    call_flow_name.clone()
                } else {
                    "".to_string()
                };
            CallerId {
                user,
                e164: None,
                display_name,
                anonymous: *anonymous,
                asserted_identity: None,
                original_number: None,
                to_number: None,
            }
        }
        Endpoint::Provider {
            number, anonymous, ..
        } => {
            if let Some(inter_tenant) = &context.inter_tenant {
                let mut user = inter_tenant.from_exten.clone();

                if !inter_tenant.extern_call_flow {
                    if let Some(tenant_extension) = SWITCHBOARD_SERVICE
                        .db
                        .get_tenant_extensions(&inter_tenant.dst_tenant_id)
                        .await
                        .and_then(|tenant_extensions| {
                            tenant_extensions.into_iter().find(|ext| {
                                ext.dst_tenant_id.eq(&inter_tenant.src_tenant_id)
                            })
                        })
                    {
                        /*
                            In this case, the call is from a callflow that bridges the accounts
                            We should send just the origin number down in the INVITE
                              so the user can call them back correctly
                        */
                        user = format!("{}{}", tenant_extension.number, user)
                    }
                }

                return CallerId {
                    user,
                    e164: None,
                    display_name: inter_tenant.from_display_name.clone(),
                    anonymous: false,
                    asserted_identity: None,
                    original_number: None,
                    to_number: None,
                };
            }

            let mut e164 = None;
            let mut user = if let Some(custom_callerid) =
                context.call_flow_custom_callerid.as_ref()
            {
                custom_callerid.to_string()
            } else if !context.call_flow_show_original_callerid.unwrap_or(true) {
                context.to.clone()
            } else if *anonymous {
                "anonymous".to_string()
            } else {
                number.to_string()
            };
            if user != "anonymous" {
                if let Some(number) = get_phone_number(&user) {
                    e164 = Some(
                        number.format().mode(phonenumber::Mode::E164).to_string(),
                    );
                    if let Some(cc) = cc.as_ref().map(|cc| cc.to_uppercase()) {
                        if !cc.is_empty() {
                            if let Some(number_cc) = number
                                .country()
                                .id()
                                .map(|c| c.as_ref().to_uppercase())
                            {
                                if cc == number_cc {
                                    user = format_national_number(&number);
                                }
                            }
                        }
                    }
                }
            }
            let display_name =
                if let Some(call_flow_name) = context.call_flow_name.as_ref() {
                    if let Some(call_queue_name) = context.call_queue_name.as_ref() {
                        format!("{call_flow_name} > {call_queue_name}")
                    } else if let Some(g) = group {
                        format!("{} > {}", call_flow_name, g)
                    } else {
                        call_flow_name.clone()
                    }
                } else {
                    "".to_string()
                };
            CallerId {
                user,
                e164,
                display_name,
                anonymous: false,
                asserted_identity: None,
                original_number: None,
                to_number: None,
            }
        }
    }
}

pub async fn redirect_inbound(context: &CallContext) -> Option<Channel> {
    let redirect_peer = context.redirect_peer.clone()?;
    Channel::new(redirect_peer)
        .get_bridge()
        .await
        .ok()?
        .get_inbound()
        .await
        .ok()
}

pub async fn redirect_context(context: &CallContext) -> Option<CallContext> {
    let redirect_peer = context.redirect_peer.clone()?;
    Channel::new(redirect_peer)
        .get_bridge()
        .await
        .ok()?
        .get_inbound()
        .await
        .ok()?
        .get_call_context()
        .await
        .ok()
}

pub fn notify_user_presence(user_uuid: &str) {
    let user_uuid = user_uuid.to_string();
    tokio::spawn(async move {
        let dialogs = user_dialogs(&user_uuid).await.unwrap_or_default();
        let presence = if let Some(d) =
            dialogs.iter().find(|d| d.state == DialogState::Confirmed)
        {
            format!("confirmed:{}:{}:{}", d.direction, d.remote, d.local)
        } else if let Some(d) = dialogs.first() {
            format!("proceeding:{}:{}:{}", d.direction, d.remote, d.local)
        } else if SWITCHBOARD_SERVICE
            .db
            .get_user("admin", &user_uuid)
            .await
            .and_then(|u| u.global_dnd)
            == Some(true)
        {
            "DND".to_string()
        } else {
            "None".to_string()
        };
        let key = &format!("nebula:last_presence:{user_uuid}");
        let old_presence = REDIS
            .getset::<String>(key, &presence)
            .await
            .unwrap_or_default();
        let _ = REDIS.expire(key, 86400).await;
        if old_presence == presence {
            // presence dosen't change, so no notify
            return;
        }

        let _ = notify_dialog(
            SWITCHBOARD_SERVICE
                .config
                .all_proxies()
                .map(|p| p.to_vec())
                .unwrap_or_else(|| {
                    vec![SWITCHBOARD_SERVICE.config.proxy_host.clone()]
                }),
            user_uuid.to_string(),
        )
        .await;
    });
}

pub async fn report_channels() {
    if SWITCHBOARD_SERVICE.config.proxies.is_some() {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            tokio::spawn(async move {
                let _ = report_channels_job().await;
            });
        }
    }
}

lazy_static::lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::new();
}

async fn report_channels_job() -> Result<()> {
    let n = REDIS.scard(ALL_CHANNELS).await?;
    if let Some(proxies) = SWITCHBOARD_SERVICE.config.proxies.as_ref() {
        for host in proxies {
            let switchboard_host =
                SWITCHBOARD_SERVICE.config.switchboard_host.clone();
            tokio::spawn(async move {
                let url = format!(
                    "http://{host}/report_switchboard_channels/{switchboard_host}/{n}"
                );
                let _ = HTTP_CLIENT.put(url).send().await;
            });
        }
    }
    Ok(())
}

pub async fn monitor_channels() {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        tokio::spawn(async move {
            let _ = one_monitor().await;
        });
    }
}

async fn one_monitor() -> Result<()> {
    let mut cursor = 0;
    loop {
        let (new_cursor, channels) = REDIS.sscan(ALL_CHANNELS, cursor, 100).await?;
        for channel in channels {
            tokio::spawn(async move {
                if let Err(e) = monitor_channel(channel.clone()).await {
                    tracing::error!(
                        channel = channel,
                        "monitor channel error: {e:?}"
                    );
                    let _ = REDIS.del(&format!("nebula:channel:{channel}")).await;
                    let _ = REDIS.srem(ALL_CHANNELS, &channel).await;
                }
            });
        }

        if new_cursor == 0 {
            return Ok(());
        }

        cursor = new_cursor;
    }
}

async fn monitor_channel(channel: String) -> Result<()> {
    let channel = Channel::new(channel);
    if channel.is_hangup().await {
        let _ = REDIS.srem(ALL_CHANNELS, &channel.id).await;
        return Ok(());
    }

    if channel.is_answered().await {
        let mode = REDIS
            .hget(&format!("nebula:channel:{}", &channel.id), "mode")
            .await
            .unwrap_or_else(|_| "".to_string());
        let mode = MediaMode::from_str(&mode).unwrap_or(MediaMode::Sendrecv);
        let last_received_time: u64 =
            REDIS.hget(&channel.redis_key(), "last_rtp_packet").await?;
        let ctime = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)?
            .as_secs();
        let duration = ctime - last_received_time;
        if duration > 60 * 3 {
            if mode != MediaMode::Sendrecv && duration < 3600 * 3 {
                return Ok(());
            }
            if let Ok(context) = channel.get_call_context().await {
                if context.transfer.is_some() && duration < 60 * 60 * 24 {
                    return Ok(());
                }

                if context.callback.is_some() && duration < 60 * 60 * 24 {
                    return Ok(());
                }
            }
            if let Ok(last_mode_change) = REDIS
                .hget::<u64>(&channel.redis_key(), "last_mode_change")
                .await
            {
                if ctime - last_mode_change < 180 {
                    return Ok(());
                }
            }
            info!(
                channel = channel.id,
                "hangup channel {} due to rtp inactivity {ctime} {last_received_time}",
                channel.id
            );
            let _ = channel.hangup(None).await;
            if let Ok(bridge) = channel.get_bridge().await {
                let _ = bridge.channel_hangup(&channel.id).await;
            }
        }
    } else {
        let context = channel.get_call_context().await?;
        let now = Utc::now().timestamp();
        let start = context.started_at.timestamp();
        if (now - start) > 60 * 5 {
            info!(
                channel = channel.id,
                "hangup channel {} due to no answer for more than 5 mintues",
                channel.id
            );
            let _ = channel.hangup(None).await;
            if let Ok(bridge) = channel.get_bridge().await {
                let _ = bridge.channel_hangup(&channel.id).await;
            }
        }
    }
    Ok(())
}

fn get_alert_info_header(
    patterns: &[AlertInfoPatterns],
    caller_id: &CallerId,
    internal_call: bool,
) -> Option<String> {
    for pattern in patterns {
        for p in pattern.patterns.iter() {
            match p.as_str() {
                "internal_call" => {
                    if internal_call {
                        return Some(pattern.header.clone());
                    }
                }
                "external_call" => {
                    if !internal_call {
                        return Some(pattern.header.clone());
                    }
                }
                _ => {
                    if callerid_match_pattern(caller_id, p) {
                        return Some(pattern.header.clone());
                    }
                }
            }
        }
    }
    None
}

fn callerid_match_pattern(caller_id: &CallerId, pattern: &str) -> bool {
    if pattern == "anonymous" && caller_id.anonymous {
        return true;
    }

    if let Ok(re) = regex::Regex::new(pattern) {
        if let Some(e164) = caller_id.e164.as_ref() {
            if re.is_match(e164) {
                return true;
            }
        } else if re.is_match(&caller_id.user) {
            return true;
        }
    }

    false
}

#[async_recursion]
pub(crate) async fn user_locations(
    user: &User,
    internal_call: bool,
) -> Vec<Location> {
    if user.disable_call_waiting && user.is_talking().await {
        return Vec::new();
    }

    if user.num_channels().await.unwrap_or(0) > 50 {
        // limit a user's "active" channels to be 50
        // this can protect cases where there's a loop somewhere
        // and constantly called a user
        return Vec::new();
    }

    let mut locations = Usrloc::lookup(user).await.unwrap_or_default();
    locations.retain(|l| !l.dnd || (l.dnd_allow_internal && internal_call));

    let apps = DB.get_mobile_apps(user).await.unwrap_or_default();
    let mut app_locations: Vec<Location> = stream::iter(apps)
        .filter_map(|app| async move {
            // if self
            //     .is_app_unavailable(&app.device, &app.device_token)
            //     .await
            // {
            //     return None;
            // }
            if app.dnd {
                if !app.dnd_allow_internal {
                    return None;
                }
                if !internal_call {
                    return None;
                }
            }

            let uri = Uri {
                user: Some(user.name.clone()),
                host: "10.132.0.57".to_string(),
                port: Some(5190),
                transport: TransportType::Tcp,
                ..Default::default()
            };
            Some(Location {
                user: user.name.clone(),
                proxy_host: SWITCHBOARD_SERVICE
                    .config
                    .london_proxy_host()
                    .to_string(),
                from_host: user
                    .domain
                    .clone()
                    .unwrap_or_else(|| "talk.yay.com".to_string()),
                to_host: user
                    .domain
                    .clone()
                    .unwrap_or_else(|| "talk.yay.com".to_string()),
                display_name: user.display_name.clone(),
                req_uri: uri.clone(),
                dst_uri: uri,
                route: Some(Uri {
                    host: "23.251.134.254".to_string(),
                    transport: TransportType::Tcp,
                    ..Default::default()
                }),
                intra: false,
                inter_tenant: None,
                encryption: false,
                device: Some(app.device.clone()),
                device_token: Some(app.device_token.clone()),
                dnd: false,
                dnd_allow_internal: false,
                provider: None,
                is_registration: None,
                user_agent: Some(format!("yay {}", app.device)),
                tenant_id: Some(user.tenant_id.clone()),
                teams_refer_cx: None,
                ziron_tag: None,
                diversion: None,
                pai: None,
                global_dnd: false,
                global_forward: None,
                room_auth_token: None,
            })
        })
        .collect()
        .await;
    locations.append(&mut app_locations);

    let msteam = user.msteam.clone().unwrap_or_default();
    let msteam_domain = user.msteam_domain.clone().unwrap_or_default();
    if !msteam.is_empty() && !msteam_domain.is_empty() {
        let host = "sip.pstnhub.microsoft.com";
        let req_uri = Uri {
            scheme: "sip".to_string(),
            user: Some(msteam.clone()),
            host: host.to_string(),
            transport: TransportType::Tls,
            ..Default::default()
        };
        let dst_uri = Uri {
            scheme: "sip".to_string(),
            host: host.to_string(),
            transport: TransportType::Tls,
            ..Default::default()
        };
        let location = Location {
            user: msteam,
            proxy_host: SWITCHBOARD_SERVICE.config.london_proxy_host().to_string(),
            display_name: None,
            from_host: msteam_domain.clone(),
            to_host: msteam_domain,
            req_uri,
            dst_uri,
            intra: false,
            inter_tenant: None,
            encryption: true,
            route: None,
            device: None,
            device_token: None,
            dnd: false,
            dnd_allow_internal: false,
            is_registration: None,
            provider: Some(true),
            user_agent: None,
            tenant_id: Some(user.tenant_id.clone()),
            teams_refer_cx: None,
            ziron_tag: None,
            diversion: None,
            pai: None,
            global_dnd: false,
            global_forward: None,
            room_auth_token: None,
        };
        locations.push(location);
    }

    locations
}

async fn check_peer_addr(media_id: &str, ip: &str, port: u16) {
    let peer_addr = REDIS
        .hget(&format!("nebula:media_stream:{media_id}"), "peer_addr")
        .await
        .unwrap_or_else(|_| "".to_string());
    if let Ok(peer_addr) = peer_addr.parse::<SocketAddrV4>() {
        if peer_addr.port() == port {
            return;
        }
        let _ = REDIS
            .hsetex(
                &format!("nebula:media_stream:{media_id}"),
                "peer_addr",
                &format!("{}:{port}", peer_addr.ip()),
            )
            .await;
        let _ = SWITCHBOARD_SERVICE
            .media_rpc_client
            .set_peer_addr(media_id.to_string(), *peer_addr.ip(), port)
            .await;
        return;
    }

    let _ = REDIS
        .hsetex(
            &format!("nebula:media_stream:{media_id}"),
            "peer_addr",
            &format!("{ip}:{port}"),
        )
        .await;
}
