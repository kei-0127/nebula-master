use super::server::MEDIA_SERVICE;
use crate::socket::RawSocket;
use crate::stream::{
    MediaStreamReceiver, StreamReceiverMessage, StreamSenderPoolMessage,
};
use anyhow::Result;
use nebula_redis::DistributedMutex;
use nebula_redis::REDIS;
use nebula_utils::uuid_v5;
use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rsa::Rsa,
    x509::{X509NameBuilder, X509},
};
use std::time::Instant;
use std::{collections::HashMap, net::Ipv4Addr, string::ToString};
use strum_macros;
use strum_macros::EnumString;
use thiserror::Error;
use tokio::sync::mpsc::{
    channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
};
use tokio::{self, time::Duration};

const THRESHOLD: f64 = 0.6;
const ACTUAL_THRESHOLD: f64 = THRESHOLD * 0x7fff as f64;
const ALPHA: f64 = 7.48338;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("no available port")]
    NoPort,

    #[error("no sample rate")]
    NoSampleRate,
}

pub struct Certificate {
    pub fingerprint: String,
    pub x509cert: X509,
    pub private_key: PKey<Private>,
}

pub struct NewSession {}

impl NewSession {
    pub async fn process_packets(
        port: u16,
        peer_ip: Ipv4Addr,
        peer_port: u16,
        sender: UnboundedSender<StreamReceiverMessage>,
        receiver: UnboundedReceiver<StreamReceiverMessage>,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<()> {
        let left = port % 2;
        let rtcp_only = left > 0;
        let port = port - left;
        let channel: String = REDIS.get(&format!("nebula:session:{}", port)).await?;
        let id = uuid_v5(&format!("{}:{}", channel, port));

        let local_sender = sender.clone();
        tokio::spawn(async move {
            loop {
                if local_sender.is_closed() {
                    return;
                }
                let _ = REDIS.expire(&format!("nebula:session:{}", port), 30).await;
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
        });

        MediaStreamReceiver::process_packets(
            id,
            channel,
            peer_ip,
            peer_port,
            rtcp_only,
            sender,
            receiver,
            stream_sender_pool,
        )
        .await
    }
}

#[derive(Clone, Debug)]
pub enum SessionPoolMessage {
    Packet {
        port: u16,
        peer_ip: Ipv4Addr,
        peer_port: u16,
        packet: Vec<u8>,
        now: Instant,
    },
    Stop(String),
}

pub struct NewSessionPool {
    sessions: HashMap<String, UnboundedSender<StreamReceiverMessage>>,
    stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
}

impl NewSessionPool {
    pub async fn new(
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Self {
        Self {
            sessions: HashMap::new(),
            stream_sender_pool,
        }
    }

    async fn listen_port(
        port: u16,
        mut rx: Receiver<()>,
        session_pool_sender: UnboundedSender<SessionPoolMessage>,
    ) {
        let udp_socket = tokio::net::UdpSocket::bind(format!(
            "{}:{port}",
            MEDIA_SERVICE.config.media_ip
        ))
        .await
        .unwrap();

        let mut buf = [0; 5000];
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    rx.close();
                    return;
                }
                result = udp_socket.recv_from(&mut buf) => {
                    match result {
                        Ok((n, addr)) => {
                            let peer_ip = match addr.ip() {
                                std::net::IpAddr::V4(ip) => ip,
                                std::net::IpAddr::V6(_) => continue,
                            };
                            let peer_port = addr.port();
                            let _ = session_pool_sender.send(SessionPoolMessage::Packet {
                                port,
                                peer_ip,
                                peer_port,
                                packet: buf[..n].to_vec(),
                                now: Instant::now(),
                            });
                        }
                        Err(_) => {
                            rx.close();
                            return;
                        }
                    }
                }
            }
        }
    }

    pub async fn run(&mut self) {
        let (session_pool_sender, mut session_pool_receiver) = unbounded_channel();

        {
            let dst_ip_octets = MEDIA_SERVICE.config.media_ip.octets();
            let local_session_pool_sender = session_pool_sender.clone();
            let mut listeners: HashMap<u16, Sender<()>> = HashMap::new();
            tokio::spawn(async move {
                let mut buf = [0; 5000];
                let raw_socket = RawSocket::new(true).unwrap();
                loop {
                    match raw_socket.recv(&mut buf).await {
                        Ok(n) => {
                            if !(buf[16] == dst_ip_octets[0]
                                && buf[17] == dst_ip_octets[1]
                                && buf[18] == dst_ip_octets[2]
                                && buf[19] == dst_ip_octets[3])
                            {
                                continue;
                            }
                            let port = (buf[22] as u16) << 8 | (buf[23] as u16);
                            match port {
                                port if port >= 10000 && port <= 40000 => {
                                    let needs_to_create = if let Some(listener) =
                                        listeners.get(&port)
                                    {
                                        listener.is_closed()
                                    } else {
                                        true
                                    };

                                    if needs_to_create {
                                        let (tx, rx) = channel(1);
                                        listeners.insert(port, tx);
                                        let session_pool_sender =
                                            local_session_pool_sender.clone();
                                        tokio::spawn(async move {
                                            Self::listen_port(
                                                port,
                                                rx,
                                                session_pool_sender,
                                            )
                                            .await;
                                        });
                                    }
                                }
                                _ => (),
                            }
                        }
                        Err(_e) => {}
                    }
                }
            });
        }

        loop {
            if let Some(msg) = session_pool_receiver.recv().await {
                match msg {
                    SessionPoolMessage::Stop(session_id) => {
                        self.sessions.remove(&session_id);
                    }
                    SessionPoolMessage::Packet {
                        port,
                        peer_ip,
                        peer_port,
                        packet,
                        now,
                    } => {
                        let session_id =
                            format!("{}-{}:{}", port, peer_ip, peer_port);
                        if let Some(sender) = self.sessions.get(&session_id) {
                            if sender
                                .send(StreamReceiverMessage::Packet(
                                    packet.clone(),
                                    now,
                                ))
                                .is_ok()
                            {
                                continue;
                            }
                        }

                        let (sender, receiver) = unbounded_channel();
                        let _ =
                            sender.send(StreamReceiverMessage::Packet(packet, now));
                        self.sessions.insert(session_id.clone(), sender.clone());
                        let stream_sender_pool = self.stream_sender_pool.clone();
                        let session_pool_sender = session_pool_sender.clone();
                        tokio::spawn(async move {
                            match NewSession::process_packets(
                                port,
                                peer_ip,
                                peer_port,
                                sender.clone(),
                                receiver,
                                stream_sender_pool,
                            )
                            .await
                            {
                                Ok(_) => {}
                                Err(_) => {}
                            }
                            let _ = session_pool_sender
                                .send(SessionPoolMessage::Stop(session_id));
                        });
                    }
                }
            } else {
                return;
            }
        }
    }
}

