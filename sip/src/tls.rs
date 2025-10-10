use std::{
    collections::HashMap, convert::TryFrom, fs::File, net::SocketAddr, str::FromStr,
    sync::Arc,
};

use anyhow::{anyhow, Result};
use async_channel::Sender;
use nebula_redis::{DistributedMutex, REDIS};
use tokio::{
    io::{split, AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock},
};
use tokio_rustls::{
    rustls::{
        self, Certificate, ClientConfig, OwnedTrustAnchor, PrivateKey, ServerConfig,
    },
    TlsAcceptor, TlsConnector, TlsStream,
};

use crate::{
    message::{Message, Uri},
    transport::{get_send_ip, local_send, CertResolver, TransportType},
};

lazy_static::lazy_static! {
    static ref YAY_COM_CLINET_CONFIG: Arc<ClientConfig> = tls_client_config(false).unwrap();
    static ref CALLSWITCHONE_CLINET_CONFIG: Arc<ClientConfig> = tls_client_config(true).unwrap();
}

pub enum TlsConn {
    Remote(String),
    Local(Arc<Mutex<WriteHalf<TlsStream<TcpStream>>>>, SocketAddr),
}

pub struct TlsTransport {
    proxy_ip: Arc<RwLock<String>>,
    conns: Arc<
        RwLock<
            HashMap<
                String,
                (Arc<Mutex<WriteHalf<TlsStream<TcpStream>>>>, SocketAddr),
            >,
        >,
    >,
    msg_sender: Sender<Message>,
    local_ip: String,
    report_udp: Arc<RwLock<Option<Arc<tokio::net::UdpSocket>>>>,
    provider_port: Arc<RwLock<Option<u16>>>,
}

