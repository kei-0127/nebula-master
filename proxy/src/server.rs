use crate::{
    presence::Presence, provider::ProviderService, router::tenant_switchboard_host,
};

use super::router::Router;
use anyhow::{anyhow, Error, Result};
use axum::{
    extract::{Extension, Path, Query},
    Json,
};
use itertools::Itertools;
use lazy_static::lazy_static;
use nebula_db::{
    api::Database,
    message::{
        AnswerResponse, NewIntraRequest, NewInvite, NotifyEvent, ParkStatus,
        ProxyHangup, Response, RpcChannel, RpcConvertToRoom, RpcDialog,
        RpcUpdateCallerid, RpcVoicemailNotification,
    },
    models::AccountQueueStatsResponse,
    usrloc::{Usrloc, UsrlocData, CONTACT, LOCATION_ID, RECEIVED, USER},
};
use nebula_redis::{
    redis::{self},
    REDIS,
};
use nebula_rpc::client::RpcClient;
use nebula_rpc::{client::notify_dialog, server::RpcServer};
use nebula_rpc::{client::SwitchboardRpcClient, message::*};
use nebula_utils::{get_hostname, get_local_ip, uuid_v5};
use serde::Deserialize;
use sip::message::Location;
use sip::transport::LocalSendRequest;
use sip::{
    message::{Address, Cseq, Message, Method, Uri, Via},
    transaction::TM,
    transport::TransportType,
};
use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use std::{net::ToSocketAddrs, str::FromStr};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::{
        oneshot::{self, error::TryRecvError},
        Mutex,
    },
};
use tracing::error;

lazy_static! {
    pub static ref PRPOXY_SERVICE: ProxyService = ProxyService::new().unwrap();
}

#[derive(Debug, Error)]
pub enum ProxyServerError {
    #[error("No redis host")]
    NoRedisHost,
    #[error("No db host")]
    NoDBHost,
    #[error("No ip")]
    NoIP,
    #[error("No Port")]
    NoPort,
}

#[derive(Deserialize)]
pub struct Config {
    pub redis: String,
    pub db: String,
    pub location: Option<String>,
    pub proxy_ip: String,
    pub udp_listen_proxy_ip: Option<bool>,
    pub provider_udp_port: Option<u16>,
    pub provider_tls_port: Option<u16>,
    pub proxy_host: String,
    pub switchboard_host: String,
    pub proxies: Option<Vec<String>>,
    all_proxies: Option<Vec<String>>,
    pub internal_ips: Option<Vec<String>>,
    pub switchboards: Vec<String>,
    pub switchboard_endpoint: Option<String>,
}

impl Config {
    pub fn all_proxies(&self) -> Option<&[String]> {
        self.all_proxies.as_deref().or(self.proxies.as_deref())
    }
}

pub struct ProxyService {
    pub config: Config,
    pub switchboard_rpc: SwitchboardRpcClient,
    pub db: Database,
    pub router: Router,
    pub local_ip: String,
    pub proxy_host: String,
    hostname: String,
    last_ping: Arc<Mutex<(bool, Instant)>>,
}

pub struct ProxyServer {}

impl ProxyService {
    pub fn new() -> Result<ProxyService> {
        let contents = fs::read_to_string("/etc/nebula/nebula.conf")?;
        let config: Config = toml::from_str(&contents)?;
        let db = Database::new(config.db.as_ref())?;
        let local_ip =
            get_local_ip().ok_or_else(|| anyhow!("can't get local ip"))?;
        let hostname =
            get_hostname().ok_or_else(|| anyhow!("can't get hostname"))?;
        let switchboard_rpc = SwitchboardRpcClient::new();
        let proxy_host = if config.udp_listen_proxy_ip.unwrap_or(false) {
            config.proxy_host.clone()
        } else {
            format!("{local_ip}:8121")
        };
        Ok(ProxyService {
            config,
            switchboard_rpc,
            router: Router::new(),
            db,
            local_ip,
            proxy_host,
            last_ping: Arc::new(Mutex::new((true, Instant::now()))),
            hostname,
        })
    }

    async fn process_rpc(&self, rpc_msg: RpcMessage) {
        let result = self.do_process_rpc(rpc_msg.clone()).await;
        let start = std::time::Instant::now();
        if rpc_msg.method != RpcMethod::NotifyDialog
            && rpc_msg.method != RpcMethod::LocalSend
            && rpc_msg.method != RpcMethod::RoomRpc
        {
            tracing::info!(
                "proxy finished ({} ms) process rpc method {} params {} result: {result:?}",
                start.elapsed().as_millis(),
                rpc_msg.method,
                rpc_msg.params,
            );
        }
    }

    async fn do_process_rpc(&self, rpc_msg: RpcMessage) -> Result<()> {
        match rpc_msg.method {
            RpcMethod::Invite => self.handle_invite(rpc_msg.params).await?,
            RpcMethod::ProxyHangup => self.handle_hangup(rpc_msg.params).await?,
            RpcMethod::Answer => self.handle_answer(rpc_msg.params).await?,
            RpcMethod::SetResponse => self.handle_response(rpc_msg.params).await?,
            RpcMethod::Ack => self.handle_ack(rpc_msg.params).await?,
            RpcMethod::NotifyReferSuccess => {
                self.notify_refer_success(rpc_msg.id).await?
            }
            RpcMethod::NotifyReferTrying => {
                self.notify_refer_trying(rpc_msg.id).await?
            }
            RpcMethod::UpdateCallerid => {
                self.update_callerid(rpc_msg.params).await?
            }
            RpcMethod::ConvertToRoom => self.convert_to_room(rpc_msg.params).await?,
            RpcMethod::NotifyVoicemail => {
                self.notify_voicemail(rpc_msg.params).await?
            }
            RpcMethod::ReInvite => {
                self.reinvite(rpc_msg.params).await?;
            }
            RpcMethod::NewIntra => {
                self.new_intra(rpc_msg.params).await?;
            }
            RpcMethod::ProxyHangupIntra => {
                self.hangup_intra(rpc_msg.id).await?;
            }
            RpcMethod::AnswerIntra => {
                self.answer_intra(rpc_msg.params).await?;
            }
            RpcMethod::IntraSessoinProgress => {
                self.intra_session_progress(rpc_msg.params).await?;
            }
            RpcMethod::NotifyDialog => self.notify_dialog(rpc_msg.params).await?,
            RpcMethod::LocalSend => {
                self.local_send(rpc_msg.id, rpc_msg.params).await?
            }
            RpcMethod::PublishPark => self.publish_park(rpc_msg.params).await?,
            RpcMethod::OptionsMobileApp => {
                self.options_mobile_app(rpc_msg.params).await?
            }
            RpcMethod::SyncProvisioning => {
                self.sync_provisioning(rpc_msg.params).await?
            }
            RpcMethod::NotifyEvent => self.notify_event(rpc_msg.params).await?,
            RpcMethod::RoomRpc => {
                let msg: RoomRpcMessage = serde_json::from_value(rpc_msg.params)?;
                let mut needs_log = true;
                if let Some(method) = msg.rpc.get_method() {
                    if method
                        == RoomServerNotificationMethod::MemberAudioLevel.to_string()
                        || method
                            == RoomServerNotificationMethod::MemberAvailableRate
                                .to_string()
                        || method
                            == RoomServerNotificationMethod::MemberVideoAllocations
                                .to_string()
                    {
                        needs_log = false;
                    }
                }
                if needs_log {
                    tracing::info!(
                        "proxy process rpc method room_rpc params {msg:?}"
                    );
                }
                let text = serde_json::to_string(&msg.rpc)?;
                let addr = msg
                    .addr
                    .to_socket_addrs()?
                    .next()
                    .ok_or(anyhow!("can't get socket addr"))?;
                let uri = Uri {
                    scheme: "sip".to_string(),
                    host: addr.ip().to_string(),
                    ip: addr.ip().to_string(),
                    port: Some(addr.port()),
                    transport: TransportType::Wss,
                    ..Default::default()
                };
                self.send_api(&text, &uri).await?;
            }
            _ => {}
        };
        Ok(())
    }

