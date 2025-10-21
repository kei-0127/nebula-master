// WebRTC Peer Connection
// WebRTC peer connection for real-time media connections
// Handles ICE connectivity, DTLS security and media stream management

use std::{
    collections::HashMap,
    convert::TryInto,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use bytecodec::{DecodeExt, EncodeExt};
use byteorder::{BigEndian, ByteOrder};
use nebula_redis::REDIS;
use nebula_utils::uuid_v5;
use sdp::sdp::{
    Crypto, CryptoKind, Dtls, Ice, MediaStreamTrack, MediaType, SessionDescKind,
    SessionDescription, PEER_CONNECTION_PORT,
};
use strum_macros::EnumString;
use stun_codec::{
    rfc5389::{attributes, methods, Attribute},
    MessageClass, MessageDecoder, MessageEncoder,
};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    Mutex,
};
use tokio::{net::UdpSocket, sync::mpsc::Sender};
use uuid::Uuid;
use webrtc_util::Unmarshal;

use crate::{
    dtls::DtlsTransport,
    packet::RtpPacket,
    rtcp::RtcpPacket,
    server::MEDIA_SERVICE,
    session::get_cert,
    stream::{
        set_crypto, MediaStreamReceiver, StreamReceiverMessage,
        StreamSenderPoolMessage,
    },
};

// STUN protocol constants
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442; // STUN magic cookie for message validation

// WebRTC peer connection events
// These events are emitted during the peer connection lifecycle to notify about important state changes and connectioin events

#[derive(strum_macros::Display, EnumString, PartialEq, Clone, Debug)]
pub enum PeerConnectionEvent {
    #[strum(serialize = "dtls_done")]
    DtlsDone,   //DTLS handshake completed successfully
    #[strum(serialize = "stop")]
    Stop,
}

#[derive(Clone, Debug)]
pub enum PeerConnectionPoolMessage {
    Packet(SocketAddr, Vec<u8>, Instant),
    Stop(SocketAddr),
}

#[derive(Clone, Debug)]
pub enum PeerConnectionMessage {
    Packet(Vec<u8>, Instant),
    Event(PeerConnectionEvent),
}

#[derive(Clone, Debug)]
pub enum PeerConnectionTrackMessage {
    Packet(Vec<u8>, Instant),
}

pub struct PeerConnectionPool {
    connections: HashMap<SocketAddr, UnboundedSender<PeerConnectionMessage>>,
    udp_socket: Arc<UdpSocket>,
    stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
}

impl PeerConnectionPool {

    // Bind the shared UDP socket and prepare a pool for per-address peer connections
    pub async fn new(
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<PeerConnectionPool> {
        let udp_socket = Arc::new(
            UdpSocket::bind(format!(
                "{}:{}",
                MEDIA_SERVICE.config.media_ip, PEER_CONNECTION_PORT
            ))
            .await?,
        );
        Ok(PeerConnectionPool {
            connections: HashMap::new(),
            udp_socket,
            stream_sender_pool,
        })
    }

    // Main loop: reads UDP, routes packets to an existing connection or spawns a new one
    pub async fn run(&mut self) {
        let (pool_sender, mut pool_receiver) = unbounded_channel();

        let local_pool_sender = pool_sender.clone();
        let udp_socket = self.udp_socket.clone();

        // UDP ingress -> queue as PoolMessage::Packet(addr, bytes, now)
        tokio::spawn(async move {
            let mut buf = vec![0; 9000];
            loop {
                if let Ok((n, addr)) = udp_socket.recv_from(&mut buf).await {
                    let _ =
                        local_pool_sender.send(PeerConnectionPoolMessage::Packet(
                            addr,
                            buf[..n].to_vec(),
                            Instant::now(),
                        ));
                }
            }
        });

        // Per-addr worker lifecycle:create on first packet; remove on Stop
        loop {
            if let Some(msg) = pool_receiver.recv().await {
                match msg {
                    PeerConnectionPoolMessage::Packet(addr, packet, now) => {
                        if let Some(conn) = self.connections.get(&addr) {
                            if conn
                                .send(PeerConnectionMessage::Packet(
                                    packet.clone(),
                                    now,
                                ))
                                .is_ok()
                            {
                                continue;
                            }
                        }

                        let (sender, receiver) = unbounded_channel();
                        let _ = sender.send(PeerConnectionMessage::Packet(
                            packet.clone(),
                            now,
                        ));
                        self.connections.insert(addr, sender.clone());
                        let udp_socket = self.udp_socket.clone();
                        let stream_sender_pool = self.stream_sender_pool.clone();
                        let pool_sender = pool_sender.clone();
                        tokio::spawn(async move {
                            let _ = PeerConnection::process_packets(
                                packet,
                                udp_socket,
                                addr,
                                sender,
                                receiver,
                                stream_sender_pool,
                            )
                            .await;
                            let _ = pool_sender
                                .send(PeerConnectionPoolMessage::Stop(addr));
                        });
                    }
                    PeerConnectionPoolMessage::Stop(addr) => {
                        self.connections.remove(&addr);
                        tokio::spawn(async move {
                            let _ = REDIS
                                .del(&format!("nebula:peer_connection:{}", addr))
                                .await;
                        });
                    }
                }
            }
        }
    }

    pub fn udp_socket(&self) -> Arc<UdpSocket> {
        self.udp_socket.clone()
    }
}

#[derive(Clone, Debug)]
struct WebRTC {
    channel: String,
    tracks: HashMap<Uuid, MediaStreamTrack>,
    local_ice: Ice,
    remote_ice: Ice,
    ice_id: String,
    local_dtls: Dtls,
    remote_dtls: Dtls,
    dtls_started: bool,
    dtls_done: bool,
    dtls_last_packet: Arc<Mutex<Vec<u8>>>,
}

impl WebRTC {

