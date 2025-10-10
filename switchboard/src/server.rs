use crate::channel::CreateOutboundCall;
use crate::room::Room;

use super::channel::Channel;
use anyhow::{anyhow, Error, Result};
use axum::extract::{Path, Query};
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use futures::TryStreamExt;
use lazy_static::lazy_static;
use nebula_db::api::{is_channel_answered, Database};
use nebula_db::message::{
    AbandonedQueueRecordCalledback, AnswerResponse, AudioLevelNotification,
    AvailableRateNotification, ChannelEvent, DialogInfo, Hangup, InviteRequest,
    NewInboundChannelRequest, NotifyEvent, ReferRequest, Response, RoomMember,
    RpcDtmf, RpcFinishRecording, SendReaction, UserPresence, UserRegistration,
    VideoAllocationNotification,
};
use nebula_db::models::{
    AccountQueueStatsResponse, AuxCodeResponse, CdrEvent, CdrItemChange,
    IndividualQueueLoginResponse, NewCdrItemFull, QueueLoginResponse, QueueRecord,
};
use nebula_db::usrloc::{Usrloc, UsrlocData};
use nebula_redis::REDIS;
use nebula_rpc::client::{
    notify_dialog, ProxyRpcClient, RoomRpcClient, RpcClient, SwitchboardRpcClient,
};
use nebula_rpc::media::MediaRpcClient;
use nebula_rpc::message::*;
use nebula_rpc::server::RpcServer;
use nebula_utils::sha256;
use reqwest::StatusCode;
use sdp::sdp::SessionDescription;
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use storagev1::StorageClient;
use thiserror::Error;
use tokio;
use tokio::fs::File;
use tokio::io::BufWriter;
use tokio_util::io::StreamReader;
use tracing::info;
use yaypi::api::YaypiClient;

lazy_static! {
    pub static ref SWITCHBOARD_SERVICE: SwitchboardService =
        SwitchboardService::new().unwrap();
}

#[derive(Debug, Error)]
pub enum RpcServerError {
    #[error("no redis host")]
    NoRedisHost,

    #[error("no db host")]
    NoDBHost,
    #[error("no ip")]
    NoIP,
}

#[derive(Deserialize)]
pub struct Config {
    pub media_ip: Ipv4Addr,
    pub redis: String,
    pub db: String,
    pub proxy_host: String,
    default_proxy_host: String,
    pub au_proxy_host: String,
    pub switchboard_host: String,
    pub media_host: String,
    pub proxies: Option<Vec<String>>,
    // force the `london_proxy_host` to go through default_proxy_host
    // this is useful for local test enviroments where the local IP can't be
    // authed by the providers
    force_default_proxy_host: Option<bool>,
    all_proxies: Option<Vec<String>>,
}

impl Config {
    /// The outbound calls needs to go out from the london proxy
    /// because the source IP at the providers are london proxy lb IP
    /// Also calls to mobile apps and Teams need to go through london proxy
    /// as well
    pub fn london_proxy_host(&self) -> &str {
        if self.force_default_proxy_host == Some(true) {
            &self.default_proxy_host
        } else if self.proxy_host.starts_with("10.154") {
            // the IP starts with 10.154 is in London cluter, so just use the
            // local proxy host
            &self.proxy_host
        } else {
            &self.default_proxy_host
        }
    }

    pub fn all_proxies(&self) -> Option<&[String]> {
        self.all_proxies.as_deref().or(self.proxies.as_deref())
    }
}

pub struct SwitchboardService {
    pub config: Config,
    pub media_rpc_client: MediaRpcClient,
    pub proxy_rpc: ProxyRpcClient,
    pub room_rpc: RoomRpcClient,
    pub switchboard_rpc: SwitchboardRpcClient,
    pub yaypi_client: YaypiClient,
    pub db: Database,
}

#[derive(Deserialize)]
struct UserRegistrationUpdate {
    pub dnd: bool,
    pub dnd_allow_internal: bool,
}

#[derive(Deserialize)]
struct RegistrationNotify {
    pub event: String,
    pub content: String,
    pub content_type: Option<String>,
}

#[derive(Deserialize)]
struct NewVoicemail {
    voicemail_user: String,
    caller: String,
    callee: String,
    duration: i64,
    time: u64,
}

#[derive(Deserialize)]
struct SetAuxCodeRequest {
    code: String,
    set_by: Option<String>,
}

#[derive(Deserialize)]
struct SetQueueLoginRequest {
    login: bool,
    set_by: Option<String>,
    queue_id: Option<String>,
}

#[derive(Deserialize)]
struct AbandonedQueueRecordCalledbackQuery {
    // limit of items for one page, default to 1000, max 1000
    limit: Option<u64>,
    // page number of the result, default to 1
    page: Option<u64>,
    // start time of the queue records in RFC 3339 format
    start: String,
    // end time of the queue records in RFC 3339 format
    end: Option<String>,
    // the timeout of a call is seen as called back, in seconds, e.g. 3600, 86400
    timeout: i32,
    // whether the called back needs to be answered
    needs_to_be_answered: bool,
}

#[derive(Serialize)]
struct AbandonedQueueRecordResponse {
    // total number of records
    total_records: u64,
    // limit of items for one page
    limit: u64,
    // page number of the result
    page: u64,
    // the records
    records: Vec<AbandonedQueueRecord>,
}

#[derive(Serialize)]
struct AbandonedQueueRecord {
    // the number that should be called back, in E164 format
    number: String,
    // If the call was called back, the detail of it
    called_back: Option<AbandonedQueueRecordCalledBack>,

    // queue record columns
    uuid: String,
    queue_id: String,
    start: String,
    cdr: Option<String>,
    disposition: Option<String>,
    duration: Option<i64>,
    call_duration: Option<i64>,
    wait_duration: Option<i64>,
    ring_duration: Option<i64>,
    answer_by: Option<String>,
    answer: Option<String>,
    end: Option<String>,
    ready: Option<String>,
    cdr_start: Option<String>,
    cdr_wait: Option<i64>,
    group_id: Option<String>,
    failed_times: Option<i64>,
    init_pos: Option<i64>,
    hangup_pos: Option<i64>,
}

