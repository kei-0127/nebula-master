use crate::tcp::TcpTransport;
use crate::tls::TlsTransport;

use super::message::{Message, Uri};
use super::udp::UdpTransport;
use super::wss::WssTransport;
use anyhow::{anyhow, Error, Result};
use async_channel::Sender;
use jsonrpc_lite::JsonRpc;
use lazy_static::lazy_static;
use nebula_redis::REDIS;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use strum_macros;
use strum_macros::EnumString;
use thiserror::Error;
use tokio::time;
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::{any_supported_type, CertifiedKey};
use tokio_rustls::rustls::{Certificate, PrivateKey};
use tracing::warn;

lazy_static! {
    pub static ref YAY_COM_CERT: Arc<CertifiedKey> =
        Arc::new(yay_com_certified_key());
    pub static ref TALK_YAY_COM_CERT: Arc<CertifiedKey> =
        Arc::new(talk_yay_com_certified_key());
    pub static ref TALK_VOIPCP_COM_CERT: Arc<CertifiedKey> =
        Arc::new(talk_voipcp_com_certified_key());
    pub static ref TALK_CALLSWITCHONE_COM_CERT: Arc<CertifiedKey> =
        Arc::new(talk_callswitchone_com_certified_key());
}

#[derive(
    strum_macros::Display,
    EnumString,
    Debug,
    Eq,
    PartialEq,
    Hash,
    Clone,
    Deserialize,
    Serialize,
)]
#[strum(ascii_case_insensitive)]
pub enum TransportType {
    #[strum(serialize = "udp")]
    Udp,
    #[strum(serialize = "tcp")]
    Tcp,
    #[strum(serialize = "tls")]
    Tls,
    #[strum(serialize = "ws")]
    Ws,
    #[strum(serialize = "wss")]
    Wss,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("no transport")]
    NoTransport,
    #[error("no available port")]
    NoPort,
}

impl Default for TransportType {
    fn default() -> Self {
        TransportType::Udp
    }
}

pub struct TransportManager {
    pub transports: HashMap<TransportType, Transport>,
    msg_sender: Sender<Message>,
    api_sender: Sender<(JsonRpc, String)>,
}

impl TransportManager {
    pub fn new(
        msg_sender: Sender<Message>,
        api_sender: Sender<(JsonRpc, String)>,
        local_ip: String,
    ) -> TransportManager {
        let mut transports = HashMap::new();
        transports.insert(TransportType::Udp, Transport::Udp(UdpTransport::new()));
        transports.insert(
            TransportType::Tcp,
            Transport::Tcp(TcpTransport::new(msg_sender.clone(), local_ip.clone())),
        );
        transports.insert(
            TransportType::Wss,
            Transport::Wss(WssTransport::new(local_ip.clone())),
        );
        transports.insert(
            TransportType::Tls,
            Transport::Tls(TlsTransport::new(msg_sender.clone(), local_ip.clone())),
        );

        tokio::spawn(async move {
            let key = format!("nebula:sip_server:{}:live", local_ip);
            let mut ticker = time::interval(Duration::from_millis(2000));
            loop {
                ticker.tick().await;
                let _ = REDIS.setex(&key, 5, "yes").await;
            }
        });

        TransportManager {
            msg_sender,
            transports,
            api_sender,
        }
    }

    pub async fn send(&self, msg: &Message) -> Result<usize> {
        // println!("transport manager send msg {}", msg);
        let dest = msg.dest()?;
        let trans_type = &dest.transport;
        let trans = self
            .transports
            .get(trans_type)
            .ok_or(anyhow!("transport not supported"))?;
        trans.send(msg.to_string(), &dest, msg.callid.clone()).await
    }

    pub async fn send_api(&self, text: &str, dest: &Uri) -> Result<()> {
        let trans = self
            .transports
            .get(&TransportType::Wss)
            .ok_or(anyhow!("transport not supported"))?;
        if let Transport::Wss(wss) = trans {
            wss.send(text.to_string(), dest, "".to_string()).await?;
        }

        Ok(())
    }

