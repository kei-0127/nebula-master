use std::str::FromStr;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use crate::transport::TransportType;
use crate::{
    message::{Message, Uri},
    transport::{get_send_ip, local_send},
};
use anyhow::{anyhow, Result};
use async_channel::Sender;
use nebula_redis::{DistributedMutex, REDIS};
use tokio::sync::RwLock;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    sync::Mutex,
};

pub enum TcpConn {
    Remote(String),
    Local(Arc<Mutex<OwnedWriteHalf>>),
}

pub struct TcpTransport {
    proxy_ip: Arc<RwLock<String>>,
    conns: Arc<RwLock<HashMap<String, Arc<Mutex<OwnedWriteHalf>>>>>,
    msg_sender: Sender<Message>,
    local_ip: String,
    report_udp: Arc<RwLock<Option<Arc<tokio::net::UdpSocket>>>>,
}

impl TcpTransport {
    pub fn new(msg_sender: Sender<Message>, local_ip: String) -> Self {
        let conns = Arc::new(RwLock::new(HashMap::new()));
        Self {
            proxy_ip: Arc::new(RwLock::new("".to_string())),
            conns,
            msg_sender,
            local_ip,
            report_udp: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn listen(
        &self,
        proxy_ip: &str,
        report_udp: Arc<tokio::net::UdpSocket>,
    ) -> Result<()> {
        {
            *self.proxy_ip.write().await = proxy_ip.to_string();
        }

        {
            *self.report_udp.write().await = Some(report_udp.clone());
        }

        let listener = TcpListener::bind("0.0.0.0:5060").await?;
        let local_conns = self.conns.clone();
        let msg_sender = self.msg_sender.clone();
        let local_ip = self.local_ip.clone();
        tokio::spawn(async move {
            while let Ok((tcp_stream, peer_addr)) = listener.accept().await {
                let (recv, send) = tcp_stream.into_split();
                {
                    local_conns
                        .write()
                        .await
                        .insert(peer_addr.to_string(), Arc::new(Mutex::new(send)));
                }
                REDIS
                    .set(
                        &format!("nebula:conn:{}:{}", TransportType::Tcp, peer_addr),
                        &local_ip,
                    )
                    .await
                    .unwrap_or("".to_string());
                let msg_sender = msg_sender.clone();
                let conns = local_conns.clone();
                let local_ip = local_ip.clone();
                tokio::spawn(async move {
                    serve_tcp_stream(conns, recv, peer_addr, msg_sender, local_ip)
                        .await;
                });
            }
        });
        Ok(())
    }

    async fn get_conn(&self, dest: &Uri) -> Result<TcpConn> {
        let addr = &dest.addr();
        let conn = { self.conns.read().await.get(addr).cloned() };
        if let Some(conn) = conn {
            return Ok(TcpConn::Local(conn));
        };

        // do a check on send_ip before lock
        let send_ip = get_send_ip(&TransportType::Tcp, addr).await?;
        if !send_ip.is_empty() && self.local_ip != send_ip {
            return Ok(TcpConn::Remote(send_ip));
        };

        let mutex =
            DistributedMutex::new(format!("nebula:tcp:{}:connect:lock", addr));
        mutex.lock().await;

        let exits = { self.conns.read().await.contains_key(addr) };
        if !exits {
            let send_ip = get_send_ip(&TransportType::Tcp, addr).await?;
            if !send_ip.is_empty() && self.local_ip != send_ip {
                return Ok(TcpConn::Remote(send_ip));
            };

            if dest.is_registration == Some(true) {
                return Err(anyhow!("registration connection has closed"));
            }

            let tcp_stream = TcpStream::connect(&addr).await?;
            let (recv, send) = tcp_stream.into_split();
            {
                self.conns
                    .write()
                    .await
                    .insert(addr.to_string(), Arc::new(Mutex::new(send)));
            }

            REDIS
                .set(
                    &format!("nebula:conn:{}:{}", TransportType::Tcp, addr),
                    &self.local_ip,
                )
                .await
                .unwrap_or("".to_string());
            let peer_addr = SocketAddr::from_str(addr)?;
            let msg_sender = self.msg_sender.clone();
            let local_conns = self.conns.clone();
            let local_ip = self.local_ip.clone();
            tokio::spawn(async move {
                serve_tcp_stream(local_conns, recv, peer_addr, msg_sender, local_ip)
                    .await;
            });
        }
        Ok(TcpConn::Local(
            self.conns
                .read()
                .await
                .get(addr)
                .cloned()
                .ok_or(anyhow!("can't find tcp connection in the pool"))?,
        ))
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
                        format!("tcp:{}:5060", self.local_ip),
                    );
                    msg.add_generic_header(
                        "X-Siptrace-Toip".to_string(),
                        format!("tcp:{}:{}", dest.ip, dest.get_port()),
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

        let conn = self.get_conn(dest).await?;
        match conn {
            TcpConn::Remote(ip) => {
                local_send(&ip, dest, msg, callid).await?;
            }
            TcpConn::Local(conn) => {
                let mut conn = conn.lock().await;
                conn.write_all(msg.as_bytes()).await?;
                conn.flush().await?;
            }
        };
        Ok(0)
    }

    pub async fn local_uri(&self) -> Uri {
        Uri {
            scheme: "sip".to_string(),
            host: self.proxy_ip.read().await.clone(),
            transport: TransportType::Tcp,
            port: Some(5060),
            ..Default::default()
        }
    }

    pub async fn has_addr(&self, addr: &str) -> bool {
        self.conns.read().await.contains_key(addr)
    }
}

async fn serve_tcp_stream(
    conns: Arc<RwLock<HashMap<String, Arc<Mutex<OwnedWriteHalf>>>>>,
    recv: OwnedReadHalf,
    peer_addr: SocketAddr,
    msg_sender: Sender<Message>,
    local_ip: String,
) {
    let mut reader = BufReader::new(recv);
    loop {
        match Message::async_parse(&mut reader).await {
            Ok(mut msg) => {
                let _ = REDIS
                    .set(
                        &format!("nebula:conn:{}:{}", TransportType::Tcp, peer_addr),
                        &local_ip,
                    )
                    .await;
                msg.remote_uri = Some(Uri {
                    scheme: "sip".to_string(),
                    host: peer_addr.ip().to_string(),
                    ip: peer_addr.ip().to_string(),
                    port: Some(peer_addr.port()),
                    transport: TransportType::Tcp,
                    ..Default::default()
                });
                let _ = msg_sender.send(msg).await;
            }
            Err(e) => {
                println!(
                    "tcp connection {} parse sip message error {}, quit",
                    peer_addr, e
                );
                let _ = REDIS
                    .del(&format!(
                        "nebula:conn:{}:{}",
                        TransportType::Tcp,
                        peer_addr
                    ))
                    .await;
                conns.write().await.remove(&peer_addr.to_string());
                return;
            }
        }
    }
}
