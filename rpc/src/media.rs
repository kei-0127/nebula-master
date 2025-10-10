use std::net::Ipv4Addr;

use anyhow::Result;
use codec::InitT38Params;
use nebula_db::{
    message::{
        RedisRecorder, RpcDtmf, RpcFileRecorder, RpcMediaStream, RpcMoh,
        RpcRecording, RpcReinviteCrypto, RpcRingtone, RpcSession, RpcSound,
        RpcStopSession, RpcVideoDestinationLayer, SetMediaModeReq, StreamPeer,
    },
    models::MusicOnHold,
};
use nebula_redis::REDIS;
use serde_json::json;
use uuid::Uuid;

use crate::{
    client::RpcClient,
    message::{ActiveSpeaker, RpcMessage, RpcMethod, MEDIA_STREAM},
};

#[derive(Clone, Default)]
pub struct MediaRpcClient {}

pub trait HasSound {
    fn get_sound(&self) -> String;
}

impl MediaRpcClient {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn send_msg(&self, msg: &RpcMessage) -> Result<()> {
        RpcClient::send_to_stream(MEDIA_STREAM, msg).await?;
        Ok(())
    }

    pub async fn send_msg_to_media_stream(
        &self,
        media_stream_id: &str,
        msg: &RpcMessage,
    ) -> Result<()> {
        REDIS
            .xaddex(
                &format!("nebula:media_stream:{}:stream", media_stream_id),
                "message",
                &serde_json::to_string(&msg)?,
                60,
            )
            .await?;
        Ok(())
    }

