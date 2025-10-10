use super::message::*;
use anyhow::{Error, Result};
use jsonrpc_lite::JsonRpc;
use nebula_db::message::{
    AnswerResponse, AudioLevelNotification, AvailableRateNotification,
    CallDirection, Endpoint, Hangup, InviteRequest, NewInboundChannelRequest,
    NewIntraRequest, NewInvite, NotifyEvent, ParkStatus, ProxyHangup, ReferRequest,
    ReferTo, Response, RoomMember, RpcChannel, RpcConvertToRoom, RpcDialog, RpcDtmf,
    RpcFinishRecording, RpcUpdateCallerid, RpcVoicemailNotification, SessionPeer,
    SetCryptoReq, TransferContext, VideoAllocation, VideoAllocationNotification,
};
use nebula_redis::REDIS;
use serde_json::{self, json};
use sip::message::{
    Address, CallerId, HangupReason, InterTenantInfo, Location, TeamsReferCx, Uri,
};
use std::{collections::HashMap, net::Ipv4Addr, time::Duration};

pub struct RpcClient {
    stream_name: String,
}

impl RpcClient {
    pub fn new(stream_name: String) -> Self {
        RpcClient { stream_name }
    }

    pub async fn send_to_stream(stream_name: &str, msg: &RpcMessage) -> Result<()> {
        REDIS
            .xadd_maxlen(
                stream_name,
                "message",
                &serde_json::to_string(&msg)?,
                1000000,
            )
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct ProxyRpcClient {
    client: reqwest::Client,
}

impl Default for ProxyRpcClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyRpcClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, proxy_host: &str) -> String {
        format!("http://{proxy_host}/internal/stream")
    }

    async fn send_msg(&self, proxy_host: &str, msg: &RpcMessage) -> Result<()> {
        if proxy_host.is_empty() {
            return Err(anyhow::anyhow!("proxy host is empty"));
        }
        self.client
            .post(self.url(proxy_host))
            .json(msg)
            .send()
            .await?;
        Ok(())
    }