    async fn notify_event(&self, params: serde_json::Value) -> Result<()> {
        let event: NotifyEvent = serde_json::from_value(params)?;

        let location = event.location;

        let mut notify = Message::default();
        notify.method = Some(Method::NOTIFY);
        notify.request_uri = Some(location.req_uri);
        notify.remote_uri = Some(if let Some(uri) = location.route.as_ref() {
            notify.destination = Some(location.dst_uri.clone());
            uri.clone()
        } else {
            location.dst_uri.clone()
        });
        notify.cseq = Cseq {
            method: Method::NOTIFY,
            seq: 1,
        };
        notify.callid = nebula_utils::uuid();
        notify.from = Address {
            uri: Uri {
                user: Some(location.user.clone()),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            },
            tag: Some(nebula_utils::rand_string(10)),
            ..Default::default()
        };
        notify.to = Address {
            uri: Uri {
                user: Some(location.user),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        notify.add_generic_header("Event".to_string(), event.event);
        notify.subscription_state = Some("terminated".to_string());

        if !event.content.is_empty() {
            notify.body = Some(event.content);
        }
        if let Some(content_type) = event.content_type {
            notify.content_type = Some(content_type);
        }

        TM.send(&notify).await?;
        Ok(())
    }

    async fn sync_provisioning(&self, params: serde_json::Value) -> Result<()> {
        let location: Location = serde_json::from_value(params)?;

        let user_agent = location
            .user_agent
            .as_ref()
            .ok_or_else(|| anyhow!("no user agent"))?;
        let event = match user_agent.to_lowercase() {
            a if a.contains("yealink") => "check-sync;reboot=false".to_string(),
            a if a.contains("grandstream") => "resync".to_string(),
            a if a.contains("cisco") => "resync".to_string(),
            a if a.contains("maxwell") => "check-sync;reboot=false".to_string(),
            _ => return Err(anyhow!("not supported user agent")),
        };

        let mut notify = Message::default();
        notify.method = Some(Method::NOTIFY);
        notify.request_uri = Some(location.req_uri);
        notify.remote_uri = Some(if let Some(uri) = location.route.as_ref() {
            notify.destination = Some(location.dst_uri.clone());
            uri.clone()
        } else {
            location.dst_uri.clone()
        });
        notify.cseq = Cseq {
            method: Method::NOTIFY,
            seq: 1,
        };
        notify.callid = nebula_utils::uuid();
        notify.from = Address {
            uri: Uri {
                user: Some(location.user.clone()),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            },
            tag: Some(nebula_utils::rand_string(10)),
            ..Default::default()
        };
        notify.to = Address {
            uri: Uri {
                user: Some(location.user),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        notify.add_generic_header("Event".to_string(), event);
        notify.subscription_state = Some("terminated".to_string());

        TM.send(&notify).await?;
        Ok(())
    }

    async fn options_mobile_app(&self, params: serde_json::Value) -> Result<()> {
        let location: Location = serde_json::from_value(params)?;
        let mut options = Message::default();
        options.method = Some(Method::OPTIONS);
        options.request_uri = Some(Uri {
            user: Some(location.user.clone()),
            host: "talk.yay.com".to_string(),
            ..Default::default()
        });
        options.remote_uri = Some(if let Some(uri) = location.route.as_ref() {
            options.destination = Some(location.dst_uri.clone());
            uri.clone()
        } else {
            location.dst_uri.clone()
        });
        options.cseq = Cseq {
            method: Method::OPTIONS,
            seq: 1,
        };
        options.from = Address {
            uri: Uri {
                user: Some(location.user.clone()),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            },
            tag: Some(nebula_utils::rand_string(10)),
            ..Default::default()
        };
        options.to = Address {
            uri: Uri {
                user: Some(location.user.clone()),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        options.callid = nebula_utils::uuid();
        options.device = location.device;
        options.device_token = location.device_token;
        TM.send(&options).await?;

        Ok(())
    }

    async fn publish_park(&self, params: serde_json::Value) -> Result<()> {
        let req: ParkStatus = serde_json::from_value(params)?;
        let mut publish = Message::default();
        publish.method = Some(Method::PUBLISH);
        publish.request_uri = Some(Uri {
            user: Some(req.slot),
            host: "talk.yay.com".to_string(),
            ..Default::default()
        });
        publish.remote_uri = Some(Uri {
            host: "23.251.134.254".to_string(),
            ..Default::default()
        });
        publish.cseq = Cseq {
            method: Method::PUBLISH,
            seq: 1,
        };
        publish.from = Address {
            uri: Uri {
                user: Some(req.from),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            },
            tag: Some(nebula_utils::rand_string(10)),
            ..Default::default()
        };
        publish.to = Address {
            uri: Uri {
                user: Some(req.to),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        publish.callid = uuid_v5(&format!("{}:park", &req.uuid)).to_string();
        publish.add_generic_header(
            "Park-Direction".to_string(),
            req.direction.to_string(),
        );
        publish.add_generic_header(
            "Park-Status".to_string(),
            if req.parked {
                "confirmed".to_string()
            } else {
                "terminated".to_string()
            },
        );
        TM.send(&publish).await?;
        Ok(())
    }

    async fn local_send(
        &self,
        callid: String,
        params: serde_json::Value,
    ) -> Result<()> {
        let req: LocalSendRequest = serde_json::from_value(params)?;
        let trans = TM
            .transport
            .transports
            .get(&req.uri.transport)
            .ok_or(anyhow!("transport not supported"))?;
        if let Err(e) = trans.send(req.msg, &req.uri, callid.clone()).await {
            error!(
                channel = callid,
                "local send error to {}: {e:#}",
                req.uri.addr()
            )
        }
        Ok(())
    }

    async fn notify_dialog(&self, params: serde_json::Value) -> Result<()> {
        let dialog: RpcDialog = serde_json::from_value(params)?;
        Presence::notify_dialog(&dialog.id).await?;
        Ok(())
    }

    async fn notify_voicemail(&self, params: serde_json::Value) -> Result<()> {
        let notification: RpcVoicemailNotification = serde_json::from_value(params)?;

        let mut notify = Message::default();

        notify.remote_uri =
            Some(if let Some(uri) = notification.location.route.as_ref() {
                notify.destination = Some(notification.location.dst_uri.clone());
                uri.clone()
            } else {
                notification.location.dst_uri.clone()
            });

        notify.request_uri = Some(notification.location.req_uri);
        notify.method = Some(Method::NOTIFY);
        notify.cseq = Cseq {
            method: Method::NOTIFY,
            seq: 1,
        };
        notify.from = Address {
            uri: Uri {
                user: Some("vm".to_string()),
                host: notification.location.from_host.clone(),
                ..Default::default()
            },
            tag: Some(nebula_utils::rand_string(10)),
            ..Default::default()
        };
        notify.to = Address {
            uri: Uri {
                user: Some(notification.location.user),
                host: notification.location.to_host.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        notify.callid = nebula_utils::uuid();
        notify.content_type = Some("application/simple-message-summary".to_string());
        notify
            .add_generic_header("Event".to_string(), "message-summary".to_string());

        notify.body = Some(
            [
                &format!(
                    "Messages-Waiting: {}",
                    if notification.new > 0 { "yes" } else { "no" }
                ),
                &format!("Voice-Message: {}/{}", notification.new, notification.old),
                "",
            ]
            .join("\r\n"),
        );

        TM.send(&notify).await?;
        Ok(())
    }

    async fn convert_to_room(&self, params: serde_json::Value) -> Result<()> {
        let notification: RpcConvertToRoom = serde_json::from_value(params)?;

        let msg_string: String = REDIS
            .hget(
                &format!("nebula:channel:{}", &notification.id),
                "dialog_message",
            )
            .await?;
        let msg = Message::from_str(&msg_string)?;
        let mut invite = TM.dialog_msg(&msg).await?;

        let cseq = REDIS
            .hincrby_key_exits(
                &format!("nebula:channel:{}", &notification.id),
                "cseq",
            )
            .await?;
        invite.method = Some(Method::INVITE);
        invite.cseq = Cseq {
            method: Method::INVITE,
            seq: cseq as i32 + 1,
        };
        invite.max_forwards = Some(70);
        invite.allow = Some(
            "SUBSCRIBE, NOTIFY, INVITE, ACK, CANCEL, BYE, REFER, INFO, OPTIONS, MESSAGE"
                .to_string(),
        );
        invite.content_type = Some("application/sdp".to_string());
        invite.content_disposition = Some("session".to_string());
        invite.body = Some(notification.body);
        invite.room = Some(notification.room);
        invite.room_auth_token = Some(notification.auth_token);

        TM.send(&invite).await?;

        Ok(())
    }

    async fn update_callerid(&self, params: serde_json::Value) -> Result<()> {
        let notification: RpcUpdateCallerid = serde_json::from_value(params)?;

        let msg_string: String = REDIS
            .hget(
                &format!("nebula:channel:{}", &notification.id),
                "dialog_message",
            )
            .await?;
        let msg = Message::from_str(&msg_string)?;
        let mut invite = TM.dialog_msg(&msg).await?;

        let cseq = REDIS
            .hincrby_key_exits(
                &format!("nebula:channel:{}", &notification.id),
                "cseq",
            )
            .await?;
        invite.method = Some(Method::INVITE);
        invite.cseq = Cseq {
            method: Method::INVITE,
            seq: cseq as i32 + 1,
        };
        invite.max_forwards = Some(70);
        invite.allow = Some(
            "SUBSCRIBE, NOTIFY, INVITE, ACK, CANCEL, BYE, REFER, INFO, OPTIONS, MESSAGE"
                .to_string(),
        );
        invite.content_type = Some("application/sdp".to_string());
        invite.content_disposition = Some("session".to_string());
        invite.body = Some(notification.body);
        invite.pai = Some(Address {
            uri: Uri {
                user: Some(notification.caller_id.user.clone()),
                host: invite.from.uri.host.clone(),
                ..Default::default()
            },
            display_name: notification.caller_id.display_name.clone(),
            ..Default::default()
        });

        TM.send(&invite).await?;

        Ok(())
    }

    async fn hangup_intra(&self, id: String) -> Result<()> {
        let _ = REDIS.expire(&format!("nebula:channel:{id}"), 60).await;
        let switchboard_host: String = REDIS
            .hget(&format!("nebula:channel:{id}"), "switchboard_host")
            .await
            .unwrap_or_else(|_| PRPOXY_SERVICE.config.switchboard_host.clone());
        PRPOXY_SERVICE
            .switchboard_rpc
            .hangup(&switchboard_host, id, true, None, None, None)
            .await?;
        Ok(())
    }

    async fn intra_session_progress(&self, params: serde_json::Value) -> Result<()> {
        let req: AnswerResponse = serde_json::from_value(params)?;
        let switchboard_host: String = REDIS
            .hget(&format!("nebula:channel:{}", req.id), "switchboard_host")
            .await
            .unwrap_or_else(|_| PRPOXY_SERVICE.config.switchboard_host.clone());
        let _ = PRPOXY_SERVICE
            .switchboard_rpc
            .set_response(
                &switchboard_host,
                req.id,
                183,
                "Session Progress".to_string(),
                req.body,
                None,
            )
            .await;
        Ok(())
    }

    async fn answer_intra(&self, params: serde_json::Value) -> Result<()> {
        let req: AnswerResponse = serde_json::from_value(params)?;
        let switchboard_host: String = REDIS
            .hget(&format!("nebula:channel:{}", req.id), "switchboard_host")
            .await
            .unwrap_or_else(|_| PRPOXY_SERVICE.config.switchboard_host.clone());
        let _ = PRPOXY_SERVICE
            .switchboard_rpc
            .answer(&switchboard_host, req.id, req.body, None, None, None)
            .await;
        Ok(())
    }

    async fn new_intra(&self, params: serde_json::Value) -> Result<()> {
        let req: NewIntraRequest = serde_json::from_value(params)?;
        tracing::info!("proxy process new intra {req:?}");

        let switchboard_host =
            tenant_switchboard_host(req.location.tenant_id.as_deref()).await;
        let _ = REDIS
            .hset(
                &format!("nebula:channel:{}", req.id),
                "switchboard_host",
                &switchboard_host,
            )
            .await;
        let _ = REDIS
            .expire(&format!("nebula:channel:{}", req.id), 86400)
            .await;
        let _ = PRPOXY_SERVICE
            .switchboard_rpc
            .new_inbound_channel(
                &switchboard_host,
                req.id.clone(),
                &req.endpoint,
                &PRPOXY_SERVICE.proxy_host,
                &req.location.user,
                &req.sdp,
                false,
                None,
                None,
                Some(req.intra),
                req.intra_chain,
                req.location.inter_tenant,
                Some(req.reply_to),
                None,
                None,
                None,
                None,
            )
            .await;
        Ok(())
    }

    async fn reinvite(&self, params: serde_json::Value) -> Result<()> {
        let req: NewInvite = serde_json::from_value(params)?;
        let _ = REDIS
            .hset(
                &format!("nebula:channel:{}", req.id),
                "switchboard_host",
                &req.switchboard_host,
            )
            .await;

        let msg_string: String = REDIS
            .hget(&format!("nebula:channel:{}", req.id), "dialog_message")
            .await?;
        let msg = Message::from_str(&msg_string)?;
        let mut invite = TM.dialog_msg(&msg).await?;

        let cseq = REDIS
            .hincrby_key_exits(&format!("nebula:channel:{}", req.id), "cseq")
            .await?;
        invite.method = Some(Method::INVITE);
        invite.cseq = Cseq {
            method: Method::INVITE,
            seq: cseq as i32 + 1,
        };
        invite.max_forwards = Some(70);
        invite.allow = Some(
            "SUBSCRIBE, NOTIFY, INVITE, ACK, CANCEL, BYE, REFER, INFO, OPTIONS, MESSAGE"
                .to_string(),
        );
        invite.content_type = Some("application/sdp".to_string());
        invite.content_disposition = Some("session".to_string());
        invite.body = Some(req.body);

        TM.send(&invite).await?;

        Ok(())
    }

    async fn notify_refer_success(&self, id: String) -> Result<()> {
        let msg_string: String = REDIS
            .hget(&format!("nebula:channel:{}", &id), "dialog_message")
            .await?;
        let msg = Message::from_str(&msg_string)?;
        let mut notify = TM.dialog_msg(&msg).await?;

        let cseq = REDIS
            .hincrby_key_exits(&format!("nebula:channel:{}", &id), "cseq")
            .await?;
        notify.method = Some(Method::NOTIFY);
        notify.max_forwards = Some(70);
        notify.cseq = Cseq {
            method: Method::NOTIFY,
            seq: cseq as i32 + 1,
        };
        notify.event = Some("refer".to_string());
        notify.subscription_state = Some("terminated;reason=noresource".to_string());
        notify.content_type = Some("message/sipfrag".to_string());
        notify.body = Some("SIP/2.0 200 OK\r\n".to_string());
        TM.send(&notify).await?;
        Ok(())
    }

    async fn notify_refer_trying(&self, id: String) -> Result<()> {
        let msg_string: String = REDIS
            .hget(&format!("nebula:channel:{}", &id), "dialog_message")
            .await?;
        let msg = Message::from_str(&msg_string)?;
        let mut notify = TM.dialog_msg(&msg).await?;

        let cseq = REDIS
            .hincrby_key_exits(&format!("nebula:channel:{}", &id), "cseq")
            .await?;
        notify.method = Some(Method::NOTIFY);
        notify.max_forwards = Some(70);
        notify.cseq = Cseq {
            method: Method::NOTIFY,
            seq: cseq as i32 + 1,
        };
        notify.event = Some("refer".to_string());
        notify.subscription_state = Some("active;expires=60".to_string());
        notify.content_type = Some("message/sipfrag".to_string());
        notify.body = Some("SIP/2.0 100 Trying\r\n".to_string());
        TM.send(&notify).await?;
        Ok(())
    }

    async fn handle_invite(&self, params: serde_json::Value) -> Result<(), Error> {
        let request: NewInvite = serde_json::from_value(params)?;
        let _ = REDIS
            .hset(
                &format!("nebula:channel:{}", request.id),
                "switchboard_host",
                &request.switchboard_host,
            )
            .await;
        let _ = REDIS
            .expire(&format!("nebula:channel:{}", request.id), 86400)
            .await;
        if let Err(e) = TM
            .invite(
                request.id.clone(),
                request.reply_to,
                request.inbound_cdr,
                request.caller_id,
                request.to,
                request.internal_call,
                request.alert_info_header,
                request.body,
                request.click_to_dial,
                request.room,
            )
            .await
        {
            error!(channel = request.id, "invite error: {e:?}");
        }
        Ok(())
    }

    async fn handle_answer(&self, params: serde_json::Value) -> Result<(), Error> {
        let response: AnswerResponse = serde_json::from_value(params)?;
        let msg_string: String = REDIS
            .hget(&format!("nebula:channel:{}", &response.id), "sip_message")
            .await?;
        let msg = Message::from_str(&msg_string)?;
        TM.answer(
            &msg,
            response.body,
            response.caller_id.clone(),
            response.cdr_uuid,
        )
        .await?;
        if let Some(dialog) = msg.dialog_id() {
            REDIS
                .set(&format!("nebula:dialog:{}", &dialog), &msg.callid)
                .await?;
        }
        Ok(())
    }

    async fn send_api(&self, text: &str, dest: &Uri) -> Result<()> {
        TM.send_api(text, dest).await?;
        Ok(())
    }

    async fn handle_response(&self, params: serde_json::Value) -> Result<(), Error> {
        let response: Response = serde_json::from_value(params)?;
        let msg_string: String = REDIS
            .hget(&format!("nebula:channel:{}", &response.id), "sip_message")
            .await?;
        let msg = Message::from_str(&msg_string)?;
        if response.code == 200 {
            TM.answer(&msg, response.body, None, None).await?;
        } else {
            TM.reply(
                &msg,
                response.code,
                response.status,
                if response.body.is_empty() {
                    None
                } else {
                    Some(response.body)
                },
                None,
            )
            .await?;
        }
        Ok(())
    }

    async fn handle_ack(&self, params: serde_json::Value) -> Result<(), Error> {
        let channel: RpcChannel = serde_json::from_value(params)?;
        let id = channel.id;
        let msg_string: String = REDIS
            .hget(&format!("nebula:channel:{}", &id), "sip_message")
            .await?;
        let resp = Message::from_str(&msg_string)?;

        let dialog_msg = REDIS
            .hget::<String>(&format!("nebula:channel:{}", &id), "dialog_message")
            .await
            .ok()
            .and_then(|msg| Message::from_str(&msg).ok());

        TM.ack(&resp, dialog_msg.as_ref()).await?;
        Ok(())
    }

    async fn handle_hangup(&self, params: serde_json::Value) -> Result<(), Error> {
        let hangup: ProxyHangup = serde_json::from_value(params)?;
        let id = hangup.id;

        let incoming_hangup: String = REDIS
            .hget(&format!("nebula:channel:{}", &id), "incoming_hangup")
            .await
            .unwrap_or_else(|_| "".to_string());

        if incoming_hangup == "yes" {
            return Ok(());
        }

        let inbound: String = REDIS
            .hget(&format!("nebula:channel:{}", &id), "inbound")
            .await
            .unwrap_or_else(|_| "".to_string());

        if !hangup.is_answered {
            let msg_string: String = REDIS
                .hget(&format!("nebula:channel:{}", &id), "sip_message")
                .await?;
            let msg = Message::from_str(&msg_string)?;
            if inbound == "yes" {
                TM.reply(&msg, 486, "Busy Here".to_string(), None, None)
                    .await?;
            } else {
                // if not inbound, we send cancel
                if let Err(e) = TM.cancel(&msg, hangup.reason.as_ref()).await {
                    error!(channel = msg.callid, "send cancel error: {e:?}");
                }
            }
        } else {
            let msg_string: String = REDIS
                .hget(&format!("nebula:channel:{}", &id), "dialog_message")
                .await?;
            let msg = Message::from_str(&msg_string)?;
            if let Err(_e) = TM.bye(&id, &msg).await {}
            if let Some(dialog) = msg.dialog_id() {
                REDIS
                    .expire(&format!("nebula:dialog:{}", &dialog), 60)
                    .await?;
            }
        }

        Ok(())
    }
}

impl Default for ProxyServer {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyServer {
    pub fn new() -> ProxyServer {
        ProxyServer {}
    }

    pub async fn run(&mut self) -> Result<(), Error> {
        let report_udp =
            Arc::new(tokio::net::UdpSocket::bind("0.0.0.0:5888").await.unwrap());
        TM.transport
            .listen(
                &PRPOXY_SERVICE.config.proxy_ip,
                report_udp.clone(),
                PRPOXY_SERVICE.config.udp_listen_proxy_ip.unwrap_or(false),
                PRPOXY_SERVICE.config.provider_udp_port,
                PRPOXY_SERVICE.config.provider_tls_port,
            )
            .await?;
        tokio::spawn(async move {
            let router_receiver = TM.router_receiver.lock().await.take().unwrap();
            loop {
                let msg = router_receiver.recv().await.unwrap();
                tokio::spawn(async move {
                    if let Err(_e) = PRPOXY_SERVICE.router.route(&msg).await {}
                });
            }
        });

        tokio::spawn(async move {
            let api_receiver = TM.api_receiver.lock().await.take().unwrap();
            loop {
                let (api, addr) = api_receiver.recv().await.unwrap();
                tokio::spawn(async move {
                    if let Err(_e) =
                        PRPOXY_SERVICE.router.handle_api(api, addr).await
                    {
                    }
                });
            }
        });

        tokio::spawn(async move {
            let mut receiver = RpcServer::new(
                format!("nebula:proxy:stream:{}", PRPOXY_SERVICE.local_ip),
                "".to_string(),
            )
            .await;
            loop {
                let (_entry, rpc_msg) = receiver.recv().await.unwrap();
                tokio::spawn(async move {
                    PRPOXY_SERVICE.process_rpc(rpc_msg).await;
                });
            }
        });

        tokio::spawn(async move {
            Self::nat_ping().await;
        });

        let location = PRPOXY_SERVICE.config.location.clone();
        tokio::spawn(async move {
            let _ = ProviderService::new(location).run().await;
        });

        {
            let mut receiver =
                RpcServer::new(PROXY_STREAM.to_string(), PROXY_GROUP.to_string())
                    .await;
            tokio::spawn(async move {
                loop {
                    let (entry, rpc_msg) = receiver.recv().await.unwrap();
                    tokio::spawn(async move {
                        let _ = entry.ack().await;
                        PRPOXY_SERVICE.process_rpc(rpc_msg).await;
                    });
                }
            });
        }

        let check_socket = Arc::new(UdpSocket::bind("0.0.0.0:6121").await?);
        check_socket
            .connect(format!(
                "{}:5060",
                if PRPOXY_SERVICE.config.udp_listen_proxy_ip.unwrap_or(false) {
                    &PRPOXY_SERVICE.config.proxy_ip
                } else {
                    "127.0.0.1"
                }
            ))
            .await?;

        let local_check_socket = check_socket.clone();
        tokio::spawn(async move {
            let mut buf = [0; 4096];
            loop {
                if let Ok(n) = local_check_socket.recv(&mut buf).await {
                    let mut buffer = buf[..n].to_vec();
                    tokio::spawn(async move {
                        if let Ok(text_str) = std::str::from_utf8_mut(&mut buffer) {
                            if let Ok(msg) = Message::from_str(text_str) {
                                let _ = REDIS
                                    .xaddex::<String>(
                                        &format!(
                                            "nebula:ping_message:{}:stream",
                                            msg.callid
                                        ),
                                        "message",
                                        text_str,
                                        10,
                                    )
                                    .await;
                            }
                        }
                    });
                }
            }
        });

        async fn ping(socket: Arc<UdpSocket>) -> Result<()> {
            let uri = Uri {
                scheme: "sip".to_string(),
                host: if PRPOXY_SERVICE.config.udp_listen_proxy_ip.unwrap_or(false) {
                    PRPOXY_SERVICE.config.proxy_ip.clone()
                } else {
                    "127.0.0.1".to_string()
                },
                transport: TransportType::Udp,
                ..Default::default()
            };

            let mut msg = Message::default();
            msg.remote_uri = Some(uri.clone());
            msg.method = Some(Method::OPTIONS);
            msg.request_uri = Some(uri.clone());
            msg.via.push(Via {
                host: "0.0.0.0".to_string(),
                branch: format!("z9hG4bK{}", nebula_utils::rand_string(20)),
                transport: uri.transport.clone(),
                ..Default::default()
            });
            msg.cseq = Cseq {
                method: Method::OPTIONS,
                seq: 1,
            };
            msg.from = Address {
                uri: uri.clone(),
                tag: Some(nebula_utils::rand_string(10)),
                ..Default::default()
            };
            msg.to = Address {
                uri: uri.clone(),
                ..Default::default()
            };
            let callid = nebula_utils::uuid();
            msg.callid = callid.clone();
            socket.send(msg.to_string().as_bytes()).await?;
            let (sender, mut receiver) = oneshot::channel();
            tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_millis(1000));
                loop {
                    tick.tick().await;
                    match receiver.try_recv() {
                        Ok(_) => break,
                        Err(TryRecvError::Closed) => break,
                        Err(TryRecvError::Empty) => (),
                    };
                    let _ = socket.send(msg.to_string().as_bytes()).await;
                }
            });

            let entries: Vec<redis::Value> = REDIS
                .xread_timeout(
                    &format!("nebula:ping_message:{}:stream", callid),
                    "0",
                    8000,
                    1,
                )
                .await?;
            let _ = sender.send(());
            let (_stream_name, entries): (String, Vec<redis::Value>) =
                redis::from_redis_value(
                    entries.first().ok_or(anyhow!("stream timeout"))?,
                )?;
            for entry in entries {
                let (_entry_id, entry_key_values): (String, Vec<String>) =
                    redis::from_redis_value(&entry)?;
                for (key, _value) in entry_key_values.iter().next_tuple() {
                    if key == "message" {
                        return Ok(());
                    }
                }
            }

            Err(anyhow!("didn't receive ping response"))
        }

        async fn health_check(
            Extension(socket): Extension<Arc<UdpSocket>>,
        ) -> Result<&'static str, axum::http::StatusCode> {
            let url = "http://10.132.0.107:8080/chat/integration/14106ead-5c2b-44cd-8478-876b533c8aa9/message";
            match ping(socket.clone()).await {
                Ok(_) => {
                    let mut last_ping = PRPOXY_SERVICE.last_ping.lock().await;
                    if !last_ping.0 {
                        *last_ping = (true, Instant::now());
                        tokio::spawn(async move {
                            let _ = reqwest::Client::new()
                                .post(url)
                                .header("X-Auth-Reseller", "yay")
                                .header("X-Auth-User", "admin")
                                .header("X-Auth-Password", "password")
                                .header("X-Auth-For", "y_yayyay")
                                .header("User-Agent", "nebula-proxy")
                                .json(&serde_json::json!({
                                    "to": "f53bb3e8-b119-40e8-807a-7e16894f12b3",
                                    "to_type": "channel",
                                    "content": format!("Nebula proxy back to normal on {}", PRPOXY_SERVICE.hostname),
                                }))
                                .timeout(std::time::Duration::from_secs(3))
                                .send()
                                .await;
                        });
                    }
                }
                Err(e) => {
                    error!("health check failed");
                    let mut last_ping = PRPOXY_SERVICE.last_ping.lock().await;
                    if last_ping.0 || last_ping.1.elapsed().as_secs() > 30 {
                        *last_ping = (false, Instant::now());
                        tokio::spawn(async move {
                            let _ = reqwest::Client::new()
                                .post(url)
                                .header("X-Auth-Reseller", "yay")
                                .header("X-Auth-User", "admin")
                                .header("X-Auth-Password", "password")
                                .header("X-Auth-For", "y_yayyay")
                                .header("User-Agent", "nebula-proxy")
                                .json(&serde_json::json!({
                                    "to": "f53bb3e8-b119-40e8-807a-7e16894f12b3",
                                    "to_type": "channel",
                                    "content": format!("Nebula proxy failed on {} error: {e}", PRPOXY_SERVICE.hostname),
                                }))
                                .timeout(std::time::Duration::from_secs(3))
                                .send()
                                .await;
                        });
                    }
                    return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                }
            };
            Ok("ok")
        }

        async fn stream_new_message(
            Json(rpc_msg): Json<RpcMessage>,
        ) -> Result<(), axum::http::StatusCode> {
            tokio::spawn(async move {
                let _ = RpcClient::send_to_stream(PROXY_STREAM, &rpc_msg).await;
            });
            Ok(())
        }

        async fn new_usrloc(
            Json(data): Json<UsrlocData>,
        ) -> Result<(), axum::http::StatusCode> {
            let _ = Usrloc::store_to_redis(&data).await;
            Ok(())
        }

        async fn tenant_switchboard_host(
            Path((tenant_id, switchboard_host)): Path<(String, String)>,
        ) {
            let key = &format!("nebula:tenant_switchboard_host:{tenant_id}");
            let _ = REDIS.setex(key, 86400, &switchboard_host).await;
        }

        async fn clear_tenant_switchboard_host(Path(tenant_id): Path<String>) {
            let key = &format!("nebula:tenant_switchboard_host:{tenant_id}");
            let _ = REDIS.del(key).await;
        }

        async fn clear_tenant_switchboard_host_if_match(
            Path((tenant_id, switchboard_host)): Path<(String, String)>,
        ) {
            let key = &format!("nebula:tenant_switchboard_host:{tenant_id}");
            let _ = REDIS.del_if_value(key, &switchboard_host).await;
        }

        async fn channel_hangup_cleanup(Path(channel_id): Path<String>) {
            let key = &format!("nebula:channel:{channel_id}");
            let _ = REDIS.expire(key, 60).await;
        }

        async fn clear_aux_log_cache(Path(user_id): Path<String>) {
            for host in PRPOXY_SERVICE.config.switchboards.iter() {
                let client = PRPOXY_SERVICE.switchboard_rpc.client.clone();
                let user_id = user_id.clone();
                let url =
                    format!("http://{host}/internal/clear_aux_log_cache/{user_id}");
                let _ = client
                    .put(url)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await;
            }
        }

        async fn clear_queue_login_log_cache(
            Path((user_id, queue_id)): Path<(String, String)>,
        ) {
            for host in PRPOXY_SERVICE.config.switchboards.iter() {
                let client = PRPOXY_SERVICE.switchboard_rpc.client.clone();
                let user_id = user_id.clone();
                let url =
                    format!("http://{host}/internal/clear_queue_login_log_cache/{user_id}/{queue_id}");
                let _ = client
                    .put(url)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await;
            }
        }

        async fn update_user_global_dnd(Path(user_id): Path<String>) {
            for host in PRPOXY_SERVICE.config.switchboards.iter() {
                let client = PRPOXY_SERVICE.switchboard_rpc.client.clone();
                let user_id = user_id.clone();
                let url = format!(
                    "http://{host}/internal/update_user_global_dnd/{user_id}"
                );
                let _ = client
                    .put(url)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await;
            }

            let _ = notify_dialog(
                PRPOXY_SERVICE
                    .config
                    .all_proxies()
                    .map(|p| p.to_vec())
                    .unwrap_or_else(|| {
                        vec![PRPOXY_SERVICE.config.proxy_host.clone()]
                    }),
                user_id.clone(),
            )
            .await;
        }

        async fn report_switchboard_channels(
            Path((switchboard_host, n)): Path<(String, usize)>,
        ) {
            if REDIS
                .get::<String>(&format!(
                    "nebula:switchboard_channels:{switchboard_host}:disabled"
                ))
                .await
                .unwrap_or_default()
                == "yes"
            {
                return;
            }

            let _ = REDIS
                .setex(
                    &format!("nebula:switchboard_channels:{switchboard_host}"),
                    10,
                    &switchboard_host,
                )
                .await;
            let _ = REDIS
                .zadd("nebula:switchboard_channels", n as i64, &switchboard_host)
                .await;
        }

        async fn in_queue_number(
            Path(queue): Path<String>,
        ) -> Result<String, axum::http::StatusCode> {
            let tenant_id = PRPOXY_SERVICE
                .db
                .get_queue_group(&queue)
                .await
                .map(|queue| queue.tenant_id);
            let switchboard_host = if let Some(tenant_id) = tenant_id {
                get_tenant_switchboard_host(&tenant_id).await
            } else {
                PRPOXY_SERVICE.config.switchboard_host.clone()
            };

            let number = PRPOXY_SERVICE
                .switchboard_rpc
                .client
                .get(format!("http://{switchboard_host}/v1/queue_stats/{queue}"))
                .send()
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?
                .text()
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;

            Ok(number)
        }

        async fn in_queue_longest(
            Path(queue): Path<String>,
        ) -> Result<String, axum::http::StatusCode> {
            let tenant_id = PRPOXY_SERVICE
                .db
                .get_queue_group(&queue)
                .await
                .map(|queue| queue.tenant_id);
            let switchboard_host = if let Some(tenant_id) = tenant_id {
                get_tenant_switchboard_host(&tenant_id).await
            } else {
                PRPOXY_SERVICE.config.switchboard_host.clone()
            };

            let number = PRPOXY_SERVICE
                .switchboard_rpc
                .client
                .get(format!(
                    "http://{switchboard_host}/v1/in_queue_longest/{queue}"
                ))
                .send()
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?
                .text()
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;

            Ok(number)
        }

        async fn account_queue_stats(
            Path(tenant_id): Path<String>,
        ) -> Result<Json<Vec<AccountQueueStatsResponse>>, axum::http::StatusCode>
        {
            let switchboard_host = get_tenant_switchboard_host(&tenant_id).await;
            let resp: Vec<AccountQueueStatsResponse> = PRPOXY_SERVICE
                .switchboard_rpc
                .client
                .get(format!(
                    "http://{switchboard_host}/v1/account_queue_stats/{tenant_id}"
                ))
                .send()
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?
                .json()
                .await
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Json(resp))
        }

        async fn parking_slot_presence(
            Path(slot): Path<String>,
        ) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
            let mut splits = slot.split('_');
            splits.next();
            let tenant_id =
                splits.next().ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let switchboard_host = get_tenant_switchboard_host(tenant_id).await;
            let json = PRPOXY_SERVICE
                .switchboard_rpc
                .client
                .get(format!("http://{switchboard_host}/v1/parking_slot/{slot}"))
                .send()
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?
                .json::<serde_json::Value>()
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
            Ok(Json::from(json))
        }

        async fn create_call(
            Json(call): Json<CreateCall>,
        ) -> axum::http::StatusCode {
            let user = if let Some(user_uuid) = call.user_uuid.as_ref() {
                PRPOXY_SERVICE.db.get_user("admin", user_uuid).await
            } else {
                None
            };

            let tenant_id = call
                .tenant_id
                .clone()
                .or_else(|| user.clone().map(|u| u.tenant_id));

            let tenant_id = match tenant_id {
                Some(tenant_id) => tenant_id,
                None => return axum::http::StatusCode::BAD_REQUEST,
            };

            let switchboard_host =
                crate::router::tenant_switchboard_host(Some(tenant_id.as_str()))
                    .await;

            let mut call = call.clone();
            call.proxy_host = Some(PRPOXY_SERVICE.config.proxy_host.clone());
            let _ = PRPOXY_SERVICE
                .switchboard_rpc
                .client
                .post(format!("http://{switchboard_host}/v1/calls"))
                .json(&call)
                .send()
                .await;

            axum::http::StatusCode::CREATED
        }

        async fn transfer_call(
            Json(call): Json<TransferCallRequest>,
        ) -> axum::http::StatusCode {
            let switchboard_host = crate::router::tenant_switchboard_host(Some(
                call.tenant_id.as_str(),
            ))
            .await;

            let _ = PRPOXY_SERVICE
                .switchboard_rpc
                .client
                .post(format!("http://{switchboard_host}/v1/transfer_calls"))
                .json(&call)
                .send()
                .await;

            axum::http::StatusCode::CREATED
        }

        async fn user_presence(
            Path(user_uuid): Path<String>,
        ) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
            let user = PRPOXY_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let switchboard_host =
                get_tenant_switchboard_host(&user.tenant_id).await;
            let json = PRPOXY_SERVICE
                .switchboard_rpc
                .client
                .get(format!(
                    "http://{switchboard_host}/v1/users/{user_uuid}/presence"
                ))
                .send()
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?
                .json::<serde_json::Value>()
                .await
                .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
            Ok(Json::from(json))
        }

        async fn queue_status(
            Path(user_uuid): Path<String>,
            Query(params): Query<HashMap<String, String>>,
        ) -> Result<String, axum::http::StatusCode> {
            let user = PRPOXY_SERVICE
                .db
                .get_user("admin", &user_uuid)
                .await
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
            let queue_id = params
                .get("queue_id")
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?;

            let switchboard_host =
                get_tenant_switchboard_host(&user.tenant_id).await;
            let status = PRPOXY_SERVICE
            .switchboard_rpc
            .client
            .get(format!(
                    "http://{switchboard_host}/v1/users/{user_uuid}/queue_status?queue_id={queue_id}"
                ))
            .send()
            .await
            .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?
            .text()
            .await
            .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;

            Ok(status)
        }

        let app = axum::Router::new()
            .route("/health-check", axum::routing::get(health_check))
            .route("/internal/stream", axum::routing::post(stream_new_message))
            .route("/usrlocs", axum::routing::post(new_usrloc))
            .route(
                "/v1/parking_slot/:slot",
                axum::routing::get(parking_slot_presence),
            )
            .route(
                "/v1/queue_stats/:queue",
                axum::routing::get(in_queue_number),
            )
            .route(
                "/v1/in_queue_longest/:queue",
                axum::routing::get(in_queue_longest),
            )
            .route(
                "/v1/account_queue_stats/:tenant_id",
                axum::routing::get(account_queue_stats),
            )
            .route(
                "/v1/users/:user_uuid/presence",
                axum::routing::get(user_presence),
            )
            .route(
                "/v1/users/:user_uuid/queue_status",
                axum::routing::get(queue_status),
            )
            .route("/v1/calls", axum::routing::post(create_call))
            .route("/v1/transfer_calls", axum::routing::post(transfer_call))
            .route(
                "/tenant_switchboard_host/:tenant_id/:switchboard_host",
                axum::routing::put(tenant_switchboard_host),
            )
            .route(
                "/clear_tenant_switchboard_host/:tenant_id",
                axum::routing::put(clear_tenant_switchboard_host),
            )
            .route(
                "/clear_tenant_switchboard_host_if_match/:tenant_id/:switchboard_host",
                axum::routing::put(clear_tenant_switchboard_host_if_match),
            )
            .route(
                "/channel_hangup_cleanup/:channel_id",
                axum::routing::put(channel_hangup_cleanup),
            )
            .route(
                "/clear_aux_log_cache/:user_id",
                axum::routing::put(clear_aux_log_cache),
            )
            .route(
                "/clear_queue_login_log_cache/:user_id/:queue_id",
                axum::routing::put(clear_queue_login_log_cache),
            )
            .route(
                "/update_user_global_dnd/:user_id",
                axum::routing::put(update_user_global_dnd),
            )
            .route(
                "/report_switchboard_channels/:switchboard_host/:n",
                axum::routing::put(report_switchboard_channels),
            )
            .layer(Extension(check_socket));
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
        axum::Server::bind(&SocketAddr::from_str("0.0.0.0:8121")?)
            .serve(app.into_make_service())
            .await?;

        Ok(())
    }

    async fn nat_ping() {
        let mut tick = tokio::time::interval(Duration::from_secs(20));
        loop {
            tick.tick().await;
            tokio::spawn(async move {
                let _ = Self::one_ping().await;
            });
        }
    }

    async fn one_ping() -> Result<()> {
        let key = Usrloc::all_contacts_key(&PRPOXY_SERVICE.proxy_host);
        let mut cursor = 0;
        loop {
            let (new_cursor, contact_keys) = REDIS.sscan(&key, cursor, 100).await?;
            for contact_key in contact_keys {
                tokio::spawn(async move {
                    let _ = Self::ping_contact(&contact_key).await;
                });
            }

            if new_cursor == 0 {
                return Ok(());
            }

            cursor = new_cursor;
        }
    }

    async fn ping_contact(contact_key: &str) -> Result<()> {
        let exists = REDIS.exists(contact_key).await?;
        if !exists {
            let _ = REDIS
                .srem(
                    &Usrloc::all_contacts_key(&PRPOXY_SERVICE.proxy_host),
                    contact_key,
                )
                .await;
            return Ok(());
        }

        let contact_map: HashMap<String, String> =
            REDIS.hgetall(contact_key).await?;
        let dst_uri = Uri::from_str(
            contact_map
                .get(RECEIVED)
                .ok_or(anyhow!("don't have received"))?,
        )?;
        if dst_uri.transport != TransportType::Udp {
            // if transport isn't udp, the connection might not
            // be made on this machine, so we check first
            // so that the packet can be directly sent from the machine
            // that the connection is actually on without go through
            // `local_send`
            if !TM
                .transport
                .has_addr(&dst_uri.transport, &dst_uri.addr())
                .await
                .unwrap_or(false)
            {
                return Ok(());
            }
        }

        if !REDIS
            .setexnx(&format!("{contact_key}:ping"), "yes", 19)
            .await
        {
            return Ok(());
        }

        let req_uri = Address::from_str(
            contact_map
                .get(CONTACT)
                .ok_or(anyhow!("don't have contact"))?,
        )?
        .uri;
        let location_id = contact_map
            .get(LOCATION_ID)
            .ok_or(anyhow!("no location id"))?;

        let mut msg = Message::default();

        let mut remote_uri = dst_uri.clone();
        remote_uri.is_registration = Some(true);
        msg.remote_uri = Some(remote_uri);

        msg.method = Some(Method::OPTIONS);
        msg.request_uri = Some(req_uri.clone());
        msg.via.push(Via {
            host: "0.0.0.0".to_string(),
            branch: format!("z9hG4bK{}", nebula_utils::rand_string(20)),
            transport: dst_uri.transport.clone(),
            ..Default::default()
        });
        msg.cseq = Cseq {
            method: Method::OPTIONS,
            seq: 1,
        };
        let mut from_uri = req_uri.clone();
        if let Some(user) = contact_map.get(USER) {
            from_uri.user = Some(user.clone());
        }
        msg.from = Address {
            uri: from_uri,
            tag: Some(nebula_utils::rand_string(10)),
            ..Default::default()
        };
        msg.to = Address {
            uri: req_uri.clone(),
            ..Default::default()
        };
        msg.callid = format!("{}@{location_id}", nebula_utils::rand_string(10));
        TM.transport.send(&msg).await?;

        Ok(())
    }
}

pub async fn get_tenant_switchboard_host(tenant_id: &str) -> String {
    let key = &format!("nebula:tenant_switchboard_host:{tenant_id}");
    let switchboard_host = REDIS.get::<String>(key).await.unwrap_or_default();
    if switchboard_host.is_empty() {
        PRPOXY_SERVICE.config.switchboard_host.clone()
    } else {
        switchboard_host
    }
}