    pub async fn stop_ringtone(&self, audio_id: String) -> Result<()> {
        let params = RpcMediaStream {
            id: audio_id.clone(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::StopRingtone,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&audio_id, &msg).await
    }

    pub async fn update_video_destination_layer(
        &self,
        src: String,
        dst: String,
        rid: Option<String>,
    ) -> Result<()> {
        let layer = RpcVideoDestinationLayer {
            src: src.clone(),
            dst,
            rid,
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::UpdateVideoDestinationLayer,
            params: serde_json::to_value(layer)?,
        };
        self.send_msg_to_media_stream(&src, &msg).await
    }

    pub async fn update_media_destinations(&self, stream_id: String) -> Result<()> {
        let session = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::UpdateMediaDestinations,
            params: serde_json::to_value(session)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn reinvite_crypto(
        &self,
        audio_id: String,
        local: String,
        remote: String,
    ) -> Result<()> {
        let params = RpcReinviteCrypto { local, remote };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::ReinviteCrypto,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&audio_id, &msg).await
    }

    pub async fn start_recording(
        &self,
        stream_id: String,
        cdr: String,
        expires: usize,
    ) -> Result<()> {
        let params = RpcRecording {
            stream_id: stream_id.clone(),
            cdr,
            expires,
        };
        let msg = RpcMessage {
            id: stream_id,
            method: RpcMethod::StartRecording,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(&msg).await
    }

    pub async fn stop_media_stream(
        &self,
        stream_id: String,
        reason: String,
        hangup: bool,
    ) -> Result<()> {
        let session = RpcStopSession { hangup, reason };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::StopSession,
            params: serde_json::to_value(session)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn stop_sound(&self, sound_id: &str, audio_id: String) -> Result<()> {
        let params = RpcSound {
            id: sound_id.to_string(),
            ..Default::default()
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::StopSound,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&audio_id, &msg).await
    }

    pub async fn set_peer_addr(
        &self,
        stream_id: String,
        peer_ip: Ipv4Addr,
        peer_port: u16,
    ) -> Result<()> {
        let params = StreamPeer { peer_ip, peer_port };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::SetPeerAddr,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn stop_ice(&self, ice_id: String) -> Result<()> {
        let msg = RpcMessage {
            id: ice_id,
            method: RpcMethod::StopIce,
            params: json!({}),
        };
        self.send_msg(&msg).await
    }

    pub async fn init_t38(
        &self,
        audio_id: String,
        t38_params: InitT38Params,
    ) -> Result<()> {
        let msg = RpcMessage {
            id: audio_id.clone(),
            method: RpcMethod::InitT38,
            params: serde_json::to_value(t38_params)?,
        };
        self.send_msg(&msg).await
    }

    pub async fn set_t38_passthrough(&self, audio_id: String) -> Result<()> {
        let params = RpcMediaStream {
            id: audio_id.clone(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::SetT38Passthrough,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&audio_id, &msg).await
    }

    pub async fn dtls_done(&self, stream_id: String) -> Result<()> {
        let params = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: stream_id.clone(),
            method: RpcMethod::DtlsDone,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn play_moh(
        &self,
        channel_id: String,
        audio_id: String,
        moh: MusicOnHold,
    ) -> Result<()> {
        let params = RpcMoh { channel_id, moh };
        let msg = RpcMessage {
            id: audio_id.clone(),
            method: RpcMethod::PlayMoh,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(&msg).await
    }

    pub async fn stop_moh(&self, audio_id: String) -> Result<()> {
        let params = RpcMediaStream {
            id: audio_id.clone(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::StopMoh,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&audio_id, &msg).await
    }

    pub async fn set_session_media_mode(
        &self,
        stream_id: String,
        mode: String,
    ) -> Result<()> {
        let params = SetMediaModeReq { mode };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::SetSessionMediaMode,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn play_sound(
        &self,
        id: String,
        channel_id: String,
        audio_id: String,
        sounds: Vec<String>,
        random: bool,
        repeat: u32,
        block: bool,
    ) -> Result<()> {
        let params = RpcSound {
            id,
            channel_id,
            sounds,
            random,
            repeat,
            block,
        };
        let msg = RpcMessage {
            id: audio_id.clone(),
            method: RpcMethod::PlaySound,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(&msg).await
    }

    pub async fn play_ringtone(
        &self,
        ringtone: String,
        channel_id: String,
        audio_id: String,
        sounds: Vec<String>,
        random: bool,
    ) -> Result<()> {
        let params = RpcRingtone {
            ringtone,
            channel_id,
            sounds,
            random,
        };
        let msg = RpcMessage {
            id: audio_id.clone(),
            method: RpcMethod::PlayRingtone,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(&msg).await
    }

    pub async fn update_sessoin(&self, port: u16) -> Result<()> {
        let req = RpcSession { port };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::UpdateSession,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(&msg).await
    }

    pub async fn record_to_redis(
        &self,
        audio_id: String,
        key: String,
        limit: usize,
        expire: u64,
    ) -> Result<()> {
        let params = RedisRecorder {
            limit,
            key,
            expire,
            pcm: Vec::new(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::RecordToRedis,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&audio_id, &msg).await
    }

    pub async fn stop_record_to_redis(
        &self,
        audio_id: String,
        key: String,
    ) -> Result<()> {
        let params = RedisRecorder {
            key,
            limit: 0,
            expire: 0,
            pcm: Vec::new(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::StopRecordToRedis,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&audio_id, &msg).await
    }

    pub async fn set_fax(&self, audio_id: String) -> Result<()> {
        let params = RpcMediaStream {
            id: audio_id.clone(),
        };
        let msg = RpcMessage {
            id: audio_id.clone(),
            method: RpcMethod::SetFax,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(&msg).await
    }

    pub async fn start_voicemail(
        &self,
        stream_id: String,
        cdr: String,
    ) -> Result<()> {
        let params = RpcRecording {
            stream_id: stream_id.clone(),
            cdr,
            expires: 0,
        };
        let msg = RpcMessage {
            id: stream_id.clone(),
            method: RpcMethod::StartVoicemail,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn pause_recording(&self, stream_id: String) -> Result<()> {
        let params = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: stream_id.clone(),
            method: RpcMethod::PauseRecording,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn resume_recording(&self, stream_id: String) -> Result<()> {
        let params = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: stream_id.clone(),
            method: RpcMethod::ResumeRecording,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn stop_record_to_file(
        &self,
        stream_id: String,
        uuid: String,
    ) -> Result<()> {
        let params = RpcFileRecorder { uuid };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::StopRecordToFile,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn record_to_file(
        &self,
        stream_id: String,
        uuid: String,
    ) -> Result<()> {
        let params = RpcFileRecorder { uuid };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::RecordToFile,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn upload_record_to_file(
        &self,
        stream_id: String,
        uuid: String,
    ) -> Result<()> {
        let params = RpcFileRecorder { uuid };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::UploadRecordToFile,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn set_active_speaker(
        &self,
        stream_id: String,
        active_video_id: Uuid,
        now: u64,
    ) -> Result<()> {
        let params = ActiveSpeaker {
            video_id: active_video_id,
            now,
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::SetActiveSpeaker,
            params: serde_json::to_value(params)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn update_video_sources(&self, stream_id: String) -> Result<()> {
        let session = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::UpdateVideoSources,
            params: serde_json::to_value(session)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn mute_stream(&self, stream_id: String) -> Result<()> {
        let session = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::MuteStream,
            params: serde_json::to_value(session)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn unmute_stream(&self, stream_id: String) -> Result<()> {
        let session = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::UnmuteStream,
            params: serde_json::to_value(session)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }

    pub async fn send_dtmf(&self, audio_id: String, dtmf: String) -> Result<()> {
        let req = RpcDtmf {
            audio_id: audio_id.clone(),
            dtmf,
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::SendDtmf,
            params: serde_json::to_value(req)?,
        };
        self.send_msg_to_media_stream(&audio_id, &msg).await
    }

    pub async fn stream_start_sending(&self, stream_id: String) -> Result<()> {
        let params = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: stream_id,
            method: RpcMethod::StreamStartSend,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(&msg).await
    }

    pub async fn request_keyframe(&self, stream_id: String) -> Result<()> {
        let session = RpcMediaStream {
            id: stream_id.clone(),
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::RequestKeyframe,
            params: serde_json::to_value(session)?,
        };
        self.send_msg_to_media_stream(&stream_id, &msg).await
    }
}