impl TlsTransport {
    pub fn new(msg_sender: Sender<Message>, local_ip: String) -> Self {
        let conns = Arc::new(RwLock::new(HashMap::new()));
        Self {
            proxy_ip: Arc::new(RwLock::new("".to_string())),
            conns,
            msg_sender,
            local_ip,
            report_udp: Arc::new(RwLock::new(None)),
            provider_port: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn local_uri(&self, is_provider: bool) -> Uri {
        Uri {
            scheme: "sip".to_string(),
            host: self.proxy_ip.read().await.clone(),
            transport: TransportType::Tls,
            port: if is_provider {
                *self.provider_port.read().await
            } else {
                Some(5061)
            },
            ..Default::default()
        }
    }

    pub async fn listen(
        &self,
        proxy_ip: &str,
        report_udp: Arc<tokio::net::UdpSocket>,
        provider_tls_port: Option<u16>,
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

        self.listen_port(5061, tls_config.clone(), report_udp.clone())
            .await?;
        if let Some(provider_tls_port) = provider_tls_port {
            *self.provider_port.write().await = Some(provider_tls_port);
            self.listen_port(
                provider_tls_port,
                tls_config.clone(),
                report_udp.clone(),
            )
            .await?;
        }

        Ok(())
    }

    async fn listen_port(
        &self,
        port: u16,
        tls_config: Arc<ServerConfig>,
        report_udp: Arc<tokio::net::UdpSocket>,
    ) -> Result<()> {
        let acceptor = TlsAcceptor::from(tls_config);

        let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
        let local_addr = listener.local_addr()?;
        let local_conns = self.conns.clone();
        let msg_sender = self.msg_sender.clone();
        let local_ip = self.local_ip.clone();
        tokio::spawn(async move {
            while let Ok((tcp_stream, peer_addr)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let msg_sender = msg_sender.clone();
                let local_conns = local_conns.clone();
                let local_ip = local_ip.clone();
                let report_udp = report_udp.clone();
                tokio::spawn(async move {
                    if let Ok(tls_stream) = acceptor.accept(tcp_stream).await {
                        let (recv, send) = split(tls_stream.into());
                        {
                            local_conns.write().await.insert(
                                peer_addr.to_string(),
                                (Arc::new(Mutex::new(send)), local_addr),
                            );
                        }
                        REDIS
                            .set(
                                &format!(
                                    "nebula:conn:{}:{}",
                                    TransportType::Tls,
                                    peer_addr
                                ),
                                &local_ip,
                            )
                            .await
                            .unwrap_or("".to_string());
                        serve_tls_stream(
                            local_conns,
                            recv,
                            &peer_addr.to_string(),
                            &peer_addr.ip().to_string(),
                            &peer_addr.ip().to_string(),
                            peer_addr.port(),
                            msg_sender,
                            local_ip,
                            report_udp,
                            local_addr,
                        )
                        .await;
                    }
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
                        format!("tls:{}:5061", self.local_ip),
                    );
                    msg.add_generic_header(
                        "X-Siptrace-Toip".to_string(),
                        format!("tls:{}:{}", dest.ip, dest.get_port()),
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

        let conn = match self.get_conn(dest).await {
            Ok(conn) => conn,
            Err(e) => {
                println!("tls get conn error {}", e);
                return Err(e);
            }
        };
        match conn {
            TlsConn::Remote(ip) => {
                local_send(&ip, dest, msg, callid).await?;
            }
            TlsConn::Local(conn, local_addr) => {
                // info!("tls msg send {}", msg);
                {
                    if let Some(socket) = self.report_udp.read().await.clone() {
                        if let Ok(mut msg) = Message::from_str(&msg) {
                            msg.add_generic_header(
                                "X-Siptrace-Fromip".to_string(),
                                format!(
                                    "tls:{}:{}",
                                    self.local_ip,
                                    local_addr.port()
                                ),
                            );
                            msg.add_generic_header(
                                "X-Siptrace-Toip".to_string(),
                                format!("tls:{}:{}", dest.ip, dest.get_port()),
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
                            let _ = socket
                                .send_to(
                                    msg.to_string().as_bytes(),
                                    "10.132.0.117:5888",
                                )
                                .await;
                        }
                    }
                }
                let mut conn = conn.lock().await;
                conn.write_all(msg.as_bytes()).await?;
                conn.flush().await?;
            }
        };
        Ok(0)
    }

    async fn get_conn(&self, dest: &Uri) -> Result<TlsConn> {
        let is_callswitch_one = dest
            .contact_host
            .as_ref()
            .map(|h| h.ends_with("talk.callswitchone.com"))
            .unwrap_or(false);

        let addr = &format!(
            "{}:{}{}",
            dest.host,
            dest.get_port(),
            if is_callswitch_one {
                ":callswitchone"
            } else {
                ""
            }
        );

        let conn = { self.conns.read().await.get(addr).cloned() };
        if let Some((conn, local_addr)) = conn {
            return Ok(TlsConn::Local(conn, local_addr));
        };

        let send_ip = get_send_ip(&TransportType::Tls, addr).await?;
        if !send_ip.is_empty() && self.local_ip != send_ip {
            return Ok(TlsConn::Remote(send_ip));
        };

        let mutex =
            DistributedMutex::new(format!("nebula:tls:{}:connect:lock", addr));
        mutex.lock().await;

        let exits = { self.conns.read().await.contains_key(addr) };
        if !exits {
            let send_ip = get_send_ip(&TransportType::Tls, addr).await?;
            if !send_ip.is_empty() && self.local_ip != send_ip {
                return Ok(TlsConn::Remote(send_ip));
            };
            
            if dest.is_registration == Some(true) {
                return Err(anyhow!("registration connection has closed"));
            }

            let client_config = if is_callswitch_one {
                CALLSWITCHONE_CLINET_CONFIG.clone()
            } else {
                YAY_COM_CLINET_CONFIG.clone()
            };

            let connector = TlsConnector::from(client_config);

            println!("tcp stream connect addr {}", addr);
            let tcp_stream = TcpStream::connect(&dest.addr()).await?;
            let local_addr = tcp_stream.local_addr()?;
            let domain = rustls::ServerName::try_from(
                addr.split(':').next().ok_or(anyhow!("can't split addr"))?,
            )
            .map_err(|_e| anyhow!("can't parse domain"))?;
            println!("tls domain is {:?}", domain);
            let tls_stream = connector.connect(domain, tcp_stream).await?;
            let (recv, send) = split(tls_stream.into());
            {
                self.conns.write().await.insert(
                    addr.to_string(),
                    (Arc::new(Mutex::new(send)), local_addr),
                );
            }

            REDIS
                .set(
                    &format!("nebula:conn:{}:{}", TransportType::Tls, addr),
                    &self.local_ip,
                )
                .await
                .unwrap_or("".to_string());
            let msg_sender = self.msg_sender.clone();
            let local_conns = self.conns.clone();

            let dest = dest.clone();
            let addr = addr.to_string();
            let local_ip = self.local_ip.clone();
            let report_udp = self.report_udp.read().await.clone().unwrap();
            tokio::spawn(async move {
                serve_tls_stream(
                    local_conns,
                    recv,
                    &addr,
                    &dest.host,
                    &dest.ip,
                    dest.get_port(),
                    msg_sender,
                    local_ip,
                    report_udp,
                    local_addr,
                )
                .await;
            });
        }
        let (conn, local_addr) = self
            .conns
            .read()
            .await
            .get(addr)
            .cloned()
            .ok_or(anyhow!("can't find tls connection in the pool"))?;
        Ok(TlsConn::Local(conn, local_addr))
    }

    pub async fn has_addr(&self, addr: &str) -> bool {
        self.conns.read().await.contains_key(addr)
    }
}

async fn serve_tls_stream(
    conns: Arc<
        RwLock<
            HashMap<
                String,
                (Arc<Mutex<WriteHalf<TlsStream<TcpStream>>>>, SocketAddr),
            >,
        >,
    >,
    recv: ReadHalf<TlsStream<TcpStream>>,
    peer_addr: &str,
    peer_host: &str,
    peer_ip: &str,
    peer_port: u16,
    msg_sender: Sender<Message>,
    local_ip: String,
    report_udp: Arc<tokio::net::UdpSocket>,
    local_addr: SocketAddr,
) {
    let mut reader = BufReader::new(recv);
    loop {
        match Message::async_parse(&mut reader).await {
            Ok(mut msg) => {
                let _ = REDIS
                    .set(
                        &format!("nebula:conn:{}:{}", TransportType::Tls, peer_addr),
                        &local_ip,
                    )
                    .await;
                msg.remote_uri = Some(Uri {
                    scheme: "sip".to_string(),
                    host: peer_host.to_string(),
                    ip: peer_ip.to_string(),
                    port: Some(peer_port),
                    transport: TransportType::Tls,
                    ..Default::default()
                });
                // info!("tls msg receive {}", msg);
                {
                    let mut msg = msg.clone();
                    msg.add_generic_header(
                        "X-Siptrace-Fromip".to_string(),
                        format!("tls:{}:{}", peer_ip, peer_port),
                    );
                    msg.add_generic_header(
                        "X-Siptrace-Toip".to_string(),
                        format!("tls:{local_ip}:{}", local_addr.port()),
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
                    let _ = report_udp
                        .send_to(msg.to_string().as_bytes(), "10.132.0.117:5888")
                        .await;
                }
                let _ = msg_sender.send(msg).await;
            }
            Err(e) => {
                println!(
                    "tls connection {} parse sip message error {}, quit",
                    peer_addr, e
                );
                let _ = REDIS
                    .del(&format!(
                        "nebula:conn:{}:{}",
                        TransportType::Tls,
                        peer_addr
                    ))
                    .await;
                conns.write().await.remove(peer_addr);
                return;
            }
        }
    }
}

fn tls_client_config(is_callswitch_one: bool) -> Result<Arc<ClientConfig>> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add_server_trust_anchors(
        webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
            OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }),
    );

    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(File::open(
        if is_callswitch_one {
            "/root/talk.callswitchone.com.crt"
        } else {
            "/root/talk.yay.com.crt"
        },
    )?))
    .map(|mut certs| certs.drain(..).map(Certificate).collect())?;
    let mut key: Vec<PrivateKey> = rustls_pemfile::rsa_private_keys(
        &mut std::io::BufReader::new(File::open(if is_callswitch_one {
            "/root/talk.callswitchone.com.key"
        } else {
            "/root/talk.yay.com.key"
        })?),
    )
    .map(|mut keys| keys.drain(..).map(PrivateKey).collect())?;

    Ok(Arc::new(
        ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_store)
            .with_single_cert(certs, key.remove(0))?,
    ))
}
