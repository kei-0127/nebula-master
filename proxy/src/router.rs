//! # SIP Message Router
//! 
//! This module handles SIP message routing decisions. It's the brain of the proxy,
//! deciding where each SIP message should go based on user location, authentication,
//! and routing rules.
//! 
//! ## Key Responsibilities
//! 
//! - **Route Decisions**: Determine where to send each SIP message
//! - **User Lookup**: Find user locations in the database
//! - **Authentication**: Verify user credentials before routing
//! - **Load Balancing**: Distribute calls across multiple switchboard instances
//! - **External Routing**: Route calls to external SIP providers
//! 
//! ## Routing Logic
//! 
//! 1. **Validate Message**: Check if the SIP message is well-formed
//! 2. **Authenticate User**: Verify user credentials
//! 3. **Lookup Destination**: Find where the called user is located
//! 4. **Route Decision**: Choose the best path for the message
//! 5. **Forward Message**: Send the message to the chosen destination
//! 
//! ## Common Routing Scenarios
//! 
//! - **Local to Local**: Both users in the same system
//! - **Local to External**: Outbound call to external provider
//! - **External to Local**: Inbound call from external provider
//! - **Registration**: User location updates
//! - **Presence**: User availability updates

use crate::presence::Presence;

use super::auth::Auth;
use super::server::PRPOXY_SERVICE;
use anyhow::{anyhow, Error, Result};
use jsonrpc_lite::JsonRpc;
use jwt::VerifyWithKey;
use nebula_db::{
    message::{
        AuthRequest, ConvertToRoom, CreateTempRoom, Endpoint, JoinRoomRequest,
        ReferTo, UserAuthClaim, JWT_CLAIMS_KEY,
    },
    usrloc::{Usrloc, UsrlocData, PROXY_HOST},
};
use nebula_redis::REDIS;
use nebula_rpc::{client::notify_dialog, message::RoomRequestMethod};
use sdp::{
    payload::{PayloadType, Rtpmap},
    sdp::{SessionDescription, DUMMY_RTP_PORT, INTERNAL_AUDIO_ORDER},
};
use sip::message::{Authorization, Message, MessageError, Method};
use sip::transaction::{TransactionError, TM};
use sip::transport::TransportType;
use sip::{message::Uri, transaction::TransactionType};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;