pub async fn get_cert_fingerprint() -> Result<String> {
    let fingerprint = REDIS
        .hget("nebula:dtls:cert", "fingerprint")
        .await
        .unwrap_or("".to_string());
    if fingerprint == "" {
        let cert = get_cert().await?;
        Ok(cert.fingerprint)
    } else {
        Ok(fingerprint)
    }
}

async fn get_cert_from_cache() -> Option<Certificate> {
    let cert_map: HashMap<String, String> =
        REDIS.hgetall("nebula:dtls:cert").await.ok()?;
    let crt = cert_map.get("crt")?;
    let key = cert_map.get("key")?;
    let fingerprint = cert_map.get("fingerprint")?.to_string();
    let x509cert = X509::from_pem(crt.as_bytes()).ok()?;
    let private_key = PKey::private_key_from_pem(key.as_bytes()).ok()?;

    Some(Certificate {
        fingerprint,
        x509cert,
        private_key,
    })
}

pub async fn get_cert() -> Result<Certificate> {
    let mutex = DistributedMutex::new("nebula:dtls:cert:lock".to_string());
    mutex.lock().await;

    if let Some(cert) = get_cert_from_cache().await {
        return Ok(cert);
    }

    let cert = gen_cert()?;
    let crt = &cert.x509cert.to_pem()?;
    let key = &cert.private_key.private_key_to_pem_pkcs8()?;

    REDIS
        .hmset(
            "nebula:dtls:cert",
            vec![
                ("crt", std::str::from_utf8(crt)?),
                ("key", std::str::from_utf8(key)?),
                ("fingerprint", &cert.fingerprint),
            ],
        )
        .await?;
    REDIS.expire("nebula:dtls:cert", 60 * 60 * 24 * 360).await?;

    Ok(cert)
}

fn gen_cert() -> Result<Certificate> {
    let rsa = Rsa::generate(2048)?;
    let pkey = PKey::from_rsa(rsa)?;

    let mut x509_name = X509NameBuilder::new()?;
    x509_name.append_entry_by_nid(Nid::COMMONNAME, "nebula")?;
    let x509_name = x509_name.build();

    let mut big = BigNum::new()?;
    big.rand(128, MsbOption::MAYBE_ZERO, false)?;
    let serial_number = big.to_asn1_integer()?;

    let mut x509 = X509::builder()?;
    x509.set_subject_name(&x509_name)?;
    x509.set_issuer_name(&x509_name)?;
    x509.set_pubkey(&pkey)?;
    x509.set_serial_number(&serial_number)?;
    x509.set_version(0)?;
    x509.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    x509.set_not_after(Asn1Time::days_from_now(365)?.as_ref())?;
    x509.sign(&pkey, MessageDigest::sha256())?;
    let x509 = x509.build();

    let fingerprint = x509
        .digest(MessageDigest::sha256())?
        .iter()
        .map(|n| format!("{:02X?}", n))
        .collect::<Vec<String>>()
        .join(":");

    Ok(Certificate {
        fingerprint,
        x509cert: x509,
        private_key: pkey,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionLocality {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, strum_macros::Display, EnumString)]
pub enum SessionDirection {
    #[strum(serialize = "send")]
    Send,
    #[strum(serialize = "receive")]
    Receive,
}

pub fn mix(x: i16, y: i16) -> i16 {
    let buf = x as f64 + y as f64;
    (if buf > ACTUAL_THRESHOLD {
        mix_normalization(buf / 0x7fff as f64) * 0x7fff as f64
    } else if buf < -ACTUAL_THRESHOLD {
        -mix_normalization(-buf / 0x7fff as f64) * 0x7fff as f64
    } else {
        buf
    }) as i16
}

fn mix_normalization(x: f64) -> f64 {
    let a = (1.0 - THRESHOLD) / (1.0 + ALPHA).ln();
    let b = ALPHA / (2.0 - THRESHOLD);
    THRESHOLD + a * (1.0 + b * (x - THRESHOLD)).ln()
}
