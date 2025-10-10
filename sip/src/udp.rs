use super::message::Message;
use super::message::Uri;
use super::transport::TransportType;
use anyhow::{anyhow, Error, Result};
use async_channel::Sender;
use std::str;
use std::str::FromStr;
use std::sync::Arc;
use tokio;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::error;

pub struct UdpTransport {
    sockets: Arc<RwLock<Vec<Arc<UdpSocket>>>>,
    proxy_ip: Arc<RwLock<String>>,
    provider_port: Arc<RwLock<Option<u16>>>,
}

impl UdpTransport {
    pub fn new() -> Self {
        UdpTransport {
            sockets: Arc::new(RwLock::new(Vec::new())),
            proxy_ip: Arc::new(RwLock::new("".to_string())),
            provider_port: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn local_uri(&self, is_provider: bool) -> Uri {
        Uri {
            scheme: "sip".to_string(),
            host: self.proxy_ip.read().await.clone(),
            transport: TransportType::Udp,
            port: if is_provider {
                *self.provider_port.read().await
            } else {
                Some(5060)
            },
            ..Default::default()
        }
    }

    pub async fn listen(
        &self,
        proxy_ip: &str,
        msg_sender: Sender<Message>,
        udp_listen_proxy_ip: bool,
        provider_udp_port: Option<u16>,
    ) -> Result<()> {
        {
            *self.proxy_ip.write().await = proxy_ip.to_string();
        }

        {
            let addr = format!(
                "{}:5060",
                if udp_listen_proxy_ip {
                    proxy_ip
                } else {
                    "0.0.0.0"
                }
            );

            let socket = UdpSocket::bind(&addr).await?;
            let recv = Arc::new(socket);
            let send = recv.clone();

            let msg_sender = msg_sender.clone();
            tokio::spawn(async move {
                UdpTransport::run(recv, msg_sender, false).await;
                error!("udp stopped");
            });

            self.sockets.write().await.push(send);
        }

        {
            if let Some(port) = provider_udp_port {
                let addr = format!(
                    "{}:{port}",
                    if udp_listen_proxy_ip {
                        proxy_ip
                    } else {
                        "0.0.0.0"
                    }
                );

                let socket = UdpSocket::bind(&addr).await?;
                let recv = Arc::new(socket);
                let send = recv.clone();

                tokio::spawn(async move {
                    UdpTransport::run(recv, msg_sender, true).await;
                    error!("udp stopped");
                });

                self.sockets.write().await.push(send);
                *self.provider_port.write().await = Some(port);
            }
        }

        Ok(())
    }

    async fn run(
        socket: Arc<UdpSocket>,
        msg_sender: Sender<Message>,
        is_provider: bool,
    ) {
        let mut buf = [0; 4096];
        let msg_sender = Arc::new(msg_sender);
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((i, addr)) => {
                    if let Some(text_str) = str::from_utf8_mut(&mut buf[..i]).ok() {
                        let local_msg_sender = msg_sender.clone();
                        let text = text_str.to_string();
                        tokio::spawn(async move {
                            let msg = nebula_task::spawn_task(move || {
                                Message::from_str(text.as_ref())
                            })
                            .await;
                            if let Ok(Ok(mut msg)) = msg {
                                msg.remote_uri = Some(Uri {
                                    scheme: "sip".to_string(),
                                    host: addr.ip().to_string(),
                                    ip: addr.ip().to_string(),
                                    port: Some(addr.port()),
                                    transport: TransportType::Udp,
                                    is_provider: Some(is_provider),
                                    ..Default::default()
                                });
                                let _ = local_msg_sender.send(msg).await;
                            }
                        });
                    }
                }
                Err(e) => error!("udp socket receive error {e}"),
            }
        }
    }

    pub async fn send(&self, msg: String, dest: &Uri) -> Result<usize, Error> {
        let addr = dest.addr();
        let udp = {
            let index = if dest.is_provider.unwrap_or(false)
                && self.provider_port.read().await.is_some()
            {
                1
            } else {
                0
            };
            self.sockets
                .read()
                .await
                .get(index)
                .ok_or(anyhow!("no udp socket"))?
                .clone()
        };
        let result = udp.send_to(msg.as_bytes(), addr).await;
        if let Err(e) = result.as_ref() {
            error!("udp socket send error {e}");
        }
        Ok(result?)
    }
}
