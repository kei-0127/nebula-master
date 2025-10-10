use codec::vp8::PixelSize;
use jsonrpc_lite::JsonRpc;
use serde::{Deserialize, Serialize};
use strum_macros;
use strum_macros::EnumString;
use uuid::Uuid;

pub const SWITCHBOARD_STREAM: &str = "nebula:switchboard:stream";
pub const SWITCHBOARD_GROUP: &str = "switchboard";
pub const PROXY_STREAM: &str = "nebula:proxy:stream";
pub const PROXY_GROUP: &str = "proxy";
pub const MEDIA_STREAM: &str = "nebula:media:stream";
pub const MEDIA_GROUP: &str = "media";

#[derive(
    strum_macros::Display,
    EnumString,
    Debug,
    PartialEq,
    Clone,
    Deserialize,
    Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RpcMethod {
    Invite,
    ProxyHangup,
    ProxyHangupIntra,
    AnswerIntra,
    IntraSessoinProgress,
    Hangup,
    Cancel,
    RequestKeyframe,
    LocalSend,
    SetResponse,
    NewInboundChannel,
    NewIntra,
    RoomRpc,
    ConvertToRoom,
    SendReaction,
    ReInvite,
    Refer,
    NotifyDialog,
    NotifyReferSuccess,
    NotifyReferTrying,
    UpdateCallerid,
    NotifyVoicemail,
    NotifyAudioLevel,
    NotifyVideoAllocation,
    NotifyAvailableRate,
    Answer,
    Ack,
    StopSession,
    StopIce,
    UpdateSession,
    UpdateMediaDestinations,
    UpdateVideoSources,
    UpdateVideoDestinationLayer,
    MuteStream,
    UnmuteStream,
    SetSessionCrypto,
    SetSessionPeer,
    SetPeerAddr,
    SetSessionMediaMode,
    SetActiveSpeaker,
    SetT38Passthrough,
    ReinviteCrypto,
    PauseRecording,
    ResumeRecording,
    StartRecording,
    StreamStartSend,
    DtlsDone,
    StartVoicemail,
    RecordToRedis,
    StopRecordToRedis,
    StopRecordToFile,
    RecordToFile,
    UploadRecordToFile,
    PlayRingtone,
    StopRingtone,
    SetFax,
    InitT38,
    PlaySound,
    StopSound,
    PlayMoh,
    StopMoh,
    SetRtpmap,
    SendDtmf,
    ReceiveDtmf,
    FinishCallRecording,
    PublishPark,
    OptionsMobileApp,
    SyncProvisioning,
    NotifyEvent,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RpcMessage {
    pub method: RpcMethod,
    pub id: String,
    pub params: serde_json::Value,
}

#[derive(
    strum_macros::Display,
    EnumString,
    Debug,
    PartialEq,
    Clone,
    Deserialize,
    Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RoomRequestMethod {
    RegisterRoomApi,
    WatchRoom,
    ConvertToRoom,
    CreateTempRoom,
    InviteExten,
    Authenticate,
    JoinRoom,
    SubscribeRoom,
    AllowMember,
    RejectMember,
    RemoveMember,
    GetRoomMembers,
    GetWaitingRoomMembers,
    MuteMemberAudio,
    UnmuteMemberAudio,
    MuteMemberVideo,
    UnmuteMemberVideo,
}

#[derive(
    strum_macros::Display,
    EnumString,
    Debug,
    PartialEq,
    Eq,
    Clone,
    Deserialize,
    Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RoomNotificationMethod {
    StillWaiting,
    LeaveRoom,
    RaiseHand,
    UnraiseHand,
    SendReaction,
    StartScreenShare,
    StopScreenShare,
    MuteAudio,
    UnmuteAudio,
    MuteVideo,
    UnmuteVideo,
}

#[derive(
    strum_macros::Display,
    EnumString,
    Debug,
    PartialEq,
    Eq,
    Clone,
    Deserialize,
    Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RoomServerNotificationMethod {
    MemberJoinRoom,
    MemberLeaveRoom,
    MemberLeaveWaitingRoom,
    MemberWaiting,
    MemberWaitingAllowed,
    MemberWaitingRejected,
    MemberAllowed,
    MemberRejected,
    MemberMuteAudio,
    MemberUnmuteAudio,
    MemberMuteVideo,
    MemberUnmuteVideo,
    MemberRaiseHand,
    MemberUnraiseHand,
    MemberStartScreenShare,
    MemberStopScreenShare,
    MemberSendReaction,
    MemberAudioLevel,
    MemberAvailableRate,
    MemberVideoAllocations,
    RoomActiveSpeaker,
    RoomClearActiveSpeaker,
    RoomInvite,
    RoomInviteReject,
    RoomInviteCompletedElsewhere,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RoomRpcMessage {
    pub rpc: JsonRpc,
    pub addr: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct VideoSourceInfo {
    pub width: usize,
    pub height: usize,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct IncomingVideoInfo {
    pub uuid: Uuid,
    pub ssrc: u32,
    pub size: PixelSize,
    pub rid: String,
    pub rate: Option<u64>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct AllocatedVideoSSrc {
    pub uuid: Uuid,
    pub rid: String,
    pub ssrc: u32,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ActiveSpeaker {
    pub video_id: Uuid,
    pub now: u64,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct CreateCall {
    pub user_uuid: Option<String>,
    pub destination: String,
    pub display_name: Option<String>,
    pub tenant_id: Option<String>,
    pub targets: Option<Vec<serde_json::Value>>,
    pub proxy_host: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct TransferCallRequest {
    pub tenant_id: String,
    pub call_id: String,
    pub target: String,
    pub auto_answer: Option<bool>,
}