    // Load channel metadata for this peer from Redis using the peer address
    async fn from_peer_addr(addr: SocketAddr) -> Result<Self> {
        let channel: String = REDIS
            .get(&format!("nebula:peer_connection:{}", addr))
            .await?;
        Self::from_channel(channel).await
    }

    // Build WebRTC  state (ICE/DTLS/tracks) from the channel's local/remote SDPs in Redis
    async fn from_channel(channel: String) -> Result<Self> {
        let remote_sdp: String = REDIS
            .hget(
                &format!("nebula:channel:{}", &channel),
                &SessionDescKind::Remote.to_string(),
            )
            .await?;
        let remote_sdp = SessionDescription::from_str(&remote_sdp)?;
        let remote_ice = remote_sdp
            .ice()
            .ok_or(anyhow!("remote sdp doens't have ice"))?;
        let remote_dtls = remote_sdp
            .dtls()
            .ok_or(anyhow!("remote sdp doesn't have dtls"))?;

        let local_sdp: String = REDIS
            .hget(
                &format!("nebula:channel:{}", &channel),
                &SessionDescKind::Local.to_string(),
            )
            .await?;
        let local_sdp = SessionDescription::from_str(&local_sdp)?;
        let local_ice = local_sdp
            .ice()
            .ok_or(anyhow!("local sdp doens't have ice"))?;
        let local_dtls = local_sdp
            .dtls()
            .ok_or(anyhow!("local sdp doesn't have dtls"))?;

        let ice_id =
            format!("{}:{}:{}", &channel, &local_ice.ufrag, &remote_ice.ufrag);
        let dtls_done = REDIS
            .hget(&format!("nebula:ice_id:{}", ice_id), "dtls_done")
            .await
            .unwrap_or("".to_string())
            == "yes";
        let mut streams = HashMap::new();
        let local_tracks = local_sdp.media_stream_tracks();
        let remote_tracks = remote_sdp.media_stream_tracks();
        for (mid, remote_track) in remote_tracks {
            if let Some(_local_track) = local_tracks.get(&mid) {
                let id = format!("{}:{}", channel, mid);
                let id = uuid_v5(&id);
                streams.insert(id, remote_track);
            }
        }

        Ok(WebRTC {
            channel,
            tracks: streams,
            local_ice,
            remote_ice,
            ice_id,
            local_dtls,
            remote_dtls,
            dtls_started: false,
            dtls_done,
            dtls_last_packet: Arc::new(Mutex::new(Vec::new())),
        })
    }
}

pub struct PeerConnection {
    udp: Arc<UdpSocket>,
    peer_addr: SocketAddr,
    dtls_sender: Option<Sender<Vec<u8>>>,
    rtp_receiver: Option<UnboundedReceiver<PeerConnectionTrackMessage>>,
    rtp_sender: UnboundedSender<PeerConnectionTrackMessage>,
    webrtc: WebRTC,
    stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
}

impl PeerConnection {
    // Initialize a per-peer connection
    // If no cached channel exists, treat the first packet as STUN and resolve the channel via USERNAME
    async fn new(
        first_packet: Vec<u8>,
        udp: Arc<UdpSocket>,
        peer_addr: SocketAddr,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<Self> {
        let webrtc = if let Ok(webrtc) = WebRTC::from_peer_addr(peer_addr).await {
            webrtc
        } else {
            let stun_msg = MessageDecoder::<Attribute>::new()
                .decode_from_bytes(&first_packet)?
                .map_err(|e| anyhow!("{:?}", e))?;
            if stun_msg.class() != MessageClass::Request {
                return Err(anyhow!("not stun request"))?;
            }
            let username: &attributes::Username = stun_msg
                .get_attribute()
                .ok_or(anyhow!("no username is stun request"))?;
            let channel: String = REDIS
                .get(&format!("nebula:ice:{}", username.name()))
                .await?;

            REDIS
                .set(&format!("nebula:peer_connection:{}", peer_addr), &channel)
                .await?;
            WebRTC::from_channel(channel).await?
        };

        let (sender, receiver) = unbounded_channel();
        Ok(Self {
            udp,
            peer_addr,
            dtls_sender: None,
            rtp_receiver: Some(receiver),
            rtp_sender: sender,
            webrtc,
            stream_sender_pool,
        })
    }

    // Per-peer task: handles DTLS, STUN and forwards RTP/RTCP to stream receivers
    async fn process_packets(
        first_packet: Vec<u8>,
        udp_socket: Arc<UdpSocket>,
        peer_addr: SocketAddr,
        sender: UnboundedSender<PeerConnectionMessage>,
        mut receiver: UnboundedReceiver<PeerConnectionMessage>,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<()> {
        let mut conn =
            Self::new(first_packet, udp_socket, peer_addr, stream_sender_pool)
                .await?;

        // when the peer connection is created, set the send_ip for video tracks to be local ip,
        // because we want the receiving and sending of video packets to be on the same machine,
        // so that we can simply pass the trasport layer cc rtcp packet from reciving end to sending
        // end via channel without going through the network.
        // We need the tranport cc on the sending end to caculuate the available sending rate
        // for allocating the video sending layers
        for (uuid, track) in &conn.webrtc.tracks {
            if track.media_type == MediaType::Video {
                let _ = REDIS
                    .hsetex(
                        &format!("nebula:media_stream:{}", uuid),
                        "ip",
                        &MEDIA_SERVICE.local_ip,
                    )
                    .await;
            }
        }

        let ice_id = conn.webrtc.ice_id.clone();
        tokio::spawn(async move {
            let _ = Self::process_event(&ice_id, sender).await;
        });

        let rtp_sender = conn.rtp_sender.clone();
        // Drive connection until idle or closed; packets go through receive_packet, events via process_event
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                     return Ok(())
                }
                 _ = rtp_sender.closed() => {
                     return Ok(())
                 }
                 result = receiver.recv() => {
                     let msg = result.ok_or(anyhow!("receiver closed"))?;
                     match msg {
                         PeerConnectionMessage::Packet(packet, now) => {
                           if let Err(e) = conn.receive_packet(packet, now).await {
                           }
                         }
                         PeerConnectionMessage::Event(event) => match event {
                             PeerConnectionEvent::DtlsDone => {
                                 conn.webrtc.dtls_done = true;
                             }
                             PeerConnectionEvent::Stop => {
                                 tracing::info!(
                                     channel=conn.webrtc.channel,
                                     "peer connection received stop, webrtc dtls done {}, peer addr {}", conn.webrtc.dtls_done, conn.peer_addr);
                                 return Ok(());
                             }
                         },
                     }
                 }
            }
        }
    }