/// SIP message router that decides where to send each message
/// 
/// The router is the core component that makes routing decisions based on:
/// - User authentication status
/// - User location information
/// - Call destination (local vs external)
/// - Load balancing requirements
pub struct Router {
    auth: Auth,
    usrloc: Usrloc,
    presence: Presence,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Router {
            auth: Auth::new(),
            usrloc: Usrloc::new(),
            presence: Presence::new(),
        }
    }

    async fn route_resp(&self, msg: &Message) -> Result<(), Error> {
        if msg.cseq.method == Method::REGISTER {
            let _ = REDIS
                .xaddex::<String>(
                    &format!(
                        "nebula:register_response:{}:{}",
                        msg.callid, msg.cseq.seq
                    ),
                    "message",
                    &msg.to_string(),
                    60,
                )
                .await;
            return Ok(());
        }
        if msg.cseq.method == Method::OPTIONS {
            let location_id = msg.callid.split('@').nth(1);
            if let Some(location_id) = location_id {
                if let Some(user) = msg.from.uri.user.as_ref() {
                    let user = user.to_lowercase();
                    let key = Usrloc::contact_key(&user, location_id);
                    if REDIS
                        .hget::<String>(&key, PROXY_HOST)
                        .await
                        .unwrap_or_default()
                        != PRPOXY_SERVICE.config.proxy_host
                    {
                        // the udp load balancing could have shifted to a different zone
                        // we'll need to update the proxy_host
                        let data = UsrlocData {
                            user,
                            callid: "".to_string(),
                            location_id: location_id.to_string(),
                            proxy_host: PRPOXY_SERVICE.config.proxy_host.clone(),
                            contact: "".to_string(),
                            received: "".to_string(),
                            registered_at: "".to_string(),
                            expires_at: "".to_string(),
                            user_agent: "".to_string(),
                            expires: 0,
                        };
                        for host in PRPOXY_SERVICE.config.switchboards.iter() {
                            let host = host.to_string();
                            let client =
                                PRPOXY_SERVICE.switchboard_rpc.client.clone();
                            let data = data.clone();
                            tokio::spawn(async move {
                                let url = format!(
                                    "http://{host}/internal/usrlocs/proxy_host"
                                );
                                let _ = client.post(url).json(&data).send().await;
                            });
                        }
                    }
                }
            }
            return Ok(());
        }
        if msg.cseq.method != Method::INVITE {
            return Ok(());
        }
        let code = msg.code.as_ref().ok_or(MessageError::NotResponse)?;
        let id = &msg.callid;
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", id),
                "resp_code",
                &format!("{}", *code),
            )
            .await;

        let switchboard_host: String = REDIS
            .hget(&format!("nebula:channel:{}", id), "switchboard_host")
            .await?;
        match code {
            code if *code < 200 => {
                PRPOXY_SERVICE
                    .switchboard_rpc
                    .set_response(
                        &switchboard_host,
                        msg.callid.clone(),
                        *code,
                        msg.status.clone().unwrap_or_default(),
                        msg.body.clone().unwrap_or_default(),
                        msg.remote_uri.as_ref().map(|uri| uri.host.clone()),
                    )
                    .await?;
            }
            code if *code < 300 => {
                REDIS
                    .hset(
                        &format!("nebula:channel:{}", &msg.callid),
                        "sip_message",
                        &msg.to_string(),
                    )
                    .await?;
                REDIS
                    .hsetnx(
                        &format!("nebula:channel:{}", &msg.callid),
                        "dialog_message",
                        &msg.to_string(),
                    )
                    .await?;
                if let Some(contact) = msg.contact.as_ref() {
                    REDIS
                        .hset(
                            &format!("nebula:channel:{}", &msg.callid),
                            "contact",
                            &contact.to_string(),
                        )
                        .await?;
                }
                if let Some(dialog) = msg.dialog_id() {
                    REDIS
                        .setex(
                            &format!("nebula:dialog:{}", &dialog),
                            86400,
                            &msg.callid,
                        )
                        .await?;
                }
                if let Some(remote_uri) = &msg.remote_uri {
                    REDIS
                        .hset(
                            &format!("nebula:channel:{}", &msg.callid),
                            "remote_uri",
                            &serde_json::to_string(&remote_uri)?,
                        )
                        .await?;
                }
                PRPOXY_SERVICE
                    .switchboard_rpc
                    .answer(
                        &switchboard_host,
                        msg.callid.clone(),
                        msg.body.clone().unwrap_or_default(),
                        None,
                        None,
                        msg.remote_uri.as_ref().map(|uri| uri.host.clone()),
                    )
                    .await?;
            }
            status => {
                if *status == 407 && msg.cseq.seq < 3 {
                    let auth = msg
                        .proxy_authenticate
                        .as_ref()
                        .ok_or_else(|| anyhow!("no proxy authenticate"))?;
                    if !auth.realm.is_empty() {
                        if let Some(provider) = PRPOXY_SERVICE
                            .db
                            .get_provider_by_realm(&auth.realm)
                            .await
                        {
                            let user = provider.user.as_ref().ok_or_else(|| {
                                anyhow!("provider doens't have user")
                            })?;
                            let password =
                                provider.password.as_ref().ok_or_else(|| {
                                    anyhow!("provider doens't have password")
                                })?;

                            let tx =
                                TM.get_tx(msg, &TransactionType::Client).await?;
                            let mut invite = tx.get_request().await?;
                            invite.cseq.seq += 1;

                            let uri = provider.uri.as_ref().ok_or_else(|| {
                                anyhow!("provider doens't have uri")
                            })?;
                            let uri = Uri::from_str(uri)?;
                            let digest = Auth::digest(
                                user,
                                password,
                                &auth.realm,
                                &uri.to_string(),
                                &Method::INVITE.to_string(),
                                &auth.nonce,
                                None,
                                None,
                                None,
                            );
                            invite.proxy_authorization = Some(Authorization {
                                username: user.to_string(),
                                realm: auth.realm.clone(),
                                nonce: auth.nonce.clone(),
                                uri: uri.to_string(),
                                response: digest,
                                cnonce: None,
                                qop: None,
                                nonce_count: None,
                            });
                            TM.send(&invite).await?;
                            return Ok(());
                        }
                    }
                }

                let answered = is_channel_answered(id).await;
                if answered {
                } else {
                    REDIS
                        .hset(
                            &format!("nebula:channel:{}", id),
                            "incoming_hangup",
                            "yes",
                        )
                        .await?;
                    let redirect = if *code == 302 {
                        msg.contact.clone().and_then(|c| c.uri.user)
                    } else {
                        None
                    };
                    PRPOXY_SERVICE
                        .switchboard_rpc
                        .hangup(
                            &switchboard_host,
                            msg.callid.clone(),
                            true,
                            Some(*code as usize),
                            redirect,
                            None,
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    pub async fn handle_api(&self, rpc: JsonRpc, addr: String) -> Result<()> {
        if let Some(method) = rpc.get_method() {
            if method == "sip" {
                let params = rpc
                    .get_params()
                    .ok_or_else(|| anyhow!("sip doens't have params"))?;
                let mut msg: Message =
                    serde_json::from_value(serde_json::to_value(params)?)?;
                let addr = SocketAddr::from_str(&addr)?;
                msg.remote_uri = Some(Uri {
                    scheme: "sip".to_string(),
                    host: addr.ip().to_string(),
                    port: Some(addr.port()),
                    transport: TransportType::Wss,
                    ..Default::default()
                });
                self.route(&msg).await?;
                return Ok(());
            }

            let switchboard_host =
                switchboard_host_from_rpc_and_addr(&rpc, &addr).await;
            PRPOXY_SERVICE
                .switchboard_rpc
                .room_rpc(&switchboard_host, rpc, addr, &PRPOXY_SERVICE.proxy_host)
                .await?;
        }

        Ok(())
    }

    pub async fn route(&self, msg: &Message) -> Result<(), Error> {
        if !msg.is_request() {
            self.route_resp(msg).await?;
            return Ok(());
        }

        let method = msg.method.as_ref().ok_or(TransactionError::RouteStop)?;
        match method {
            Method::CANCEL => {
                self.cancel(msg).await?;
                return Ok(());
            }
            Method::OPTIONS => {
                TM.reply(msg, 200, "OK".to_string(), None, None).await?;
                return Ok(());
            }
            _ => (),
        };

        if let Some(dialog) = msg.dialog_id() {
            let channel_id: String = REDIS
                .get(&format!("nebula:dialog:{}", &dialog))
                .await
                .unwrap_or_else(|_| "".to_string());
            if channel_id.is_empty() {
                let _ = TM
                    .reply(msg, 400, "Bad Request".to_string(), None, None)
                    .await;
                return Err(anyhow!("can't find matching dialog"));
            }
            match method {
                Method::BYE => {
                    self.bye(msg).await?;
                }
                Method::REFER => {
                    self.refer(msg).await?;
                }
                Method::INFO => {
                    self.info(msg).await?;
                }
                Method::SUBSCRIBE => {
                    self.subscribe(msg).await?;
                }
                Method::INVITE => {
                    REDIS
                        .hset(
                            &format!("nebula:channel:{}", &msg.callid),
                            "sip_message",
                            &msg.to_string(),
                        )
                        .await?;
                    if let Some(contact) = msg.contact.as_ref() {
                        REDIS
                            .hset(
                                &format!("nebula:channel:{}", &msg.callid),
                                "contact",
                                &contact.to_string(),
                            )
                            .await?;
                    }
                    let switchboard_host: String = REDIS
                        .hget(
                            &format!("nebula:channel:{}", msg.callid),
                            "switchboard_host",
                        )
                        .await
                        .unwrap_or_else(|_| {
                            PRPOXY_SERVICE.config.switchboard_host.clone()
                        });
                    PRPOXY_SERVICE
                        .switchboard_rpc
                        .reinvite(
                            &switchboard_host,
                            msg.callid.clone(),
                            msg.body.clone().unwrap_or_default(),
                        )
                        .await?;
                }
                Method::ACK => {
                    let switchboard_host: String = REDIS
                        .hget(
                            &format!("nebula:channel:{}", msg.callid),
                            "switchboard_host",
                        )
                        .await
                        .unwrap_or_else(|_| {
                            PRPOXY_SERVICE.config.switchboard_host.clone()
                        });
                    PRPOXY_SERVICE
                        .switchboard_rpc
                        .ack(&switchboard_host, msg.callid.clone())
                        .await?;
                }
                _ => (),
            };
            return Ok(());
        }

        if msg.global_dnd.as_deref() == Some("yes") {
            TM.reply(msg, 486, "Global DND is on".to_string(), None, None)
                .await?;
            return Err(TransactionError::RouteStop)?;
        }

        if let Some(global_forward) = msg.global_forward.as_ref() {
            TM.reply(
                msg,
                302,
                "Moved Temporarily".to_string(),
                None,
                Some(global_forward.to_string()),
            )
            .await?;
            return Err(TransactionError::RouteStop)?;
        }

        let endpoint = match self.auth.get_endpoint(msg).await {
            None => {
                let resp = self.auth.challenge(msg)?;
                TM.respond(msg, &resp).await?;
                Err(TransactionError::RouteStop)?
            }
            Some(e) => e,
        };

        match method {
            Method::REGISTER => self.register(msg, &endpoint).await?,
            Method::SUBSCRIBE => self.subscribe(msg).await?,
            Method::INVITE => self.invite(msg, &endpoint).await?,
            _ => {
                TM.reply(msg, 405, "Method Not Allowed".to_string(), None, None)
                    .await?
            }
        };

        Ok(())
    }

    async fn info(&self, msg: &Message) -> Result<()> {
        if msg.content_type.as_deref() == Some("application/media_control+xml")
            && msg.body.as_ref().map(|b| b.contains("picture_fast_update"))
                == Some(true)
        {
            let switchboard_host: String = REDIS
                .hget(
                    &format!("nebula:channel:{}", msg.callid),
                    "switchboard_host",
                )
                .await
                .unwrap_or_else(|_| PRPOXY_SERVICE.config.switchboard_host.clone());

            PRPOXY_SERVICE
                .switchboard_rpc
                .request_keyframe(&switchboard_host, msg.callid.clone())
                .await?;
            let _ = TM.reply(msg, 200, "OK".to_string(), None, None).await;
        }

        Ok(())
    }

    async fn refer(&self, msg: &Message) -> Result<()> {
        let refer_to = msg
            .refer_to
            .as_ref()
            .ok_or_else(|| anyhow!("sip message doesn't have Refer-To"))?;
        let to = if let Some(replaces) = refer_to.uri.headers.get("Replaces") {
            ReferTo::Channel(replaces.split(';').next().unwrap_or("").to_string())
        } else if refer_to.uri.scheme == "tel" {
            ReferTo::Exten(refer_to.uri.host.clone())
        } else if refer_to.uri.host.ends_with("pstnhub.microsoft.com")
            && msg
                .refer_by
                .as_ref()
                .and_then(|r| r.uri.params.get("x-m").cloned().flatten())
                .is_some()
        {
            let refer_by = msg
                .refer_by
                .as_ref()
                .ok_or_else(|| anyhow!("no referred-by"))?
                .clone();
            ReferTo::Teams {
                refer_to: refer_to.to_owned(),
                refer_by,
            }
        } else {
            ReferTo::Exten(
                refer_to.uri.user.clone().unwrap_or_else(|| "".to_string()),
            )
        };
        let switchboard_host: String = REDIS
            .hget(
                &format!("nebula:channel:{}", msg.callid),
                "switchboard_host",
            )
            .await
            .unwrap_or_else(|_| PRPOXY_SERVICE.config.switchboard_host.clone());
        PRPOXY_SERVICE
            .switchboard_rpc
            .refer(
                &switchboard_host,
                msg.callid.clone(),
                to,
                refer_to.clone(),
                msg.refer_by.clone(),
            )
            .await?;
        TM.reply(msg, 202, "Accepted".to_string(), None, None)
            .await?;

        Ok(())
    }

    async fn bye(&self, msg: &Message) -> Result<(), Error> {
        if let Some(dialog) = msg.dialog_id() {
            REDIS
                .expire(&format!("nebula:dialog:{}", &dialog), 60)
                .await?;
        }

        let id = msg.callid.clone();
        let answered = is_channel_answered(&id).await;
        if answered {
            REDIS
                .hset(&format!("nebula:channel:{}", id), "incoming_hangup", "yes")
                .await?;

            let switchboard_host: String = REDIS
                .hget(&format!("nebula:channel:{}", id), "switchboard_host")
                .await
                .unwrap_or_else(|_| PRPOXY_SERVICE.config.switchboard_host.clone());

            PRPOXY_SERVICE
                .switchboard_rpc
                .hangup(&switchboard_host, id, true, None, None, None)
                .await?;
            TM.reply(msg, 200, "OK".to_string(), None, None).await?;
            return Ok(());
        }

        TM.reply(msg, 400, "Invalid Request".to_string(), None, None)
            .await?;
        Ok(())
    }

    async fn cancel(&self, msg: &Message) -> Result<(), Error> {
        let switchboard_host: String = REDIS
            .hget(
                &format!("nebula:channel:{}", msg.callid),
                "switchboard_host",
            )
            .await
            .unwrap_or_else(|_| PRPOXY_SERVICE.config.switchboard_host.clone());
        PRPOXY_SERVICE
            .switchboard_rpc
            .cancel(&switchboard_host, msg.callid.clone())
            .await?;
        TM.reply(msg, 200, "Canceling".to_string(), None, None)
            .await?;

        let msg_string: String = REDIS
            .hget(&format!("nebula:channel:{}", &msg.callid), "sip_message")
            .await?;
        let msg = Message::from_str(&msg_string)?;
        TM.reply(&msg, 487, "Request Terminated".to_string(), None, None)
            .await?;

        Ok(())
    }

    async fn subscribe(&self, msg: &Message) -> Result<()> {
        let expires = msg.expires.unwrap_or(0);
        if expires > 0 && expires < 600 {
            // only allow subscribe period to be more than 10 minutes
            TM.reply(msg, 423, "Interval Too Brief".to_string(), None, None)
                .await?;
            return Err(TransactionError::RouteStop)?;
        }

        let mut msg = msg.clone();
        if msg.to.tag.is_none() {
            msg.to.tag = Some(nebula_utils::rand_string(10));
        }

        let result = self.presence.subscribe(&msg).await;

        let mut resp = Message::default();
        resp.code = Some(200);
        resp.status = Some("OK".to_string());
        resp.via = msg.via.clone();
        resp.from = msg.from.clone();
        resp.to = msg.to.clone();
        resp.callid = msg.callid.clone();
        resp.cseq = msg.cseq.clone();
        resp.expires = msg.expires.clone();
        let _ = TM.respond(&msg, &resp).await;

        if let Ok(watcher) = result {
            let expires = msg.expires.unwrap_or(0);
            if expires > 0 {
                let _ = watcher.notify().await;
            }
        }

        Err(TransactionError::RouteStop)?
    }

    async fn register(
        &self,
        msg: &Message,
        endpoint: &Endpoint,
    ) -> Result<(), Error> {
        if msg.method.as_ref().unwrap_or(&Method::INVITE) != &Method::REGISTER {
            return Ok(());
        }

        if !Usrloc::check_usrloc(msg).await {
            return Ok(());
        }

        let data = self
            .usrloc
            .save(&PRPOXY_SERVICE.proxy_host, msg)
            .await
            .ok_or_else(|| anyhow!("cant save usrloc"))?;
        for host in PRPOXY_SERVICE.config.switchboards.iter() {
            let host = host.to_string();
            let data = data.clone();
            let client = PRPOXY_SERVICE.switchboard_rpc.client.clone();
            tokio::spawn(async move {
                let url = format!("http://{host}/internal/usrlocs");
                let _ = client.post(url).json(&data).send().await;
            });
        }
        let resp = self.usrloc.response(msg, 200, "OK", data.expires as i32);

        {
            let register = data.expires > 0;
            let register = register.to_string();
            let key = &format!("nebula:last_register:{}", data.user);
            let old_register = REDIS
                .getset::<String>(key, &register)
                .await
                .unwrap_or_default();
            let _ = REDIS.expire(key, 86400).await;

            if old_register != register {
                if let Some(user) = endpoint.user() {
                    let _ = notify_dialog(
                        PRPOXY_SERVICE
                            .config
                            .all_proxies()
                            .map(|p| p.to_vec())
                            .unwrap_or_else(|| {
                                vec![PRPOXY_SERVICE.config.proxy_host.clone()]
                            }),
                        user.uuid.clone(),
                    )
                    .await;
                }
            }
        }

        TM.respond(msg, &resp).await?;

        Err(TransactionError::RouteStop)?
    }

    async fn invite(&self, msg: &Message, endpoint: &Endpoint) -> Result<()> {
        let uri = msg
            .request_uri
            .as_ref()
            .ok_or(TransactionError::RouteStop)?;

        TM.reply(msg, 100, "Trying".to_string(), None, None).await?;

        let mut msg = msg.clone();
        if msg.to.tag.is_none() {
            msg.to.tag = Some(nebula_utils::rand_string(10));
        }

        REDIS
            .hset(
                &format!("nebula:channel:{}", &msg.callid),
                "sip_message",
                &msg.to_string(),
            )
            .await?;
        REDIS
            .hset(
                &format!("nebula:channel:{}", &msg.callid),
                "dialog_message",
                &msg.to_string(),
            )
            .await?;
        if let Some(contact) = msg.contact.as_ref() {
            REDIS
                .hset(
                    &format!("nebula:channel:{}", &msg.callid),
                    "contact",
                    &contact.to_string(),
                )
                .await?;
        }

        if let Some(remote_uri) = &msg.remote_uri {
            REDIS
                .hset(
                    &format!("nebula:channel:{}", &msg.callid),
                    "remote_uri",
                    &serde_json::to_string(&remote_uri)?,
                )
                .await?;
        }
        REDIS
            .hset(&format!("nebula:channel:{}", &msg.callid), "inbound", "yes")
            .await?;

        let to = if uri.scheme == "tel" {
            uri.host.clone()
        } else {
            uri.user.clone().unwrap_or_else(|| "".to_string())
        };
        let switchboard_host = tenant_switchboard_host(endpoint.tenant_id()).await;
        let _ = REDIS
            .hset(
                &format!("nebula:channel:{}", msg.callid),
                "switchboard_host",
                &switchboard_host,
            )
            .await;
        let _ = REDIS
            .expire(&format!("nebula:channel:{}", msg.callid), 86400)
            .await;
        let sdp = msg.body.as_deref().unwrap_or("");
        let raw_transfer = msg.transfer_target.as_ref().and_then(|t| {
            if !t.is_empty() {
                Some((
                    t.to_string(),
                    msg.transfer_auto_answer
                        .as_ref()
                        .map(|a| a != "no")
                        .unwrap_or(true),
                    sdp.is_empty(),
                ))
            } else {
                None
            }
        });
        let sdp = if raw_transfer.is_some() && sdp.is_empty() {
            let mut rtpmaps: Vec<Rtpmap> = INTERNAL_AUDIO_ORDER
                .iter()
                .map(|pt| pt.get_rtpmap())
                .collect();
            rtpmaps.push(PayloadType::TelephoneEvent.get_rtpmap());
            SessionDescription::create_basic(
                Ipv4Addr::new(127, 0, 0, 1),
                DUMMY_RTP_PORT,
                None,
                rtpmaps,
            )
            .to_string()
        } else {
            sdp.to_string()
        };
        let _ = PRPOXY_SERVICE
            .switchboard_rpc
            .new_inbound_channel(
                &switchboard_host,
                msg.callid.clone(),
                endpoint,
                &PRPOXY_SERVICE.proxy_host,
                &to,
                &sdp,
                false,
                msg.replaces.clone(),
                None,
                None,
                Vec::new(),
                None,
                None,
                raw_transfer,
                None,
                None,
                msg.remote_uri.as_ref().map(|uri| uri.host.clone()),
            )
            .await;

        Err(TransactionError::RouteStop)?
    }
}

async fn pick_switchboard_host() -> Result<String> {
    let key = "nebula:switchboard_channels";
    let hosts = REDIS.zrange(key, 0, -1).await?;
    for host in hosts {
        if REDIS
            .exists(&format!("nebula:switchboard_channels:{host}"))
            .await
            .unwrap_or(false)
        {
            return Ok(host);
        }
        let _ = REDIS.zrem(key, &host).await;
    }
    Err(anyhow!("no switchboard host available"))
}

async fn is_switchboard_host_live(switchboard_host: &str) -> bool {
    REDIS
        .exists(&format!("nebula:switchboard_channels:{switchboard_host}"))
        .await
        .unwrap_or(false)
}

async fn decide_switchboard_host(tenant_id: &str) -> String {
    let key = &format!("nebula:tenant_switchboard_host:{tenant_id}");
    let switchboard_host: String =
        REDIS.get_and_ex(key, 86400).await.unwrap_or_default();

    let is_live = if !switchboard_host.is_empty() {
        is_switchboard_host_live(&switchboard_host).await
    } else {
        false
    };

    if !is_live {
        if !switchboard_host.is_empty() {
            tracing::error!(
                "the existing switchboard host {switchboard_host} isn't live"
            );
        }
        let picked_switchboard_host = pick_switchboard_host()
            .await
            .unwrap_or_else(|_| PRPOXY_SERVICE.config.switchboard_host.clone());

        REDIS
            .set_or_ex(key, &picked_switchboard_host, 86400)
            .await
            .unwrap_or(picked_switchboard_host)
    } else {
        switchboard_host
    }
}

pub(crate) async fn tenant_switchboard_host(tenant_id: Option<&str>) -> String {
    if let Some(tenant_id) = tenant_id {
        if let Some(proxies) = PRPOXY_SERVICE.config.all_proxies() {
            let switchboard_host = decide_switchboard_host(tenant_id).await;
            for host in proxies {
                if host != &PRPOXY_SERVICE.config.proxy_host {
                    let host = host.to_string();
                    let switchboard_host = switchboard_host.clone();
                    let tenant_id = tenant_id.to_string();
                    let client = PRPOXY_SERVICE.switchboard_rpc.client.clone();
                    tokio::spawn(async move {
                        let url =
                            format!("http://{host}/tenant_switchboard_host/{tenant_id}/{switchboard_host}");
                        let _ = client.put(url).send().await;
                    });
                }
            }
            return switchboard_host;
        }
    }

    PRPOXY_SERVICE.config.switchboard_host.clone()
}

async fn is_channel_answered(id: &str) -> bool {
    let mut switchboard_host: String = REDIS
        .hget(&format!("nebula:channel:{}", id), "switchboard_host")
        .await
        .unwrap_or_else(|_| "".to_string());
    if switchboard_host.is_empty() {
        switchboard_host = PRPOXY_SERVICE.config.switchboard_host.clone();
    }
    let resp = PRPOXY_SERVICE
        .switchboard_rpc
        .client
        .get(&format!(
            "http://{switchboard_host}/v1/channels/{id}/answered"
        ))
        .send()
        .await;
    match resp {
        Ok(resp) => resp.text().await.ok() == Some("yes".to_string()),
        Err(_) => false,
    }
}

async fn switchboard_host_from_rpc_and_addr(rpc: &JsonRpc, addr: &str) -> String {
    let key = format!("nebula:room_rpc_addr_switchboard:{addr}");
    let switchboard: String = REDIS.get(&key).await.unwrap_or_default();
    if !switchboard.is_empty() {
        return switchboard;
    }

    let switchboard_host = match switchboard_host_from_rpc(rpc).await {
        Ok(host) => host,
        Err(e) => {
            tracing::error!("get switchboard host from rpc {rpc:?} error: {e:?}");
            PRPOXY_SERVICE.config.switchboard_host.clone()
        }
    };
    let _ = REDIS.setex(&key, 86400, &switchboard_host).await;
    switchboard_host
}

async fn switchboard_host_from_rpc(rpc: &JsonRpc) -> Result<String> {
    if let JsonRpc::Request(_) = &rpc {
        let method = rpc.get_method().ok_or_else(|| anyhow!("no method"))?;
        let params = rpc.get_params().ok_or_else(|| anyhow!("no params"))?;
        let method = RoomRequestMethod::from_str(method)?;
        let params = serde_json::to_value(params)?;
        match method {
            RoomRequestMethod::Authenticate => {
                let request: AuthRequest = serde_json::from_value(params)?;
                let room = PRPOXY_SERVICE
                    .db
                    .get_room(&request.room)
                    .await
                    .ok_or(anyhow!("room not exists"))?;
                Ok(tenant_switchboard_host(Some(room.tenant_id.as_str())).await)
            }
            RoomRequestMethod::CreateTempRoom => {
                let request: CreateTempRoom = serde_json::from_value(params)?;
                let user = if let Some(device) = request.device.as_ref() {
                    let device_token = request.device_token.as_deref().unwrap_or("");
                    PRPOXY_SERVICE
                        .db
                        .get_user_by_device_token(device, device_token)
                        .await
                } else if let Some(username) = request.username.as_ref() {
                    let password = request.password.as_deref().unwrap_or("");
                    let user = PRPOXY_SERVICE.db.get_user_by_name(username).await;
                    user.filter(|u| u.password == password)
                } else {
                    None
                };

                let user = user.ok_or_else(|| anyhow!("no user"))?;
                Ok(tenant_switchboard_host(Some(user.tenant_id.as_str())).await)
            }
            RoomRequestMethod::ConvertToRoom => {
                let request: ConvertToRoom = serde_json::from_value(params)?;

                let user =
                    PRPOXY_SERVICE.db.get_user_by_name(&request.username).await;
                let user = user.ok_or_else(|| anyhow!("no user"))?;

                Ok(tenant_switchboard_host(Some(user.tenant_id.as_str())).await)
            }
            RoomRequestMethod::JoinRoom => {
                let request: JoinRoomRequest = serde_json::from_value(params)?;
                let claims: UserAuthClaim = request
                    .auth_token
                    .verify_with_key(&*JWT_CLAIMS_KEY)
                    .map_err(|_| anyhow!("authentication error"))?;
                let mut splits = claims.room.split(':');
                let first = splits
                    .next()
                    .ok_or_else(|| anyhow!("room id not long enough"))?;
                let second = splits.next();
                if second.is_some() {
                    // if second got value, then it's a temp room with the format of
                    // tenant_id:random_id
                    Ok(tenant_switchboard_host(Some(first)).await)
                } else {
                    let room = PRPOXY_SERVICE
                        .db
                        .get_room(first)
                        .await
                        .ok_or(anyhow!("room not exists"))?;
                    Ok(tenant_switchboard_host(Some(room.tenant_id.as_str())).await)
                }
            }
            _ => Err(anyhow!(format!(
                "can't find switchboard host for api request, method: {method:?}"
            ))),
        }
    } else {
        Err(anyhow!("api is not request"))
    }
}