#[derive(Serialize)]
struct AbandonedQueueRecordCalledBack {
    // the cdr id of the called back call
    cdr: String,
    // the start time of the called back call
    time: String,
}

pub struct Server {
    hostname: String,
}

#[derive(Deserialize, Debug)]
struct QueueLoginUsersRequest {
    tenant_uuid: String,
    queues: Vec<String>,
}

impl SwitchboardService {
    pub fn new() -> Result<SwitchboardService> {
        let contents = fs::read_to_string("/etc/nebula/nebula.conf")?;
        let config: Config = toml::from_str(&contents)?;

        let db = Database::new(config.db.as_ref())?;

        let media_rpc_client = MediaRpcClient::new();
        let proxy_rpc = ProxyRpcClient::new();
        let switchboard_rpc = SwitchboardRpcClient::new();
        let room_rpc = RoomRpcClient::new();

        Ok(SwitchboardService {
            config,
            db,
            proxy_rpc,
            room_rpc,
            switchboard_rpc,
            media_rpc_client,
            yaypi_client: YaypiClient::new(),
        })
    }
}

impl Server {
    pub async fn new() -> Result<Arc<Server>, Error> {
        let hostname = fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap_or_else(|_| "".to_string());

        let server = Arc::new(Server { hostname });
        Ok(server)
    }

    pub async fn process_rpc(rpc_msg: RpcMessage) -> Result<(), Error> {
        let channel = Channel {
            id: rpc_msg.id.clone(),
        };
        let params = rpc_msg.params;
        match rpc_msg.method {
            RpcMethod::NewInboundChannel => {
                let request: NewInboundChannelRequest =
                    serde_json::from_value(params)?;
                if request.existing {
                    let channel = Channel::new(request.id.clone());
                    let mut context = channel.get_call_context().await?;
                    if context.cdr.is_none() {
                        context.cdr =
                            Some(SWITCHBOARD_SERVICE.db.create_cdr().await?.uuid);
                        channel.set_call_context(&context).await?;
                    }
                    channel.switch(request).await?;
                    return Ok(());
                }
                let channel = Channel::new_inbound(&request).await?;
                channel.switch(request).await?;
            }
            RpcMethod::ReInvite => {
                let request: InviteRequest = serde_json::from_value(params)?;
                channel.reinvite(request).await?;
            }
            RpcMethod::Refer => {
                let request: ReferRequest = serde_json::from_value(params)?;
                channel.refer(request).await?;
            }
            RpcMethod::ReceiveDtmf => {
                let request: RpcDtmf = serde_json::from_value(params)?;
                channel.receive_dtmf(request.dtmf).await?;
            }
            RpcMethod::FinishCallRecording => {
                let request: RpcFinishRecording = serde_json::from_value(params)?;
                if let Ok(uuid) = uuid::Uuid::from_str(&request.cdr) {
                    if let Some(cdr) = SWITCHBOARD_SERVICE.db.get_cdr(uuid).await {
                        let _ = SWITCHBOARD_SERVICE
                            .yaypi_client
                            .call_recording_finished(&request.cdr, &cdr.tenant_id)
                            .await;
                        tokio::spawn(async move {
                            let _ = SWITCHBOARD_SERVICE
                                .db
                                .update_cdr(
                                    uuid,
                                    CdrItemChange {
                                        recording_uploaded: Some(true),
                                        ..Default::default()
                                    },
                                )
                                .await;
                        });
                    }
                }
            }
            RpcMethod::Ack => {
                let _ = REDIS.hdel(&channel.redis_key(), "expect_ack").await;
                let _ = channel.stream_start_send().await;
                channel.new_event(ChannelEvent::AnswerAck, "ack").await?;
            }
            RpcMethod::RequestKeyframe => {
                channel.request_keyframe().await?;
            }
            RpcMethod::Answer => {
                let response: AnswerResponse = serde_json::from_value(params)?;
                let mut sdp = SessionDescription::from_str(&response.body)?;
                let is_ip_global = Ipv4Addr::from_str(&sdp.connection.addr)
                    .map(|ip| is_ip_global(&ip))
                    .unwrap_or(false);
                println!("is_ip_gloal {} {is_ip_global}", sdp.connection.addr);
                if !is_ip_global {
                    if let Some(remote_ip) = &response.remote_ip {
                        println!("set sdp to {remote_ip}");
                        sdp.connection.addr = remote_ip.clone();
                    }
                }
                channel.handle_answer(&sdp).await?;
            }
            RpcMethod::SetResponse => {
                let response: Response = serde_json::from_value(params)?;
                channel.handle_response(&response).await?;
            }
            RpcMethod::Hangup => {
                println!("receive rpc hangup");
                let hangup: Hangup = serde_json::from_value(params)?;
                channel
                    .handle_hangup(
                        hangup.from_proxy,
                        hangup.code,
                        hangup.redirect.clone(),
                    )
                    .await?;
            }
            RpcMethod::Cancel => {
                println!("receive rpc hangup");
                channel.handle_cancel().await?;
            }
            RpcMethod::RoomRpc => {
                let request: RoomRpcMessage = serde_json::from_value(params)?;
                Room::handle_rpc(rpc_msg.id, request).await?;
            }
            RpcMethod::NotifyVideoAllocation => {
                let allocation: VideoAllocationNotification =
                    serde_json::from_value(params)?;
                Room::new(allocation.room)
                    .handle_video_allocation(
                        allocation.member,
                        allocation.allocations,
                    )
                    .await?;
            }
            RpcMethod::NotifyAudioLevel => {
                let audio_level: AudioLevelNotification =
                    serde_json::from_value(params)?;
                Room::new(audio_level.room)
                    .handle_audio_level(audio_level.member, audio_level.level)
                    .await?;
            }
            RpcMethod::NotifyAvailableRate => {
                let available_rate: AvailableRateNotification =
                    serde_json::from_value(params)?;
                Room::new(available_rate.room)
                    .handle_available_rate(
                        available_rate.member,
                        available_rate.rate,
                    )
                    .await?;
            }
            RpcMethod::SendReaction => {
                let request: SendReaction = serde_json::from_value(params)?;
                let channel = Channel::new(request.call_id);
            }
            _ => {
                println!("got unhandled rpc msg method {}", rpc_msg.method);
            }
        }
        Ok(())
    }