    pub async fn listen(
        &self,
        proxy_ip: &str,
        report_udp: Arc<tokio::net::UdpSocket>,
        udp_listen_proxy_ip: bool,
        provider_udp_port: Option<u16>,
        provider_tls_port: Option<u16>,
    ) -> Result<()> {
        for (_, trans) in self.transports.iter() {
            trans
                .listen(
                    proxy_ip,
                    self.msg_sender.clone(),
                    self.api_sender.clone(),
                    report_udp.clone(),
                    udp_listen_proxy_ip,
                    provider_udp_port,
                    provider_tls_port,
                )
                .await?;
        }
        Ok(())
    }

    pub async fn local_uri(
        &self,
        trans: TransportType,
        is_provider: bool,
    ) -> Result<Uri> {
        let trans = self
            .transports
            .get(&trans)
            .ok_or(anyhow!("transport type not supported"))?;
        Ok(trans.local_uri(is_provider).await)
    }

    pub async fn has_addr(&self, trans: &TransportType, addr: &str) -> Result<bool> {
        let trans = self
            .transports
            .get(trans)
            .ok_or_else(|| anyhow!("transport type not supported"))?;
        Ok(trans.has_addr(addr).await)
    }
}

pub enum Transport {
    Udp(UdpTransport),
    Tcp(TcpTransport),
    Wss(WssTransport),
    Tls(TlsTransport),
}

// impl fmt::Debug for Transport {
//     fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
//         f.debug_struct("Transport")
//             .field("trans_type", &self.trans_type)
//             .finish()
//     }
// }

impl Transport {
    pub async fn send(
        &self,
        msg: String,
        dest: &Uri,
        callid: String,
    ) -> Result<usize, Error> {
        match self {
            Transport::Udp(t) => t.send(msg, dest).await,
            Transport::Tcp(t) => t.send(msg, dest, callid).await,
            Transport::Wss(t) => t.send(msg, dest, callid).await,
            Transport::Tls(t) => t.send(msg, dest, callid).await,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn listen(
        &self,
        proxy_ip: &str,
        msg_sender: Sender<Message>,
        api_sender: Sender<(JsonRpc, String)>,
        report_udp: Arc<tokio::net::UdpSocket>,
        udp_listen_proxy_ip: bool,
        provider_udp_port: Option<u16>,
        provider_tls_port: Option<u16>,
    ) -> Result<(), Error> {
        match self {
            Transport::Udp(t) => {
                t.listen(
                    proxy_ip,
                    msg_sender,
                    udp_listen_proxy_ip,
                    provider_udp_port,
                )
                .await
            }
            Transport::Tcp(t) => t.listen(proxy_ip, report_udp).await,
            Transport::Wss(t) => {
                t.listen(proxy_ip, msg_sender, api_sender, report_udp).await
            }
            Transport::Tls(t) => {
                t.listen(proxy_ip, report_udp, provider_tls_port).await
            }
        }
    }

    pub async fn local_uri(&self, is_provider: bool) -> Uri {
        match self {
            Transport::Udp(t) => t.local_uri(is_provider).await,
            Transport::Tcp(t) => t.local_uri().await,
            Transport::Tls(t) => t.local_uri(is_provider).await,
            Transport::Wss(t) => t.local_uri().await,
        }
    }

    pub async fn has_addr(&self, addr: &str) -> bool {
        match self {
            Transport::Udp(_) => true,
            Transport::Tcp(t) => t.has_addr(addr).await,
            Transport::Tls(t) => t.has_addr(addr).await,
            Transport::Wss(t) => t.has_addr(addr).await,
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct LocalSendRequest {
    pub uri: Uri,
    pub msg: String,
}

pub async fn get_send_ip(transport: &TransportType, addr: &str) -> Result<String> {
    let send_ip = REDIS
        .get(&format!("nebula:conn:{}:{}", transport, addr))
        .await
        .unwrap_or("".to_string());
    if !send_ip.is_empty() {
        let ip_live = REDIS
            .get(&format!("nebula:sip_server:{}:live", &send_ip))
            .await
            .unwrap_or("".to_string())
            == "yes";
        if !ip_live {
            REDIS
                .del(&format!("nebula:conn:{}:{}", transport, addr))
                .await?;
            warn!(
                "get send ip is not live {transport} addr:{addr} send_ip {send_ip}"
            );
            return Ok("".to_string());
        }
    }
    Ok(send_ip)
}

lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::new();
}

pub async fn local_send(
    local_ip: &str,
    uri: &Uri,
    msg: String,
    callid: String,
) -> Result<()> {
    REDIS
        .xadd_maxlen(
            &format!("nebula:proxy:stream:{local_ip}"),
            "message",
            &serde_json::to_string(&serde_json::json!({
                "id": callid,
                "method": "local_send",
                "params": serde_json::to_value(LocalSendRequest {
                    uri: uri.clone(),
                    msg,
                })?,
            }))?,
            1000,
        )
        .await?;
    Ok(())
}

pub struct CertResolver {}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        match client_hello.server_name() {
            Some(s) => match s {
                _ if s.ends_with("talk.yay.com") => Some(TALK_YAY_COM_CERT.clone()),
                _ if s.ends_with("voipcp.com") => Some(TALK_VOIPCP_COM_CERT.clone()),
                _ if s.ends_with("callswitchone.com") => {
                    Some(TALK_CALLSWITCHONE_COM_CERT.clone())
                }
                _ => Some(YAY_COM_CERT.clone()),
            },
            None => Some(YAY_COM_CERT.clone()),
        }
    }
}

fn yay_com_certified_key() -> CertifiedKey {
    let cert = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open("/root/yay.com.crt").unwrap(),
    ))
    .map(|mut cert| cert.drain(..).map(Certificate).collect())
    .unwrap();
    let mut key: Vec<PrivateKey> =
        rustls_pemfile::rsa_private_keys(&mut std::io::BufReader::new(
            std::fs::File::open("/root/yay.com.key").unwrap(),
        ))
        .map(|mut keys| keys.drain(..).map(PrivateKey).collect())
        .unwrap();
    CertifiedKey {
        cert,
        key: any_supported_type(&key.remove(0)).unwrap(),
        ocsp: None,
        sct_list: None,
    }
}

