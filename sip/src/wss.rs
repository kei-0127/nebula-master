use crate::transport::CertResolver;
use crate::transport::{get_send_ip, local_send};

use super::message::Message;
use super::message::Uri;
use super::transport::TransportType;
use anyhow::{anyhow, Result};
use async_channel::Sender;
use futures_util::stream::SplitSink;
use futures_util::stream::StreamExt;
use futures_util::SinkExt;
use jsonrpc_lite::JsonRpc;
use nebula_redis::REDIS;
use serde_json::json;
use std::str::FromStr;
use std::sync::Arc;
use std::{collections::HashMap, net::SocketAddr};
use tokio;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio_rustls::rustls::{self};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::handshake::server::{Request, Response},
};

type WssConn = (
    Arc<
        Mutex<
            SplitSink<WebSocketStream<TlsStream<TcpStream>>, tungstenite::Message>,
        >,
    >,
    SocketAddr,
);

pub struct WssTransport {
    proxy_ip: Arc<RwLock<String>>,
    local_ip: String,
    conns: Arc<RwLock<HashMap<String, WssConn>>>,
    report_udp: Arc<RwLock<Option<Arc<tokio::net::UdpSocket>>>>,
}

impl WssTransport {
    pub fn new(local_ip: String) -> WssTransport {
        let conns = Arc::new(RwLock::new(HashMap::new()));
        WssTransport {
            conns,
            proxy_ip: Arc::new(RwLock::new("".to_string())),
            local_ip,
            report_udp: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn local_uri(&self) -> Uri {
        Uri {
            scheme: "sip".to_string(),
            host: self.proxy_ip.read().await.clone(),
            transport: TransportType::Wss,
            port: Some(443),
            ..Default::default()
        }
    }

    pub async fn listen(
        &self,
        proxy_ip: &str,
        msg_sender: Sender<Message>,
        api_sender: Sender<(JsonRpc, String)>,
        report_udp: Arc<tokio::net::UdpSocket>,
    ) -> Result<()> {
        {
            *self.proxy_ip.write().await = proxy_ip.to_string();
        }

        {
            *self.report_udp.write().await = Some(report_udp.clone());
        }

        let tls_config = Arc::new(
            rustls::ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(CertResolver {})),
        );
        let acceptor = TlsAcceptor::from(tls_config);
        let listener = TcpListener::bind("0.0.0.0:443").await?;
        let local_addr = listener.local_addr()?;

        let local_conns = self.conns.clone();
        let local_ip = self.local_ip.clone();
        tokio::spawn(async move {
            loop {
                let (tcp_stream, peer_addr) = listener
                    .accept()
                    .await
                    .expect("can't accept new connection");
                let msg_sender = msg_sender.clone();
                let api_sender = api_sender.clone();
                let conns = local_conns.clone();
                let local_ip = local_ip.clone();
                let acceptor = acceptor.clone();
                let report_udp = report_udp.clone();
                tokio::spawn(async move {
                    let tls_stream = acceptor.accept(tcp_stream).await?;
                    println!("got tls stream");
                    let mut subprotocol = "".to_string();
                    let callback = |req: &Request, mut response: Response| {
                        if let Some(protocol) =
                            req.headers().get("Sec-WebSocket-Protocol")
                        {
                            subprotocol =
                                protocol.to_str().unwrap_or("").to_string();
                            response
                                .headers_mut()
                                .insert("Sec-WebSocket-Protocol", protocol.clone());
                        }
                        Ok(response)
                    };
                    let wss_stream = accept_hdr_async(tls_stream, callback).await?;
                    let (send_stream, mut recv) = wss_stream.split();
                    {
                        conns.write().await.insert(
                            peer_addr.to_string(),
                            (Arc::new(Mutex::new(send_stream)), local_addr),
                        );
                    }
                    REDIS
                        .set(
                            &format!(
                                "nebula:conn:{}:{}",
                                TransportType::Wss,
                                peer_addr
                            ),
                            &local_ip,
                        )
                        .await
                        .unwrap_or_else(|_| "".to_string());
                    println!("got tls stream {peer_addr}");
                    while let Some(Ok(msg)) = recv.next().await {
                        let _ = Self::handle_wss_msg(
                            msg,
                            &peer_addr,
                            &local_ip,
                            &subprotocol,
                            &api_sender,
                            &local_addr,
                            &report_udp,
                            &msg_sender,
                        )
                        .await;
                    }

                    println!("wss conn {} disconnected", peer_addr);
                    let _ = api_sender
                        .send((
                            JsonRpc::notification_with_params(
                                "leave_room",
                                json!({}),
                            ),
                            peer_addr.to_string(),
                        ))
                        .await;
                    let _ = REDIS
                        .del(&format!(
                            "nebula:conn:{}:{}",
                            TransportType::Wss,
                            peer_addr
                        ))
                        .await;
                    let _ =
                        REDIS
                            .expire(
                                &format!(
                                    "nebula:room_rpc_addr_switchboard:{peer_addr}",
                                ),
                                10,
                            )
                            .await;
                    {
                        conns.write().await.remove(&peer_addr.to_string());
                    }

                    Ok(()) as Result<()>
                });
            }
        });
        Ok(())
    }

    pub async fn send(
        &self,
        msg: String,
        dest: &Uri,
        callid: String,
    ) -> Result<usize> {
        {
            if let Some(socket) = self.report_udp.read().await.clone() {
                if let Ok(mut msg) = Message::from_str(&msg) {
                    msg.add_generic_header(
                        "X-Siptrace-Fromip".to_string(),
                        format!("wss:{}:443", self.local_ip),
                    );
                    msg.add_generic_header(
                        "X-Siptrace-Toip".to_string(),
                        format!("wss:{}:{}", dest.ip, dest.get_port()),
                    );
                    msg.add_generic_header(
                        "X-Siptrace-Time".to_string(),
                        format!(
                            "{} 607029",
                            std::time::SystemTime::now()
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                        ),
                    );
                    let _ = socket
                        .send_to(msg.to_string().as_bytes(), "10.132.0.117:5888")
                        .await;
                }
            }
        }

        let addr = dest.addr();
        let (conn, local_addr) =
            {
                let exits = { self.conns.read().await.contains_key(&addr) };
                if !exits {
                    let send_ip = get_send_ip(&TransportType::Wss, &addr).await?;
                    if !send_ip.is_empty() && self.local_ip != send_ip {
                        local_send(&send_ip, dest, msg, callid).await?;
                        return Ok(1);
                    };
                    return Ok(1);
                }
                self.conns.read().await.get(&addr).cloned().ok_or_else(|| {
                    anyhow!("can't find wss connection in the pool")
                })?
            };
        // info!("wss msg send {}", msg);
        conn.lock()
            .await
            .send(tungstenite::Message::Text(msg.to_string()))
            .await?;
        {
            if let Some(socket) = self.report_udp.read().await.clone() {
                if let Ok(mut msg) = Message::from_str(&msg) {
                    msg.add_generic_header(
                        "X-Siptrace-Fromip".to_string(),
                        format!("wss:{}:{}", self.local_ip, local_addr.port()),
                    );
                    msg.add_generic_header(
                        "X-Siptrace-Toip".to_string(),
                        format!("wss:{}:{}", dest.ip, dest.get_port()),
                    );
                    msg.add_generic_header(
                        "X-Siptrace-Time".to_string(),
                        format!(
                            "{} 607029",
                            std::time::SystemTime::now()
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                        ),
                    );
                    let _ = socket
                        .send_to(msg.to_string().as_bytes(), "10.132.0.117:5888")
                        .await;
                }
            }
        }
        Ok(1)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_wss_msg(
        msg: tungstenite::protocol::Message,
        peer_addr: &SocketAddr,
        local_ip: &str,
        subprotocol: &str,
        api_sender: &Sender<(JsonRpc, String)>,
        local_addr: &SocketAddr,
        report_udp: &tokio::net::UdpSocket,
        msg_sender: &Sender<Message>,
    ) -> Result<()> {
        let _ = REDIS
            .set(
                &format!("nebula:conn:{}:{}", TransportType::Wss, peer_addr),
                local_ip,
            )
            .await;
        let text = msg.to_text()?;
        match subprotocol {
            "api" => {
                println!("received api {}", text);
                if let Ok(rpc) = JsonRpc::parse(text) {
                    let _ = api_sender.send((rpc, peer_addr.to_string())).await;
                }
            }
            _ => match Message::from_str(text) {
                Ok(mut m) => {
                    m.remote_uri = Some(Uri {
                        scheme: "sip".to_string(),
                        host: peer_addr.ip().to_string(),
                        ip: peer_addr.ip().to_string(),
                        port: Some(peer_addr.port()),
                        transport: TransportType::Wss,
                        ..Default::default()
                    });
                    // info!("wss msg receive {}", m);
                    {
                        let mut msg = m.clone();
                        msg.add_generic_header(
                            "X-Siptrace-Fromip".to_string(),
                            format!("wss:{}:{}", peer_addr.ip(), peer_addr.port()),
                        );
                        msg.add_generic_header(
                            "X-Siptrace-Toip".to_string(),
                            format!("wss:{local_ip}:{}", local_addr.port()),
                        );
                        msg.add_generic_header(
                            "X-Siptrace-Time".to_string(),
                            format!(
                                "{} 607029",
                                std::time::SystemTime::now()
                                    .duration_since(
                                        std::time::SystemTime::UNIX_EPOCH
                                    )
                                    .unwrap()
                                    .as_secs()
                            ),
                        );
                        let _ = report_udp
                            .send_to(msg.to_string().as_bytes(), "10.132.0.117:5888")
                            .await;
                    }
                    let _ = msg_sender.send(m).await;
                }
                Err(e) => println!("parse sip message got error {}\n{}", e, text),
            },
        }

        Ok(())
    }

    pub async fn has_addr(&self, addr: &str) -> bool {
        self.conns.read().await.contains_key(addr)
    }
}