    pub async fn run(_server: Arc<Server>) -> Result<(), Error> {
        tokio::spawn(async move {
            let _ = nebula_db::api::Database::monitor_cache().await;
        });

        tokio::spawn(async move {
            crate::channel::monitor_channels().await;
        });

        tokio::spawn(async move {
            crate::channel::report_channels().await;
        });

        tokio::spawn(async move {
            crate::room::Room::start().await;
        });

        {
            let mut receiver = RpcServer::new(
                SWITCHBOARD_STREAM.to_string(),
                SWITCHBOARD_GROUP.to_string(),
            )
            .await;
            tokio::spawn(async move {
                loop {
                    if let Some((entry, rpc_msg)) = receiver.recv().await {
                        tokio::spawn(async move {
                            entry.ack().await;
                            let start = std::time::Instant::now();
                            let result = Server::process_rpc(rpc_msg.clone()).await;
                            if result.is_err()
                                || (rpc_msg.method
                                    != RpcMethod::NotifyVideoAllocation
                                    && rpc_msg.method
                                        != RpcMethod::NotifyAvailableRate
                                    && rpc_msg.method != RpcMethod::NotifyAudioLevel)
                            {
                                info!("switchboard finished ({} ms) proccess rpc {rpc_msg:?} result {result:?}", start.elapsed().as_millis());
                            }
                        });
                    }
                }
            });
        }

        async fn health_check() -> &'static str {
            "ok"
        }

        async fn user_app_update(
            Path(user_uuid): Path<String>,
            Json(app): Json<serde_json::Value>,
        ) -> Result<&'static str, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;

