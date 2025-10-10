use super::fsm::{self, Input, State};
use super::message::{Address, MessageError, Method};
use super::message::{Cseq, Message, Uri, Via};
use super::transport::{TransportManager, TransportType};
use crate::message::GenericHeader;
use crate::message::{CallerId, HangupReason, Location};
use anyhow::{anyhow, Error, Result};
use async_channel::{Receiver, Sender};
use indexmap::IndexMap;
use jsonrpc_lite::JsonRpc;
use lazy_static::lazy_static;
use nebula_redis::{DistributedMutex, REDIS};
use nebula_utils::{get_local_ip, sha256};
use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use strum_macros;
use strum_macros::EnumString;
use thiserror::Error;
use tokio::sync::Mutex;

const STATE: &str = "state";
const REQUEST: &str = "request";
const RESPONSE: &str = "response";
const LAST_RESPONSE: &str = "last_response";
const ADDR_HOST: &str = "addr_host";
const ADDR_IP: &str = "addr_ip";
const ADDR_PORT: &str = "addr_port";
const ADDR_TYPE: &str = "addr_type";
const ADDR_IS_PROVIDER: &str = "addr_is_provider";
const T1: Duration = Duration::from_millis(150);

lazy_static! {
    pub static ref TM: TransactionManager = TransactionManager::new();
}

#[derive(Debug, Error)]
pub enum TransactionError {
    #[error("transaction not exit")]
    TransactionNotExist,
    #[error("transaction alreday exit")]
    TransactionExist,
    #[error("transaction not valid message")]
    TransactionNotValidMessage,
    #[error("addr in transaction invalid")]
    AddrInvalid,
    #[error("route stopped here")]
    RouteStop,
}

#[derive(strum_macros::Display, EnumString, PartialEq, Eq, Clone, Debug)]
pub enum TransactionType {
    Client,
    Server,
}

#[derive(Clone, Debug)]
pub struct Transaction {
    pub key: TransactionKey,
}

#[derive(Clone, Debug)]
pub struct TransactionKey {
    branch: String,
    pub method: Method,
    callid: String,
    pub tx_type: TransactionType,
    encoded: String,
}