    // Classify and route a single UDP packet (STUN, DTLS or RTP/RTCP)
    async fn receive_packet(&mut self, packet: Vec<u8>, now: Instant) -> Result<()> {
        if is_stun_packet(&packet) {
            let stun_msg = MessageDecoder::<Attribute>::new()
                .decode_from_bytes(&packet)?
                .map_err(|e| anyhow!("{:?}", e))?;
            self.handle_stun(&stun_msg).await?;
            return Ok(());
        }

        if is_dtls_packet(&packet) {
            if let Some(sender) = self.dtls_sender.as_ref() {
                let _ = sender.send(packet).await;
            } else {
                let packet = { self.webrtc.dtls_last_packet.lock().await.clone() };
                if packet.len() > 0 {
                    self.udp.send_to(&packet, &self.peer_addr).await?;
                }
            }
            return Ok(());
        }

        if self.webrtc.dtls_done && self.rtp_receiver.is_some() {
            if let Err(e) = self.process_rtp().await {}
        }

        self.rtp_sender
            .send(PeerConnectionTrackMessage::Packet(packet, now))?;

        Ok(())
    }

    // handle STUN binding requests/responses and kick off DTLS according to setup
    async fn handle_stun(
        &mut self,
        msg: &stun_codec::Message<Attribute>,
    ) -> Result<()> {
        if msg.class() == MessageClass::Request {
            // when receive stun, set last_rtp_packet to keep the channel alive
            let _ = REDIS
                .hsetex(
                    &format!("nebula:channel:{}", &self.webrtc.channel),
                    "last_rtp_packet",
                    &std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)?
                        .as_secs()
                        .to_string(),
                )
                .await;
            let mut resp: stun_codec::Message<Attribute> = stun_codec::Message::new(
                MessageClass::SuccessResponse,
                methods::BINDING,
                msg.transaction_id(),
            );
            resp.add_attribute(Attribute::XorMappedAddress(
                attributes::XorMappedAddress::new(self.peer_addr),
            ));
            resp.add_attribute(Attribute::MessageIntegrity(
                attributes::MessageIntegrity::new_short_term_credential(
                    &resp,
                    &self.webrtc.local_ice.pwd,
                )?,
            ));
            resp.add_attribute(Attribute::Fingerprint(
                attributes::Fingerprint::new(&resp)?,
            ));
            let mut encoder = MessageEncoder::new();
            let resp = encoder.encode_into_bytes(resp)?;
            self.udp.send_to(&resp, self.peer_addr).await?;

            if self.webrtc.local_dtls.setup == "actpass" {
                self.start_dtls().await?;
            }

            let mut req: stun_codec::Message<Attribute> = stun_codec::Message::new(
                MessageClass::Request,
                methods::BINDING,
                stun_codec::TransactionId::new(
                    nebula_utils::rand_bytes(12).as_slice().try_into()?,
                ),
            );
            req.add_attribute(Attribute::Username(attributes::Username::new(
                format!(
                    "{}:{}",
                    &self.webrtc.remote_ice.ufrag, &self.webrtc.local_ice.ufrag,
                ),
            )?));
            req.add_attribute(Attribute::MessageIntegrity(
                attributes::MessageIntegrity::new_short_term_credential(
                    &req,
                    &self.webrtc.remote_ice.pwd,
                )?,
            ));
            req.add_attribute(Attribute::Fingerprint(attributes::Fingerprint::new(
                &req,
            )?));
            let mut encoder = MessageEncoder::new();
            let req = encoder.encode_into_bytes(req)?;
            self.udp.send_to(&req, self.peer_addr).await?;
        } else if msg.class() == MessageClass::SuccessResponse {
            if self.webrtc.local_dtls.setup == "active" {
                self.start_dtls().await?;
            }
        }
        Ok(())
    }

    // Create a DTLS transport, shuttle DTLS records to/from UDP and derive SRTP keys
    async fn start_dtls(&mut self) -> Result<()> {
        if self.webrtc.dtls_done || self.webrtc.dtls_started {
            return Ok(());
        }

        if self.webrtc.local_dtls.setup == "active" {
            let success = REDIS
                .hsetnx(
                    &format!("nebula:ice_id:{}", self.webrtc.ice_id),
                    "dtls_ip",
                    &MEDIA_SERVICE.local_ip.clone(),
                )
                .await?;
            if !success {
                return Ok(());
            }
        }

        let cert = get_cert().await?;
        let dtls_is_server = self.webrtc.local_dtls.setup == "actpass";
        let (dtls_transport, dtls_sender, mut dtls_receiver) =
            nebula_task::spawn_task(move || {
                DtlsTransport::new(cert, dtls_is_server)
            })
            .await??;
        self.dtls_sender = Some(dtls_sender);
        self.webrtc.dtls_started = true;

        let local_udp = self.udp.clone();
        let peer_addr = self.peer_addr.clone();
        let dtls_last_packet = self.webrtc.dtls_last_packet.clone();
        tokio::spawn(async move {
            loop {
                if let Some(data) = dtls_receiver.recv().await {
                    let _ = local_udp.send_to(&data, peer_addr).await;
                    *dtls_last_packet.lock().await = data;
                } else {
                    return;
                }
            }
        });

        let streams = self.webrtc.tracks.clone();
        let ice_id = self.webrtc.ice_id.clone();
        let peer_addr = self.peer_addr.clone();
        tokio::spawn(async move {
            let is_server = dtls_transport.is_server;
            if let Ok(buf) = dtls_transport.get_srtp_key().await {
                let key1 = &buf[0..16];
                let key2 = &buf[16..32];
                let salt1 = &buf[32..46];
                let salt2 = &buf[46..60];
                let (remote, local) = if is_server {
                    ((key1, salt1), (key2, salt2))
                } else {
                    ((key2, salt2), (key1, salt1))
                };
                let local_crypto = Crypto {
                    tag: "1".to_string(),
                    master_key: local.0.to_vec(),
                    salt: local.1.to_vec(),
                    suite: "AES_CM_128_HMAC_SHA1_80".to_string(),
                    tag_len: 10,
                };
                let remote_crypto = Crypto {
                    tag: "1".to_string(),
                    master_key: remote.0.to_vec(),
                    salt: remote.1.to_vec(),
                    suite: "AES_CM_128_HMAC_SHA1_80".to_string(),
                    tag_len: 10,
                };
                for (uuid, _) in streams {
                    let _ = REDIS
                        .hsetex(
                            &format!("nebula:media_stream:{}", &uuid.to_string()),
                            "peer_addr",
                            &peer_addr.to_string(),
                        )
                        .await;
                    let _ = set_crypto(
                        CryptoKind::Remote,
                        &uuid.to_string(),
                        &remote_crypto,
                    )
                    .await;
                    let _ = set_crypto(
                        CryptoKind::Local,
                        &uuid.to_string(),
                        &local_crypto,
                    )
                    .await;
                    let _ = MEDIA_SERVICE
                        .media_rpc_client
                        .dtls_done(uuid.to_string())
                        .await;
                }
                let _ = REDIS
                    .hset(&format!("nebula:ice_id:{}", ice_id), "dtls_done", "yes")
                    .await;
                let _ =
                    Self::new_event(&ice_id, PeerConnectionEvent::DtlsDone, "done")
                        .await;
            }
        });

        Ok(())
    }

    // Consume Redis stream of peer-connection events and forward to the connection task
    async fn process_event(
        ice_id: &str,
        sender: UnboundedSender<PeerConnectionMessage>,
    ) -> Result<()> {
        let stream = format!("nebula:peer_connection:{}:stream", ice_id);
        let mut key_id = "0".to_string();
        loop {
            tokio::select! {
                 _ = sender.closed() => {
                      return Ok(())
                 }
                 result = REDIS.xread_next_entry_timeout(&stream, &key_id, 1000) => {
                      if let Ok((new_key_id, event, _value)) = result {
                          key_id = new_key_id;
                          if let Ok(event) = PeerConnectionEvent::from_str(&event) {
                              let _ = sender.send(PeerConnectionMessage::Event(event));
                          }
                      }
                 }
            }
        }
    }

    pub async fn new_event(
        ice_id: &str,
        event: PeerConnectionEvent,
        value: &str,
    ) -> Result<()> {
        let stream = format!("nebula:peer_connection:{}:stream", ice_id);
        REDIS.xadd(&stream, &event.to_string(), value).await?;
        if event == PeerConnectionEvent::Stop {
            REDIS.expire(&stream, 30).await?;
            REDIS.del(&format!("nebula:ice_id:{}", ice_id)).await?;
        }
        Ok(())
    }

    // Bridge incoming RTP/RTCP to a per-track `MediaStreamReceiver` (spawned on first packet)
    async fn process_rtp(&mut self) -> Result<()> {
        let mut rtp_receiver =
            self.rtp_receiver.take().ok_or(anyhow!("no rtp receiver"))?;
        let peer_ip = match self.peer_addr.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => Err(anyhow!("no support for ipv6"))?,
        };
        let peer_port = self.peer_addr.port();

        let stream_sender_pool = self.stream_sender_pool.clone();
        let webrtc = self.webrtc.clone();
        let mut streams: HashMap<u32, Uuid> = HashMap::new();
        let mut track_senders: HashMap<
            Uuid,
            UnboundedSender<StreamReceiverMessage>,
        > = HashMap::new();
        tokio::spawn(async move {
            loop {
                if let Some(msg) = rtp_receiver.recv().await {
                    match msg {
                        PeerConnectionTrackMessage::Packet(packet, now) => {
                            let is_rtcp = RtpPacket::is_rtcp(&packet);
                            let ssrc = if is_rtcp {
                                RtcpPacket::get_ssrc(&packet)
                            } else {
                                RtpPacket::get_ssrc(&packet)
                            };
                            if let Some(sender) = streams
                                .get(&ssrc)
                                .and_then(|uuid| track_senders.get(uuid))
                            {
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

                            let mut track_id = None;
                            for (uuid, track) in &webrtc.tracks {
                                if track.ssrc == ssrc {
                                    // match the track ssrc
                                    track_id = Some(*uuid);
                                    break;
                                }
                            }
                            if track_id.is_none() {
                                for (uuid, track) in &webrtc.tracks {
                                    if track.ssrc == 0 {
                                        // if there wasn't any matching ssrc, it's a rid video stream
                                        // try to find a track that didn't have ssrc
                                        track_id = Some(*uuid);
                                        break;
                                    }
                                }
                            }
                            if track_id.is_none() && is_rtcp {
                                for (uuid, track) in &webrtc.tracks {
                                    if track.media_type == MediaType::Video {
                                        // some video rtcp always have a ssrc that is 0 or 1
                                        track_id = Some(*uuid);
                                        break;
                                    }
                                }
                            }

                            let uuid = match track_id {
                                Some(uuid) => uuid,
                                None => {
                                    continue;
                                }
                            };

                            if streams.get(&ssrc).is_none() {
                                streams.insert(ssrc, uuid);
                                if let Some(sender) = track_senders.get(&uuid) {
                                    let _ = sender.send(
                                        StreamReceiverMessage::Packet(packet, now),
                                    );
                                    continue;
                                }
                            }

                            let (sender, receiver) = unbounded_channel();
                            let _ = sender
                                .send(StreamReceiverMessage::Packet(packet, now));
                            track_senders.insert(uuid, sender.clone());
                            let channel = webrtc.channel.clone();
                            let stream_sender_pool = stream_sender_pool.clone();
                            tokio::spawn(async move {
                                let _ = MediaStreamReceiver::process_packets(
                                    uuid,
                                    channel,
                                    peer_ip,
                                    peer_port,
                                    false,
                                    sender,
                                    receiver,
                                    stream_sender_pool,
                                )
                                .await;
                            });
                        }
                    }
                } else {
                    return;
                }
            }
        });

        Ok(())
    }
}

// async fn process_rtp(tacks:  streams:&mut HashMap<u32, UnboundedSender<StreamMessage>>) {

// }

fn is_stun_packet(buf: &[u8]) -> bool {
    buf.len() >= 20 && BigEndian::read_u32(&buf[4..8]) == STUN_MAGIC_COOKIE
}

fn is_dtls_packet(buf: &[u8]) -> bool {
    buf.len() >= 13 && (buf[0] > 19 && buf[0] < 64)
}