            let status = app
                .get("status")
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?
                .as_str()
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let device = app
                .get("device")
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?
                .as_str()
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let device_token = app
                .get("device_token")
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?
                .as_str()
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            user.update_app_status(device, device_token, status).await;
            Ok("ok")
        }

        async fn create_call(
            Json(call): Json<CreateCall>,
        ) -> axum::http::StatusCode {
            tokio::spawn(async move {
                let _ = Channel::create_call(call).await;
            });
            axum::http::StatusCode::CREATED
        }

        async fn transfer_call(
            Json(call): Json<TransferCallRequest>,
        ) -> Result<axum::http::StatusCode, axum::http::StatusCode> {
            let channel = Channel::new(call.call_id);
            channel
                .transfer_call(
                    &call.target,
                    call.auto_answer.unwrap_or(true),
                    false,
                    "",
                )
                .await
                .map_err(|e| {
                    println!("transfer call error: {e}");
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                })?;
            Ok(axum::http::StatusCode::CREATED)
        }

        async fn create_outbound_call(
            Json(call): Json<CreateOutboundCall>,
        ) -> (axum::http::StatusCode, String) {
            match Channel::create_outbound_call(call).await {
                Ok(_) => (axum::http::StatusCode::CREATED, "OK".to_string()),
                Err(e) => (axum::http::StatusCode::BAD_REQUEST, e.to_string()),
            }
        }

        async fn user_dialogs(
            Path(user_uuid): Path<String>,
        ) -> Result<Json<Vec<DialogInfo>>, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let dialogs = nebula_db::api::user_dialogs(&user.uuid)
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
            Ok(Json::from(dialogs))
        }

        async fn parking_slot_presence(
            Path(slot): Path<String>,
        ) -> Result<Json<HashMap<String, String>>, axum::http::StatusCode> {
            let key = format!("nebula:park:{}", slot);
            let exits = REDIS
                .exists(&format!("nebula:park:{}", slot))
                .await
                .unwrap_or(false);
            if !exits {
                return Err(axum::http::StatusCode::NOT_FOUND);
            }

            let details: HashMap<String, String> = REDIS
                .hgetall(&format!("nebula:park:{}:details", slot))
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

            Ok(Json::from(details))
        }

        async fn get_user_aux_code(
            Path(user_uuid): Path<String>,
        ) -> Result<Json<AuxCodeResponse>, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let code = user
                .current_aux_code()
                .await
                .ok_or(axum::http::StatusCode::NOT_FOUND)?;

            Ok(Json(AuxCodeResponse {
                set_by: code.set_by,
                code: code.code,
                time: code.time.to_rfc3339(),
            }))
        }

        async fn get_individual_queue_login(
            Path((user_uuid, queue_id)): Path<(String, String)>,
        ) -> Result<Json<IndividualQueueLoginResponse>, axum::http::StatusCode>
        {
            let _user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let login = SWITCHBOARD_SERVICE
                .db
                .user_last_individual_queue_login(&user_uuid, &queue_id)
                .await
                .ok_or(axum::http::StatusCode::NOT_FOUND)?;

            Ok(Json(IndividualQueueLoginResponse {
                set_by: login.set_by,
                login: login.login,
                time: login.time.to_rfc3339(),
                queue_id,
            }))
        }

        async fn get_list_queue_login(
            Path(user_uuid): Path<String>,
            Json(queues): Json<Vec<String>>,
        ) -> Result<Json<Vec<IndividualQueueLoginResponse>>, axum::http::StatusCode>
        {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;

            let mut res: Vec<IndividualQueueLoginResponse> = vec![];

            for queue_id in queues {
                let last_login_data = match SWITCHBOARD_SERVICE
                    .db
                    .user_last_individual_queue_login(&user.uuid, &queue_id)
                    .await
                {
                    Some(last_login) => IndividualQueueLoginResponse {
                        set_by: last_login.set_by,
                        login: last_login.login,
                        time: last_login.time.to_rfc3339(),
                        queue_id,
                    },
                    None => IndividualQueueLoginResponse {
                        set_by: "".to_string(),
                        login: true,
                        time: "".to_string(),
                        queue_id,
                    },
                };

                res.push(last_login_data);
            }

            Ok(Json(res))
        }

        async fn get_user_queue_login_log(
            Path(user_uuid): Path<String>,
            Query(params): Query<HashMap<String, String>>,
        ) -> Result<Json<Vec<QueueLoginResponse>>, axum::http::StatusCode> {
            let _user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let start = params
                .get("start")
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let start = chrono::DateTime::parse_from_rfc3339(start)
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;

            let end = params
                .get("end")
                .and_then(|end| chrono::DateTime::parse_from_rfc3339(end).ok());

            let mut logs = SWITCHBOARD_SERVICE
                .db
                .get_queue_login_log(&user_uuid, start, end)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|queue_login| QueueLoginResponse {
                    set_by: queue_login.set_by,
                    login: queue_login.login,
                    time: queue_login.time.to_rfc3339(),
                    queue_id: queue_login.queue_id,
                })
                .collect::<Vec<_>>();

            if params.get("include_last").is_some_and(|val| val.eq("yes")) {
                if let Some(queue_login) = SWITCHBOARD_SERVICE
                    .db
                    .get_last_queue_login_log(&user_uuid, start)
                    .await
                {
                    logs.push(QueueLoginResponse {
                        set_by: queue_login.set_by,
                        login: queue_login.login,
                        time: queue_login.time.to_rfc3339(),
                        queue_id: queue_login.queue_id,
                    })
                }
            }

            Ok(Json(logs))
        }

        async fn get_user_aux_code_log(
            Path(user_uuid): Path<String>,
            Query(params): Query<HashMap<String, String>>,
        ) -> Result<Json<Vec<AuxCodeResponse>>, axum::http::StatusCode> {
            let _user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let start = params
                .get("start")
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let start = chrono::DateTime::parse_from_rfc3339(start)
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;

            let end = params
                .get("end")
                .and_then(|end| chrono::DateTime::parse_from_rfc3339(end).ok());

            let mut logs = SWITCHBOARD_SERVICE
                .db
                .get_aux_log(&user_uuid, start, end)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|code| AuxCodeResponse {
                    set_by: code.set_by,
                    code: code.code,
                    time: code.time.to_rfc3339(),
                })
                .collect::<Vec<_>>();

            if params.get("include_last").is_some_and(|val| val.eq("yes")) {
                if let Some(code) = SWITCHBOARD_SERVICE
                    .db
                    .get_last_aux_log(&user_uuid, start)
                    .await
                {
                    logs.push(AuxCodeResponse {
                        set_by: code.set_by,
                        code: code.code,
                        time: code.time.to_rfc3339(),
                    })
                }
            }

            Ok(Json(logs))
        }

        async fn set_user_aux_code(
            Path(user_uuid): Path<String>,
            Json(aux_code): Json<SetAuxCodeRequest>,
        ) -> Result<(), axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let _ = SWITCHBOARD_SERVICE
                .db
                .insert_sipuser_aux_log(
                    user.uuid.clone(),
                    Utc::now(),
                    aux_code.code,
                    aux_code.set_by.unwrap_or_else(|| "api".to_string()),
                )
                .await;

            // now clear cache
            let _ = reqwest::Client::new()
                .put(format!(
                    "http://{}/clear_aux_log_cache/{user_uuid}",
                    SWITCHBOARD_SERVICE.config.proxy_host
                ))
                .send()
                .await;

            let _ = notify_dialog(
                SWITCHBOARD_SERVICE
                    .config
                    .all_proxies()
                    .map(|p| p.to_vec())
                    .unwrap_or_else(|| {
                        vec![SWITCHBOARD_SERVICE.config.proxy_host.clone()]
                    }),
                user.uuid.clone(),
            )
            .await;
            Ok(())
        }

        async fn set_queue_login(
            Path(user_uuid): Path<String>,
            Json(login): Json<SetQueueLoginRequest>,
        ) -> Result<(), axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let _ = SWITCHBOARD_SERVICE
                .db
                .insert_sipuser_queue_login(
                    user.uuid.clone(),
                    Utc::now(),
                    login.set_by.unwrap_or_else(|| "api".to_string()),
                    login.queue_id.clone(),
                    login.login,
                )
                .await;

            // now clear cache
            let _ = reqwest::Client::new()
                .put(format!(
                    "http://{}/clear_queue_login_log_cache/{user_uuid}/{}",
                    SWITCHBOARD_SERVICE.config.proxy_host,
                    if let Some(queue_id) = login.queue_id.as_ref() {
                        queue_id
                    } else {
                        "None"
                    }
                ))
                .send()
                .await;

            if login.queue_id.is_none() {
                // Reset Aux code if exists
                if user
                    .current_aux_code()
                    .await
                    .map(|code| !code.code.to_lowercase().eq("available"))
                    .is_some()
                {
                    let _ = SWITCHBOARD_SERVICE
                        .db
                        .insert_sipuser_aux_log(
                            user.uuid.clone(),
                            Utc::now(),
                            "Available".to_string(),
                            "Queue Login".to_string(),
                        )
                        .await;
                    let _ = reqwest::Client::new()
                        .put(format!(
                            "http://{}/clear_aux_log_cache/{}",
                            SWITCHBOARD_SERVICE.config.proxy_host, user.uuid
                        ))
                        .send()
                        .await;
                }
                // only notify presence if for all queues
                let _ = notify_dialog(
                    SWITCHBOARD_SERVICE
                        .config
                        .all_proxies()
                        .map(|p| p.to_vec())
                        .unwrap_or_else(|| {
                            vec![SWITCHBOARD_SERVICE.config.proxy_host.clone()]
                        }),
                    user.uuid.clone(),
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
                    format!("{}:queue_login_logout", user.uuid),
                )
                .await;
            }
            Ok(())
        }

        async fn user_presence(
            Path(user_uuid): Path<String>,
        ) -> Result<Json<UserPresence>, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let presence = user.presence().await;
            Ok(Json::from(presence))
        }

        async fn notify_voicemail(
            Path(user_uuid): Path<String>,
        ) -> Result<String, axum::http::StatusCode> {
            let vm_user = SWITCHBOARD_SERVICE
                .db
                .get_voicemail_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let _ = Channel::notify_voicemail(&vm_user).await;
            Ok("OK".to_string())
        }

        async fn user_sync_provisioning(
            Path(user_uuid): Path<String>,
        ) -> Result<String, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let locations = Usrloc::lookup(&user)
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            for location in locations {
                let _ = SWITCHBOARD_SERVICE
                    .proxy_rpc
                    .sync_provisioning(&location.proxy_host.clone(), location)
                    .await;
            }
            Ok("OK".to_string())
        }

        async fn queue_status(
            Path(user_uuid): Path<String>,
            Query(params): Query<HashMap<String, String>>,
        ) -> Result<String, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let queue_id = params
                .get("queue_id")
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let status = user.queue_status(queue_id).await;
            Ok(status)
        }

        async fn login_as_users(
            Path(user_uuid): Path<String>,
        ) -> Result<Json<Vec<String>>, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            Ok(Json::from(
                SWITCHBOARD_SERVICE
                    .db
                    .get_login_users(&user.uuid)
                    .await
                    .unwrap_or_default(),
            ))
        }

        async fn login_from_users(
            Path(user_uuid): Path<String>,
        ) -> Result<Json<Vec<String>>, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            Ok(Json::from(
                SWITCHBOARD_SERVICE
                    .db
                    .get_login_from_users(&user.uuid)
                    .await
                    .unwrap_or_default(),
            ))
        }

        async fn clear_login_from_users(
            Path(user_uuid): Path<String>,
        ) -> Result<axum::http::StatusCode, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let _ = SWITCHBOARD_SERVICE.db.clear_login_users(&user.uuid).await;
            Ok(axum::http::StatusCode::OK)
        }

        async fn user_registration(
            Path(user_uuid): Path<String>,
        ) -> Result<Json<Vec<UserRegistration>>, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let registrations = user.registrations().await;
            Ok(Json::from(registrations))
        }

        async fn retrieve_user_registrations(
            users: String,
        ) -> Result<
            Json<HashMap<String, Vec<UserRegistration>>>,
            axum::http::StatusCode,
        > {
            let mut all_registrations = HashMap::new();
            for user in users.split(',') {
                if let Some(user) =
                    SWITCHBOARD_SERVICE.db.get_user("admin", user).await
                {
                    let registrations = user.registrations().await;
                    all_registrations.insert(user.uuid, registrations);
                }
            }
            Ok(Json::from(all_registrations))
        }

        async fn user_registration_notify(
            Path((user_uuid, registration)): Path<(String, String)>,
            Json(params): Json<RegistrationNotify>,
        ) -> (axum::http::StatusCode, String) {
            let user = SWITCHBOARD_SERVICE.db.get_user("admin", &user_uuid).await;
            let user = match user {
                Some(user) => user,
                None => {
                    return (
                        axum::http::StatusCode::NOT_FOUND,
                        "no such sip user".to_string(),
                    )
                }
            };

            let hashed_callid = sha256(&registration);
            let location_key = &Usrloc::location_key(&user.name.to_lowercase());
            let location = Usrloc::lookup_one(
                &user,
                &hashed_callid,
                location_key,
                &mut HashSet::new(),
            )
            .await;
            let location = match location {
                Some(location) => location,
                None => {
                    return (
                        axum::http::StatusCode::NOT_FOUND,
                        "no such registration".to_string(),
                    )
                }
            };

            let _ = SWITCHBOARD_SERVICE
                .proxy_rpc
                .notify_event(
                    &location.proxy_host.clone(),
                    NotifyEvent {
                        event: params.event,
                        content: params.content,
                        content_type: params.content_type,
                        location,
                    },
                )
                .await;

            (axum::http::StatusCode::OK, "ok".to_string())
        }

        async fn user_registration_update(
            Path((user_uuid, registration)): Path<(String, String)>,
            Json(params): Json<UserRegistrationUpdate>,
        ) -> (axum::http::StatusCode, String) {
            let user = SWITCHBOARD_SERVICE.db.get_user("admin", &user_uuid).await;
            let user = match user {
                Some(user) => user,
                None => {
                    return (
                        axum::http::StatusCode::NOT_FOUND,
                        "no such sip user".to_string(),
                    )
                }
            };
            let hashed_callid = sha256(&registration);
            let location_key = &Usrloc::location_key(&user.name.to_lowercase());
            let location = Usrloc::lookup_one(
                &user,
                &hashed_callid,
                location_key,
                &mut HashSet::new(),
            )
            .await;
            if location.is_none() {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    "no such registration".to_string(),
                );
            }

            let contact_key =
                Usrloc::contact_key(&user.name.to_lowercase(), &hashed_callid);
            let _ = REDIS
                .hmset(
                    &contact_key,
                    vec![
                        ("dnd", if params.dnd { "yes" } else { "no" }),
                        (
                            "dnd_allow_internal",
                            if params.dnd_allow_internal {
                                "yes"
                            } else {
                                "no"
                            },
                        ),
                    ],
                )
                .await;

            (axum::http::StatusCode::OK, "ok".to_string())
        }

        async fn user_last_queue_call(
            Path(user_uuid): Path<String>,
            Query(params): Query<HashMap<String, String>>,
        ) -> Result<Json<QueueRecord>, axum::http::StatusCode> {
            let user = SWITCHBOARD_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            if let Some(group_id) = params.get("group_id") {
                let record = SWITCHBOARD_SERVICE
                    .db
                    .get_user_last_queue_call_by_group(&user.uuid, group_id)
                    .await
                    .ok_or(axum::http::StatusCode::NOT_FOUND)?;
                return Ok(Json::from(record));
            }
            if let Some(queue_id) = params.get("queue_id") {
                let record = SWITCHBOARD_SERVICE
                    .db
                    .get_user_last_queue_call_by_queue(&user.uuid, queue_id)
                    .await
                    .ok_or(axum::http::StatusCode::NOT_FOUND)?;
                return Ok(Json::from(record));
            }

            Err(axum::http::StatusCode::NOT_FOUND)
        }

        async fn account_queue_stats(
            Path(tenant_id): Path<String>,
        ) -> Result<Json<Vec<AccountQueueStatsResponse>>, axum::http::StatusCode>
        {
            let groups = SWITCHBOARD_SERVICE
                .db
                .get_account_queue_groups(&tenant_id)
                .await
                .unwrap_or_default();

            let mut resp = Vec::new();
            for group in groups {
                let key = format!("nebula:callqueue:{}:calls", &group.uuid);
                let in_queue = REDIS.zcard(&key).await.unwrap_or(0);
                let duration =
                    in_queue_longest_duration(&group.uuid).await.unwrap_or(0);

                resp.push(AccountQueueStatsResponse {
                    queue_group: group.uuid,
                    in_queue,
                    in_queue_longest: duration,
                });
            }
            Ok(Json(resp))
        }

        async fn in_queue_number(
            Path(queue): Path<String>,
        ) -> Result<String, axum::http::StatusCode> {
            let key = format!("nebula:callqueue:{}:calls", &queue);
            let number = REDIS
                .zcard(&key)
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

            Ok(number.to_string())
        }

        async fn in_queue_longest(
            Path(queue): Path<String>,
        ) -> Result<String, axum::http::StatusCode> {
            let duration = in_queue_longest_duration(&queue)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(duration.to_string())
        }

        async fn queue_records_by_cdr(
            Path(cdr_id): Path<String>,
        ) -> Result<Json<Vec<QueueRecord>>, axum::http::StatusCode> {
            let records = SWITCHBOARD_SERVICE
                .db
                .get_queue_records_by_cdr(&cdr_id)
                .await
                .unwrap_or_default();
            Ok(Json(records))
        }

        async fn abandoned_queue_records_called_back(
            Path(queue_id): Path<uuid::Uuid>,
            Query(params): Query<AbandonedQueueRecordCalledbackQuery>,
        ) -> impl IntoResponse {
            let start = match chrono::DateTime::parse_from_rfc3339(&params.start) {
                Ok(t) => t,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        format!("start is invalid, error: {e}"),
                    )
                        .into_response()
                }
            };
            let end = params
                .end
                .as_ref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());

            let limit = params.limit.unwrap_or(1000).min(1000);
            let page = params.page.unwrap_or(1);

            let (count, records) = SWITCHBOARD_SERVICE
                .db
                .get_abandoned_queue_records(
                    queue_id.to_string(),
                    start.into(),
                    end.map(|end| end.into()),
                    limit as i64,
                    (page.saturating_sub(1) * limit) as i64,
                )
                .await
                .unwrap_or_default();

            let mut full_records = Vec::new();
            for queue_record in records {
                if let Ok(record) = SWITCHBOARD_SERVICE
                    .db
                    .abandoned_queue_record_called_back(
                        &queue_record,
                        params.needs_to_be_answered,
                        params.timeout,
                    )
                    .await
                {
                    full_records.push(AbandonedQueueRecord {
                        number: record.number,
                        called_back: match record.called_back {
                            AbandonedQueueRecordCalledback::NotCalledback => None,
                            AbandonedQueueRecordCalledback::WithinTimeout(_) => None,
                            AbandonedQueueRecordCalledback::Calledback(
                                uuid,
                                date_time,
                            ) => Some(AbandonedQueueRecordCalledBack {
                                cdr: uuid.to_string(),
                                time: date_time.to_rfc3339(),
                            }),
                        },

                        uuid: queue_record.uuid.to_string(),
                        queue_id: queue_record.queue_id,
                        start: queue_record.start.to_rfc3339(),
                        cdr: queue_record.cdr,
                        disposition: queue_record.disposition,
                        duration: queue_record.duration,
                        call_duration: queue_record.call_duration,
                        wait_duration: queue_record.wait_duration,
                        ring_duration: queue_record.ring_duration,
                        answer_by: queue_record.answer_by,
                        answer: queue_record.answer.map(|t| t.to_rfc3339()),
                        end: queue_record.end.map(|e| e.to_rfc3339()),
                        ready: queue_record.ready.map(|t| t.to_rfc3339()),
                        cdr_start: queue_record.cdr_start.map(|t| t.to_rfc3339()),
                        cdr_wait: queue_record.cdr_wait,
                        group_id: queue_record.group_id,
                        failed_times: queue_record.failed_times,
                        init_pos: queue_record.init_pos,
                        hangup_pos: queue_record.hangup_pos,
                    })
                }
            }

            let response = AbandonedQueueRecordResponse {
                total_records: count as u64,
                limit,
                page,
                records: full_records,
            };
            Json(response).into_response()
        }

        /*
            We take in the queue with all the users.
            Return the queue with only the logged in users.
        */
        async fn queue_user_logins(
            Json(req): Json<QueueLoginUsersRequest>,
        ) -> Result<Json<HashMap<String, Vec<String>>>, axum::http::StatusCode>
        {
            // User uuid -> all queue login status
            let mut all_queue_cache: HashMap<String, bool> = HashMap::new();

            // Queue Uuid -> vec of logged in users
            let mut res: HashMap<String, Vec<String>> = HashMap::new();

            for queue_id in req.queues {
                let Some(queue_users) = SWITCHBOARD_SERVICE
                    .db
                    .get_queue_group_members(&req.tenant_uuid, &queue_id)
                    .await
                else {
                    continue;
                };

                let mut login_users: Vec<String> = vec![];

                for user_uuid in queue_users {
                    let all_queue_login = match all_queue_cache.get(&user_uuid) {
                        Some(login) => *login,
                        None => {
                            let user_login = SWITCHBOARD_SERVICE
                                .db
                                .user_last_queue_login(&user_uuid)
                                .await
                                .is_none_or(|login| login.login);

                            all_queue_cache.insert(user_uuid.clone(), user_login);
                            user_login
                        }
                    };

                    if !all_queue_login {
                        continue;
                    }

                    let is_queue_login = SWITCHBOARD_SERVICE
                        .db
                        .user_last_individual_queue_login(&user_uuid, &queue_id)
                        .await
                        .is_none_or(|login| login.login);

                    if is_queue_login {
                        login_users.push(user_uuid);
                    }
                }

                res.insert(queue_id, login_users);
            }

            return Ok(Json(res));
        }

        async fn cdr_events(
            Path(cdr_id): Path<uuid::Uuid>,
        ) -> Result<Json<Vec<CdrEvent>>, axum::http::StatusCode> {
            let events = SWITCHBOARD_SERVICE
                .db
                .get_cdr_events(cdr_id)
                .await
                .unwrap_or_default();
            Ok(Json(events))
        }

        async fn room_members(
            Path(room): Path<String>,
        ) -> Result<Json<Vec<RoomMember>>, axum::http::StatusCode> {
            let room = Room::new(room);
            let members = room.members().await;
            Ok(Json::from(members))
        }

        async fn stream_new_message(
            Json(msg): Json<RpcMessage>,
        ) -> Result<(), axum::http::StatusCode> {
            tokio::spawn(async move {
                let _ = RpcClient::send_to_stream(SWITCHBOARD_STREAM, &msg).await;
            });
            Ok(())
        }

        async fn new_usrloc(
            Json(data): Json<UsrlocData>,
        ) -> Result<(), axum::http::StatusCode> {
            let _ = Usrloc::store_to_redis(&data).await;
            Ok(())
        }

        async fn update_usrloc(
            Json(data): Json<UsrlocData>,
        ) -> Result<(), axum::http::StatusCode> {
            let _ = Usrloc::update_usrloc(&data).await;
            Ok(())
        }

        async fn new_cdr(
            Json(data): Json<NewCdrItemFull>,
        ) -> Result<String, axum::http::StatusCode> {
            let cdr = SWITCHBOARD_SERVICE
                .db
                .create_cdr_full(data)
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(cdr.uuid.to_string())
        }

        async fn do_upload_cdr_recording(
            cdr_id: uuid::Uuid,
            expires: usize,
            request: axum::extract::BodyStream,
        ) -> Result<()> {
            let body = request
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err));
            let body = StreamReader::new(body);
            futures::pin_mut!(body);

            let mut writer: Vec<u8> = vec![];
            tokio::io::copy(&mut body, &mut writer).await?;

            StorageClient::new()
                .upload_content(
                    &format!("aura-call-recordings-{}", expires),
                    &format!("{cdr_id}.wav"),
                    writer,
                )
                .await?;

            SWITCHBOARD_SERVICE
                .db
                .update_cdr(
                    cdr_id,
                    CdrItemChange {
                        recording: Some(format!("{expires}/{cdr_id}.wav")),
                        ..Default::default()
                    },
                )
                .await?;
            Ok(())
        }

        async fn upload_cdr_recording(
            Path((cdr_id, expires)): Path<(uuid::Uuid, usize)>,
            request: axum::extract::BodyStream,
        ) -> impl IntoResponse {
            if SWITCHBOARD_SERVICE.db.get_cdr(cdr_id).await.is_none() {
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
            match do_upload_cdr_recording(cdr_id, expires, request).await {
                Ok(()) => ().into_response(),
                Err(e) => {
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                        .into_response()
                }
            }
        }

        async fn new_voicemail(
            Json(data): Json<NewVoicemail>,
        ) -> Result<String, axum::http::StatusCode> {
            let voicemail_id = uuid::Uuid::new_v4().to_string();
            SWITCHBOARD_SERVICE
                .db
                .create_voicemail(
                    voicemail_id.clone(),
                    data.voicemail_user,
                    data.caller,
                    data.callee,
                    data.duration,
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(data.time),
                )
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(voicemail_id)
        }

        async fn clear_aux_log_cache(
            Path(user_id): Path<String>,
        ) -> Result<(), axum::http::StatusCode> {
            let key = format!("nebula:cache:user:{user_id}:last_aux_code");
            let _ = REDIS.del(&key).await;
            Ok(())
        }

        async fn clear_queue_login_log_cache(
            Path((user_id, queue_id)): Path<(String, String)>,
        ) -> Result<(), axum::http::StatusCode> {
            let key = if queue_id == "None" {
                format!("nebula:cache:user:{user_id}:last_queue_login")
            } else {
                format!("nebula:cache:user:{user_id}:last_individual_queue_login:{queue_id}")
            };
            let _ = REDIS.del(&key).await;
            Ok(())
        }

        async fn update_user_global_dnd(
            Path(user_id): Path<String>,
        ) -> Result<(), axum::http::StatusCode> {
            let key = format!("nebula:cache:user:uuid:{user_id}");
            let _ = REDIS.del(&key).await;
            Ok(())
        }

        async fn update_voicemail(
            Path(voicemail_id): Path<String>,
            request: axum::extract::BodyStream,
        ) -> Result<(), axum::http::StatusCode> {
            SWITCHBOARD_SERVICE
                .db
                .get_voicemail_message(&voicemail_id)
                .await
                .ok_or(axum::http::StatusCode::NOT_FOUND)?;
            let body = request
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err));

            let path = format!("/tmp/{voicemail_id}.wav");
            {
                let body = StreamReader::new(body);
                futures::pin_mut!(body);

                let mut file =
                    BufWriter::new(File::create(&path).await.map_err(|_| {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    })?);

                // Copy the body into the file.
                tokio::io::copy(&mut body, &mut file)
                    .await
                    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            }

            let mp3_path = format!("/tmp/{voicemail_id}.mp3");
            tokio::process::Command::new("sox")
                .arg(&path)
                .arg("-C")
                .arg("16.2")
                .arg(&mp3_path)
                .output()
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

            StorageClient::new()
                .upload("aura-voicemails", &format!("{voicemail_id}.wav"), &path)
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            StorageClient::new()
                .upload("aura-voicemails", &format!("{voicemail_id}.mp3"), &mp3_path)
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            let _ = tokio::fs::remove_file(&path).await;
            let _ = tokio::fs::remove_file(&mp3_path).await;

            Ok(())
        }

        async fn channel_is_answered(
            Path(channel): Path<String>,
        ) -> Result<String, axum::http::StatusCode> {
            let answered = if is_channel_answered(&channel).await {
                "yes"
            } else {
                "no"
            };
            Ok(answered.to_string())
        }

        let app = axum::Router::new()
            .route("/health-check", axum::routing::get(health_check))
            .route(
                "/v1/users/:user_uuid/app",
                axum::routing::put(user_app_update),
            )
            .route(
                "/v1/users/:user_uuid/presence",
                axum::routing::get(user_presence),
            )
            .route(
                "/v1/users/:user_uuid/queue_login",
                axum::routing::put(set_queue_login),
            )
            .route(
                "/v1/users/:user_uuid/individual_queue_login/:queue_id",
                axum::routing::get(get_individual_queue_login),
            )
            .route(
                "/v1/users/:user_uuid/list_queue_login",
                axum::routing::post(get_list_queue_login),
            )
            .route(
                "/v1/users/:user_uuid/queue_login_log",
                axum::routing::get(get_user_queue_login_log),
            )
            .route(
                "/v1/users/:user_uuid/aux_code",
                axum::routing::put(set_user_aux_code),
            )
            .route(
                "/v1/users/:user_uuid/aux_code",
                axum::routing::get(get_user_aux_code),
            )
            .route(
                "/v1/users/:user_uuid/aux_code_log",
                axum::routing::get(get_user_aux_code_log),
            )
            .route(
                "/v1/users/:user_uuid/dialogs",
                axum::routing::get(user_dialogs),
            )
            .route(
                "/v1/users/:user_uuid/sync_provisioning",
                axum::routing::put(user_sync_provisioning),
            )
            .route(
                "/v1/users/:user_uuid/registration/:registration",
                axum::routing::put(user_registration_update),
            )
            .route(
                "/v1/users/:user_uuid/registration/:registration/event",
                axum::routing::post(user_registration_notify),
            )
            .route(
                "/v1/users/:user_uuid/queue_status",
                axum::routing::get(queue_status),
            )
            .route(
                "/v1/users/:user_uuid/registration",
                axum::routing::get(user_registration),
            )
            .route(
                "/v1/user_registrations",
                axum::routing::post(retrieve_user_registrations),
            )
            .route(
                "/v1/users/:user_uuid/last_queue_call",
                axum::routing::get(user_last_queue_call),
            )
            .route(
                "/v1/users/:user_uuid/login_as_users",
                axum::routing::get(login_as_users),
            )
            .route(
                "/v1/users/:user_uuid/login_from_users",
                axum::routing::get(login_from_users),
            )
            .route(
                "/v1/users/:user_uuid/login_from_users",
                axum::routing::delete(clear_login_from_users),
            )
            .route("/v1/calls", axum::routing::post(create_call))
            .route("/v1/transfer_calls", axum::routing::post(transfer_call))
            .route(
                "/v1/outbound_calls",
                axum::routing::post(create_outbound_call),
            )
            .route(
                "/v1/parking_slot/:slot",
                axum::routing::get(parking_slot_presence),
            )
            .route(
                "/v1/queue_stats/:queue",
                axum::routing::get(in_queue_number),
            )
            .route(
                "/v1/queue_user_logins",
                axum::routing::post(queue_user_logins),
            )
            .route(
                "/v1/account_queue_stats/:tenant_id",
                axum::routing::get(account_queue_stats),
            )
            .route(
                "/v1/in_queue_longest/:queue",
                axum::routing::get(in_queue_longest),
            )
            .route(
                "/v1/queue_records_by_cdr/:cdr_id",
                axum::routing::get(queue_records_by_cdr),
            )
            .route(
                "/v1/abandoned_queue_records_called_back/:queue_id",
                axum::routing::get(abandoned_queue_records_called_back),
            )
            .route("/v1/cdr_events/:cdr_id", axum::routing::get(cdr_events))
            .route("/v1/rooms/:room/members", axum::routing::get(room_members))
            .route(
                "/v1/channels/:channel/answered",
                axum::routing::get(channel_is_answered),
            )
            .route(
                "/v1/voicemail_users/:user_uuid",
                axum::routing::put(notify_voicemail),
            )
            .route("/internal/stream", axum::routing::post(stream_new_message))
            .route("/internal/usrlocs", axum::routing::post(new_usrloc))
            .route(
                "/internal/usrlocs/proxy_host",
                axum::routing::post(update_usrloc),
            )
            .route("/internal/cdrs", axum::routing::post(new_cdr))
            .route(
                "/internal/cdrs/:cdr_id/:days",
                axum::routing::put(upload_cdr_recording),
            )
            .route(
                "/internal/clear_aux_log_cache/:user_id",
                axum::routing::put(clear_aux_log_cache),
            )
            .route(
                "/internal/clear_queue_login_log_cache/:user_id/:queue_id",
                axum::routing::put(clear_queue_login_log_cache),
            )
            .route(
                "/internal/update_user_global_dnd/:user_id",
                axum::routing::put(update_user_global_dnd),
            )
            .route("/internal/voicemails", axum::routing::post(new_voicemail))
            .route(
                "/internal/voicemails/:voicemail_id",
                axum::routing::put(update_voicemail),
            );
        // .layer(
        //     TraceLayer::new_for_http()
        //         .make_span_with(
        //             tower_http::trace::DefaultMakeSpan::new()
        //                 .level(tracing::Level::INFO),
        //         )
        //         .on_response(
        //             tower_http::trace::DefaultOnResponse::new()
        //                 .level(tracing::Level::INFO),
        //         ),
        // );

        axum::Server::bind(&SocketAddr::from_str("0.0.0.0:8122")?)
            .serve(app.into_make_service())
            .await?;
        Ok(())
    }
}

async fn in_queue_longest_duration(group: &str) -> Result<u64> {
    let key = format!("nebula:callqueue:{group}:calls");
    let mut value = REDIS
        .zrange_withscores(&key, 0, 0)
        .await
        .unwrap_or_default()
        .into_iter();
    value.next().ok_or_else(|| anyhow!("no next value"))?;
    let score = value.next().ok_or_else(|| anyhow!("no next value"))?;
    let score: u64 = score.parse()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let duration = now.saturating_sub(score);
    Ok(duration)
}

pub fn is_ip_global(ip: &Ipv4Addr) -> bool {
    !(ip.octets()[0] == 0
        || ip.is_private()
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0b1100_0000 == 0b0100_0000))
        || ip.is_loopback()
        || ip.is_link_local()
        || (ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 0)
        || ip.is_documentation()
        || (ip.octets()[0] == 198 && (ip.octets()[1] & 0xfe) == 18)
        || (ip.octets()[0] & 240 == 240 && !ip.is_broadcast())
        || ip.is_broadcast())
}