    pub async fn set_response(
        &self,
        proxy_host: &str,
        id: String,
        code: i32,
        status: String,
        body: String,
    ) -> Result<()> {
        let req = Response {
            id: id.clone(),
            code,
            status,
            body,
            remote_ip: None,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::SetResponse,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await?;
        Ok(())
    }

    pub async fn answer(
        &self,
        proxy_host: &str,
        id: String,
        sdp: String,
        caller_id: Option<CallerId>,
        cdr_uuid: Option<String>,
    ) -> Result<(), Error> {
        let params = AnswerResponse {
            id: id.clone(),
            body: sdp,
            caller_id,
            cdr_uuid,
            remote_ip: None,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::Answer,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn notify_event(
        &self,
        proxy_host: &str,
        event: NotifyEvent,
    ) -> Result<(), Error> {
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::NotifyEvent,
            params: serde_json::to_value(event)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn sync_provisioning(
        &self,
        proxy_host: &str,
        location: Location,
    ) -> Result<(), Error> {
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::SyncProvisioning,
            params: serde_json::to_value(location)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn invite(
        &self,
        proxy_host: &str,
        id: String,
        reply_to: String,
        inbound_cdr: Option<String>,
        caller_id: CallerId,
        to: Location,
        internal_call: bool,
        alert_info_header: Option<String>,
        body: String,
        click_to_dial: bool,
        room: Option<String>,
        switchboard_host: String,
    ) -> Result<(), Error> {
        let req = NewInvite {
            id: id.clone(),
            reply_to,
            inbound_cdr,
            caller_id,
            to,
            internal_call,
            alert_info_header,
            body,
            click_to_dial,
            room,
            switchboard_host,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::Invite,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await?;
        Ok(())
    }

    pub async fn new_intra(
        &self,
        proxy_host: &str,
        id: String,
        endpoint: Endpoint,
        location: Location,
        reply_to: String,
        sdp: String,
        intra: String,
        intra_chain: Vec<String>,
    ) -> Result<()> {
        let req = NewIntraRequest {
            id: id.clone(),
            endpoint,
            reply_to,
            location,
            sdp,
            intra,
            intra_chain,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::NewIntra,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await?;
        Ok(())
    }

    pub async fn answer_intra(
        &self,
        proxy_host: &str,
        id: String,
        sdp: String,
    ) -> Result<()> {
        let req = AnswerResponse {
            id: id.clone(),
            body: sdp,
            caller_id: None,
            cdr_uuid: None,
            remote_ip: None,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::AnswerIntra,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn intra_session_progress(
        &self,
        proxy_host: &str,
        id: String,
        sdp: String,
    ) -> Result<()> {
        let req = AnswerResponse {
            id: id.clone(),
            body: sdp,
            caller_id: None,
            cdr_uuid: None,
            remote_ip: None,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::IntraSessoinProgress,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn hangup_intra(&self, proxy_host: &str, id: String) -> Result<()> {
        let req = ProxyHangup {
            id: id.clone(),
            code: None,
            redirect: None,
            reason: None,
            is_answered: false,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::ProxyHangupIntra,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn notify_refer_success(
        &self,
        proxy_host: &str,
        id: String,
    ) -> Result<()> {
        let params = RpcChannel { id: id.clone() };
        let msg = RpcMessage {
            id,
            method: RpcMethod::NotifyReferSuccess,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn notify_refer_trying(
        &self,
        proxy_host: &str,
        id: String,
    ) -> Result<()> {
        let params = RpcChannel { id: id.clone() };
        let msg = RpcMessage {
            id,
            method: RpcMethod::NotifyReferTrying,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn hangup(
        &self,
        proxy_host: &str,
        id: String,
        code: Option<usize>,
        redirect: Option<String>,
        reason: Option<HangupReason>,
        is_answered: bool,
    ) -> Result<()> {
        let req = ProxyHangup {
            id: id.clone(),
            code,
            redirect,
            reason,
            is_answered,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::ProxyHangup,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn convert_to_room(
        &self,
        proxy_host: &str,
        id: String,
        body: String,
        room: String,
        auth_token: String,
    ) -> Result<()> {
        let params = RpcConvertToRoom {
            id: id.clone(),
            room,
            auth_token,
            body,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::ConvertToRoom,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn update_caller_id(
        &self,
        proxy_host: &str,
        id: String,
        caller_id: CallerId,
        body: String,
    ) -> Result<()> {
        let params = RpcUpdateCallerid {
            id: id.clone(),
            caller_id,
            body,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::UpdateCallerid,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn reinvite(
        &self,
        proxy_host: &str,
        id: String,
        body: String,
        switchboard_host: String,
    ) -> Result<()> {
        let req = NewInvite {
            id: id.clone(),
            body,
            reply_to: "".to_string(),
            inbound_cdr: None,
            caller_id: CallerId {
                user: "".to_string(),
                e164: None,
                display_name: "".to_string(),
                anonymous: false,
                asserted_identity: None,
                original_number: None,
                to_number: None,
            },
            to: Location {
                req_uri: Uri::default(),
                dst_uri: Uri::default(),
                proxy_host: "".to_string(),
                user: "".to_string(),
                from_host: "".to_string(),
                to_host: "".to_string(),
                display_name: None,
                intra: false,
                inter_tenant: None,
                encryption: false,
                route: None,
                device: None,
                device_token: None,
                dnd: false,
                dnd_allow_internal: false,
                is_registration: None,
                provider: None,
                user_agent: None,
                tenant_id: None,
                teams_refer_cx: None,
                ziron_tag: None,
                diversion: None,
                pai: None,
                global_dnd: false,
                global_forward: None,
                room_auth_token: None,
            },
            internal_call: false,
            alert_info_header: None,
            click_to_dial: false,
            switchboard_host,
            room: None,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::ReInvite,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn ack(&self, proxy_host: &str, id: String) -> Result<(), Error> {
        let req = RpcChannel { id: id.clone() };
        let msg = RpcMessage {
            id,
            method: RpcMethod::Ack,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn notify_voicemail(
        &self,
        location: Location,
        new: usize,
        old: usize,
    ) -> Result<()> {
        let proxy_host = location.proxy_host.clone();
        let params = RpcVoicemailNotification { location, new, old };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::NotifyVoicemail,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(&proxy_host, &msg).await
    }

    pub async fn options_mobile_app(
        &self,
        proxy_host: &str,
        location: Location,
    ) -> Result<()> {
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::OptionsMobileApp,
            params: serde_json::to_value(location)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn publish_park(
        &self,
        proxy_host: &str,
        uuid: String,
        slot: String,
        parked: bool,
        direction: CallDirection,
        from: String,
        to: String,
    ) -> Result<(), Error> {
        let req = ParkStatus {
            uuid: uuid.clone(),
            slot,
            parked,
            direction,
            from,
            to,
        };
        let msg = RpcMessage {
            id: uuid,
            method: RpcMethod::PublishPark,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(proxy_host, &msg).await
    }
}

pub struct SwitchboardRpcClient {
    pub client: reqwest::Client,
}

impl Default for SwitchboardRpcClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SwitchboardRpcClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, switchboard_host: &str) -> String {
        format!("http://{switchboard_host}/internal/stream")
    }

    async fn send_msg(
        &self,
        switchboard_host: &str,
        msg: &RpcMessage,
    ) -> Result<()> {
        if switchboard_host.is_empty() {
            return Err(anyhow::anyhow!("proxy host is empty"));
        }
        self.client
            .post(self.url(switchboard_host))
            .json(msg)
            .send()
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_inbound_channel(
        &self,
        switchboard_host: &str,
        id: String,
        endpoint: &Endpoint,
        proxy_host: &str,
        to: &str,
        sdp: &str,
        existing: bool,
        replaces: Option<String>,
        callflow: Option<(String, String)>,
        intra: Option<String>,
        intra_chain: Vec<String>,
        inter_tenant: Option<InterTenantInfo>,
        reply_to: Option<String>,
        raw_transfer: Option<(String, bool, bool)>,
        transfer: Option<TransferContext>,
        teams_refer_cx: Option<TeamsReferCx>,
        remote_ip: Option<String>,
    ) -> Result<()> {
        let req = NewInboundChannelRequest {
            id: id.clone(),
            from: endpoint.clone(),
            proxy_host: proxy_host.to_string(),
            to: to.to_string(),
            sdp: sdp.to_string(),
            existing,
            refer: None,
            transfer,
            raw_transfer,
            intra,
            intra_chain,
            inter_tenant,
            redirect: None,
            reply_to,
            replaces,
            callflow,
            teams_refer_cx,
            remote_ip,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::NewInboundChannel,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn reinvite(
        &self,
        switchboard_host: &str,
        id: String,
        body: String,
    ) -> Result<()> {
        let req = InviteRequest {
            id: id.clone(),
            from: "".to_string(),
            to: Location {
                req_uri: Uri::default(),
                dst_uri: Uri::default(),
                proxy_host: "".to_string(),
                user: "".to_string(),
                from_host: "".to_string(),
                to_host: "".to_string(),
                display_name: None,
                intra: false,
                inter_tenant: None,
                encryption: false,
                route: None,
                device: None,
                device_token: None,
                dnd: false,
                dnd_allow_internal: false,
                provider: None,
                is_registration: None,
                user_agent: None,
                tenant_id: None,
                teams_refer_cx: None,
                ziron_tag: None,
                diversion: None,
                pai: None,
                global_dnd: false,
                global_forward: None,
                room_auth_token: None,
            },
            body,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::ReInvite,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn set_response(
        &self,
        switchboard_host: &str,
        id: String,
        code: i32,
        status: String,
        body: String,
        remote_ip: Option<String>,
    ) -> Result<()> {
        let req = Response {
            id: id.clone(),
            code,
            status,
            body,
            remote_ip,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::SetResponse,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await?;
        Ok(())
    }

    pub async fn answer(
        &self,
        switchboard_host: &str,
        id: String,
        sdp: String,
        caller_id: Option<CallerId>,
        cdr_uuid: Option<String>,
        remote_ip: Option<String>,
    ) -> Result<()> {
        let params = AnswerResponse {
            id: id.clone(),
            body: sdp,
            caller_id,
            cdr_uuid,
            remote_ip,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::Answer,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(switchboard_host, &msg).await?;
        Ok(())
    }

    pub async fn request_keyframe(
        &self,
        switchboard_host: &str,
        id: String,
    ) -> Result<()> {
        let req = RpcChannel { id: id.clone() };
        let msg = RpcMessage {
            id,
            method: RpcMethod::RequestKeyframe,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn refer(
        &self,
        switchboard_host: &str,
        id: String,
        to: ReferTo,
        refer_to: Address,
        refer_by: Option<Address>,
    ) -> Result<()> {
        let params = ReferRequest {
            id: id.clone(),
            to,
            refer_to,
            refer_by,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::Refer,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn cancel(
        &self,
        switchboard_host: &str,
        id: String,
    ) -> Result<(), Error> {
        let req = RpcChannel { id: id.clone() };
        let msg = RpcMessage {
            id,
            method: RpcMethod::Cancel,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn hangup(
        &self,
        switchboard_host: &str,
        id: String,
        from_proxy: bool,
        code: Option<usize>,
        redirect: Option<String>,
        reason: Option<HangupReason>,
    ) -> Result<()> {
        let req = Hangup {
            id: id.clone(),
            from_proxy,
            code,
            redirect,
            reason,
        };
        let msg = RpcMessage {
            id,
            method: RpcMethod::Hangup,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn ack(
        &self,
        switchboard_host: &str,
        id: String,
    ) -> Result<(), Error> {
        let req = RpcChannel { id: id.clone() };
        let msg = RpcMessage {
            id,
            method: RpcMethod::Ack,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn room_rpc(
        &self,
        switchboard_host: &str,
        rpc: JsonRpc,
        addr: String,
        proxy_host: &str,
    ) -> Result<()> {
        let room_rpc = RoomRpcMessage { rpc, addr };
        let msg = RpcMessage {
            id: proxy_host.to_string(),
            method: RpcMethod::RoomRpc,
            params: serde_json::to_value(room_rpc)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn receive_dtmf(
        &self,
        switchboard_host: &str,
        id: String,
        audio_id: String,
        dtmf: String,
    ) -> Result<(), Error> {
        let req = RpcDtmf { dtmf, audio_id };
        let msg = RpcMessage {
            id,
            method: RpcMethod::ReceiveDtmf,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn notify_audio_level(
        &self,
        switchboard_host: &str,
        room: String,
        member: String,
        level: u8,
    ) -> Result<()> {
        let params = AudioLevelNotification {
            room,
            member,
            level,
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::NotifyAudioLevel,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn notify_video_allocations(
        &self,
        switchboard_host: &str,
        room: String,
        member: String,
        allocations: HashMap<String, VideoAllocation>,
    ) -> Result<()> {
        let params = VideoAllocationNotification {
            room,
            member,
            allocations,
        };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::NotifyVideoAllocation,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn finish_call_recording(
        &self,
        switchboard_host: &str,
        cdr: String,
    ) -> Result<(), Error> {
        let req = RpcFinishRecording { cdr };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::FinishCallRecording,
            params: serde_json::to_value(req)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }

    pub async fn notify_available_rate(
        &self,
        switchboard_host: &str,
        room: String,
        member: String,
        rate: u64,
    ) -> Result<()> {
        let params = AvailableRateNotification { room, member, rate };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::NotifyAvailableRate,
            params: serde_json::to_value(params)?,
        };
        self.send_msg(switchboard_host, &msg).await
    }
}

pub struct RoomRpcClient {
    client: reqwest::Client,
}

impl Default for RoomRpcClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomRpcClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, proxy_host: &str) -> String {
        format!("http://{proxy_host}/internal/stream")
    }

    async fn send_msg(&self, proxy_host: &str, msg: &RpcMessage) -> Result<()> {
        if proxy_host.is_empty() {
            return Err(anyhow::anyhow!("proxy host is empty"));
        }
        self.client
            .post(self.url(proxy_host))
            .json(msg)
            .send()
            .await?;
        Ok(())
    }

    pub async fn room_rpc(
        &self,
        proxy_host: &str,
        rpc: JsonRpc,
        addr: String,
    ) -> Result<()> {
        let room_rpc = RoomRpcMessage { rpc, addr };
        let msg = RpcMessage {
            id: "".to_string(),
            method: RpcMethod::RoomRpc,
            params: serde_json::to_value(room_rpc)?,
        };
        self.send_msg(proxy_host, &msg).await
    }

    pub async fn room_active_speaker(
        &self,
        proxy_host: &str,
        member: String,
        addr: String,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::RoomActiveSpeaker.to_string(),
            json!({
                "member": member,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn room_clear_active_speaker(
        &self,
        proxy_host: &str,
        addr: String,
    ) -> Result<()> {
        let rpc = JsonRpc::notification(
            &RoomServerNotificationMethod::RoomClearActiveSpeaker.to_string(),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn notifiy_member_rejected(
        &self,
        proxy_host: &str,
        member: &str,
        addr: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberWaitingRejected.to_string(),
            serde_json::json!({
                "member": member,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr.to_string()).await
    }

    pub async fn notifiy_member_allowed(
        &self,
        proxy_host: &str,
        member: &str,
        addr: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberWaitingAllowed.to_string(),
            serde_json::json!({
                "member": member,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr.to_string()).await
    }

    pub async fn notifiy_member_waiting(
        &self,
        proxy_host: &str,
        member: &str,
        name: &str,
        user: Option<String>,
        addr: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberWaiting.to_string(),
            serde_json::json!({
                "member": member,
                "name": name,
                "user": user,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr.to_string()).await
    }

    pub async fn notifiy_room_rejected(
        &self,
        proxy_host: &str,
        addr: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberRejected.to_string(),
            serde_json::json!({}),
        );
        self.room_rpc(proxy_host, rpc, addr.to_string()).await
    }

    pub async fn notifiy_room_allowed(
        &self,
        proxy_host: &str,
        auth_token: &str,
        addr: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberAllowed.to_string(),
            serde_json::json!({
                "auth_token": auth_token,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr.to_string()).await
    }

    pub async fn notify_room_invite(
        &self,
        proxy_host: &str,
        room: &str,
        call_id: &str,
        from_user: &str,
        auth_token: &str,
        addr: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::RoomInvite.to_string(),
            serde_json::json!({
                "room": room,
                "call_id": call_id,
                "from_user": from_user,
                "auth_token": auth_token,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr.to_string()).await
    }

    pub async fn notify_room_invite_completed_elsewhere(
        &self,
        proxy_host: &str,
        room: &str,
        addr: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::RoomInviteCompletedElsewhere.to_string(),
            serde_json::json!({
                "room": room,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr.to_string()).await
    }

    pub async fn reject_room_invite(
        &self,
        proxy_host: &str,
        room: &str,
        call_id: &str,
        addr: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::RoomInviteReject.to_string(),
            serde_json::json!({
                "room": room,
                "call_id": call_id,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr.to_string()).await
    }

    pub async fn member_join_room(
        &self,
        proxy_host: &str,
        member: RoomMember,
        addr: String,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberJoinRoom.to_string(),
            serde_json::to_value(member)?,
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_leave_room(
        &self,
        proxy_host: &str,
        member: String,
        addr: String,
        by_admin: bool,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberLeaveRoom.to_string(),
            json!({
                "member": member,
                "by_admin": by_admin,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_leave_waiting_room(
        &self,
        proxy_host: &str,
        member: String,
        addr: String,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberLeaveWaitingRoom.to_string(),
            json!({
                "member": member,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_mute_audio(
        &self,
        proxy_host: &str,
        member: &str,
        addr: String,
        by_admin: bool,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberMuteAudio.to_string(),
            json!({
                "member": member,
                "by_admin": by_admin,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_unmute_audio(
        &self,
        proxy_host: &str,
        member: &str,
        addr: String,
        by_admin: bool,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberUnmuteAudio.to_string(),
            json!({
                "member": member,
                "by_admin": by_admin,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_mute_video(
        &self,
        proxy_host: &str,
        member: &str,
        addr: String,
        by_admin: bool,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberMuteVideo.to_string(),
            json!({
                "member": member,
                "by_admin": by_admin,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_unmute_video(
        &self,
        proxy_host: &str,
        member: &str,
        addr: String,
        by_admin: bool,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberUnmuteVideo.to_string(),
            json!({
                "member": member,
                "by_admin": by_admin,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_start_screen_share(
        &self,
        proxy_host: &str,
        member: String,
        addr: String,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberStartScreenShare.to_string(),
            json!({
                "member": member,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_stop_screen_share(
        &self,
        proxy_host: &str,
        member: String,
        addr: String,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberStopScreenShare.to_string(),
            json!({
                "member": member,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_raise_hand(
        &self,
        proxy_host: &str,
        member: String,
        addr: String,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberRaiseHand.to_string(),
            json!({
                "member": member,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_unraise_hand(
        &self,
        proxy_host: &str,
        member: String,
        addr: String,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberUnraiseHand.to_string(),
            json!({
                "member": member,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_send_reaction(
        &self,
        proxy_host: &str,
        member: &str,
        addr: String,
        reaction: &str,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberSendReaction.to_string(),
            json!({
                "member": member,
                "reaction": reaction,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_video_allocations(
        &self,
        proxy_host: &str,
        addr: String,
        allocations: HashMap<String, VideoAllocation>,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberVideoAllocations.to_string(),
            json!(allocations),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_audio_level(
        &self,
        proxy_host: &str,
        member: &str,
        addr: String,
        level: u8,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberAudioLevel.to_string(),
            json!({
                "member": member,
                "audio_level": level,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }

    pub async fn member_available_rate(
        &self,
        proxy_host: &str,
        addr: String,
        rate: u64,
    ) -> Result<()> {
        let rpc = JsonRpc::notification_with_params(
            &RoomServerNotificationMethod::MemberAvailableRate.to_string(),
            json!({
                "rate": rate,
            }),
        );
        self.room_rpc(proxy_host, rpc, addr).await
    }
}

lazy_static::lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::new();
}

pub async fn notify_dialog(proxy_hosts: Vec<String>, id: String) -> Result<()> {
    {
        let user_uuid = id.clone();
        tokio::spawn(async move {
            let _ = HTTP_CLIENT
                .get(&format!(
                    "http://10.132.0.36:8123/v1/sip_users/{}/notify_presence",
                    user_uuid
                ))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        });
    }

    let params = RpcDialog { id: id.clone() };
    let msg = RpcMessage {
        id,
        method: RpcMethod::NotifyDialog,
        params: serde_json::to_value(params)?,
    };

    for proxy_host in proxy_hosts {
        let msg = msg.clone();
        tokio::spawn(async move {
            if proxy_host.is_empty() {
                return Err(anyhow::anyhow!("proxy host is empty"));
            }
            HTTP_CLIENT
                .post(format!("http://{proxy_host}/internal/stream"))
                .json(&msg)
                .send()
                .await?;
            Ok(())
        });
    }
    Ok(())
}