fn talk_yay_com_certified_key() -> CertifiedKey {
    let cert = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open("/root/talk.yay.com.crt").unwrap(),
    ))
    .map(|mut cert| cert.drain(..).map(Certificate).collect())
    .unwrap();
    let mut key: Vec<PrivateKey> =
        rustls_pemfile::rsa_private_keys(&mut std::io::BufReader::new(
            std::fs::File::open("/root/talk.yay.com.key").unwrap(),
        ))
        .map(|mut keys| keys.drain(..).map(PrivateKey).collect())
        .unwrap();
    CertifiedKey {
        cert,
        key: any_supported_type(&key.remove(0)).unwrap(),
        ocsp: None,
        sct_list: None,
    }
}

fn talk_voipcp_com_certified_key() -> CertifiedKey {
    let cert = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open("/root/talk.voipcp.com.crt").unwrap(),
    ))
    .map(|mut cert| cert.drain(..).map(Certificate).collect())
    .unwrap();
    let mut key: Vec<PrivateKey> =
        rustls_pemfile::rsa_private_keys(&mut std::io::BufReader::new(
            std::fs::File::open("/root/talk.voipcp.com.key").unwrap(),
        ))
        .map(|mut keys| keys.drain(..).map(PrivateKey).collect())
        .unwrap();
    CertifiedKey {
        cert,
        key: any_supported_type(&key.remove(0)).unwrap(),
        ocsp: None,
        sct_list: None,
    }
}

fn talk_callswitchone_com_certified_key() -> CertifiedKey {
    let cert = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open("/root/talk.callswitchone.com.crt").unwrap(),
    ))
    .map(|mut cert| cert.drain(..).map(Certificate).collect())
    .unwrap();
    let mut key: Vec<PrivateKey> =
        rustls_pemfile::rsa_private_keys(&mut std::io::BufReader::new(
            std::fs::File::open("/root/talk.callswitchone.com.key").unwrap(),
        ))
        .map(|mut keys| keys.drain(..).map(PrivateKey).collect())
        .unwrap();
    CertifiedKey {
        cert,
        key: any_supported_type(&key.remove(0)).unwrap(),
        ocsp: None,
        sct_list: None,
    }
}
