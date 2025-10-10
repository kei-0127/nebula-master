use chrono::{DateTime, Utc};
use hmac::{Hmac, NewMac};
use sdp::payload::Rtpmap;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sip::message::{
    Address, CallerId, HangupReason, InterTenantInfo, Location, TeamsReferCx,
};
use std::{collections::HashMap, net::Ipv4Addr, sync::LazyLock};
use strum_macros;
use strum_macros::EnumString;
use uuid::Uuid;

use crate::models::{AuxCodeResponse, MusicOnHold, Number, Provider, Trunk, User};

const JWT_CLAIMS_SECRET: &[u8; 93] = b"g[&:^L__%+z|EZ#@<+<DnfjV5m:vW1_Q$Lj'[^7ie~r4sgAlg&)'I{o=9j/E{?0v$es|Qt]2|ut9bc59@`hf'(2Xi?Sn~";
pub static JWT_CLAIMS_KEY: LazyLock<Hmac<Sha256>> =
    LazyLock::new(|| Hmac::new_from_slice(JWT_CLAIMS_SECRET).unwrap());

#[derive(strum_macros::Display, EnumString, PartialEq, Clone, Debug)]
pub enum ChannelEvent {
    #[strum(serialize = "switch_callflow")]
    SwitchCallflow,
    #[strum(serialize = "play_sound")]
    PlaySound,
    #[strum(serialize = "hold")]
    Hold,
    #[strum(serialize = "unhold")]
    Unhold,
    #[strum(serialize = "start_dtmf")]
    StartDtmf,
    #[strum(serialize = "dtmf")]
    Dtmf,
    #[strum(serialize = "ring")]
    Ring,
    #[strum(serialize = "answer")]
    Answer,
    #[strum(serialize = "answer_ack")]
    AnswerAck,
    #[strum(serialize = "hangup")]
    Hangup,
    #[strum(serialize = "voicemail_uploaded")]
    VoicemailUploaded,
    #[strum(serialize = "sound_end")]
    SoundEnd,
    #[strum(serialize = "call_queue_wait_done")]
    CallQueueWaitDone,
    #[strum(serialize = "bridge_hangup")]
    BridgeHangup,
    #[strum(serialize = "bridge_hangup_ack")]
    BridgeHangupAck,
    #[strum(serialize = "bridge_answer")]
    BridgeAnswer,
    #[strum(serialize = "fax_end")]
    FaxEnd,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CallContext {
    pub endpoint: Endpoint,
    pub proxy_host: String,
    pub api_addr: Option<String>,
    pub location: Option<Location>,
    pub inter_tenant: Option<InterTenantInfo>,
    pub to: String,
    pub to_internal_number: Option<Number>,
    pub direction: CallDirection,
    pub call_confirmation: Option<CallConfirmation>,
    pub caller_id: Option<CallerId>,
    pub set_caller_id: Option<String>,
    pub answer_caller_id: Option<CallerId>,
    pub call_queue_id: Option<String>,
    pub call_queue_name: Option<String>,
    pub call_flow_id: Option<String>,
    pub call_flow_name: Option<String>,
    pub call_flow_show_original_callerid: Option<bool>,
    pub call_flow_custom_callerid: Option<String>,
    pub call_flow_moh: Option<String>,
    pub parent_call_flow_name: Option<String>,
    pub parent_call_flow_show_original_callerid: Option<bool>,
    pub parent_call_flow_custom_callerid: Option<String>,
    pub parent_call_flow_moh: Option<String>,
    pub intra_peer: Option<String>,
    pub intra_chain: Vec<String>,
    pub redirect_peer: Option<String>,
    pub reply_to: Option<String>,
    pub refer: Option<ReferContext>,
    pub remote_ip: Option<String>,
    pub transfer: Option<TransferContext>,
    pub cdr: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub answred_at: Option<DateTime<Utc>>,
    pub forward: bool,
    pub backup: Option<User>,
    pub backup_invite: Option<CallInvitation>,
    pub recording: Option<RecordingContext>,
    pub click_to_dial: Option<(String, String)>,
    // the number where a call queue register to call back to
    pub callback: Option<String>,
    // the callback peer channel
    pub callback_peer: Option<String>,
    pub callflow: Option<(String, String)>,
    pub room: String,
    pub remove_call_restrictions: bool,
    pub ziron_tag: Option<String>,
    pub diversion: Option<Address>,
    pub pai: Option<Address>,
}

impl CallContext {
    pub fn new(endpoint: Endpoint, to: String, direction: CallDirection) -> Self {
        Self {
            endpoint,
            api_addr: None,
            location: None,
            inter_tenant: None,
            proxy_host: "".to_string(),
            to,
            to_internal_number: None,
            direction,
            started_at: Utc::now(),
            call_confirmation: None,
            set_caller_id: None,
            caller_id: None,
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
            intra_chain: Vec::new(),
            redirect_peer: None,
            reply_to: None,
            refer: None,
            transfer: None,
            cdr: None,
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
            remote_ip: None,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CallInvitation {
    pub location: Location,
    pub caller_id: CallerId,
    pub endpoint: Endpoint,
    pub inbound_channel: String,
    pub bridge_id: String,
    pub internal_call: bool,
    pub call_confirmation: Option<CallConfirmation>,
    pub rtpmaps: Vec<Rtpmap>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct PSTNProvider {
    pub name: String,
    pub number: String,
}

#[derive(
    strum_macros::Display,
    EnumString,
    Deserialize,
    Serialize,
    Clone,
    Debug,
    PartialEq,
    Eq,
)]
pub enum UserAgentType {
    #[strum(serialize = "desktop_app")]
    DesktopApp,
    #[strum(serialize = "mobile_app")]
    MobileApp,
    #[strum(serialize = "ms_teams")]
    MsTeams,
    #[strum(serialize = "other")]
    Other,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub enum Endpoint {
    User {
        user: User,
        group: Option<String>,
        user_agent: Option<String>,
        user_agent_type: Option<UserAgentType>,
    },
    Trunk {
        trunk: Trunk,
        number: String,
        anonymous: bool,
        extension: String,
        ziron_tag: Option<String>,
    },
    Provider {
        provider: Provider,
        number: String,
        anonymous: bool,
        tenant_id: Option<String>,
        diversion: Option<Address>,
        pai: Option<Address>,
    },
}

impl Endpoint {
    pub fn id(&self) -> String {
        match self {
            Endpoint::User { user, .. } => format!("user:{}", user.uuid),
            Endpoint::Trunk { trunk, .. } => format!("trunk:{}", trunk.uuid),
            Endpoint::Provider { number, .. } => format!("provider:{}", number),
        }
    }

    pub fn cc(&self) -> String {
        match self {
            Endpoint::User { user, .. } => user.cc.clone().unwrap_or("".to_string()),
            Endpoint::Trunk { trunk, .. } => {
                trunk.cc.clone().unwrap_or("".to_string())
            }
            Endpoint::Provider { .. } => "".to_string(),
        }
    }

    pub fn tenant_id(&self) -> Option<&str> {
        match self {
            Endpoint::User { user, .. } => Some(&user.tenant_id),
            Endpoint::Provider { tenant_id, .. } => {
                tenant_id.as_ref().map(|tenant_id| tenant_id.as_str())
            }
            Endpoint::Trunk { trunk, .. } => Some(&trunk.tenant_id),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Endpoint::User { user, .. } => {
                user.display_name.clone().unwrap_or("".to_string())
            }
            Endpoint::Provider { number, .. } => number.clone(),
            Endpoint::Trunk { trunk, .. } => trunk.name.clone(),
        }
    }

    pub fn extension(&self) -> String {
        match self {
            Endpoint::User { user, .. } => user.extension.unwrap_or(0).to_string(),
            Endpoint::Provider {
                number, anonymous, ..
            } => {
                if *anonymous {
                    "anonymous".to_string()
                } else {
                    number.to_string()
                }
            }
            Endpoint::Trunk { number, .. } => number.to_string(),
        }
    }

    pub fn user(&self) -> Option<&User> {
        match &self {
            Endpoint::User { user, .. } => Some(user),
            _ => None,
        }
    }

    pub fn provider(&self) -> Option<&Provider> {
        match &self {
            Endpoint::Provider { provider, .. } => Some(provider),
            _ => None,
        }
    }

    pub fn trunk(&self) -> Option<&Trunk> {
        match &self {
            Endpoint::Trunk { trunk, .. } => Some(trunk),
            _ => None,
        }
    }

    pub fn is_internal_sdp(&self) -> bool {
        match &self {
            Endpoint::Provider { provider, .. } => provider.name == "intra",
            _ => true,
        }
    }

    pub fn is_internal_call(&self) -> bool {
        match &self {
            Endpoint::Provider { .. } => false,
            _ => true,
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct InviteRequest {
    pub id: String,
    pub from: String,
    pub to: Location,
    pub body: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub enum ReferTo {
    Channel(String),
    Exten(String),
    Teams {
        refer_to: Address,
        refer_by: Address,
    },
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ReferRequest {
    pub id: String,
    pub to: ReferTo,
    pub refer_to: Address,
    pub refer_by: Option<Address>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct NewInvite {
    pub id: String,
    pub reply_to: String,
    pub inbound_cdr: Option<String>,
    pub caller_id: CallerId,
    pub to: Location,
    pub internal_call: bool,
    pub alert_info_header: Option<String>,
    pub click_to_dial: bool,
    pub body: String,
    pub switchboard_host: String,
    pub room: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct NewIntraRequest {
    pub id: String,
    pub endpoint: Endpoint,
    pub location: Location,
    pub reply_to: String,
    pub sdp: String,
    pub intra: String,
    pub intra_chain: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcError {
    pub message: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcDialog {
    pub id: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcChannel {
    pub id: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcUpdateCallerid {
    pub id: String,
    pub caller_id: CallerId,
    pub body: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcConvertToRoom {
    pub id: String,
    pub room: String,
    pub auth_token: String,
    pub body: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct NotifyEvent {
    pub event: String,
    pub content: String,
    pub content_type: Option<String>,
    pub location: Location,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct AudioLevelNotification {
    pub room: String,
    pub member: String,
    pub level: u8,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct VideoAllocation {
    pub ssrc: u32,
    pub rid: String,
    pub rate: u64,
    pub width: usize,
    pub height: usize,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct VideoAllocationNotification {
    pub room: String,
    pub member: String,
    pub allocations: HashMap<String, VideoAllocation>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct AvailableRateNotification {
    pub room: String,
    pub member: String,
    pub rate: u64,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcVoicemailNotification {
    pub location: Location,
    pub new: usize,
    pub old: usize,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcDtmf {
    pub audio_id: String,
    pub dtmf: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcFinishRecording {
    pub cdr: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ProxyHangup {
    pub id: String,
    pub code: Option<usize>,
    pub redirect: Option<String>,
    pub reason: Option<HangupReason>,
    pub is_answered: bool,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Hangup {
    pub id: String,
    pub from_proxy: bool,
    pub code: Option<usize>,
    pub redirect: Option<String>,
    pub reason: Option<HangupReason>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ParkStatus {
    pub uuid: String,
    pub slot: String,
    pub parked: bool,
    pub direction: CallDirection,
    pub from: String,
    pub to: String,
}

#[derive(Deserialize, Serialize, Debug, Default)]
pub struct RpcSound {
    pub id: String,
    pub channel_id: String,
    pub sounds: Vec<String>,
    pub random: bool,
    pub repeat: u32,
    pub block: bool,
}

#[derive(Deserialize, Serialize)]
pub struct RpcMoh {
    pub channel_id: String,
    pub moh: MusicOnHold,
}

#[derive(Deserialize, Serialize)]
pub struct RpcRingtone {
    pub ringtone: String,
    pub channel_id: String,
    pub sounds: Vec<String>,
    pub random: bool,
}

#[derive(Deserialize, Serialize)]
pub struct SendReaction {
    pub call_id: String,
    pub reaction: String,
    pub auth_token: String,
}

#[derive(Deserialize, Serialize)]
pub struct UpdateCall {
    pub call_id: String,
    pub sdp: String,
}

#[derive(Deserialize, Serialize)]
pub struct RegisterRoomApi {
    pub username: Option<String>,
    pub password: Option<String>,
    pub device: Option<String>,
    pub device_token: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct CreateTempRoom {
    pub call_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub device: Option<String>,
    pub device_token: Option<String>,
    pub sdp: String,
    pub invited: Vec<String>,
    pub audio_muted: Option<bool>,
    pub video_muted: Option<bool>,
}

#[derive(Deserialize, Serialize)]
pub struct ConvertToRoom {
    pub call_id: String,
    pub username: String,
    pub invited: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct AuthRequest {
    pub room: String,
    pub call_id: String,
    pub display_name: Option<String>,
    pub room_password: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub device: Option<String>,
    pub device_token: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct RemoveMemberRequest {
    pub member: String,
}

#[derive(Deserialize, Serialize)]
pub struct InviteExtenRequest {
    pub extens: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct AuthorizeMemberRequest {
    pub member: String,
    pub auth_token: String,
}

#[derive(Deserialize, Serialize)]
pub struct WatchRoomRequest {
    pub auth_token: String,
}

#[derive(Deserialize, Serialize)]
pub struct JoinRoomRequest {
    pub sdp: String,
    pub auth_token: String,
    pub audio_muted: Option<bool>,
    pub video_muted: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct JoinRoomResponse {
    pub call_id: String,
    pub sdp: String,
    pub members: Vec<RoomMember>,
    pub is_admin: bool,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct RoomMember {
    pub id: String,
    pub user: Option<String>,
    pub name: String,
    pub is_admin: bool,
    pub hand_raised: bool,
    pub screen_share: bool,
    pub audio_muted: bool,
    pub video_muted: bool,
    pub audio_muted_by_admin: bool,
    pub video_muted_by_admin: bool,
    pub is_sip: bool,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SubscribeRoomMember {
    pub id: String,
    pub video_mid: String,
    pub width: Option<usize>,
    pub height: Option<usize>,
}

#[derive(Deserialize, Serialize)]
pub struct SubscribeRoomRequest {
    pub sdp: String,
    pub members: Vec<SubscribeRoomMember>,
    pub auth_token: String,
}

#[derive(Deserialize, Serialize)]
pub struct SendReactionNotification {
    pub reaction: String,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct SubscribeRoomResponse {
    pub sdp: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcSession {
    pub port: u16,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcMediaStream {
    pub id: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcVideoDestinationLayer {
    pub src: String,
    pub dst: String,
    // the rid of the src that's get sent to dst
    // if none, that means the dst is removed from src
    pub rid: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcReinviteCrypto {
    pub local: String,
    pub remote: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcStopSession {
    pub hangup: bool,
    pub reason: String,
}

#[derive(
    Deserialize,
    Serialize,
    Debug,
    Clone,
    PartialEq,
    strum_macros::Display,
    EnumString,
)]
pub enum RecordingType {
    #[strum(serialize = "call")]
    Call,
    #[strum(serialize = "voicemail")]
    Voicemail,
    #[strum(serialize = "raw")]
    Raw,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcRecording {
    pub stream_id: String,
    pub cdr: String,
    pub expires: usize,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RpcFileRecorder {
    pub uuid: String,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct RedisRecorder {
    pub limit: usize,
    pub pcm: Vec<i16>,
    pub key: String,
    pub expire: u64,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SessionPeer {
    pub port: u16,
    pub peer_ip: Ipv4Addr,
    pub peer_port: u16,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct StreamPeer {
    pub peer_ip: Ipv4Addr,
    pub peer_port: u16,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SetCryptoReq {
    pub port: u16,
    pub direction: String,
    pub crypto: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SetMediaModeReq {
    pub mode: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct CallflowResponse {
    pub response: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
    pub id: String,
    pub code: i32,
    pub status: String,
    pub body: String,
    pub remote_ip: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct AnswerResponse {
    pub id: String,
    pub body: String,
    pub caller_id: Option<CallerId>,
    pub cdr_uuid: Option<String>,
    pub remote_ip: Option<String>,
}

#[derive(
    strum_macros::Display,
    EnumString,
    Deserialize,
    Serialize,
    Debug,
    Clone,
    PartialEq,
)]
pub enum CallDirection {
    #[strum(serialize = "initiator")]
    Initiator,
    #[strum(serialize = "recipient")]
    Recipient,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ReferContext {
    pub direction: CallDirection,
    pub peer: String,
    pub slot: Option<String>,
    pub no_recording: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum TransferTarget {
    Location(Location, Endpoint),
    Number(String),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum TransferPeer {
    Initiator {
        to_channel: String,
        // when a client doing raw transfer, they could send empty sdp
        // and when we answer it, we need to send empty sdp back
        empty_sdp: bool,
    },
    Recipient {
        from_channel: String,
        target: TransferTarget,
        auto_answer: bool,
        destination: String,
    },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TransferContext {
    pub peer: TransferPeer,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RecordingContext {
    pub started: bool,
    pub expires: usize,
    pub paused: bool,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct CallConfirmation {
    pub key: String,
    pub sound: String,
    pub timeout: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NewInboundChannelRequest {
    pub id: String,
    pub from: Endpoint,
    pub to: String,
    pub sdp: String,
    pub existing: bool,
    pub proxy_host: String,
    pub refer: Option<ReferContext>,
    // transfer is the internal transfer context where the actual transfer is set up
    pub transfer: Option<TransferContext>,
    // raw_transfer is from the proxy, and it is the initial INVITE
    // with transfer target and auto answer in it
    pub raw_transfer: Option<(String, bool, bool)>,
    // when teams refer a call, we'll need to call the teams mri
    pub teams_refer_cx: Option<TeamsReferCx>,
    pub intra: Option<String>,
    pub intra_chain: Vec<String>,
    pub inter_tenant: Option<InterTenantInfo>,
    pub redirect: Option<String>,
    pub replaces: Option<String>,
    pub reply_to: Option<String>,
    pub callflow: Option<(String, String)>,
    pub remote_ip: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct UserPresence {
    pub sipuser: String,
    pub status: String,
    pub app: serde_json::Value,
    pub details: serde_json::Value,
    pub aux_code: Option<AuxCodeResponse>,
    pub queue_available: bool,
    pub queue_login: bool,
    pub global_dnd: bool,
    pub time: String,
    pub registrations: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct UserPresenceDetails {
    pub remote: String,
    pub local: String,
    pub direction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_queue_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_queue_name: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct UserRegistration {
    pub username: String,
    pub cc: String,
    pub contact: String,
    pub received: String,
    pub registered_at: String,
    pub expires: String,
    pub user_agent: String,
    pub callid: String,
    pub dc: String,
    pub location_id: String,
    pub proxy: String,
    pub dnd: bool,
    pub dnd_allow_internal: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DialogInfo {
    pub local: String,
    pub remote: String,
    pub direction: CallDirection,
    pub callid: String,
    pub state: DialogState,
    pub time: DateTime<Utc>,
    pub call_queue_id: Option<String>,
    pub call_queue_name: Option<String>,
}

#[derive(
    strum_macros::Display, EnumString, PartialEq, Clone, Serialize, Deserialize,
)]
pub enum DialogState {
    #[strum(serialize = "early")]
    Early,
    #[strum(serialize = "confirmed")]
    Confirmed,
}

#[derive(Deserialize, Serialize)]
pub struct UserAuthClaim {
    pub room: String,
    pub tenant_id: String,
    pub call_id: String,
    pub user: Option<String>,
    pub name: String,
    pub is_admin: bool,
    pub time: std::time::SystemTime,
}

#[derive(Deserialize, Serialize)]
pub struct AbandonedQueueRecord {
    pub tenant_id: String,
    pub number: String,
    pub number_formats: Vec<String>,
    pub called_back: AbandonedQueueRecordCalledback,
}

#[derive(Deserialize, Serialize)]
pub enum AbandonedQueueRecordCalledback {
    // not called back for the entire timeout duration
    NotCalledback,
    // It has called back with the called back cdr id and time
    Calledback(Uuid, DateTime<Utc>),
    // It hasn't got called back yet but it hasn't passed the timeout
    // It also includes the timestamp for our next query's start time
    WithinTimeout(DateTime<Utc>),
}