pub struct TransactionManager {
    pub transport: TransportManager,
    router_sender: Sender<Message>,
    pub router_receiver: Arc<Mutex<Option<Receiver<Message>>>>,
    pub api_receiver: Arc<Mutex<Option<Receiver<(JsonRpc, String)>>>>,
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionManager {
    pub fn new() -> TransactionManager {
        let local_ip = get_local_ip().unwrap_or("".to_string());
        let (msg_sender, msg_receiver): (Sender<Message>, Receiver<Message>) =
            async_channel::unbounded();
        let (router_sender, router_receiver): (Sender<Message>, Receiver<Message>) =
            async_channel::unbounded();
        let (api_sender, api_receiver): (
            Sender<(JsonRpc, String)>,
            Receiver<(JsonRpc, String)>,
        ) = async_channel::unbounded();

        let transport =
            TransportManager::new(msg_sender, api_sender, local_ip.clone());
        let tm = TransactionManager {
            transport,
            router_sender,
            router_receiver: Arc::new(Mutex::new(Some(router_receiver))),
            api_receiver: Arc::new(Mutex::new(Some(api_receiver))),
        };

        tokio::spawn(async move {
            loop {
                let msg = msg_receiver.recv().await.unwrap();
                tokio::spawn(async move {
                    let _ = TM.handle_msg(msg).await;
                });
            }
        });

        tm
    }

    pub async fn cancel(
        &self,
        msg: &Message,
        reason: Option<&HangupReason>,
    ) -> Result<(), Error> {
        let tx = self.get_tx(msg, &TransactionType::Client).await?;
        tx.cancel(reason).await?;
        Ok(())
    }

    pub async fn dialog_msg(&self, msg: &Message) -> Result<Message> {
        let remote_uri_str: String = REDIS
            .hget(&format!("nebula:channel:{}", &msg.callid), "remote_uri")
            .await?;
        let remote_uri: Uri = serde_json::from_str(&remote_uri_str)?;
        let mut new_msg = Message::default();
        let contact_str = REDIS
            .hget(&format!("nebula:channel:{}", &msg.callid), "contact")
            .await
            .unwrap_or("".to_string());
        println!("contact string  {}", contact_str);
        let contact_result = Address::from_str(&contact_str);
        let contact = msg.contact.as_ref().unwrap_or(
            contact_result.as_ref().unwrap_or(if msg.is_request() {
                &msg.from
            } else {
                &msg.to
            }),
        );
        new_msg.request_uri = Some(contact.uri.clone());
        new_msg.remote_uri = Some(remote_uri);
        new_msg.max_forwards = Some(70);
        if msg.is_request() {
            new_msg.route = msg.record_route.clone()
        } else {
            new_msg.route =
                msg.record_route.iter().rev().map(|a| a.clone()).collect()
        }
        match msg.is_request() {
            true => {
                new_msg.from = msg.to.clone();
                new_msg.to = msg.from.clone();
            }
            false => {
                new_msg.from = msg.from.clone();
                new_msg.to = msg.to.clone();
            }
        };
        new_msg.callid = msg.callid.clone();
        Ok(new_msg)
    }

    pub async fn bye(&self, id: &str, msg: &Message) -> Result<(), Error> {
        let remote_uri_str: String = REDIS
            .hget(&format!("nebula:channel:{}", id), "remote_uri")
            .await?;
        println!("remtoe_uri_str is {}", &remote_uri_str);
        let remote_uri: Uri = serde_json::from_str(&remote_uri_str)?;

        let mut bye = Message::default();
        bye.method = Some(Method::BYE);
        bye.max_forwards = Some(70);
        let contact_str = REDIS
            .hget(&format!("nebula:channel:{}", id), "contact")
            .await
            .unwrap_or("".to_string());
        let contact_result = Address::from_str(&contact_str);
        let contact = msg.contact.as_ref().unwrap_or(
            contact_result.as_ref().unwrap_or(if msg.is_request() {
                &msg.from
            } else {
                &msg.to
            }),
        );
        bye.request_uri = Some(contact.uri.clone());
        bye.remote_uri = Some(remote_uri);

        if msg.is_request() {
            bye.route = msg.record_route.clone()
        } else {
            bye.route = msg.record_route.iter().rev().map(|a| a.clone()).collect()
        }

        match msg.is_request() {
            true => {
                bye.from = msg.to.clone();
                bye.to = msg.from.clone();
            }
            false => {
                bye.from = msg.from.clone();
                bye.to = msg.to.clone();
            }
        };
        bye.callid = msg.callid.clone();
        let cseq = REDIS
            .hincrby_key_exits(&format!("nebula:channel:{}", &id), "cseq")
            .await?;
        bye.cseq = Cseq {
            method: Method::BYE,
            seq: cseq as i32 + 1,
        };

        self.send(&bye).await?;

        Ok(())
    }

    pub async fn ack(
        &self,
        resp: &Message,
        dialog_msg: Option<&Message>,
    ) -> Result<(), Error> {
        let tx = self.get_tx(resp, &TransactionType::Client).await?;
        let remote_uri = tx.get_remote_uri().await?;
        let local_uri = self
            .transport
            .local_uri(
                remote_uri.transport.clone(),
                remote_uri.is_provider.unwrap_or(false),
            )
            .await?;

        let mut ack = Message::default();
        ack.method = Some(Method::ACK);
        ack.remote_uri = Some(remote_uri);
        let contact = resp.contact.as_ref().ok_or(MessageError::InvalidMessage)?;
        ack.request_uri = Some(contact.uri.clone());

        let record_route = match dialog_msg.map(|m| m.record_route.clone()) {
            Some(addresses) => addresses,
            None => resp.record_route.clone(),
        };
        ack.route = record_route.iter().rev().cloned().collect();

        ack.via.push(Via {
            transport: local_uri.transport.clone(),
            host: local_uri.host.clone(),
            port: local_uri.port.clone(),
            branch: format!("z9hG4bK{}", nebula_utils::rand_string(20)),
            ..Default::default()
        });
        ack.contact = Some(Address {
            uri: local_uri.clone(),
            ..Default::default()
        });
        ack.from = resp.from.clone();
        ack.to = resp.to.clone();
        ack.callid = resp.callid.clone();
        ack.cseq = Cseq {
            method: Method::ACK,
            seq: resp.cseq.seq,
        };
        ack.max_forwards = Some(70);

        TM.transport.send(&ack).await?;

        Ok(())
    }

    pub async fn answer(
        &self,
        msg: &Message,
        body: String,
        caller_id: Option<CallerId>,
        cdr_uuid: Option<String>,
    ) -> Result<(), Error> {
        let mut resp = Message::default();

        resp.code = Some(200);
        resp.status = Some("OK".to_string());
        resp.via = msg.via.clone();
        resp.to = msg.to.clone();
        resp.from = msg.from.clone();
        resp.callid = msg.callid.clone();
        resp.cseq = msg.cseq.clone();
        resp.allow = Some(
            "SUBSCRIBE, NOTIFY, INVITE, ACK, CANCEL, BYE, REFER, INFO, OPTIONS, MESSAGE"
                .to_string(),
        );
        if !body.is_empty() {
            resp.content_type = Some("application/sdp".to_string());
        }
        resp.remote_party_id = caller_id.map(|caller_id| {
            let mut params = IndexMap::new();
            params.insert("party".to_string(), Some("calling".to_string()));
            params.insert("privacy".to_string(), Some("off".to_string()));
            params.insert("screen".to_string(), Some("no".to_string()));

            Address {
                uri: Uri {
                    user: Some(caller_id.user.clone()),
                    host: resp.from.uri.host.clone(),
                    ..Default::default()
                },
                display_name: caller_id.display_name.clone(),
                params,
                ..Default::default()
            }
        });
        resp.body = Some(body);

        if let Some(cdr_uuid) = cdr_uuid {
            let tx = self.get_tx(msg, &TransactionType::Server).await?;
            let remote_uri = tx.get_remote_uri().await?;
            if remote_uri.transport == TransportType::Wss
                || remote_uri.transport == TransportType::Ws
            {
                resp.generic_headers.push(GenericHeader {
                    name: "X-Cdr-Uuid".to_string(),
                    content: cdr_uuid,
                });
            }
        }

        self.respond(msg, &resp).await?;
        Ok(())
    }

    pub async fn reply(
        &self,
        msg: &Message,
        code: i32,
        status: String,
        sdp: Option<String>,
        redirect: Option<String>,
    ) -> Result<(), Error> {
        let mut resp = Message::default();

        resp.code = Some(code);
        resp.status = Some(status);
        resp.via = msg.via.clone();
        resp.to = msg.to.clone();
        resp.from = msg.from.clone();
        resp.callid = msg.callid.clone();
        resp.cseq = msg.cseq.clone();
        if let Some(redirect) = redirect {
            resp.contact = Some(Address {
                uri: Uri {
                    user: Some(redirect),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        if let Some(sdp) = sdp {
            resp.content_type = Some("application/sdp".to_string());
            resp.body = Some(sdp);
        }
        self.respond(msg, &resp).await?;
        Ok(())
    }

    pub async fn respond(&self, req: &Message, resp: &Message) -> Result<(), Error> {
        let tx = self.get_tx(req, &TransactionType::Server).await?;
        let mut resp = resp.clone();
        let resp_code = resp.code.unwrap_or(0);
        if req.method == Some(Method::INVITE) && resp_code > 100 && resp_code <= 200
        {
            resp.record_route = req.record_route.clone()
        }
        tx.respond(&resp).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn invite(
        &self,
        callid: String,
        reply_to: String,
        inbound_cdr: Option<String>,
        caller_id: CallerId,
        to: Location,
        internal_call: bool,
        alert_info_header: Option<String>,
        body: String,
        click_to_dial: bool,
        room: Option<String>,
    ) -> Result<Transaction, Error> {
        let mut invite = Message::default();

        let caller_id = if &to.req_uri.host == "out.simwood.com" {
            let mut caller_id = caller_id.clone();
            caller_id.user = caller_id.user[1..].to_string();
            caller_id
        } else {
            caller_id
        };

        invite.remote_uri = Some(if let Some(uri) = to.route.as_ref() {
            invite.destination = Some(to.dst_uri.clone());
            uri.clone()
        } else {
            to.dst_uri.clone()
        });

        tracing::info!(
            channel = callid,
            "Now try invite remote_uri {:?}",
            invite.remote_uri
        );

        if let Some(uri) = invite.remote_uri.as_mut() {
            uri.is_provider = to.provider;
            uri.is_registration = to.is_registration;
        }

        invite.method = Some(Method::INVITE);
        invite.callid = callid.clone();
        invite.request_uri = Some(to.req_uri.clone());
        invite.max_forwards = Some(70);

        if !to.req_uri.host.ends_with("pstnhub.microsoft.com") {
            invite.alert_info = alert_info_header;
        }

        invite.ziron_tag = to.ziron_tag;
        invite.diversion = to.diversion;
        invite.room = room;
        if to.device.is_some()
            || to.dst_uri.transport == TransportType::Ws
            || to.dst_uri.transport == TransportType::Wss
        {
            // send room_auth_token to mobile apps
            invite.room_auth_token = to.room_auth_token;
        }

        if to.global_dnd {
            invite.global_dnd = Some("yes".to_string());
        }

        invite.global_forward = to.global_forward;

        let mut from = Address {
            display_name: caller_id.display_name,
            uri: Uri {
                user: Some(caller_id.user.clone()),
                host: to.from_host.clone(),
                ..Default::default()
            },
            tag: Some(nebula_utils::rand_string(10)),
            ..Default::default()
        };
        if caller_id.anonymous {
            from.display_name = "Anonymous".to_string();
            from.uri.user = Some("anonymous".to_string());
            from.uri.host = "anonymous.invalid".to_string();
        }
        invite.from = from;
        invite.to = Address {
            uri: Uri {
                user: if to.user.is_empty() {
                    None
                } else {
                    Some(to.user.clone())
                },
                host: to.to_host.clone(),
                ..Default::default()
            },
            display_name: to.display_name.clone().unwrap_or_else(|| "".to_string()),
            ..Default::default()
        };
        invite.cseq = Cseq {
            seq: 1,
            method: Method::INVITE,
        };
        invite.allow = Some(
            "SUBSCRIBE, NOTIFY, INVITE, ACK, CANCEL, BYE, REFER, INFO, OPTIONS, MESSAGE"
                .to_string(),
        );
        if let Some(asserted_identity) = caller_id.asserted_identity {
            invite.pai = Some(Address {
                uri: Uri {
                    user: Some(asserted_identity),
                    host: to.from_host.clone(),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        if to.pai.is_some() {
            invite.pai = to.pai;
        }
        if to.req_uri.host.ends_with("flowroute.com") {
            invite.pci = Some(Address {
                uri: Uri {
                    scheme: "sip".to_string(),
                    user: Some("+19179001929".to_string()),
                    host: "talk.yay.com".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            })
        }
        if caller_id.anonymous {
            invite.privacy = Some("id".to_string());
        }
        invite.content_type = Some("application/sdp".to_string());
        invite.content_disposition = Some("session".to_string());
        if let Some(cx) = to.teams_refer_cx.as_ref() {
            invite.refer_by = Some(cx.refer_by.to_owned());
        }
        invite.body = Some(body);
        if internal_call
            && ((to.dst_uri.transport == TransportType::Wss
                || to.dst_uri.transport == TransportType::Ws)
                || (to.device.is_some() && to.device_token.is_some()))
        {
            invite.internal_call = Some("true".to_string());
        }
        if let Some(cdr) = inbound_cdr {
            if to.dst_uri.transport == TransportType::Wss
                || to.dst_uri.transport == TransportType::Ws
            {
                invite.generic_headers.push(GenericHeader {
                    name: "X-Cdr-Uuid".to_string(),
                    content: cdr,
                });
            }
        }
        if click_to_dial {
            invite.generic_headers.push(GenericHeader {
                name: "X-Click-To-Dial".to_string(),
                content: "true".to_string(),
            });
        }
        if to.req_uri.host.contains("barritel") {
            if let Some(to_number) = caller_id.to_number {
                invite.generic_headers.push(GenericHeader {
                    name: "X-Yay-Number".to_string(),
                    content: to_number,
                });
            }
        }
        invite.device = to.device;
        invite.device_token = to.device_token;
        invite.in_reply_to = Some(reply_to);

        TM.send(&invite).await
    }

    pub async fn send_api(&self, text: &str, dest: &Uri) -> Result<()> {
        self.transport.send_api(text, dest).await?;
        Ok(())
    }

    pub async fn send(&self, msg: &Message) -> Result<Transaction, Error> {
        if !msg.is_request() {
            Err(MessageError::NotRequest)?;
        }

        let mut msg = msg.clone();
        msg.resolve_remote_uri().await;
        let remote_uri = msg
            .remote_uri
            .as_ref()
            .ok_or(TransactionError::AddrInvalid)?
            .clone();
        let local_uri = self
            .transport
            .local_uri(
                remote_uri.transport.clone(),
                remote_uri.is_provider.unwrap_or(false),
            )
            .await?;

        msg.via.push(Via {
            transport: local_uri.transport.clone(),
            host: local_uri.host.clone(),
            port: local_uri.port,
            branch: format!("z9hG4bK{}", nebula_utils::rand_string(20)),
            ..Default::default()
        });
        msg.contact = Some(Address {
            uri: Uri {
                user: Some("nebula".to_string()),
                host: local_uri.host.clone(),
                port: local_uri.port,
                transport: local_uri.transport.clone(),
                ..Default::default()
            },
            ..Default::default()
        });
        let is_teams = remote_uri.host.ends_with("pstnhub.microsoft.com");
        if is_teams {
            if let Some(remote_uri) = msg.remote_uri.as_mut() {
                remote_uri.contact_host = Some(msg.from.uri.host.clone());
            }
            msg.contact = Some(Address {
                uri: Uri {
                    user: Some("nebula".to_string()),
                    host: msg.from.uri.host.clone(),
                    port: local_uri.port,
                    transport: local_uri.transport.clone(),
                    ..Default::default()
                },
                ..Default::default()
            });
        }

        let tx_type = TransactionType::Client;

        let tx = Transaction::new(
            TransactionManager::get_tx_key(&msg, &tx_type)?,
            &remote_uri,
        )
        .await?;
        tx.set_request(&msg).await?;
        if msg.method == Some(Method::INVITE) {
            REDIS
                .hset(
                    &format!("nebula:channel:{}", &msg.callid),
                    "sip_message",
                    &msg.to_string(),
                )
                .await?;
        }

        self.transport.send(&msg).await?;

        Ok(tx)
    }

    pub async fn handle_msg(&self, msg: Message) -> Result<(), Error> {
        if msg.is_request() {
            if let Some(uri) = &msg.remote_uri {
                let _ = self.send_trying(&msg, uri).await;
            }
        }

        let tx_type = match msg.is_request() {
            true => TransactionType::Server,
            false => TransactionType::Client,
        };
        if let Ok(tx) = self.get_tx(&msg, &tx_type).await {
            tx.receive(&msg).await?;
            return Ok(());
        }
        if msg.is_request() {
            if let Some(uri) = &msg.remote_uri {
                let tx_key = TransactionManager::get_tx_key(&msg, &tx_type)?;
                Transaction::new(tx_key, uri).await?;
                let _ = self.router_sender.send(msg).await;
            }
        } else if msg.cseq.method == Method::OPTIONS {
            if let Some(uri) = &msg.remote_uri {
                if uri.transport == TransportType::Udp {
                    let _ = self.router_sender.send(msg).await;
                }
            }
        }

        Ok(())
    }

    async fn send_trying(&self, msg: &Message, remote_uri: &Uri) -> Result<()> {
        if msg.method != Some(Method::INVITE) {
            return Ok(());
        }

        let mut resp = Message::default();
        resp.code = Some(100);
        resp.status = Some("Trying".to_string());
        resp.via = msg.via.clone();
        resp.to = msg.to.clone();
        resp.from = msg.from.clone();
        resp.callid = msg.callid.clone();
        resp.cseq = msg.cseq.clone();
        resp.remote_uri = Some(remote_uri.clone());
        self.transport.send(&resp).await?;

        Ok(())
    }

    async fn tx_exits(tx_key: &TransactionKey) -> bool {
        REDIS.exists(&tx_key.redis_key()).await.unwrap_or(false)
    }

    pub async fn get_tx(
        &self,
        msg: &Message,
        tx_type: &TransactionType,
    ) -> Result<Transaction, Error> {
        let key = TransactionManager::get_tx_key(msg, tx_type)?;
        if !TransactionManager::tx_exits(&key).await {
            Err(TransactionError::TransactionNotExist)?;
        }
        Ok(Transaction { key })
    }

    fn get_tx_key(
        msg: &Message,
        tx_type: &TransactionType,
    ) -> Result<TransactionKey, Error> {
        if msg.via.is_empty() {
            Err(MessageError::NoVia)?;
        }

        let branch = msg.via[0].branch.clone();
        let method = match msg.is_request() {
            true => match msg.method.as_ref().unwrap_or(&Method::INVITE) {
                &Method::ACK => Method::INVITE,
                m => m.clone(),
            },
            false => msg.cseq.method.clone(),
        };
        let callid = msg.callid.clone();
        Ok(TransactionKey::new(branch, method, callid, tx_type.clone()))
    }
}

impl Display for TransactionKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", &self.encoded)
    }
}

impl TransactionKey {
    fn new(
        branch: String,
        method: Method,
        callid: String,
        tx_type: TransactionType,
    ) -> TransactionKey {
        TransactionKey {
            encoded: sha256(&format!(
                "{}{}{}{}",
                &branch, &method, &callid, &tx_type
            )),
            branch,
            method,
            callid,
            tx_type,
        }
    }

    fn redis_key(&self) -> String {
        format!("nebula:sip:tx:{}", self)
    }
}

impl Transaction {
    pub async fn new(key: TransactionKey, uri: &Uri) -> Result<Transaction, Error> {
        let trans_type = &uri.transport;
        let state = match &key.method {
            &Method::INVITE => match key.tx_type {
                TransactionType::Server => State::Proceeding,
                TransactionType::Client => State::Calling,
            },
            _ => match key.tx_type {
                TransactionType::Server => State::Trying,
                TransactionType::Client => State::Trying,
            },
        };

        if REDIS
            .hmset_nx_ex(
                &key.redis_key(),
                600,
                vec![
                    (STATE, &state.to_string()),
                    (ADDR_HOST, &uri.host),
                    (ADDR_IP, &uri.ip),
                    (ADDR_PORT, &uri.get_port().to_string()),
                    (ADDR_TYPE, &trans_type.to_string()),
                    (
                        ADDR_IS_PROVIDER,
                        if uri.is_provider.unwrap_or(false) {
                            "yes"
                        } else {
                            "no"
                        },
                    ),
                ],
            )
            .await
            .unwrap_or(false)
        {
            let tx = Transaction { key: key.clone() };

            if key.tx_type == TransactionType::Client {
                match key.method {
                    Method::INVITE => {
                        if trans_type == &TransportType::Udp {
                            tx.register_timer(T1, Input::TimerA);
                        }
                        tx.register_timer(64 * T1, Input::TimerB);
                    }
                    _ => {
                        if trans_type == &TransportType::Udp {
                            tx.register_timer(T1, Input::TimerE);
                        }
                        tx.register_timer(64 * T1, Input::TimerF);
                    }
                }
            }

            return Ok(tx);
        }

        Err(TransactionError::TransactionExist)?
    }

    pub fn register_timer(&self, duration: Duration, input: Input) {
        let local_tx = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            let _ = fsm::spin(&local_tx, input).await;
        });
    }

    async fn receive(&self, msg: &Message) -> Result<(), Error> {
        match &self.key.tx_type {
            TransactionType::Server => {
                if let Some(method) = &msg.method {
                    let input = match method {
                        _ if method == &self.key.method => Input::Req,
                        &Method::ACK => Input::Ack,
                        _ => {
                            return Err(
                                TransactionError::TransactionNotValidMessage,
                            )?
                        }
                    };
                    let mut local_tx = self.clone();
                    fsm::spin(&mut local_tx, input).await?;
                }
                return Err(MessageError::NotRequest)?;
            }
            TransactionType::Client => {
                let code = msg.code.as_ref().ok_or(MessageError::NotResponse)?;
                let input = match code {
                    code if *code < 200 => Input::Resp1xx,
                    code if *code < 300 => Input::Resp2xx,
                    _ => Input::Resp300to699,
                };

                let _ = self.set_response(msg).await;

                let mut local_tx = self.clone();
                fsm::spin(&mut local_tx, input).await?;
            }
        };
        Ok(())
    }

    pub async fn cancel(&self, reason: Option<&HangupReason>) -> Result<(), Error> {
        let msg = self.get_request().await?;
        let mut cancel = Message::default();
        cancel.method = Some(Method::CANCEL);
        cancel.request_uri = msg.request_uri.clone();
        cancel.via = msg.via.clone();
        cancel.to = msg.to.clone();
        cancel.from = msg.from.clone();
        cancel.callid = msg.callid.clone();
        cancel.reason = reason.map(|r| match r {
            HangupReason::CompletedElsewhere => {
                "SIP;cause=200;text=\"Call completed elsewhere\"".to_string()
            }
        });
        cancel.cseq = Cseq {
            method: Method::CANCEL,
            seq: msg.cseq.seq,
        };
        let mut remote_uri = self.get_remote_uri().await?;
        if remote_uri.host.ends_with("pstnhub.microsoft.com") {
            remote_uri.contact_host = Some(msg.from.uri.host.clone());
        }
        cancel.remote_uri = Some(remote_uri);
        self.register_timer(4 * T1, Input::TimerCancel);
        self.set_request(&cancel).await?;
        TM.transport.send(&cancel).await?;

        Ok(())
    }

    async fn respond(&self, resp: &Message) -> Result<(), Error> {
        let is_teams = resp.from.uri.host.ends_with("pstnhub.microsoft.com")
            || (resp
                .record_route
                .iter()
                .any(|r| r.uri.host.ends_with("pstnhub.microsoft.com")));

        let mut resp = resp.clone();
        let remote_uri = self.get_remote_uri().await?;
        let is_provider = if is_teams {
            true
        } else {
            remote_uri.is_provider.unwrap_or(false)
        };
        let local_uri = TM
            .transport
            .local_uri(remote_uri.transport.clone(), is_provider)
            .await?;
        let code = resp.code.as_ref().ok_or(MessageError::NotResponse)?;
        if resp.cseq.method != Method::REGISTER {
            if *code == 302 && resp.contact.is_some() {
                let mut uri = local_uri.clone();
                uri.user = resp
                    .contact
                    .as_ref()
                    .and_then(|a| a.uri.user.as_ref())
                    .cloned();
                resp.contact = Some(Address {
                    uri,
                    ..Default::default()
                });
            } else {
                resp.contact = Some(Address {
                    uri: local_uri.clone(),
                    ..Default::default()
                });
            }
            if is_teams {
                resp.contact = Some(Address {
                    uri: Uri {
                        user: Some("nebula".to_string()),
                        host: resp.to.uri.host.clone(),
                        port: local_uri.port,
                        transport: local_uri.transport.clone(),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            }
            if resp.cseq.method == Method::SUBSCRIBE {
                resp.contact
                    .as_mut()
                    .map(|c| c.uri.user = Some("nebula".to_string()));
            }
        }
        self.set_last_response(&resp).await?;
        if *code == 100 {
            // We've already replied 100 trying straight after we received the INVITE
            // to be able to respond without going through the sip transcation
            // so it's only used for set_last_reponse here.
            return Ok(());
        }

        let input = match code {
            code if *code < 200 => Input::Resp1xx,
            code if *code < 300 => Input::Resp2xx,
            _ => Input::Resp300to699,
        };

        fsm::spin(self, input).await?;

        Ok(())
    }

    pub async fn reply(&self) -> Result<(), Error> {
        let remote_uri = self.get_remote_uri().await?;
        let mut resp = self.get_last_response().await?;
        resp.remote_uri = Some(remote_uri);
        let result = TM.transport.send(&resp).await;
        result?;
        Ok(())
    }

    async fn set_last_response(&self, resp: &Message) -> Result<(), Error> {
        REDIS
            .hset(&self.key.redis_key(), LAST_RESPONSE, &resp.to_string())
            .await?;
        Ok(())
    }

    pub fn new_mutex(&self) -> DistributedMutex {
        DistributedMutex::new(format!("{}:lock", self.key.redis_key()))
    }

    pub async fn get_remote_uri(&self) -> Result<Uri, Error> {
        let key = self.key.redis_key();
        let tx: HashMap<String, String> = REDIS.hgetall(&key).await?;

        let host = tx
            .get(ADDR_HOST)
            .ok_or_else(|| anyhow!("no host in tx"))?
            .clone();
        let ip = tx
            .get(ADDR_IP)
            .ok_or_else(|| anyhow!("no ip in tx"))?
            .clone();
        let port_str = tx
            .get(ADDR_PORT)
            .ok_or_else(|| anyhow!("no port in tx"))?
            .clone();
        let port = port_str.parse::<u16>()?;
        let trans_type_str = tx
            .get(ADDR_TYPE)
            .ok_or_else(|| anyhow!("no trans_type in tx"))?
            .clone();
        let trans_type = TransportType::from_str(&trans_type_str)?;
        let is_provider = tx.get(ADDR_IS_PROVIDER).map(|s| s == "yes");

        Ok(Uri {
            host,
            ip,
            port: Some(port),
            transport: trans_type,
            is_provider,
            ..Default::default()
        })
    }

    pub async fn set_request(&self, msg: &Message) -> Result<(), Error> {
        REDIS
            .hset(&self.key.redis_key(), REQUEST, &msg.to_string())
            .await?;
        Ok(())
    }

    pub async fn get_request(&self) -> Result<Message, Error> {
        let msg_string: String = REDIS.hget(&self.key.redis_key(), REQUEST).await?;
        Ok(Message::from_str(&msg_string)?)
    }

    pub async fn set_response(&self, msg: &Message) -> Result<(), Error> {
        REDIS
            .hset(&self.key.redis_key(), RESPONSE, &msg.to_string())
            .await?;
        Ok(())
    }

    pub async fn get_response(&self) -> Result<Message, Error> {
        let msg_string: String = REDIS.hget(&self.key.redis_key(), RESPONSE).await?;
        let mut msg = Message::from_str(&msg_string)?;
        msg.remote_uri = Some(self.get_remote_uri().await?);
        Ok(msg)
    }

    pub async fn incr_resend(&self) -> Result<usize> {
        REDIS
            .hincrby_key_exits(&self.key.redis_key(), "resend_count")
            .await
    }

    pub async fn resend(&self) -> Result<()> {
        let mut origin = self.get_request().await?;
        origin.remote_uri = Some(self.get_remote_uri().await?);
        tokio::spawn(async move {
            if let Err(e) = TM.transport.send(&origin).await {
                if origin.method == Some(Method::CANCEL) {
                    tracing::error!(
                        channel = origin.callid,
                        "re send cancel error: {e:?}"
                    );
                }
            }
        });
        Ok(())
    }

    pub async fn passup(&self) -> Result<(), Error> {
        let resp = self.get_response().await?;
        let _ = TM.router_sender.send(resp).await;
        Ok(())
    }

    pub async fn ack(&self) -> Result<(), Error> {
        let origin = self.get_request().await?;
        let mut ack = Message::default();
        ack.method = Some(Method::ACK);
        ack.request_uri = origin.request_uri;
        ack.via = origin.via;
        ack.from = origin.from;
        ack.to = origin.to;
        ack.callid = origin.callid;
        ack.cseq = Cseq {
            seq: origin.cseq.seq,
            method: Method::ACK,
        };
        ack.remote_uri = Some(self.get_remote_uri().await?);
        TM.transport.send(&ack).await?;
        Ok(())
    }

    async fn get_last_response(&self) -> Result<Message, Error> {
        let msg_str: String =
            REDIS.hget(&self.key.redis_key(), LAST_RESPONSE).await?;
        Ok(Message::from_str(&msg_str)?)
    }

    pub async fn get_state(&self) -> Result<State, Error> {
        let state: String = REDIS.hget(&self.key.redis_key(), STATE).await?;
        Ok(State::from_str(&state)?)
    }

    pub async fn set_state(&self, state: &State) -> Result<(), Error> {
        REDIS
            .hset(&self.key.redis_key(), STATE, &state.to_string())
            .await?;
        Ok(())
    }

    pub async fn delete(&self) -> Result<()> {
        REDIS.del(&self.key.redis_key()).await?;
        Ok(())
    }
}
