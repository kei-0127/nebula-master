// # Media Stream Processing
// 
// RTP media streams for real-time communication.
// Processes audio/video packets, manages codecs, and ensures high-quality delivery.
// 
// MediaStreamReceiver - AudioStreamReceiver - StreamSenderPool (RTP out)
// │ │ └─ Voicemail/Redis record
// │ └─ RPC state changes
// └─ VideoStreamReceiver - StreamSenderPool (RTP out)
// ├─ NACK/TCC/PLI to senders
// └─ Allocation updates (via VideoStreamSource)

// StreamSenderPool
// ├─ UDP RTP in - RtpPacket(src,dst) - per-stream StreamSender
// ├─ UDP RTCP in - RtcpPacket(uuid) - per-stream StreamSender
// ├─ Recording in - InboundRecording(uuid) - per-stream
// └─ RPC (MEDIA_STREAM) - MediaService::process_rpc - per-stream
// StreamSender (per stream)
// AudioLocal
// ├─ prepare_rtp_packets -> encrypt_rtp -> RtpOut::send (tick)
// ├─ process_inbound_recording -> decode -> record
// └─ process_rpc_msg (SetPeerAddr, DTMF, Recording, Sounds, T38)
// AudioRemote
// └─ forward RTP/RTCP to remote_udp/remote_rtcp_udp
// VideoLocal
// ├─ receive_rtp: VP8 rewrite, RTX remember
// ├─ send_rtp_packet: TCC seq -> (S)RTP -> send
// ├─ process_transportcc: GoogCC -> available_rate
// └─ process_transportnack: RTX resend
// VideoRemote
// └─ forward RTP/RTCP to remote_udp/remote_rtcp_udp
// VideoPeer
// └─ send_frame: encode(H.264) -> RTP -> (S)RTP -> send
// Control/reporting lines (dotted)
// Redis: SDP/crypto/ip/layers/keepalive
// MediaService.switchboard_rpc_client:
// notify_audio_level, notify_available_rate, finish_call_recording, request_keyframe, update_video_destination_layer, notify_video_allocations

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::SeekFrom;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{self, Duration, Instant, SystemTime};

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ByteOrder, LittleEndian};
use bytes::{Buf, Bytes, BytesMut};
use codec::dtmf::{DtmfDecoder, Tone};
use codec::h264::H264;
use codec::vp8::{PixelSize, TruncatedPictureId, TruncatedTl0PicIdx};
use codec::{vp8, InitT38Params, VideoCodec};
use codec::{Codec, Fax};
use lame::Lame;
use nebula_db::api::is_channel_hangup;
use nebula_db::message::{
    ChannelEvent, RedisRecorder, RpcDtmf, RpcFileRecorder, RpcMoh, RpcRecording,
    RpcReinviteCrypto, RpcRingtone, RpcSound, RpcStopSession,
    RpcVideoDestinationLayer, SetMediaModeReq, StreamPeer, VideoAllocation,
};
use nebula_redis::{DistributedMutex, REDIS};
use nebula_rpc::message::{
    AllocatedVideoSSrc, IncomingVideoInfo, RpcMessage, RpcMethod, VideoSourceInfo,
    MEDIA_GROUP, MEDIA_STREAM,
};
use nebula_rpc::server::RpcServer;
use nix::sys::socket::{InetAddr, SockAddr};
use parking_lot::Mutex as BlockMutex;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc;
use rtcp::transport_feedbacks::transport_layer_nack::{
    nack_pairs_from_sequence_numbers, TransportLayerNack,
};
use sdp::payload::{PayloadType, Rtpmap};
use sdp::sdp::{
    Crypto, CryptoKind, MediaDescKind, MediaDescription, MediaMode, MediaType,
    SdpType, LOCAL_RTP_PEER_PORT, PEER_CONNECTION_PORT,
};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{
    unbounded_channel, OwnedPermit, UnboundedReceiver, UnboundedSender,
};
use tokio::task::JoinSet;
use tracing::{info, warn};
use uuid::Uuid;
use webrtc_util::{Marshal, Unmarshal};

use crate::buffer::RtpBuffer;
use crate::date_rate::DataSize;
use crate::googcc::{self, CongestionController};
use crate::nack::NackSender;
use crate::packet::{Ipv4Packet, RtpPacket};
use crate::resampler::Resampler;
use crate::rtcp::{RtcpPacket, RTCP_HEADER_LEN};
use crate::rtx::RtxSender;
use crate::server::{MediaService, MEDIA_SERVICE};
use crate::socket::RawSocket;
use crate::sound::{
    local_record_mp3_file_path, local_record_wav_file_path, pcm_to_wav, SessionSound,
};
use crate::srtp::CryptoContext;
use crate::transportcc::{expand_truncated_counter, TccReceiver, TccSender};

// Network ports for media streams
const REMOTE_RTP_PORT: u16 = 9876;  // RTP media packets
const REMOTE_RTCP_PORT: u16 = 9877; // RTCP control packets
const RECORDING_PORT: u16 = 8765;   // Call recording

// Audio processing thresholds
const THRESHOLD: f64 = 0.6;         // Silence detection threshold
const ACTUAL_THRESHOLD: f64 = THRESHOLD * 0x7fff as f64; // Convert to 16-bit range
const ALPHA: f64 = 7.48338;         // Audio processing coefficient

// Network quality control
const NACK_CALCULATION_INTERVAL: Duration = Duration::from_millis(20); // Minimum interval between NACK computations/sends (rate limit)

// DTMF (touch-tone) digits supported
const DTMF_DIGITS: [&'static str; 17] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "*", "#", "A", "B", "C", "D",
    "X", // X is used for special purposes
];

// RTP sequence number types
pub type TruncatedSequenceNumber = u16; // What actually goes in the packet (16-bit)
pub type FullSequenceNumber = u64;      // Extended sequence number (48-bit due to SRTP limits)
pub type FullTimestamp = u64;           // RTP timestamp
pub type Ssrc = u32;                    // Synchronization source identifier

/// Outgoing RTP packet handler
/// Manages outgoing RTP packets and transmission to remote peers
pub struct RtpOut {
    rtp_packets: Vec<RtpPacket>,  // Packets to send
    t38: bool,                     // Fax over IP (T.38) mode
    peer_transport: PeerTransport, // Network transport
    mode: MediaMode,               // Send/receive mode
}

impl RtpOut {
    /// Flush prepared RTP. Skips fax/T.38 and non-sendrecv; otherwise sends all packets to the peer.
    async fn send(self) -> Result<()> {
        // Skip T.38 fax mode
        if self.t38 {
            return Ok(());
        }

        // Only send in sendrecv mode
        if self.mode != MediaMode::Sendrecv {
            return Ok(());
        }

        // Send all RTP packets
        for rtp_packet in self.rtp_packets {
            self.peer_transport.send(rtp_packet.data()).await?;
        }

        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RpcSource {
    Pool,
    Sender,
}

#[derive(Clone, Debug)]
pub enum StreamReceiverMessage {
    Packet(Vec<u8>, Instant),
    Rpc(RpcMessage),
    UpdateAllocatedVideos(HashMap<Uuid, AllocatedVideo>),
}

pub enum StreamSenderMessage {
    RtpPacket(Uuid, Bytes),
    RtcpPacket(Vec<u8>),
    Rpc(RpcMessage, RpcSource),
    InboundRecording(Vec<u8>),
    PrepareRtpPacket(OwnedPermit<RtpOut>),
    TransportLayerCc(TransportLayerCc, Instant),
    TransportLayerNack(TransportLayerNack, Instant),
    VideoFrame(ffmpeg_next::frame::Video),
}

#[derive(Clone)]
pub enum StreamSenderPoolMessage {
    RtpPacket(Uuid, Uuid, Bytes),
    RtcpPacket(Uuid, Vec<u8>),
    Rpc(Uuid, RpcMessage),
    InboundRecording(Uuid, Vec<u8>),
    TransportLayerCc(Uuid, TransportLayerCc, Instant),
    TransportLayerNack(Uuid, TransportLayerNack, Instant),
    VideoFrame(Uuid, ffmpeg_next::frame::Video),
    Stop(Uuid),
}

#[derive(Clone)]
pub enum PeerTransport {
    LocalPeer(UnboundedSender<StreamReceiverMessage>),
    PeerConnection(Arc<UdpSocket>, SocketAddr),
    SessionSocket(Arc<RawSocket>, Ipv4Packet, SockAddr),
}

impl PeerTransport {
    /// Send bytes to the peer using the active transport (local channel, UDP, or raw IP).
    pub async fn send(&self, buf: &[u8]) -> Result<()> {
        match &self {
            PeerTransport::LocalPeer(sender) => {
                sender.send(StreamReceiverMessage::Packet(
                    buf.to_vec(),
                    Instant::now(),
                ))?;
            }
            PeerTransport::PeerConnection(udp, addr) => {
                udp.send_to(buf, addr).await?;
            }
            PeerTransport::SessionSocket(socket, ip_packet, addr) => {
                let mut ip_packet = ip_packet.clone();
                ip_packet.set_payload(buf);
                socket.send_to(ip_packet.get_slice(), addr).await?;
            }
        }
        Ok(())
    }

    /// Update the peer IP/port (after ICE or re-INVITE) so future packets go to the right place.
    pub fn set_peer_addr(&mut self, peer_ip: Ipv4Addr, peer_port: u16) {
        match &mut *self {
            PeerTransport::LocalPeer(_) => {}
            PeerTransport::PeerConnection(_, ref mut addr) => {
                *addr = SocketAddr::new(IpAddr::V4(peer_ip), peer_port);
            }
            PeerTransport::SessionSocket(_, ref mut packet, ref mut addr) => {
                packet.set_destination_ip(peer_ip);
                packet.set_destination_port(peer_port);
                *addr = SockAddr::new_inet(InetAddr::new(
                    nix::sys::socket::IpAddr::V4(
                        nix::sys::socket::Ipv4Addr::from_std(&peer_ip),
                    ),
                    peer_port,
                ));
            }
        }
    }

    /// True if this is an in-process LocalPeer (no network hops).
    fn is_local(&self) -> bool {
        matches!(self, PeerTransport::LocalPeer(_))
    }
}

// VP8 IDs that we rewrite when forwarding
// Convenience struct to track all 4 IDs together for Vp8SimulcastRtpForwarder
#[derive(Default, Debug, Clone, Eq, PartialEq)]
struct Vp8RewrittenIds {
    seqnum: FullSequenceNumber,
    timestamp: FullTimestamp,
    picture_id: vp8::FullPictureId,
    tl0_pic_idx: vp8::FullTl0PicIdx,
}

impl Vp8RewrittenIds {
    fn new(
        seqnum: FullSequenceNumber,
        timestamp: FullTimestamp,
        picture_id: vp8::FullPictureId,
        tl0_pic_idx: vp8::FullTl0PicIdx,
    ) -> Self {
        Self {
            seqnum,
            timestamp,
            picture_id,
            tl0_pic_idx,
        }
    }

    fn checked_sub(&self, other: &Self) -> Option<Self> {
        Some(Self::new(
            self.seqnum.checked_sub(other.seqnum)?,
            self.timestamp.checked_sub(other.timestamp)?,
            self.picture_id.checked_sub(other.picture_id)?,
            self.tl0_pic_idx.checked_sub(other.tl0_pic_idx)?,
        ))
    }

    fn checked_add(&self, other: &Self) -> Option<Self> {
        Some(Self::new(
            self.seqnum.checked_add(other.seqnum)?,
            self.timestamp.checked_add(other.timestamp)?,
            self.picture_id.checked_add(other.picture_id)?,
            self.tl0_pic_idx.checked_add(other.tl0_pic_idx)?,
        ))
    }

    fn max(&self, other: &Self) -> Self {
        use std::cmp::max;
        Self::new(
            max(self.seqnum, other.seqnum),
            max(self.timestamp, other.timestamp),
            max(self.picture_id, other.picture_id),
            max(self.tl0_pic_idx, other.tl0_pic_idx),
        )
    }
}

pub enum MediaStreamReceiver {
    Audio(AudioStreamReceiver),
    Video(VideoStreamReceiver),
}

impl MediaStreamReceiver {
    /// Background RPC loop for this stream. Forwards control messages to the data path.
    async fn stream_process_rpc(
        id: Uuid,
        sender: UnboundedSender<StreamReceiverMessage>,
    ) -> Result<()> {
        let key = format!("nebula:media_stream:{id}:stream");
        let mut receiver = RpcServer::new(key, "".to_string()).await;
        loop {
            tokio::select! {
                 _ = sender.closed() => {
                      return Ok(())
                 }
                 result = receiver.recv() => {
                      if let Some((_entry_id,rpc_msg)) = result {
                          let _ = sender.send(StreamReceiverMessage::Rpc(rpc_msg));
                      }
                 }
            }
        }
    }

    /// Stream receiver main loop. Reads messages, handles RTP/RTCP, routing, recording, and RPCs.
    pub async fn process_packets(
        id: Uuid,
        channel: String,
        peer_ip: Ipv4Addr,
        peer_port: u16,
        rtcp_only: bool,
        sender: UnboundedSender<StreamReceiverMessage>,
        mut receiver: UnboundedReceiver<StreamReceiverMessage>,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<()> {
        let uuid = id.clone();

        let stream_redis_key = format!("nebula:media_stream:{id}");

        let id = id.to_string();
        let offer_media = get_media_by_directoin(SdpType::Offer, &id)
            .await
            .ok_or(anyhow!("no offer media"))?;
        let answer_media = get_media_by_directoin(SdpType::Answer, &id)
            .await
            .ok_or(anyhow!("no answer media"))?;

        if peer_port != LOCAL_RTP_PEER_PORT && !rtcp_only {
            set_peer_addr(&id, peer_ip, peer_port).await?;
        }

        {
            let local_id = uuid;
            let sender = sender.clone();
            tokio::spawn(async move {
                let _ = Self::stream_process_rpc(local_id, sender).await;
            });
        }

        if let MediaType::Image = offer_media.media_type {
            let key = format!("nebula:media_stream:{id}:destinations");
            let destinations: Vec<String> = REDIS.smembers(&key).await?;
            let destinations: Vec<Uuid> = destinations
                .iter()
                .filter_map(|d| Uuid::from_str(d).ok())
                .collect();

            loop {
                let msg = match receive_msg(&mut receiver, &stream_redis_key).await?
                {
                    Some(msg) => msg,
                    None => continue,
                };
                match msg {
                    StreamReceiverMessage::Packet(packet, _) => {
                        let packet = Bytes::from(packet);
                        for destination in &destinations {
                            let msg = StreamSenderPoolMessage::RtpPacket(
                                uuid,
                                *destination,
                                packet.clone(),
                            );
                            let _ = stream_sender_pool.send(msg);
                        }
                    }
                    StreamReceiverMessage::Rpc(rpc_msg) => {
                        if rpc_msg.method == RpcMethod::StopSession {
                            return Ok(());
                        }
                    }
                    StreamReceiverMessage::UpdateAllocatedVideos(_) => {}
                }
            }
        }

        let rtpmap = offer_media
            .negotiate_rtpmap(&answer_media)
            .ok_or(anyhow!("no rtmap"))?;
        let dtmf_rtpmap = answer_media.dtmf_rtpmap(None);
        let mut dtmf_pts = HashSet::new();
        for rtpmap in offer_media.get_rtpmaps() {
            if rtpmap.name == PayloadType::TelephoneEvent {
                dtmf_pts.insert(rtpmap.type_number);
            }
        }
        for rtpmap in answer_media.get_rtpmaps() {
            if rtpmap.name == PayloadType::TelephoneEvent {
                dtmf_pts.insert(rtpmap.type_number);
            }
        }
        let crypto = if offer_media.is_webrtc() {
            let crypto = get_crypto(CryptoKind::Remote, &id)
                .await
                .ok_or(anyhow!("dtls not negotiated yet"))?;
            Some(crypto)
        } else {
            let local_media = get_media(MediaDescKind::Local, &id)
                .await
                .ok_or(anyhow!("no local media"))?;
            let remote_media = get_media(MediaDescKind::Remote, &id)
                .await
                .ok_or(anyhow!("no reomte media"))?;
            negotiate_crypto(&id, &local_media, &remote_media)
                .await
                .map(|r| r.1)
        };

        let remote_media = get_media(MediaDescKind::Remote, &id)
            .await
            .ok_or(anyhow!("no remote media"))?;

        match offer_media.media_type {
            MediaType::Image => loop {
                let msg = match receive_msg(&mut receiver, &stream_redis_key).await?
                {
                    Some(msg) => msg,
                    None => continue,
                };
                match msg {
                    StreamReceiverMessage::Packet(_packet, _) => {}
                    StreamReceiverMessage::UpdateAllocatedVideos(_) => {}
                    StreamReceiverMessage::Rpc(rpc_msg) => {
                        if rpc_msg.method == RpcMethod::StopSession {
                            return Ok(());
                        }
                    }
                }
            },
            MediaType::Audio => {
                let peer_addr = format!("{peer_ip}:{peer_port}");
                let has_crypto = crypto.is_some();
                let mut stream = AudioStreamReceiver::new(
                    uuid,
                    channel.clone(),
                    rtpmap,
                    dtmf_rtpmap,
                    dtmf_pts,
                    crypto,
                    remote_media.port == LOCAL_RTP_PEER_PORT
                        && peer_port != LOCAL_RTP_PEER_PORT,
                    peer_addr.clone(),
                    stream_sender_pool,
                )
                .await?;
                loop {
                    let msg =
                        match receive_msg(&mut receiver, &stream_redis_key).await? {
                            Some(msg) => msg,
                            None => continue,
                        };
                    match msg {
                        StreamReceiverMessage::Packet(packet, _) => {
                            if RtpPacket::is_rtcp(&packet) {
                                let _ = stream.receive_rtcp_packet(packet).await;
                            } else {
                                let _ = stream.receive_rtp_packet(packet).await;
                            }
                        }
                        StreamReceiverMessage::UpdateAllocatedVideos(_) => {}
                        StreamReceiverMessage::Rpc(rpc_msg) => {
                            info!(
                                channel = channel,
                                stream = uuid.to_string(),
                                "audio receiver receive {rpc_msg:?}",
                            );
                            if rpc_msg.method == RpcMethod::StopSession {
                                if let Some(voicemail) = stream.voicemail.as_mut() {
                                    voicemail.finish().await?;
                                }
                                return Ok(());
                            }
                            if rpc_msg.method == RpcMethod::SetPeerAddr {
                                info!(
                                    channel = channel,
                                    stream = uuid.to_string(),
                                    "audio receiver crypto {has_crypto} receive new SetPeerAddr {:?}, old peer_addr is {peer_addr}, and exit", rpc_msg.params
                                );
                                return Ok(());
                            }
                            let _ = stream.process_rpc_msg(rpc_msg).await;
                        }
                    }
                }
            }
            MediaType::Video => {
                let ssrc = get_ssrc(&id).await.unwrap_or(0);
                let ssrc = if ssrc == 0 { uuid.as_fields().0 } else { ssrc };
                let mut stream = VideoStreamReceiver::new(
                    uuid,
                    channel.clone(),
                    sender,
                    ssrc,
                    rtpmap,
                    crypto,
                    stream_sender_pool.clone(),
                )
                .await;

                // send an empty rtcp packet to trigger the creation of video sender stream
                // the reason for that is the video allocation is done in sender stream,
                // and before the video allocation is done, there won't be any packets sent to it
                let _ = stream_sender_pool
                    .send(StreamSenderPoolMessage::RtcpPacket(uuid, vec![]));

                loop {
                    let msg =
                        match receive_msg(&mut receiver, &stream_redis_key).await? {
                            Some(msg) => msg,
                            None => continue,
                        };
                    match msg {
                        StreamReceiverMessage::Packet(packet, now) => {
                            if RtpPacket::is_rtcp(&packet) {
                                let _ = stream.receive_rtcp_packet(packet).await;
                            } else {
                                let _ = stream.receive_rtp_packet(packet, now).await;
                            }
                        }
                        StreamReceiverMessage::UpdateAllocatedVideos(
                            allocated_videos,
                        ) => {
                            stream.update_allocated_videos(allocated_videos);
                        }
                        StreamReceiverMessage::Rpc(rpc_msg) => {
                            if rpc_msg.method == RpcMethod::StopSession {
                                info!(
                                    channel = channel,
                                    stream = uuid.to_string(),
                                    "video receiver receive StopSession",
                                );
                                return Ok(());
                            }
                            if rpc_msg.method == RpcMethod::SetPeerAddr {
                                info!(
                                    channel = channel,
                                    stream = uuid.to_string(),
                                    "video receiver receive SetPeerAddr",
                                );
                                return Ok(());
                            }
                            let _ = stream.process_rpc_msg(rpc_msg).await;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct RemoteStreamSenderMonitor {
    live_ips: Arc<scc::HashSet<String>>,
    all_ips: Arc<scc::HashSet<String>>,
}

impl Default for RemoteStreamSenderMonitor {
    /// Default constructor that just calls new().
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteStreamSenderMonitor {
    /// Create a liveness monitor backed by concurrent sets.
    pub fn new() -> Self {
        Self {
            live_ips: Arc::new(scc::HashSet::new()),
            all_ips: Arc::new(scc::HashSet::new()),
        }
    }

    /// Periodically check Redis to update liveness for this IP.
    async fn run_check_ip(&self, ip: String) {
        let mut ticker = tokio::time::interval(Duration::from_millis(1000));
        let key = format!("nebula:media_server:{ip}:live");
        loop {
            self.check_ip_live(&key, &ip).await;
            ticker.tick().await;
        }
    }

    /// Add or remove IP from the live set based on Redis presence.
    async fn check_ip_live(&self, key: &str, ip: &str) {
        let exists = REDIS.exists(key).await.unwrap_or(false);
        if exists {
            if !self.live_ips.contains_async(ip).await {
                warn!("check ip live success for {}", ip);
                let _ = self.live_ips.insert_async(ip.to_string()).await;
            }
        } else if !REDIS.exists(key).await.unwrap_or(false)
            && !REDIS.exists(key).await.unwrap_or(false)
            && self.live_ips.contains_async(ip).await
        {
            warn!("check ip live failed for {}", ip);
            self.live_ips.remove_async(ip).await;
        }
    }

    /// True if we consider this IP live; unseen IPs start a background probe.
    pub async fn is_live(&self, ip: &str) -> bool {
        if ip == MEDIA_SERVICE.local_ip {
            return true;
        }

        if !self.all_ips.contains_async(ip).await {
            let _ = self.all_ips.insert_async(ip.to_string()).await;
            let _ = self.live_ips.insert_async(ip.to_string()).await;

            let m = self.clone();
            let ip = ip.to_string();
            tokio::spawn(async move {
                m.run_check_ip(ip).await;
            });
        }
        self.live_ips.contains_async(ip).await
    }
}

//              ┌─────────────────────────────┐
//              │ AudioStreamSenderPool.run() │
//              └─────────────┬───────────────┘
//                            │
//     ┌──────────────────────┼───────────────────────┐
//     │                      │                       │
//  [RTP UDP]           [RTCP UDP]             [Recording UDP]
//     │                      │                       │
//     └─────────→ StreamSenderPoolMessage →──────────┘
//                            │
//                            ▼
//                [Main Pool Event Loop]
//                            │
//            ┌───────────────┴────────────────┐
//            │ existing stream?               │
//            ├───────────────┬────────────────┤
//            │ yes            │ no            │
//            ▼                ▼               │
//    send message        spawn new stream ────┘
//                            │
//              StreamSender::process_packets()
//                            │
//                handles per-call RTP/RTCP

pub struct AudioStreamSenderPool {
    remote_rtp_udp: Arc<UdpSocket>,
    remote_rtcp_udp: Arc<UdpSocket>,
    recording_udp: Arc<UdpSocket>,
    streams: HashMap<Uuid, UnboundedSender<StreamSenderMessage>>,
    pub sender: UnboundedSender<StreamSenderPoolMessage>,
    receiver: UnboundedReceiver<StreamSenderPoolMessage>,
    pub peer_connection_udp_socket: Arc<UdpSocket>,
    monitor: RemoteStreamSenderMonitor,
    sender_runtime: Arc<Runtime>,
    timer_runtime: Arc<Runtime>,
    recording_runtime: Arc<Runtime>,
}

impl AudioStreamSenderPool {
    /// Create the shared sender pool. Binds UDP sockets and spins runtimes for timers, senders, and recording.
    pub async fn new(
        sender: UnboundedSender<StreamSenderPoolMessage>,
        receiver: UnboundedReceiver<StreamSenderPoolMessage>,
        peer_connection_udp_socket: Arc<UdpSocket>,
    ) -> Result<Self> {
        let remote_rtp_udp =
            Arc::new(UdpSocket::bind(format!("0.0.0.0:{}", REMOTE_RTP_PORT)).await?);
        let remote_rtcp_udp = Arc::new(
            UdpSocket::bind(format!("0.0.0.0:{}", REMOTE_RTCP_PORT)).await?,
        );
        let recording_udp =
            Arc::new(UdpSocket::bind(format!("0.0.0.0:{}", RECORDING_PORT)).await?);

        let monitor = RemoteStreamSenderMonitor::new();

        let timer_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("timer")
            .build()
            .unwrap();
        let timer_runtime = Arc::new(timer_runtime);

        let sender_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("sender")
            .build()
            .unwrap();
        let sender_runtime = Arc::new(sender_runtime);

        let recording_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("recording")
            .build()
            .unwrap();
        let recording_runtime = Arc::new(recording_runtime);

        Ok(Self {
            remote_rtp_udp,
            remote_rtcp_udp,
            recording_udp,
            streams: HashMap::new(),
            sender,
            receiver,
            peer_connection_udp_socket,
            monitor,
            sender_runtime,
            timer_runtime,
            recording_runtime,
        })
    }

    /// Main event loop for all senders. Spawns UDP readers and RPC consumer, then fans out messages to per-call tasks.
    pub async fn run(&mut self) {
        let local_ip = MEDIA_SERVICE.local_ip.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(500));
            let key = format!("nebula:media_server:{}:live", local_ip);
            loop {
                let _ = REDIS.setex(&key, 2, "yes").await;
                ticker.tick().await;
            }
        });

        let remote_rtp_udp = self.remote_rtp_udp.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let mut buf = [0; 5000];
            loop {
                if let Ok(n) = remote_rtp_udp.recv(&mut buf).await {
                    let mut tmp = BytesMut::with_capacity(1500);
                    tmp.resize(5000, 0);
                    tmp[..n].copy_from_slice(&buf[..n]);
                    tmp.truncate(n);
                    let packet = tmp.freeze();
                    if let Some(ext) = RtpPacket::get_extension(&packet) {
                        if ext.len() == 32 {
                            let (src, dst) = ext.split_at(16);
                            if let Ok(src) = Uuid::from_slice(src) {
                                if let Ok(dst) = Uuid::from_slice(dst) {
                                    let _ = sender.send(
                                        StreamSenderPoolMessage::RtpPacket(
                                            src, dst, packet,
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        });

        {
            let sender = self.sender.clone();
            let mut rpc_receiver =
                RpcServer::new(MEDIA_STREAM.to_string(), MEDIA_GROUP.to_string())
                    .await;
            tokio::spawn(async move {
                loop {
                    if let Some((entry, rpc_msg)) = rpc_receiver.recv().await {
                        let _ = entry.ack().await;
                        let _ = MediaService::process_rpc(rpc_msg, &sender).await;
                    }
                }
            });
        }

        {
            let remote_rtcp_udp = self.remote_rtcp_udp.clone();
            let sender = self.sender.clone();
            tokio::spawn(async move {
            let mut buf = [0; 5000];
                loop {
                    if let Ok(n) = remote_rtcp_udp.recv(&mut buf).await {
                        let (uuid, packet) = buf[..n].split_at(16);
                        if let Ok(uuid) = Uuid::from_slice(uuid) {
                            let _ =
                                sender.send(StreamSenderPoolMessage::RtcpPacket(
                                    uuid,
                                packet.to_vec(),
                                ));
                        }
                    }
                }
            });
        }

        let recording_udp = self.recording_udp.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let mut buf = [0; 5000];
            loop {
                if let Ok(n) = recording_udp.recv(&mut buf).await {
                    let mut tmp = BytesMut::with_capacity(1500);
                    tmp.resize(5000, 0);
                    tmp[..n].copy_from_slice(&buf[..n]);
                    tmp.truncate(n);
                    let packet = tmp.freeze();
                    if let Some(ext) = RtpPacket::get_extension(&packet) {
                        if let Ok(uuid) = Uuid::from_slice(ext) {
                            let _ = sender.send(
                                StreamSenderPoolMessage::InboundRecording(
                                    uuid, packet.to_vec(),
                                ),
                            );
                        }
                    }
                }
            }
        });

        loop {
            if let Some(msg) = self.receiver.recv().await {
                let (id, msg) = match msg {
                    StreamSenderPoolMessage::RtcpPacket(dst, packet) => {
                        (dst, StreamSenderMessage::RtcpPacket(packet))
                    }
                    StreamSenderPoolMessage::RtpPacket(src, dst, packet) => {
                        (dst, StreamSenderMessage::RtpPacket(src, packet))
                    }
                    StreamSenderPoolMessage::Rpc(id, rpc_msg) => {
                        (id, StreamSenderMessage::Rpc(rpc_msg, RpcSource::Pool))
                    }
                    StreamSenderPoolMessage::InboundRecording(id, packet) => {
                        (id, StreamSenderMessage::InboundRecording(packet))
                    }
                    StreamSenderPoolMessage::TransportLayerCc(id, cc, now) => {
                        (id, StreamSenderMessage::TransportLayerCc(cc, now))
                    }
                    StreamSenderPoolMessage::TransportLayerNack(id, nack, now) => {
                        (id, StreamSenderMessage::TransportLayerNack(nack, now))
                    }
                    StreamSenderPoolMessage::VideoFrame(id, frame) => {
                        (id, StreamSenderMessage::VideoFrame(frame))
                    }
                    StreamSenderPoolMessage::Stop(id) => {
                        self.streams.remove(&id);
                        continue;
                    }
                };

                if let Some(sender) =
                    self.streams.get(&id).filter(|s| !s.is_closed())
                {
                    let _ = sender.send(msg);
                } else {
                    let (sender, receiver) = unbounded_channel();
                    let _ = sender.send(msg);
                    self.streams.insert(id, sender.clone());
                    let remote_rtp_udp = self.remote_rtp_udp.clone();
                    let remote_rtcp_udp = self.remote_rtcp_udp.clone();
                    let recording_udp = self.recording_udp.clone();
                    let pc_udp = self.peer_connection_udp_socket.clone();
                    let stream_sender_pool = self.sender.clone();
                    let monitor = self.monitor.clone();
                    let timer_runtime = self.timer_runtime.clone();
                    let sender_runtime = self.sender_runtime.clone();
                    let recording_runtime = self.recording_runtime.clone();
                    tokio::spawn(async move {
                        let _ = StreamSender::process_packets(
                            id,
                            sender_runtime,
                            remote_rtp_udp,
                            remote_rtcp_udp,
                            recording_udp,
                            sender,
                            receiver,
                            pc_udp,
                            monitor,
                            timer_runtime,
                            recording_runtime,
                            stream_sender_pool.clone(),
                        )
                        .await;
                        let _ = stream_sender_pool
                            .send(StreamSenderPoolMessage::Stop(id));
                    });
                }
            }
        }
    }
}

pub struct StreamVoicemail {
    channel: String,
    cdr: String,
    rate: u32,

    url: String,
    buffer: Vec<u8>,
    offset: usize,
    lame: Arc<BlockMutex<Lame>>,

    samples: u32,
    wav: Vec<u8>,
    wav_file_initiated: bool,
}

impl StreamVoicemail {
    pub async fn new(
        uuid: Uuid,
        channel: String,
        cdr: String,
        rate: u32,
    ) -> Result<Self> {
        let key = format!("nebula:media_stream:{}", &uuid.to_string());
        let url = REDIS
            .hget(&key, "voicemail_url")
            .await
            .unwrap_or("".to_string());
        let (offset, url) = if url.is_empty() {
            let url = MEDIA_SERVICE
                .storage_client
                .new_resumable_upload("aura-voicemails", &format!("{}.mp3", cdr))
                .await?;
            REDIS.hsetex(&key, "voicemail_url", &url).await?;
            (0, url)
        } else {
            (
                MEDIA_SERVICE
                    .storage_client
                    .check_resumable_upload(&url)
                    .await?,
                url,
            )
        };

        let lame = new_lame(rate, 1).await?;

        Ok(Self {
            channel,
            rate,
            lame,
            cdr,
            url,
            offset,
            buffer: Vec::new(),
            samples: 0,
            wav: Vec::new(),
            wav_file_initiated: false,
        })
    }

    /// Finalize MP3 upload, flush WAV metadata, and notify that the voicemail uploaded.
    async fn finish(&mut self) -> Result<()> {
        MEDIA_SERVICE
            .storage_client
            .finish_upload(&self.url, &self.buffer, self.offset)
            .await?;

        self.upload_wav().await?;
        Ok(())
    }

    /// Append WAV and chunk-upload MP3 using LAME; stays off the hot path.
    async fn new_pcm(&mut self, pcm: Vec<i16>) -> Result<()> {
        let path = format!("/var/lib/nebula/voicemails/{}.wav", &self.cdr);
        if !self.wav_file_initiated {
            self.wav_file_initiated = true;
            if let Ok(mut file) = tokio::fs::File::create(&path).await {
                if let Ok(wav) = pcm_to_wav(self.rate, vec![]).await {
                    file.write_all(&wav).await?;
                    file.flush().await?;
                }
            }
        }

        let chunk = 256 * 1024;

        let mut wav = vec![0; pcm.len() * 2];
        for (i, v) in pcm.iter().enumerate() {
            LittleEndian::write_i16(&mut wav[2 * i..2 * i + 2], *v);
        }
        self.wav.extend_from_slice(&wav);
        self.samples += pcm.len() as u32;
        if self.wav.len() > chunk {
            if let Ok(mut file) =
                tokio::fs::OpenOptions::new().append(true).open(&path).await
            {
                file.write_all(&self.wav).await?;
                self.wav.clear();
            }
        }

        let lame = self.lame.clone();
        if let Ok(buf) = nebula_task::spawn_task(move || -> Result<Vec<u8>> {
            let mut buffer = [0; 5000];
            let n = lame.lock().encode(&pcm, &pcm, &mut buffer)?;
            Ok(buffer[..n].to_vec())
        })
        .await?
        {
            self.buffer.extend_from_slice(&buf);
            if self.buffer.len() > chunk {
                let range = MEDIA_SERVICE
                    .storage_client
                    .resume_upload(&self.url, &self.buffer[..chunk], self.offset)
                    .await?;
                self.buffer = self.buffer[range - self.offset + 1..].to_vec();
                self.offset = range + 1;
            }
        }
        Ok(())
    }

    /// Fix WAV header sizes, upload the WAV artifact, and clean up the temp file.
    async fn upload_wav(&self) -> Result<()> {
        let path = format!("/var/lib/nebula/voicemails/{}.wav", &self.cdr);
        if !self.wav.is_empty() {
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await?;
            file.write_all(&self.wav).await?;
        }

        {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .await?;
            let size = self.samples * 2;
            file.seek(SeekFrom::Start(4)).await?;
            file.write_u32_le(size + 36).await?;
            file.seek(SeekFrom::Start(40)).await?;
            file.write_u32_le(size).await?;
        }

        MEDIA_SERVICE
            .storage_client
            .upload("aura-voicemails", &format!("{}.wav", self.cdr), &path)
            .await?;
        tokio::fs::remove_file(&path).await?;

        REDIS
            .xadd(
                &format!("nebula:channel:{}:stream", &self.channel),
                &ChannelEvent::VoicemailUploaded.to_string(),
                "uploaded",
            )
            .await?;
        Ok(())
    }
}

pub struct AudioStreamReceiver {
    id: Uuid,
    channel: String,
    rtpmap: Rtpmap,
    dtmf_rtpmap: Option<Rtpmap>,
    dtmf_decoder: Option<Arc<BlockMutex<DtmfDecoder>>>,
    dtmf_pts: HashSet<u8>,
    destinations: Option<Vec<Uuid>>,
    stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    crypto: Option<CryptoContext>,
    voicemail: Option<StreamVoicemail>,
    redis_recorder: Option<RedisRecorder>,
    codec: Arc<BlockMutex<Box<dyn Codec>>>,
    mode: MediaMode,
    last_keepalive_update: u64,
    room: String,
    last_reported_audio_level: u8,
    last_reported: u64,
    muted: bool,
    t38_passthrough: bool,
    // when the call is on a switched device, we need to disable the destinations
    // so that the original leg's audio doesn't get to the other end
    disable_destinations: bool,
    last_seq: u16,
}

impl AudioStreamReceiver {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        id: Uuid,
        channel: String,
        rtpmap: Rtpmap,
        dtmf_rtpmap: Option<Rtpmap>,
        dtmf_pts: HashSet<u8>,
        crypto: Option<CryptoContext>,
        disable_destinations: bool,
        peer_addr: String,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<Self> {
        let codec = rtpmap.name.get_codec().await?;

        let mut voicemail = None;
        let voicemail_cdr = REDIS
            .hget(
                &format!("nebula:media_stream:{}", &id.to_string()),
                "voicemail",
            )
            .await
            .unwrap_or("".to_string());
        if !voicemail_cdr.is_empty() {
            voicemail = StreamVoicemail::new(
                id,
                channel.clone(),
                voicemail_cdr,
                rtpmap.get_rate(),
            )
            .await
            .ok();
        }

        let room = REDIS
            .hget(&format!("nebula:media_stream:{id}"), "room")
            .await
            .unwrap_or_else(|_| "".to_string());

        let t38_passthrough = REDIS
            .hget(&format!("nebula:media_stream:{id}"), "t38_passthrough")
            .await
            .unwrap_or_else(|_| "".to_string())
            == "yes";

        info!(
            channel = channel,
            stream = id.to_string(),
            "new audio stream receiver {channel} peer_addr {peer_addr} t38 {t38_passthrough}"
        );

        let dtmf_decoder = if dtmf_rtpmap.is_none() {
            Some(DtmfDecoder::new())
        } else {
            None
        };

        Ok(Self {
            id,
            channel,
            rtpmap,
            dtmf_rtpmap,
            dtmf_decoder: dtmf_decoder.map(|d| Arc::new(BlockMutex::new(d))),
            dtmf_pts,
            destinations: None,
            crypto,
            stream_sender_pool,
            voicemail,
            redis_recorder: None,
            codec: Arc::new(BlockMutex::new(codec)),
            mode: MediaMode::Sendrecv,
            last_keepalive_update: 0,
            room,
            last_reported_audio_level: 0,
            last_reported: 0,
            muted: false,
            t38_passthrough,
            disable_destinations,
            last_seq: 0,
        })
    }

    /// Handle RPCs for the audio receiver: mute/mode, crypto reinvite, voicemail/redis recording,
    /// destination refresh, DTMF/sound-related controls.
    async fn process_rpc_msg(&mut self, rpc_msg: RpcMessage) -> Result<()> {
        match &rpc_msg.method {
            RpcMethod::UpdateMediaDestinations => {
                self.destinations = None;
            }
            RpcMethod::SetSessionMediaMode => {
                let req: SetMediaModeReq = serde_json::from_value(rpc_msg.params)?;
                let mode = MediaMode::from_str(&req.mode)?;
                self.mode = mode;
            }
            RpcMethod::SetT38Passthrough => {
                self.t38_passthrough = true;
            }
            RpcMethod::ReinviteCrypto => {
                let req: RpcReinviteCrypto = serde_json::from_value(rpc_msg.params)?;
                let local = MediaDescription::from_str(&req.local)?;
                let remote = MediaDescription::from_str(&req.remote)?;
                if let Some((_, remote)) =
                    negotiate_crypto(&self.id.to_string(), &local, &remote).await
                {
                    self.crypto = Some(remote);
                }
            }
            RpcMethod::MuteStream => {
                self.muted = true;
            }
            RpcMethod::UnmuteStream => {
                self.muted = false;
            }
            RpcMethod::StartVoicemail => {
                let voicemail: RpcRecording =
                    serde_json::from_value(rpc_msg.params)?;
                self.start_voicemail(voicemail.cdr).await?;
            }
            RpcMethod::RecordToRedis => {
                let mut recorder: RedisRecorder =
                    serde_json::from_value(rpc_msg.params)?;
                let rate = self.rtpmap.get_rate();
                recorder.limit *= (rate / 1000) as usize;
                self.redis_recorder = Some(recorder);
            }
            RpcMethod::StopRecordToRedis => {
                let recorder: RedisRecorder =
                    serde_json::from_value(rpc_msg.params)?;
                if let Some(redis_recorder) = self.redis_recorder.as_mut() {
                    if redis_recorder.key == recorder.key {
                        let sample_rate = self.rtpmap.get_rate();
                        tokio::spawn(async move {
                            if let Ok(wav) =
                                pcm_to_wav(sample_rate, recorder.pcm).await
                            {
                                let _ = REDIS
                                    .setex(
                                        &recorder.key,
                                        recorder.expire,
                                        &base64::encode(&wav),
                                    )
                                    .await;
                            }
                        });
                        self.redis_recorder = None;
                    }
                }
            }
            _ => (),
        }
        Ok(())
    }

    /// Process RTCP for audio. Decrypt if needed and refresh activity/keepalive markers.
    async fn receive_rtcp_packet(&mut self, mut rtcp_packet: Vec<u8>) -> Result<()> {
        let _ = REDIS
            .hsetex(
                &format!("nebula:channel:{}", &self.channel),
                "last_rtp_packet",
                &time::SystemTime::now()
                    .duration_since(time::SystemTime::UNIX_EPOCH)?
                    .as_secs()
                    .to_string(),
            )
            .await;
        if let Some(crypto) = self.crypto.as_ref() {
            let crypto = crypto.state.clone();
            rtcp_packet =
                nebula_task::spawn_task(move || crypto.decrypt_rtcp(&rtcp_packet))
                    .await??;
        }

        Ok(())
    }

    /// Per incoming audio RTP:
    /// - refresh keepalive (every ~5s)
    /// - if SRTP: decrypt
    /// - if not Sendrecv or muted: drop
    /// - if T.38 passthrough: fan-out raw RTP to destinations and return
    /// - parse RTP; handle telephone-event PT (dedupe by timestamp, publish DTMF and notify switchboard)
    ///   or run in-band DTMF decoder when needed
    /// - fan-out RTP to destinations and inbound recording
    /// - if voicemail on: decode to PCM and append
    /// - if Redis recorder on: accumulate PCM and store WAV when full
    /// - if in a room: report audio level from ext id=1 (rate-limited)
    pub async fn receive_rtp_packet(
        &mut self,
        mut raw_rtp_packet: Vec<u8>,
    ) -> Result<()> {
        let now = time::SystemTime::now()
            .duration_since(time::SystemTime::UNIX_EPOCH)?
            .as_secs();
        if now > self.last_keepalive_update + 5 {
            REDIS
                .hsetex(
                    &format!("nebula:channel:{}", &self.channel),
                    "last_rtp_packet",
                    &time::SystemTime::now()
                        .duration_since(time::SystemTime::UNIX_EPOCH)?
                        .as_secs()
                        .to_string(),
                )
                .await?;
            let key = format!("nebula:media_stream:{}", &self.id.to_string());
            REDIS
                .hsetex(
                    &key,
                    "last_rtp_packet",
                    &time::SystemTime::now()
                        .duration_since(time::SystemTime::UNIX_EPOCH)?
                        .as_secs()
                        .to_string(),
                )
                .await?;
            self.last_keepalive_update = now;
        }

        if let Some(crypto) = self.crypto.as_mut() {
            let roc = crypto.update_roc(&raw_rtp_packet).await;
            let crypto = crypto.state.clone();
            raw_rtp_packet = nebula_task::spawn_task(move || {
                crypto.decrypt_rtp(roc, &raw_rtp_packet)
            })
            .await??;
        }

        if self.mode != MediaMode::Sendrecv {
            return Ok(());
        }

        if self.muted {
            return Ok(());
        }

        if self.t38_passthrough {
            let destinations = match &self.destinations {
                Some(destinations) => destinations,
                None => {
                    let key =
                        format!("nebula:media_stream:{}:destinations", self.id);
                    let destinations: Vec<String> = REDIS.smembers(&key).await?;
                    let destinations: Vec<Uuid> = destinations
                        .iter()
                        .filter_map(|d| Uuid::from_str(d).ok())
                        .collect();

                    self.destinations = Some(destinations);
                    self.destinations.as_ref().unwrap()
                }
            };
            let packet = Bytes::from(raw_rtp_packet.clone());
            for destination in destinations {
                let msg = StreamSenderPoolMessage::RtpPacket(
                    self.id,
                    *destination,
                    packet.clone(),
                );
                let _ = self.stream_sender_pool.send(msg);
            }
            return Ok(());
        }

        let rtp_packet = rtp::packet::Packet::unmarshal(&mut bytes::Bytes::from(
            raw_rtp_packet.clone(),
        ))?;

        let pt = rtp_packet.header.payload_type;
        if pt != self.rtpmap.type_number && self.dtmf_pts.contains(&pt) {
            let payload = rtp_packet.payload.slice(..);
            let ts = rtp_packet.header.timestamp;
            if payload[1] & 0x80 > 0 {
                let last_timestamp: Result<u32> = REDIS
                    .getset(
                        &format!("nebula:media_stream:{}:dtmf_timestamp", self.id),
                        &ts.to_string(),
                    )
                    .await;
                REDIS
                    .expire(
                        &format!("nebula:media_stream:{}:dtmf_timestamp", self.id),
                        10,
                    )
                    .await?;
                if last_timestamp.is_err() || ts != last_timestamp.unwrap() {
                    let dtmf = DTMF_DIGITS
                        .get(payload[0] as usize)
                        .ok_or(anyhow!("receive invalid DTMF"))?
                        .to_string();
                    REDIS
                        .xadd(
                            &format!("nebula:channel:{}:stream", &self.channel),
                            "dtmf",
                            &dtmf,
                        )
                        .await?;
                    MEDIA_SERVICE
                        .switchboard_rpc_client
                        .receive_dtmf(
                            &MEDIA_SERVICE.config.switchboard_host,
                            self.channel.clone(),
                            self.id.to_string(),
                            dtmf,
                        )
                        .await?;
                }
            }
            return Ok(());
        }

        if let Some(dtmf_decoder) = self.dtmf_decoder.clone() {
            let payload = rtp_packet.payload.slice(..).to_vec();
            let pcm = Self::decode(self.codec.clone(), payload).await?;

            let changes = nebula_task::spawn_task(move || {
                let pcm: Vec<f32> =
                    pcm.iter().map(|v| *v as f32 / i16::MAX as f32).collect();
                dtmf_decoder.lock().process(&pcm)
            })
            .await?;

            for (tone, state) in changes {
                if state == codec::dtmf::State::On {
                    let dtmf = tone.as_char().to_string();
                    REDIS
                        .xadd(
                            &format!("nebula:channel:{}:stream", &self.channel),
                            "dtmf",
                            &dtmf,
                        )
                        .await?;
                    MEDIA_SERVICE
                        .switchboard_rpc_client
                        .receive_dtmf(
                            &MEDIA_SERVICE.config.switchboard_host,
                            self.channel.clone(),
                            self.id.to_string(),
                            dtmf,
                        )
                        .await?;
                }
            }
        }

        if self.disable_destinations {
            return Ok(());
        }

        let destinations = match &self.destinations {
            Some(destinations) => destinations,
            None => {
                let key = format!("nebula:media_stream:{}:destinations", self.id);
                let destinations: Vec<String> = REDIS.smembers(&key).await?;
                let destinations: Vec<Uuid> = destinations
                    .iter()
                    .filter_map(|d| Uuid::from_str(d).ok())
                    .collect();

                self.destinations = Some(destinations);
                self.destinations.as_ref().unwrap()
            }
        };
            let packet = Bytes::from(raw_rtp_packet);
        for destination in destinations {
            let msg = StreamSenderPoolMessage::RtpPacket(
                self.id,
                *destination,
                packet.clone(),
            );
            let _ = self.stream_sender_pool.send(msg);
        }
        let _ =
            self.stream_sender_pool
                .send(StreamSenderPoolMessage::InboundRecording(
                    self.id,
                        packet.to_vec(),
                ));

        if let Some(voicemail) = self.voicemail.as_mut() {
            let payload = rtp_packet.payload.slice(..).to_vec();
            let pcm = Self::decode(self.codec.clone(), payload).await?;
            voicemail.new_pcm(pcm).await?;
        }

        if let Some(redis_recorder) = self.redis_recorder.as_mut() {
            let payload = rtp_packet.payload.slice(..).to_vec();
            let pcm = Self::decode(self.codec.clone(), payload).await?;
            redis_recorder.pcm.extend_from_slice(&pcm);
            if redis_recorder.pcm.len() >= redis_recorder.limit {
                let recorder = redis_recorder.clone();
                let sample_rate = self.rtpmap.get_rate();
                self.redis_recorder = None;
                tokio::spawn(async move {
                    if let Ok(wav) = pcm_to_wav(sample_rate, recorder.pcm).await {
                        let _ = REDIS
                            .setex(
                                &recorder.key,
                                recorder.expire,
                                &base64::encode(&wav),
                            )
                            .await;
                    }
                });
            }
        }

        if !self.room.is_empty() {
            for ext in rtp_packet.header.extensions.iter() {
                if ext.id == 1 {
                    let level = ext.payload.first();
                    let audio_level = level
                        .map(|level| 120u8.saturating_sub(*level & 0b0111_1111));
                    if let Some(level) = audio_level {
                        let mut report = false;
                        if self.last_reported + 1 < now
                            && (level as i32 - self.last_reported_audio_level as i32)
                                .abs()
                                > 10
                        {
                            self.last_reported_audio_level = level;
                            self.last_reported = now;
                            report = true;
                        }

                        let room = self.room.clone();
                        let channel = self.channel.clone();
                        tokio::spawn(async move {
                            let _ = REDIS
                                .xadd_maxlen::<String>(
                                    &format!(
                                        "nebula:room:{room}:members:{channel}:levels",
                                    ),
                                    "level",
                                    &level.to_string(),
                                    50,
                                )
                                .await;
                            if report {
                                let _ = MEDIA_SERVICE
                                    .switchboard_rpc_client
                                    .notify_audio_level(
                                        &MEDIA_SERVICE.config.switchboard_host,
                                        room.clone(),
                                        channel.clone(),
                                        level,
                                    )
                                    .await;
                            }
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Start voicemail capture for this stream using the negotiated sample rate.
    async fn start_voicemail(&mut self, cdr: String) -> Result<()> {
        if self.voicemail.is_some() {
            return Ok(());
        }

        REDIS
            .hsetnx(
                &format!("nebula:media_stream:{}", &self.id.to_string()),
                "voicemail",
                &cdr,
            )
            .await
            .unwrap_or(false);

        let rate = self.rtpmap.get_rate();
        self.voicemail = Some(
            StreamVoicemail::new(self.id, self.channel.clone(), cdr, rate).await?,
        );
        Ok(())
    }

    /// Decode a codec payload to PCM on a worker thread to avoid blocking.
    async fn decode(
        codec: Arc<BlockMutex<Box<dyn Codec>>>,
        payload: Vec<u8>,
    ) -> Result<Vec<i16>> {
        let pcm = nebula_task::spawn_task(move || -> Result<Vec<i16>> {
            let mut buf = [0; 5000];
            let n = codec.lock().decode(&payload, &mut buf)?;
            Ok(buf[..n].to_vec())
        })
        .await??;
        Ok(pcm)
    }
}

pub struct FileRecorder {
    uuid: String,
    sender: UnboundedSender<Vec<i16>>,
}

impl FileRecorder {
    /// Upload a finished local recording (WAV/MP3) for the given UUID to storage.
    pub async fn upload(uuid: String) -> Result<()> {
        let wav_path = local_record_wav_file_path(&uuid);
        let mp3_path = local_record_mp3_file_path(&uuid);
        MEDIA_SERVICE
            .storage_client
            .upload("aura-sounds", &format!("{}.wav", uuid), &wav_path)
            .await?;
        MEDIA_SERVICE
            .storage_client
            .upload("aura-sounds", &format!("{}.mp3", uuid), &mp3_path)
            .await?;
        Ok(())
    }

    /// Start a local file recorder. Spawns a background task to write WAV/MP3 progressively.
    pub fn new(
        uuid: String,
        rate: u32,
        ptime: usize,
        stream_sender: UnboundedSender<StreamSenderMessage>,
    ) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

        let wav_path = local_record_wav_file_path(&uuid);
        let mp3_path = local_record_mp3_file_path(&uuid);
        tokio::spawn(async move {
            let _ =
                Self::run(receiver, wav_path, mp3_path, rate, ptime, stream_sender)
                    .await;
        });

        Self { uuid, sender }
    }

    /// Background loop: on every ptime, pull PCM (or silence), append WAV/MP3 buffers, and flush in chunks.
    async fn run(
        mut receiver: UnboundedReceiver<Vec<i16>>,
        wav_path: String,
        mp3_path: String,
        rate: u32,
        ptime: usize,
        stream_sender: UnboundedSender<StreamSenderMessage>,
    ) -> Result<()> {
        let lame = new_lame(rate, 1).await?;

        let mut wav = Vec::new();
        let mut mp3 = Vec::new();
        let chunk = 256 * 1024;
        let pcm_len = (rate / 1000) as usize * ptime;
        let mut samples = 0;

        let mut wav_file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&wav_path)
            .await?;
        if let Ok(wav) = pcm_to_wav(rate, vec![]).await {
            wav_file.write_all(&wav).await?;
            wav_file.flush().await?;
        }
        let mut mp3_file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&mp3_path)
            .await?;

        let mut ticker = tokio::time::interval(Duration::from_millis(ptime as u64));
        loop {
            ticker.tick().await;
            if stream_sender.is_closed() {
                break;
            }

            let pcm = match receiver.try_recv() {
                Ok(pcm) => pcm,
                Err(TryRecvError::Empty) => vec![0; pcm_len],
                Err(TryRecvError::Disconnected) => {
                    break;
                }
            };

            let mut pcm_wav = vec![0; pcm.len() * 2];
            for (i, v) in pcm.iter().enumerate() {
                LittleEndian::write_i16(&mut pcm_wav[2 * i..2 * i + 2], *v);
            }
            samples += pcm.len() as u32;
            wav.extend_from_slice(&pcm_wav);
            if wav.len() > chunk {
                wav_file.write_all(&wav).await?;
                wav.clear();
            }

            let lame = lame.clone();
            if let Ok(buf) = nebula_task::spawn_task(move || -> Result<Vec<u8>> {
                let mut buffer = [0; 5000];
                let n = lame.lock().encode(&pcm, &pcm, &mut buffer)?;
                Ok(buffer[..n].to_vec())
            })
            .await?
            {
                mp3.extend_from_slice(&buf);
                if mp3.len() > chunk {
                    mp3_file.write_all(&mp3).await?;
                    mp3.clear();
                }
            }
        }

        if !wav.is_empty() {
            wav_file.write_all(&wav).await?;
        }

        let size = samples * 2;
        wav_file.seek(SeekFrom::Start(4)).await?;
        wav_file.write_u32_le(size + 36).await?;
        wav_file.seek(SeekFrom::Start(40)).await?;
        wav_file.write_u32_le(size).await?;

        if !mp3.is_empty() {
            mp3_file.write_all(&mp3).await?;
        }

        Ok(())
    }
}

pub struct StreamRecording {
    inbound_sender: UnboundedSender<Vec<i16>>,
    outbound_sender: UnboundedSender<Vec<i16>>,
    inbound_resampler: Option<Arc<parking_lot::Mutex<Resampler>>>,
    outbound_resampler: Option<Arc<parking_lot::Mutex<Resampler>>>,
    rate: u32,
    paused: bool,
}

impl StreamRecording {
    /// Start dual-channel stream recording (inbound/outbound). Sets up metadata, channels, and optional resamplers.
    pub async fn new(
        uuid: Uuid,
        channel: String,
        cdr: String,
        expires: usize,
        rate: u32,
        ptime: usize,
        recording_rate: u32,
        recording_runtime: &Runtime,
        store_info: bool,
        stream_sender: UnboundedSender<StreamSenderMessage>,
    ) -> Result<Self> {
        let (inbound_sender, inbound_receiver) = unbounded_channel();
        let (outbound_sender, outbound_receiver) = unbounded_channel();
        recording_runtime.spawn(async move {
            if store_info {
                let key = &format!("nebula:media_stream:{uuid}");
                let _ = REDIS.hsetex(key, "cdr", &cdr).await;
                let _ = REDIS
                    .hsetex(key, "recording_expires", &expires.to_string())
                    .await;
                let _ = REDIS
                    .hsetex(key, "recording_rate", &recording_rate.to_string())
                    .await;
            }

            info!(
                channel = channel,
                cdr = cdr,
                "start to run stream recording"
            );
            let result = Self::run(
                uuid,
                channel.clone(),
                inbound_receiver,
                outbound_receiver,
                &cdr,
                expires,
                recording_rate,
                ptime,
                stream_sender,
            )
            .await;
            info!(
                channel = channel,
                cdr = cdr,
                "finish to run stream recording {:?}",
                result
            );
        });

        let (inbound_resampler, outbound_resampler) = if rate != recording_rate {
            (
                Some(Arc::new(parking_lot::Mutex::new(
                    Resampler::new(rate, recording_rate).await?,
                ))),
                Some(Arc::new(parking_lot::Mutex::new(
                    Resampler::new(rate, recording_rate).await?,
                ))),
            )
        } else {
            (None, None)
        };

        Ok(Self {
            inbound_sender,
            outbound_sender,
            rate: recording_rate,
            inbound_resampler,
            outbound_resampler,
            paused: false,
        })
    }

    /// Recording worker loop. Each ptime:
    /// - pull inbound/outbound PCM (fill silence if empty), mix to mono and encode to MP3 (also stereo MP3)
    /// - append to local files and to resumable-upload buffers; periodically flush chunks
    /// On stop: if channel not hung up, persist buffers to Redis to resume later; else finalize both uploads, delete temp files, and notify switchboard.
    async fn run(
        uuid: Uuid,
        channel: String,
        mut inbound_receiver: UnboundedReceiver<Vec<i16>>,
        mut outbound_receiver: UnboundedReceiver<Vec<i16>>,
        cdr: &str,
        expires: usize,
        rate: u32,
        ptime: usize,
        stream_sender: UnboundedSender<StreamSenderMessage>,
    ) -> Result<()> {
        let key = format!("nebula:media_stream:{}", &uuid.to_string());
        let recording_url = {
            REDIS
                .hget(&key, "recording_url")
                .await
                .unwrap_or("".to_string())
        };
        let path = format!("/var/lib/nebula/recordings/{}.mp3", cdr);
        let stereo_path = format!("/var/lib/nebula/recordings/{}-stereo.mp3", cdr);
        let (mut offset, recording_url) = if recording_url.is_empty() {
            let url = MEDIA_SERVICE
                .storage_client
                .new_resumable_upload(
                    &format!("aura-call-recordings-{}", expires),
                    &format!("{}.mp3", cdr),
                )
                .await?;
            REDIS.hsetex(&key, "recording_url", &url).await?;
            (0, url)
        } else {
            (
                MEDIA_SERVICE
                    .storage_client
                    .check_resumable_upload(&recording_url)
                    .await?,
                recording_url,
            )
        };

        let stereo_recording_url = {
            REDIS
                .hget(&key, "stereo_recording_url")
                .await
                .unwrap_or("".to_string())
        };
        let (mut stereo_offset, stereo_recording_url) =
            if stereo_recording_url.is_empty() {
                let url = MEDIA_SERVICE
                    .storage_client
                    .new_resumable_upload(
                        &format!("aura-call-recordings-{}", expires),
                        &format!("{}-stereo.mp3", cdr),
                    )
                    .await?;
                let _ = REDIS.hsetex(&key, "stereo_recording_url", &url).await;
                (0, url)
            } else {
                (
                    MEDIA_SERVICE
                        .storage_client
                        .check_resumable_upload(&stereo_recording_url)
                        .await?,
                    stereo_recording_url,
                )
            };

        let lame = new_lame(rate, 1).await?;
        let stereo_lame = new_lame(rate, 2).await?;

        let pcm_len = (rate / 1000) as usize * ptime;
        let chunk = 256 * 1024;
        let mut buffer = {
            REDIS
                .hget::<String>(&key, "recording_buffer")
                .await
                .ok()
                .and_then(|s| base64::decode(s).ok())
                .unwrap_or(Vec::new())
        };
        let mut stereo_buffer = {
            REDIS
                .hget::<String>(&key, "stereo_recording_buffer")
                .await
                .ok()
                .and_then(|s| base64::decode(s).ok())
                .unwrap_or(Vec::new())
        };
        let mut ticker = tokio::time::interval(Duration::from_millis(ptime as u64));
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .ok();
        let mut stereo_file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stereo_path)
            .await
            .ok();

        loop {
            ticker.tick().await;
            if stream_sender.is_closed() {
                if let Some(file) = file.as_mut() {
                    let _ = file.write_all(&buffer).await;
                }
                if let Some(file) = stereo_file.as_mut() {
                    let _ = file.write_all(&stereo_buffer).await;
                }

                if !is_channel_hangup(&channel).await {
                    info!("not hangup, flush recording {}", cdr);
                    let _ = REDIS
                        .hsetex(&key, "recording_buffer", &base64::encode(&buffer))
                        .await;
                    let _ = REDIS
                        .hsetex(
                            &key,
                            "stereo_recording_buffer",
                            &base64::encode(&stereo_buffer),
                        )
                        .await;
                } else {
                    info!(
                        channel = channel,
                        cdr = cdr,
                        stream = uuid.to_string(),
                        "hangup, finish recording upload"
                    );
                    MEDIA_SERVICE
                        .storage_client
                        .finish_upload(&recording_url, &buffer, offset)
                        .await?;
                    MEDIA_SERVICE
                        .storage_client
                        .finish_upload(
                            &stereo_recording_url,
                            &stereo_buffer,
                            stereo_offset,
                        )
                        .await?;
                    let _ = tokio::fs::remove_file(path).await;
                    let _ = tokio::fs::remove_file(stereo_path).await;
                    let _ = MEDIA_SERVICE
                        .switchboard_rpc_client
                        .finish_call_recording(
                            &MEDIA_SERVICE.config.switchboard_host,
                            cdr.to_string(),
                        )
                        .await;
                }
                return Ok(());
            }

            let out_pcm = match outbound_receiver.try_recv() {
                Ok(pcm) => pcm,
                Err(_) => vec![0; pcm_len],
            };

            let mut in_pcm = Vec::new();
            loop {
                match inbound_receiver.try_recv() {
                    Ok(mut pcm) => {
                        in_pcm.append(&mut pcm);
                        if in_pcm.len() >= pcm_len {
                            break;
                        }
                    }
                    Err(_) => {
                        if in_pcm.len() < pcm_len {
                            in_pcm.append(&mut vec![0; pcm_len - in_pcm.len()]);
                        }
                        break;
                    }
                }
            }

            let pcm = {
                let in_pcm = in_pcm.clone();
                let out_pcm = out_pcm.clone();
                nebula_task::spawn_task(move || {
                    out_pcm
                        .iter()
                        .enumerate()
                        .map(|(i, v)| mix(*v, *in_pcm.get(i).unwrap_or(&0)))
                        .collect::<Vec<i16>>()
                })
                .await?
            };

            let lame = lame.clone();
            if let Ok(buf) = nebula_task::spawn_task(move || -> Result<Vec<u8>> {
                let mut buffer = [0; 5000];
                let n = lame.lock().encode(&pcm, &pcm, &mut buffer)?;
                Ok(buffer[..n].to_vec())
            })
            .await?
            {
                buffer.extend_from_slice(&buf);
                if buffer.len() > chunk {
                    if let Ok(range) = MEDIA_SERVICE
                        .storage_client
                        .resume_upload(&recording_url, &buffer[..chunk], offset)
                        .await
                    {
                        if let Some(file) = file.as_mut() {
                            let _ =
                                file.write_all(&buffer[..range - offset + 1]).await;
                        }
                        buffer = buffer[range - offset + 1..].to_vec();
                        offset = range + 1;
                    }
                }
            }

            let stereo_lame = stereo_lame.clone();
            if let Ok(buf) = nebula_task::spawn_task(move || -> Result<Vec<u8>> {
                let mut buffer = [0; 5000];
                let n = stereo_lame.lock().encode(&in_pcm, &out_pcm, &mut buffer)?;
                Ok(buffer[..n].to_vec())
            })
            .await?
            {
                stereo_buffer.extend_from_slice(&buf);
                if stereo_buffer.len() > chunk {
                    if let Ok(range) = MEDIA_SERVICE
                        .storage_client
                        .resume_upload(
                            &stereo_recording_url,
                            &stereo_buffer[..chunk],
                            stereo_offset,
                        )
                        .await
                    {
                        if let Some(file) = stereo_file.as_mut() {
                            let _ = file
                                .write_all(
                                    &stereo_buffer[..range - stereo_offset + 1],
                                )
                                .await;
                        }
                        stereo_buffer =
                            stereo_buffer[range - stereo_offset + 1..].to_vec();
                        stereo_offset = range + 1;
                    }
                }
            }
        }
    }
}

pub struct StreamLocalState {
    uuid: Uuid,
    peer_transport: PeerTransport,
    rtp_packet: RtpPacket,
    source_rtp: HashMap<u32, Arc<RtpBuffer>>,
    ringtone: Option<SessionSound>,
    ringtone_playing: bool,
    moh: Option<SessionSound>,
    moh_playing: bool,
    sound: Option<SessionSound>,
    codec: Arc<BlockMutex<Box<dyn Codec>>>,
    inbound_recording_codec: Arc<BlockMutex<Box<dyn Codec>>>,
    rtpmap: Rtpmap,
    ptime: usize,
    ts_len: u32,
    dtmf_rtpmap: Option<Rtpmap>,
    dtmf: Vec<(Option<u32>, Vec<Vec<u8>>)>,
    crypto: Option<CryptoContext>,
    recording: Option<StreamRecording>,
    file_recorder: Option<FileRecorder>,
    fax: Option<Arc<BlockMutex<Fax>>>,
    mode: MediaMode,
    last_mode_change: u64,
    last_had_audio: SystemTime,
    last_played_no_audio_beep: Option<SystemTime>,
    running: bool,
    t38_passthrough: bool,
}

impl StreamLocalState {
    /// Construct per-stream sender state: codec, RTP template, optional recording, and SRTP context.
    pub async fn new(
        uuid: Uuid,
        channel: String,
        peer_transport: PeerTransport,
        rtpmap: Rtpmap,
        ptime: usize,
        dtmf_rtpmap: Option<Rtpmap>,
        crypto: Option<CryptoContext>,
        t38_passthrough: bool,
        recording_runtime: &Runtime,
        stream_sender: UnboundedSender<StreamSenderMessage>,
    ) -> Result<Self> {
        let codec = rtpmap.name.get_codec().await?;

        let mut recording = None;
        let key = format!("nebula:media_stream:{}", &uuid.to_string());
        let cdr = REDIS.hget(&key, "cdr").await.unwrap_or("".to_string());
        let expires = REDIS.hget(&key, "recording_expires").await.unwrap_or(0);
        let rate = rtpmap.get_rate();
        let recording_rate = REDIS
            .hget::<u32>(&key, "recording_rate")
            .await
            .ok()
            .unwrap_or(rate);
        if !cdr.is_empty() {
            tracing::info!(
                channel = channel,
                stream = uuid.to_string(),
                "had recording started before"
            );
            recording = Some(
                StreamRecording::new(
                    uuid,
                    channel,
                    cdr,
                    expires,
                    rate,
                    ptime,
                    recording_rate,
                    recording_runtime,
                    false,
                    stream_sender,
                )
                .await?,
            );
        }

        let mut rtp_packet = RtpPacket::new();
        rtp_packet.set_payload_type(rtpmap.type_number);
        rtp_packet.set_ssrc(uuid.as_fields().0);

        Ok(Self {
            uuid,
            peer_transport,
            rtp_packet,
            source_rtp: HashMap::new(),
            ringtone: None,
            ringtone_playing: false,
            moh: None,
            moh_playing: false,
            sound: None,
            codec: Arc::new(BlockMutex::new(codec)),
            inbound_recording_codec: Arc::new(BlockMutex::new(
                rtpmap.name.get_codec().await?,
            )),
            ts_len: rtpmap.rate / 1000 * ptime as u32,
            rtpmap,
            ptime,
            dtmf_rtpmap,
            dtmf: vec![],
            crypto,
            recording,
            file_recorder: None,
            fax: None,
            running: false,
            t38_passthrough,
            mode: MediaMode::Sendrecv,
            last_had_audio: time::SystemTime::now(),
            last_played_no_audio_beep: None,
            last_mode_change: time::SystemTime::now()
                .duration_since(time::SystemTime::UNIX_EPOCH)?
                .as_secs(),
        })
    }

    /// Advance only the sequence number on a clone, useful for repeated payloads (e.g., DTMF).
    pub fn next_seq_rtp_packet(&self, rtp_packet: &mut RtpPacket) -> RtpPacket {
        rtp_packet.next_seq();
        rtp_packet.clone()
    }

    /// Advance timestamp and sequence for the next media packet at the configured ptime.
    pub fn next_rtp_packet(&mut self) -> RtpPacket {
        self.rtp_packet.next(self.ts_len);
        self.rtp_packet.clone()
    }
}

/// Create a loopback peer for local streams (no network hop).
async fn new_local_peer(
    uuid: Uuid,
    stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
) -> Result<PeerTransport> {
    let peer: String = REDIS
        .hget(
            &format!("nebula:media_stream:{}", &uuid.to_string()),
            "peer",
        )
        .await?;
    let peer_uuid = Uuid::from_str(&peer)?;
    let channel: String = REDIS
        .hget(&format!("nebula:media_stream:{}", &peer), "channel")
        .await?;
    let local_ip = Ipv4Addr::from_str(&MEDIA_SERVICE.local_ip)?;
    let (sender, mut receiver) = unbounded_channel();

    tokio::spawn(async move {
        let (mut stream_sender, mut stream_receiver) = unbounded_channel();
        let local_stream_sender = stream_sender.clone();
        let local_channel = channel.clone();
        let local_stream_sender_pool = stream_sender_pool.clone();
        tokio::spawn(async move {
            let _ = MediaStreamReceiver::process_packets(
                peer_uuid,
                local_channel,
                local_ip,
                LOCAL_RTP_PEER_PORT,
                false,
                local_stream_sender,
                stream_receiver,
                local_stream_sender_pool,
            )
            .await;
        });

        loop {
            if let Some(msg) = receiver.recv().await {
                if let Err(_e) = stream_sender.send(msg) {
                    let sender_receiver = unbounded_channel();
                    stream_sender = sender_receiver.0;
                    stream_receiver = sender_receiver.1;
                    let local_stream_sender = stream_sender.clone();
                    let local_channel = channel.clone();
                    let local_stream_sender_pool = stream_sender_pool.clone();
                    tokio::spawn(async move {
                        let _ = MediaStreamReceiver::process_packets(
                            peer_uuid,
                            local_channel,
                            local_ip,
                            LOCAL_RTP_PEER_PORT,
                            false,
                            local_stream_sender,
                            stream_receiver,
                            local_stream_sender_pool,
                        )
                        .await;
                    });
                }
            } else {
                return;
            }
        }
    });
    Ok(PeerTransport::LocalPeer(sender))
}

/// Build the right transport for a stream: local loopback, peer-conn UDP, or raw socket.
async fn new_peer_transport(
    uuid: Uuid,
    pc_udp: Arc<UdpSocket>,
    stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
) -> Result<PeerTransport> {
    let local_media = get_media(MediaDescKind::Local, &uuid.to_string())
        .await
        .ok_or(anyhow!("no local media"))?;
    let remote_media = get_media(MediaDescKind::Remote, &uuid.to_string())
        .await
        .ok_or(anyhow!("no reomte media"))?;

    let peer_transport = if remote_media.port == LOCAL_RTP_PEER_PORT {
        new_local_peer(uuid, stream_sender_pool).await?
    } else if local_media.port == PEER_CONNECTION_PORT {
        let peer_addr = get_peer_addr(&uuid.to_string()).await?;
        PeerTransport::PeerConnection(pc_udp, peer_addr)
    } else {
        let peer_addr = get_peer_addr(&uuid.to_string()).await?;
        let peer_ip = peer_addr.ip();
        let peer_ip = match peer_ip {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_ip) => Err(anyhow!("ipv6 not supported"))?,
        };
        let peer_port = peer_addr.port();
        let mut ip_packet = Ipv4Packet::new();
        ip_packet.set_source_ip(MEDIA_SERVICE.config.media_ip);
        ip_packet.set_source_port(local_media.port);
        ip_packet.set_destination_ip(peer_ip);
        ip_packet.set_destination_port(peer_port);
        let addr = SockAddr::new_inet(InetAddr::from_std(&peer_addr));
        PeerTransport::SessionSocket(
            Arc::new(RawSocket::new(false)?),
            ip_packet,
            addr,
        )
    };
    Ok(peer_transport)
}

pub struct AudioStreamSenderLocal {
    uuid: Uuid,
    channel: String,
    is_user: bool,
    rtpmap: Rtpmap,
    dtmf_rtpmap: Option<Rtpmap>,
    ptime: usize,
    state: StreamLocalState,
    sender: UnboundedSender<StreamSenderMessage>,
    stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    webrtc: bool,
    recording_runtime: Arc<Runtime>,
    timer_runtime: Arc<Runtime>,
}

impl AudioStreamSenderLocal {
    pub async fn new(
        uuid: Uuid,
        rtpmap: Rtpmap,
        dtmf_rtpmap: Option<Rtpmap>,
        ptime: usize,
        crypto: Option<CryptoContext>,
        webrtc: bool,
        peer_transport: PeerTransport,
        sender: UnboundedSender<StreamSenderMessage>,
        timer_runtime: Arc<Runtime>,
        t38_passthrough: bool,
        recording_runtime: Arc<Runtime>,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<Self> {
        let channel: String = REDIS
            .hget(
                &format!("nebula:media_stream:{}", &uuid.to_string()),
                "channel",
            )
            .await?;
        let is_user = REDIS
            .hget(
                &format!("nebula:media_stream:{}", &uuid.to_string()),
                "is_user",
            )
            .await
            .unwrap_or("no".to_string())
            == "yes";
        let expect_ack = REDIS
            .hget(&format!("nebula:channel:{channel}"), "expect_ack")
            .await
            .unwrap_or("no".to_string())
            == "yes";

        let mut stream = Self {
            uuid,
            channel: channel.clone(),
            is_user,
            webrtc,
            dtmf_rtpmap: dtmf_rtpmap.clone(),
            state: StreamLocalState::new(
                uuid,
                channel,
                peer_transport,
                rtpmap.clone(),
                ptime,
                dtmf_rtpmap,
                crypto,
                t38_passthrough,
                &recording_runtime,
                sender.clone(),
            )
            .await?,
            rtpmap,
            ptime,
            sender,
            stream_sender_pool,
            timer_runtime,
            recording_runtime,
        };

        stream.check_rtp_seq_and_timestamp().await;

        if !expect_ack {
            stream.start_run();
        }

        Ok(stream)
    }

    /// Audio sender inbound handling: either pass through T.38 to peer transport,
    /// or buffer/match sources by SSRC/PT and feed the jitter buffer for local mixing/encoding.
    async fn receive_rtp(
        &mut self,
        source_id: Uuid,
        rtp_packet: Vec<u8>,
    ) -> Result<()> {
        let ssrc = RtpPacket::get_ssrc(&rtp_packet);
        let rtp_buffer = {
            let pt = RtpPacket::get_payload_type(&rtp_packet);

            let rtp_buffer = {
                if let Some(rtp_buffer) = self.state.source_rtp.get(&ssrc) {
                    if rtp_buffer.remote_rtpmap.type_number == pt {
                        Some(rtp_buffer.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            match rtp_buffer {
                Some(rtp_buffer) => rtp_buffer,
                None => {
                    let pt = pt.to_string();
                    let remote_rtpmap = if let Some(rtpmap) = Rtpmap::pt_default(&pt)
                    {
                        rtpmap
                    } else {
                        let remote_media =
                            get_media(MediaDescKind::Remote, &source_id.to_string())
                                .await
                                .ok_or_else(|| anyhow!("no remote media"))?;
                        remote_media.pt_rtpmap(&pt).ok_or_else(|| {
                            anyhow!(
                                "no matching rtpmap {} {} {}",
                                source_id.to_string(),
                                pt,
                                remote_media
                            )
                        })?
                    };
                    let rtp_buffer = Arc::new(
                        RtpBuffer::new(
                            ssrc,
                            remote_rtpmap,
                            self.rtpmap.clone(),
                            self.ptime,
                        )
                        .await?,
                    );
                    {
                        self.state.source_rtp.insert(ssrc, rtp_buffer.clone());
                    }
                    rtp_buffer
                }
            }
        };
        rtp_buffer.add(RtpPacket::from_vec(rtp_packet)).await?;
        Ok(())
    }

    async fn process_rpc_msg(&mut self, rpc_msg: RpcMessage) -> Result<()> {
        tracing::info!(
            channel = self.channel,
            stream = self.uuid.to_string(),
            "AudioStreamSenderLocal receive rpc msg {rpc_msg:?}"
        );
        match &rpc_msg.method {
            RpcMethod::SetPeerAddr => {
                let peer: StreamPeer = serde_json::from_value(rpc_msg.params)?;
                self.state
                    .peer_transport
                    .set_peer_addr(peer.peer_ip, peer.peer_port);
            }
            RpcMethod::SetT38Passthrough => {
                self.state.t38_passthrough = true;
            }
            RpcMethod::SendDtmf => {
                let dtmf: RpcDtmf = serde_json::from_value(rpc_msg.params)?;
                let tone = Tone::from_char(
                    dtmf.dtmf
                        .chars()
                        .next()
                        .ok_or_else(|| anyhow!("don't have char"))?,
                )
                .ok_or_else(|| anyhow!("not valida dtmf"))?;
                let (low, high) = tone.frequency();

                if self.dtmf_rtpmap.is_none() {
                    let sound = SessionSound::new(
                        nebula_utils::uuid(),
                        "".to_string(),
                        vec![format!("tone://50,100,{low},{high}")],
                        false,
                        1,
                        true,
                        self.rtpmap.get_rate(),
                        self.ptime,
                    );
                    self.state.sound = Some(sound);
                    return Ok(());
                }

                let mut dtmf_payloads = Vec::new();

                let mut dtmf_payload = Vec::new();
                let id = dtmf_byte(&dtmf.dtmf).ok_or(anyhow!("can't send dtmf"))?;
                dtmf_payload.push(id);
                let volume = 10;
                dtmf_payload.push(volume);
                let duration =
                    { (self.state.rtpmap.rate / 1000 * self.ptime as u32) as u16 };
                dtmf_payload.push((duration >> 8) as u8);
                dtmf_payload.push(duration as u8);

                dtmf_payloads.push(dtmf_payload.clone());

                for i in 2..6 {
                    let mut dtmf_payload = dtmf_payload.clone();
                    let duration = duration * i;
                    dtmf_payload[2] = (duration >> 8) as u8;
                    dtmf_payload[3] = duration as u8;
                    dtmf_payloads.insert(0, dtmf_payload);
                }
                dtmf_payloads[0][1] = volume + 128;

                self.state.dtmf.push((None, dtmf_payloads));
            }
            RpcMethod::DtlsDone => {
                if self.webrtc {
                    if self.state.crypto.is_none() {
                        self.state.crypto =
                            get_crypto(CryptoKind::Local, &self.uuid.to_string())
                                .await;
                    }
                    self.start_run();
                }
            }
            RpcMethod::RecordToFile => {
                let recorder: RpcFileRecorder =
                    serde_json::from_value(rpc_msg.params)?;
                self.record_to_file(recorder.uuid).await?;
            }
            RpcMethod::StopRecordToFile => {
                let recorder: RpcFileRecorder =
                    serde_json::from_value(rpc_msg.params)?;
                self.stop_record_to_file(recorder.uuid).await?;
            }
            RpcMethod::UploadRecordToFile => {
                let recorder: RpcFileRecorder =
                    serde_json::from_value(rpc_msg.params)?;
                self.upload_record_to_file(recorder.uuid).await;
            }
            RpcMethod::StreamStartSend => {
                self.start_run();
            }
            RpcMethod::StartRecording => {
                info!(
                    channel = self.channel,
                    stream = self.uuid.to_string(),
                    "process start recording"
                );
                let recording: RpcRecording =
                    serde_json::from_value(rpc_msg.params)?;
                self.start_recording(recording.cdr, recording.expires)
                    .await?;
            }
            RpcMethod::PauseRecording => {
                self.pause_recording().await?;
            }
            RpcMethod::ResumeRecording => {
                self.resume_recording().await?;
            }
            RpcMethod::PlayRingtone => {
                info!(
                    channel = self.channel,
                    stream = self.uuid.to_string(),
                    "process play ringtone"
                );
                let ringtone: RpcRingtone = serde_json::from_value(rpc_msg.params)?;
                self.state.ringtone_playing = true;
                if let Some(current_ringtone) = self.state.ringtone.as_ref() {
                    if current_ringtone.id == ringtone.ringtone {
                        return Ok(());
                    }
                }
                self.state.ringtone = Some(SessionSound::new(
                    ringtone.ringtone,
                    ringtone.channel_id,
                    ringtone.sounds,
                    ringtone.random,
                    0,
                    true,
                    self.rtpmap.get_rate(),
                    self.ptime,
                ));
            }
            RpcMethod::StopRingtone => {
                info!(
                    channel = self.channel,
                    stream = self.uuid.to_string(),
                    "process stop ringtone"
                );
                self.state.ringtone_playing = false;
            }
            RpcMethod::PlayMoh => {
                let moh: RpcMoh = serde_json::from_value(rpc_msg.params)?;
                let channel_id = moh.channel_id;
                let moh = moh.moh;
                self.state.moh_playing = true;
                if let Some(current_moh) = self.state.moh.as_ref() {
                    if current_moh.id == moh.play_id {
                        return Ok(());
                    }
                }
                self.state.moh = Some(SessionSound::new(
                    moh.play_id,
                    channel_id,
                    moh.sounds,
                    moh.random,
                    0,
                    true,
                    self.state.rtpmap.get_rate(),
                    self.state.ptime,
                ));
            }
            RpcMethod::StopMoh => {
                self.state.moh_playing = false;
            }
            RpcMethod::InitT38 => {
                if let Some(fax) = self.state.fax.clone() {
                    let params: InitT38Params =
                        serde_json::from_value(rpc_msg.params)?;
                    if let Some(receiver) =
                        nebula_task::spawn_task(move || fax.lock().init_t38(params))
                            .await?
                    {
                        let peer_transport = self.state.peer_transport.clone();
                        tokio::spawn(async move {
                            loop {
                                match receiver.recv().await {
                                    Ok(msg) => {
                                        let _ = peer_transport.send(&msg).await;
                                    }
                                    Err(_) => return,
                                }
                            }
                        });
                    }
                }
            }
            RpcMethod::SetFax => {
                let channel_id = self.channel.clone();
                self.state.fax = nebula_task::spawn_task(move || {
                    Some(Fax::new(channel_id, false))
                })
                .await?;

                info!(
                    channel = self.channel,
                    stream = self.uuid.to_string(),
                    "set fax for {}",
                    self.channel
                );
            }
            RpcMethod::PlaySound => {
                let sound: RpcSound = serde_json::from_value(rpc_msg.params)?;
                let sound = SessionSound::new(
                    sound.id,
                    sound.channel_id,
                    sound.sounds,
                    sound.random,
                    sound.repeat,
                    sound.block,
                    self.rtpmap.get_rate(),
                    self.ptime,
                );
                self.state.sound = Some(sound);
                // force moh and ringtone to stop if we start to play sound
                self.state.moh_playing = false;
                self.state.ringtone_playing = false;
            }
            RpcMethod::StopSound => {
                let sound: RpcSound = serde_json::from_value(rpc_msg.params)?;
                let sound_id = sound.id;
                let sound = self.state.sound.as_ref().ok_or(anyhow!("no sound"))?;
                if sound.id == sound_id {
                    self.state.sound = None;
                }
            }
            RpcMethod::SetSessionMediaMode => {
                let req: SetMediaModeReq = serde_json::from_value(rpc_msg.params)?;
                let mode = MediaMode::from_str(&req.mode)?;
                self.state.mode = mode;
                self.state.last_mode_change = time::SystemTime::now()
                    .duration_since(time::SystemTime::UNIX_EPOCH)?
                    .as_secs();
            }
            _ => (),
        };
        Ok(())
    }

    async fn pause_recording(&mut self) -> Result<()> {
        if let Some(recording) = self.state.recording.as_mut() {
            recording.paused = true;
        }
        Ok(())
    }

    async fn resume_recording(&mut self) -> Result<()> {
        if let Some(recording) = self.state.recording.as_mut() {
            recording.paused = false;
        }
        Ok(())
    }

    async fn record_to_file(&mut self, uuid: String) -> Result<()> {
        let rate = self.rtpmap.get_rate();
        self.state.file_recorder = Some(FileRecorder::new(
            uuid,
            rate,
            self.ptime,
            self.sender.clone(),
        ));
        Ok(())
    }

    async fn stop_record_to_file(&mut self, uuid: String) -> Result<()> {
        if let Some(file_recorder) = self.state.file_recorder.as_ref() {
            if file_recorder.uuid == uuid {
                self.state.file_recorder = None;
            }
        }
        Ok(())
    }

    async fn upload_record_to_file(&self, uuid: String) {
        tokio::spawn(async move {
            let _ = FileRecorder::upload(uuid).await;
        });
    }

    async fn start_recording(&mut self, cdr: String, expires: usize) -> Result<()> {
        if self.state.recording.is_some() {
            return Ok(());
        }

        let rate = self.rtpmap.get_rate();
        self.state.recording = Some(
            StreamRecording::new(
                self.uuid,
                self.channel.clone(),
                cdr,
                expires,
                rate,
                self.ptime,
                rate,
                &self.recording_runtime,
                true,
                self.sender.clone(),
            )
            .await?,
        );

        Ok(())
    }

    /// Encode PCM to codec payload on a priority worker; keeps the send loop snappy.
    async fn encode(&self, pcm: Vec<i16>) -> Result<Vec<u8>> {
        let codec = self.state.codec.clone();
        let payload =
            nebula_task::spawn_priority_task(move || -> Result<Vec<u8>> {
                let mut buf = [0; 5000];
                let n = codec.lock().encode(&pcm, &mut buf)?;
                Ok(buf[..n].to_vec())
            })
            .await??;
        Ok(payload)
    }

    /// Decode codec payload to PCM on a worker thread to avoid blocking.
    async fn decode(&self, payload: Vec<u8>) -> Result<Vec<i16>> {
        let codec = self.state.codec.clone();
        let pcm = nebula_task::spawn_task(move || -> Result<Vec<i16>> {
            let mut buf = [0; 5000];
            let n = codec.lock().decode(&payload, &mut buf)?;
            Ok(buf[..n].to_vec())
        })
        .await??;
        Ok(pcm)
    }

    /// Pop one frame per source, mix PCM (or silence); handle fax (PCM/T.38). Returns (payload?, pcm, silent, t38).
    async fn get_src_rtp(&self) -> (Option<Vec<u8>>, Vec<i16>, bool, bool) {
        let mut task_set = JoinSet::new();
        for src_rtp in self.state.source_rtp.values() {
            let src_rtp = src_rtp.clone();
            task_set.spawn(async move { src_rtp.pop().await });
        }
        let mut sources = Vec::new();
        while let Some(result) = task_set.join_next().await {
            if let Ok((payload, pcm)) = result {
                let Some(pcm) = pcm else {
                    continue;
                };
                sources.push((payload, pcm, false, false))
            }
        }

        let rate = self.rtpmap.get_rate();
        let pcm_len = (rate / 1000) as usize * self.ptime;

        {
            let fax = self.state.fax.clone();
            if let Some(fax) = fax {
                let local_fax = fax.clone();
                let is_t38 =
                    nebula_task::spawn_task(move || local_fax.lock().is_t38())
                        .await
                        .unwrap_or(false);
                let mut pcm = sources.first().map(|(_, pcm, _, _)| pcm.clone());
                if !is_t38 {
                    let pcm = nebula_task::spawn_task(move || {
                        fax.lock().process_data(pcm.as_mut(), pcm_len)
                    })
                    .await
                    .unwrap_or_else(|_| vec![0; pcm_len]);
                    return (None, pcm, false, false);
                } else {
                    let _ = nebula_task::spawn_task(move || {
                        fax.lock().t38_send();
                    })
                    .await;
                    return (
                        None,
                        pcm.unwrap_or_else(|| vec![0; pcm_len]),
                        false,
                        true,
                    );
                }
            }
        }

        if sources.is_empty() {
            (
                None,
                vec![0; pcm_len],
                // {
                //     let noise = self.state.noise.clone();
                //     nebula_task::spawn_task(move || -> Vec<i16> {
                //         let mut noise = noise.lock();
                //         (0..pcm_len).into_iter().map(|_| noise.generate()).collect()
                //     })
                //     .await
                //     .unwrap()
                // },
                !self.state.source_rtp.is_empty(),
                false,
            )
        } else if sources.len() == 1 {
            sources[0].clone()
        } else {
            let pcm = nebula_task::spawn_priority_task(move || -> Vec<i16> {
                sources.iter().fold(
                    sources[0].1.clone(),
                    |acc, (_payload, pcm, _silent, _t38)| {
                        pcm.iter()
                            .enumerate()
                            .map(|(i, v)| mix(acc[i], *v))
                            .collect::<Vec<i16>>()
                    },
                )
            })
            .await
            .unwrap();
            (None, pcm, false, false)
        }
    }

    /// Build next outbound payload/PCM for audio: pull sources (or silence), overlay ringtone/MOH/sound
    /// (blocking when configured), encode if needed, and indicate T.38 passthrough.
    async fn get_rtp_out(&mut self) -> Result<(Vec<u8>, Vec<i16>, bool)> {
        let src_rtp = self.get_src_rtp().await;

        let sound = {
            let mut set_sound_to_none = false;

            let sound = {
                if self.state.ringtone_playing {
                    if let Some(ringtone) = self.state.ringtone.as_ref() {
                        ringtone.receiver.try_recv().ok().map(|pcm| (true, pcm))
                    } else {
                        None
                    }
                } else if self.state.moh_playing {
                    if let Some(moh) = self.state.moh.as_ref() {
                        moh.receiver.try_recv().ok().map(|pcm| (true, pcm))
                    } else {
                        None
                    }
                } else if let Some(sound) = self.state.sound.as_ref() {
                    match sound.receiver.try_recv() {
                        Ok(pcm) => Some((sound.block, pcm)),
                        Err(async_channel::TryRecvError::Empty) => None,
                        Err(async_channel::TryRecvError::Closed) => {
                            set_sound_to_none = true;
                            None
                        }
                    }
                } else {
                    None
                }
            };

            if set_sound_to_none {
                self.state.sound = None;
            }

            // if self.is_user {
            //     let silent = src_rtp.2;
            //     if state.moh_playing || sound.is_some() || !silent {
            //         state.last_had_audio = SystemTime::now();
            //         if !silent {
            //             if state.last_played_no_audio_beep.is_some() {
            //                 state.last_played_no_audio_beep = None;
            //             }
            //         }
            //     } else {
            //         if let Ok(duration) = state.last_had_audio.elapsed() {
            //             let mut play_no_audio_beep = false;
            //             if duration.as_millis() > 1000 {
            //                 play_no_audio_beep = true;
            //             }
            //             if let Some(t) = state.last_played_no_audio_beep.as_ref() {
            //                 if let Ok(d) = t.elapsed() {
            //                     if d.as_millis() < 5000 {
            //                         play_no_audio_beep = false;
            //                     }
            //                 }
            //             }
            //             if play_no_audio_beep {
            //                 state.last_played_no_audio_beep =
            //                     Some(SystemTime::now());
            //                 state.sound = Some(SessionSound::new(
            //                     utils::uuid(),
            //                     "".to_string(),
            //                     vec![
            //                         "tone://50,200,300,400".to_string(),
            //                         "tone://50,200,300,400".to_string(),
            //                     ],
            //                     false,
            //                     1,
            //                     false,
            //                     self.rtpmap.get_rate(),
            //                 ));
            //             }
            //         }
            //     }
            // }

            sound
        };

        let sound_pcm = match sound {
            None => None,
            Some((block, pcm)) => {
                if block {
                    let payload = self.encode(pcm.clone()).await?;
                    return Ok((payload, pcm, false));
                } else {
                    Some(pcm)
                }
            }
        };

        let (payload, pcm, _silent, t38) = src_rtp;

        let (payload, pcm, t38) = if let Some(sound_pcm) = sound_pcm {
            let pcm = nebula_task::spawn_priority_task(move || {
                pcm.iter()
                    .enumerate()
                    .map(|(i, v)| mix(*sound_pcm.get(i).unwrap_or(&0), *v))
                    .collect::<Vec<i16>>()
            })
            .await?;

            (self.encode(pcm.clone()).await?, pcm, false)
        } else if let Some(payload) = payload {
            (payload, pcm, t38)
        } else {
            (self.encode(pcm.clone()).await?, pcm, t38)
        };
        Ok((payload, pcm, t38))
    }

    /// Align sequence and timestamp with persisted start so mid-call restarts stay seamless.
    async fn check_rtp_seq_and_timestamp(&mut self) {
        let key = &format!("nebula:media_stream:{}", &self.uuid.to_string());

        let rtp_start_field = "rtp_start_time";
        let rtp_start: Result<u128, _> = REDIS.hget(key, rtp_start_field).await;
        let rtp_start = match rtp_start {
            Ok(t) => t,
            Err(_) => {
                let now = SystemTime::now()
                    .duration_since(time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let _ = REDIS.hsetex(key, rtp_start_field, &now.to_string()).await;
                now
            }
        };

        let first_seq_field = "first_rtp_seq";
        let first_seq: Result<u16, _> = REDIS.hget(key, first_seq_field).await;
        match first_seq {
            Ok(seq) => {
                let now = SystemTime::now()
                    .duration_since(time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let duration = now.saturating_sub(rtp_start);
                let numbers = (duration / self.ptime as u128) as u64;
                let seq = ((seq as u64) + numbers) as u16;
                self.state.rtp_packet.set_sequence(seq);
            }
            Err(_) => {
                let seq = self.state.rtp_packet.sequence();
                let _ = REDIS.hsetex(key, first_seq_field, &seq.to_string()).await;
            }
        }

        let first_timestamp_field = "first_rtp_timestamp";
        let first_timestamp: Result<u32, _> =
            REDIS.hget(key, first_timestamp_field).await;
        match first_timestamp {
            Ok(timestamp) => {
                let now = SystemTime::now()
                    .duration_since(time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let duration = now.saturating_sub(rtp_start);
                let numbers = ((duration / self.ptime as u128)
                    * (self.rtpmap.rate / 1000 * self.ptime as u32) as u128)
                    as u64;
                let timestamp = ((timestamp as u64) + numbers) as u32;
                self.state.rtp_packet.set_timestamp(timestamp);
            }
            Err(_) => {
                let timestamp = self.state.rtp_packet.timestamp();
                let _ = REDIS
                    .hsetex(key, first_timestamp_field, &timestamp.to_string())
                    .await;
            }
        }
    }

    /// check the running status and webrtc status
    /// and start to run the rtp sending event loop
    /// Kick off the periodic sender task; waits for DTLS on WebRTC before running.
    fn start_run(&mut self) {
        if self.state.running {
            return;
        }
        if self.webrtc && self.state.crypto.is_none() {
            return;
        }
        self.state.running = true;

        info!(
            channel = self.channel,
            stream = self.uuid.to_string(),
            "start to run stream sender {}",
            self.channel
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let ptime = self.ptime;
        let sender = self.sender.clone();
        let channel = self.channel.clone();
        let uuid = self.uuid;
        let mut empty_rtps = 0;

        // send a dumpy RtpOut to rtp_out_tx
        // the reason for this:
        // the ticker on the rtp out sending loop will be executed immediately
        // so there's no time for the PrepareRtpOut to be done
        // and we always get a emtpy out rtp.
        // so what we do here is to "send" a dummy RtpOut
        // to prevent the "got empty out rtp" error
        let _ = tx.try_send(RtpOut {
            rtp_packets: vec![],
            t38: false,
            peer_transport: self.state.peer_transport.clone(),
            mode: MediaMode::Sendrecv,
        });

        self.timer_runtime.spawn(async move {
            let mut ticker =
                nebula_timer::Timer::new(Duration::from_millis(ptime as u64))
                    .unwrap();
            loop {
                ticker.tick().await;
                if sender.is_closed() {
                    info!(
                        channel = channel,
                        stream = uuid.to_string(),
                        "stream sending loop ends now",
                    );
                    return;
                }

                let rtp_out = rx.try_recv();

                match tx.clone().try_reserve_owned() {
                    Ok(tx) => {
                        // we send the PrepareRtpPacket so that the AudioStreamSender main loop
                        // can start to prepare for the next rtp packet.
                        let _ =
                            sender.send(StreamSenderMessage::PrepareRtpPacket(tx));
                    }
                    Err(e) => {
                        tracing::error!(
                            channel = channel,
                            stream = uuid.to_string(),
                            "rtp_out_tx reserve error: {e:?}"
                        );
                    }
                }

                match rtp_out {
                    Ok(rtp_out) => {
                        tokio::spawn(async move {
                            let _ = rtp_out.send().await;
                        });
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        empty_rtps += 1;
                        tracing::error!(
                            channel = channel,
                            stream = uuid.to_string(),
                            "sender got empty out rtp: {empty_rtps}"
                        );
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        info!(
                            channel = channel,
                            stream = uuid.to_string(),
                            "stream sending loop ends now",
                        );
                        return;
                    }
                }
            }
        });
    }

    /// run the rtp sending event loop
    async fn run(&self) {
        let mut rtp_packet = RtpPacket::new();
        rtp_packet.set_payload_type(self.rtpmap.type_number);
        rtp_packet.set_ssrc(self.uuid.as_fields().0);
        // self.check_rtp_seq_and_timestamp(&mut rtp_packet).await;

        // loop {
        //     if self.sender.is_closed() {
        //         info!(
        //             channel = self.channel,
        //             stream = self.uuid.to_string(),
        //             "stream sender stopped {}",
        //             self.channel
        //         );
        //         return;
        //     }
        //     {
        //         if self.state.read().await.t38_passthrough {
        //             return;
        //         }
        //     }
        //     let _ = self.prepare_rtp_packets().await;
        // }
    }

    /// Prepare the next batch of RTP packets (or a no-op when not sending).
    async fn prepare_rtp_packets(&mut self, tx: OwnedPermit<RtpOut>) -> Result<()> {
        let (payload, pcm, t38) = self.get_rtp_out().await?;
        let mode = self.state.mode.clone();
        let peer_transport = self.state.peer_transport.clone();
        if mode != MediaMode::Sendrecv {
            // don't try to assemble the rtp packet so that
            // the sequence etc doesn't get advanced
            tx.send(RtpOut {
                rtp_packets: Vec::new(),
                t38,
                peer_transport,
                mode,
            });
        } else {
            let (rtp_packets, pcm) = self.get_rtp_packets(payload, pcm).await?;
            tx.send(RtpOut {
                rtp_packets,
                t38,
                peer_transport,
                mode,
            });

            if !t38 && self.state.mode == MediaMode::Sendrecv {
                if let Some((paused, sender, resampler)) =
                    self.state.recording.as_ref().map(|r| {
                        (
                            r.paused,
                            r.outbound_sender.clone(),
                            r.outbound_resampler.clone(),
                        )
                    })
                {
                    if !paused {
                        self.recording_runtime.spawn(async move {
                            let pcm = if let Some(resampler) = resampler {
                                nebula_task::spawn_task(move || {
                                    resampler.lock().convert(&pcm)
                                })
                                .await?
                            } else {
                                pcm
                            };
                            sender.send(pcm)?;
                            Ok::<(), anyhow::Error>(())
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Turn a payload (or DTMF) into one or more RTP packets, handling marker and SRTP.
    async fn get_rtp_packets(
        &mut self,
        payload: Vec<u8>,
        pcm: Vec<i16>,
    ) -> Result<(Vec<RtpPacket>, Vec<i16>)> {
        let rtp_packets = {
            let mut rtp_packet = self.state.next_rtp_packet();

            if let Some((first, ts, dtmf_payload)) = self
                .get_sending_dtmf(RtpPacket::get_timestamp(rtp_packet.data()))
                .await
            {
                let dtmf_pt = self
                    .dtmf_rtpmap
                    .as_ref()
                    .map(|r| r.type_number)
                    .unwrap_or(101);
                rtp_packet.set_payload_type(dtmf_pt);
                rtp_packet.set_timestamp(ts);
                rtp_packet.set_paylod(&dtmf_payload);
                if first {
                    rtp_packet.set_marker(true);
                }

                let mut rtp_packets = Vec::new();
                rtp_packets.push(self.encrypt_rtp(rtp_packet.clone()).await?);

                if dtmf_payload[1] > 128 {
                    for _i in 0..2 {
                        let mut rtp_packet =
                            self.state.next_seq_rtp_packet(&mut rtp_packet);
                        rtp_packet.set_payload_type(dtmf_pt);
                        rtp_packet.set_timestamp(ts);
                        rtp_packet.set_paylod(&dtmf_payload);
                        rtp_packets.push(self.encrypt_rtp(rtp_packet).await?);
                    }
                }
                rtp_packets
            } else {
                rtp_packet.set_paylod(&payload);
                vec![self.encrypt_rtp(rtp_packet).await?]
            }
        };
        Ok((rtp_packets, pcm))
    }

    /// If sending DTMF, pop the next payload and timestamp; mark first packet and repeat lead frames.
    async fn get_sending_dtmf(&mut self, ts: u32) -> Option<(bool, u32, Vec<u8>)> {
        if self.state.dtmf.is_empty() {
            return None;
        }

        let (dtmf_ts, dtmf_payloads) = self.state.dtmf.iter_mut().next()?;
        let dtmf_payload = dtmf_payloads.pop();
        let mut first = false;
        let ts = match dtmf_ts {
            Some(ts) => *ts,
            None => {
                first = true;
                *dtmf_ts = Some(ts);
                ts
            }
        };
        if dtmf_payloads.is_empty() {
            self.state.dtmf.remove(0);
        }
        Some((first, ts, dtmf_payload?))
    }

    /// Encrypt RTP in place when SRTP is active; otherwise return unchanged.
    async fn encrypt_rtp(&mut self, mut rtp_packet: RtpPacket) -> Result<RtpPacket> {
        if let Some(crypto) = self.state.crypto.as_mut() {
            let roc = crypto.update_roc(rtp_packet.data()).await;
            let crypto = crypto.state.clone();
            nebula_task::spawn_priority_task(move || -> Result<RtpPacket> {
                crypto.encrypt_rtp(roc, &mut rtp_packet)?;
                Ok(rtp_packet)
            })
            .await?
        } else {
            Ok(rtp_packet)
        }
    }

    /// Try to recover a leftover fax TIFF into a PDF and signal completion in Redis.
    async fn check_fax(&self) {
        if self.state.fax.is_some()
            && tokio::fs::try_exists(PathBuf::from(format!(
                "/tmp/{}.tiff",
                self.channel
            )))
            .await
            .unwrap_or(false)
        {
            info!(
                channel = self.channel,
                stream = self.uuid.to_string(),
                "fax still got tiff when hangup, trying to recover",
            );
            let _ = tokio::process::Command::new("tiff2pdf")
                .arg("-o")
                .arg(format!("/tmp/{}.pdf", self.channel))
                .arg(format!("/tmp/{}.tiff", self.channel))
                .output()
                .await;
            if let Ok(buffer) =
                tokio::fs::read(format!("/tmp/{}.pdf", self.channel)).await
            {
                let s = base64::encode(buffer);
                let key = format!("nebula:fax:{}", self.channel);
                let _ = REDIS.set(&key, &s).await;
                let _ = REDIS.expire(&key, 60).await;
                let stream = format!("nebula:channel:{}:stream", self.channel);
                let value = r#"
                                                {
                                                    "code": 0,
                                                    "status": "OK",
                                                    "page": 1
                                                }
                                            "#;
                info!(
                    channel = self.channel,
                    stream = self.uuid.to_string(),
                    "fax {} recovered {value}",
                    self.channel
                );
                let _ = REDIS.xadd::<String>(&stream, "fax_end", value).await;
                let _ = REDIS.expire(&stream, 60).await;

                let _ =
                    tokio::fs::remove_file(format!("/tmp/{}.tiff", self.channel))
                        .await;
                let _ = tokio::fs::remove_file(format!("/tmp/{}.pdf", self.channel))
                    .await;
            }
        }
    }
}

#[derive(Clone)]
pub struct AudioStreamSenderRemote {
    id: Uuid,
    rtpmap: Rtpmap,
    peer_transport: PeerTransport,
    remote_udp: Arc<UdpSocket>,
    recording_udp: Arc<UdpSocket>,
    remote_addr: SocketAddr,
    remote_recording_addr: SocketAddr,
    remote_ip: String,
    t38_passthrough: bool,
}

impl AudioStreamSenderRemote {
    /// Create a remote audio sender bound to a specific IP; used when sending from another host.
    pub fn new(
        id: Uuid,
        rtpmap: Rtpmap,
        peer_transport: PeerTransport,
        remote_udp: Arc<UdpSocket>,
        recording_udp: Arc<UdpSocket>,
        remote_ip: String,
        t38_passthrough: bool,
    ) -> Result<Self> {
        let remote_addr: SocketAddr =
            format!("{}:{}", remote_ip, REMOTE_RTP_PORT).parse()?;
        let remote_recording_addr: SocketAddr =
            format!("{}:{}", remote_ip, RECORDING_PORT).parse()?;
        Ok(Self {
            id,
            rtpmap,
            peer_transport,
            remote_udp,
            recording_udp,
            remote_addr,
            remote_recording_addr,
            remote_ip,
            t38_passthrough,
        })
    }

    /// Forward RPC to the remote audio stream via media RPC.
    async fn process_rpc_msg(&self, rpc_msg: RpcMessage) -> Result<()> {
        MEDIA_SERVICE
            .media_rpc_client
            .send_msg_to_media_stream(&self.id.to_string(), &rpc_msg)
            .await?;
        Ok(())
    }
}

pub enum StreamSender {
    AudioLocal(AudioStreamSenderLocal),
    AudioRemote(AudioStreamSenderRemote),
    VideoLocal(VideoStreamSenderLocal),
    VideoRemote(VideoStreamSenderRemote),
    VideoPeer(VideoStreamDestination),
}

impl StreamSender {
    /// Build a sender for this stream. Chooses local vs remote and audio vs video, negotiates crypto if needed.
    async fn new(
        id: Uuid,
        sender: UnboundedSender<StreamSenderMessage>,
        remote_rtp_udp: Arc<UdpSocket>,
        remote_rtcp_udp: Arc<UdpSocket>,
        recording_udp: Arc<UdpSocket>,
        pc_udp: Arc<UdpSocket>,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
        timer_runtime: Arc<Runtime>,
        monitor: RemoteStreamSenderMonitor,
        recording_runtime: Arc<Runtime>,
    ) -> Result<Self> {
        let uuid = id.clone();
        let id = id.to_string();
        let offer_media = get_media_by_directoin(SdpType::Offer, &id)
            .await
            .ok_or(anyhow!("no local media"))?;
        let answer_media = get_media_by_directoin(SdpType::Answer, &id)
            .await
            .ok_or(anyhow!("no reomte media"))?;
        let rtpmap = offer_media
            .negotiate_rtpmap(&answer_media)
            .ok_or(anyhow!("no rtpmap"))?;
        let ptime = answer_media.ptime();

        if offer_media.media_type == MediaType::Video {
            let key = &format!("nebula:media_stream:{uuid}");
            let room: String = REDIS.hget(key, "room").await.unwrap_or_default();

            if room.is_empty() {
                let channel: String =
                    REDIS.hget(key, "channel").await.unwrap_or_default();
                let peer = VideoStreamDestination::new(
                    uuid,
                    channel,
                    pc_udp,
                    stream_sender_pool,
                )
                .await?;
                return Ok(StreamSender::VideoPeer(peer));
            }

            let send_ip = get_video_send_ip(&id).await;
            if send_ip.is_empty() {
                // For video streams, the reciving stream set the send ip so that
                // the reciving and local sending stream are on the same machine
                // If the send ip is empty, we don't do anything yet
                return Err(anyhow!("haven't received any incoming packets yet"));
            }

            let crypto = if offer_media.is_webrtc() {
                get_crypto(CryptoKind::Local, &id).await
            } else {
                let local_media = get_media(MediaDescKind::Local, &id)
                    .await
                    .ok_or(anyhow!("no local media"))?;
                let remote_media = get_media(MediaDescKind::Remote, &id)
                    .await
                    .ok_or(anyhow!("no reomte media"))?;
                negotiate_crypto(&id, &local_media, &remote_media)
                    .await
                    .map(|r| r.0)
            };

            let webrtc = offer_media.is_webrtc();
            let peer_transport =
                new_peer_transport(uuid, pc_udp, stream_sender_pool.clone()).await?;

            let stream = if send_ip == MEDIA_SERVICE.local_ip {
                let channel: String =
                    REDIS.hget(key, "channel").await.unwrap_or_default();
                let stream = VideoStreamSenderLocal::new(
                    uuid,
                    channel,
                    rtpmap,
                    webrtc,
                    crypto,
                    peer_transport,
                )
                .await;
                StreamSender::VideoLocal(stream)
            } else {
                let stream = VideoStreamSenderRemote::new(
                    uuid,
                    rtpmap,
                    webrtc,
                    crypto.map(Arc::new),
                    peer_transport,
                    send_ip,
                    remote_rtp_udp,
                    remote_rtcp_udp,
                )?;
                StreamSender::VideoRemote(stream)
            };
            return Ok(stream);
        }

        let t38_passthrough = REDIS
            .hget(&format!("nebula:media_stream:{id}"), "t38_passthrough")
            .await
            .unwrap_or_else(|_| "".to_string())
            == "yes";
        info!(
            stream = id.to_string(),
            "{id} create media sender t38 {t38_passthrough}"
        );
        let peer_transport =
            new_peer_transport(uuid, pc_udp, stream_sender_pool.clone()).await?;
        let send_ip = get_send_ip(&id, &monitor).await?;
        let sender = if send_ip == MEDIA_SERVICE.local_ip {
            let crypto = if offer_media.is_webrtc() {
                get_crypto(CryptoKind::Local, &id).await
            } else {
                let local_media = get_media(MediaDescKind::Local, &id)
                    .await
                    .ok_or(anyhow!("no local media"))?;
                let remote_media = get_media(MediaDescKind::Remote, &id)
                    .await
                    .ok_or(anyhow!("no reomte media"))?;
                negotiate_crypto(&id, &local_media, &remote_media)
                    .await
                    .map(|r| r.0)
            };
            let dtmf_rtpmap = answer_media.dtmf_rtpmap(None);

            let stream = AudioStreamSenderLocal::new(
                uuid,
                rtpmap,
                dtmf_rtpmap,
                ptime,
                crypto,
                offer_media.is_webrtc(),
                peer_transport,
                sender,
                timer_runtime,
                t38_passthrough,
                recording_runtime,
                stream_sender_pool,
            )
            .await?;
            StreamSender::AudioLocal(stream)
        } else {
            StreamSender::AudioRemote(AudioStreamSenderRemote::new(
                uuid,
                rtpmap,
                peer_transport,
                remote_rtp_udp,
                recording_udp,
                send_ip,
                t38_passthrough,
            )?)
        };
        Ok(sender)
    }

    // fn is_remote(&self) -> bool {
    //     match &self {
    //         AudioStreamSender::Local(_) => false,
    //         AudioStreamSender::Remote(_) => true,
    //     }
    // }

    async fn stream_process_rpc(
        id: Uuid,
        sender: UnboundedSender<StreamSenderMessage>,
    ) -> Result<()> {
        let key = format!("nebula:media_stream:{id}:stream");
        let mut receiver = RpcServer::new(key, "".to_string()).await;
        loop {
            tokio::select! {
                 _ = sender.closed() => {
                      return Ok(())
                 }
                 result = receiver.recv() => {
                      if let Some((_entry, rpc_msg)) = result {
                          let _ = sender.send(StreamSenderMessage::Rpc(rpc_msg, RpcSource::Sender));
                      }
                 }
            }
        }
    }

    /// Main loop for a single sender. Prioritizes RTP preparation, handles RTP/RTCP/recording, and reacts to RPCs.
    pub async fn process_packets(
        id: Uuid,
        sender_runtime: Arc<Runtime>,
        remote_rtp_udp: Arc<UdpSocket>,
        remote_rtcp_udp: Arc<UdpSocket>,
        recording_udp: Arc<UdpSocket>,
        sender: UnboundedSender<StreamSenderMessage>,
        receiver: UnboundedReceiver<StreamSenderMessage>,
        pc_udp: Arc<UdpSocket>,
        monitor: RemoteStreamSenderMonitor,
        timer_runtime: Arc<Runtime>,
        recording_runtime: Arc<Runtime>,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<()> {
        let local_sender = sender.clone();
        tokio::spawn(async move {
            let _ = Self::stream_process_rpc(id, local_sender).await;
        });

        let mut stream = Self::new(
            id,
            sender.clone(),
            remote_rtp_udp,
            remote_rtcp_udp,
            recording_udp,
            pc_udp,
            stream_sender_pool,
            timer_runtime,
            monitor.clone(),
            recording_runtime,
        )
        .await?;

        if matches!(stream, StreamSender::AudioLocal(_)) {
            // run AudioLocal on it's own tokio runtime
            let _ = sender_runtime
                .spawn(async move {
                    let _ = stream.do_process_packets(id, receiver, monitor).await;
                })
                .await;
            Ok(())
        } else {
            stream.do_process_packets(id, receiver, monitor).await
        }
    }

    /// Per-stream sender loop. Prioritizes PrepareRtpPacket, handles RTP/RTCP/feedback/recording, and exits on stop.
    async fn do_process_packets(
        &mut self,
        id: Uuid,
        mut receiver: UnboundedReceiver<StreamSenderMessage>,
        monitor: RemoteStreamSenderMonitor,
    ) -> Result<()> {
        let id = id.to_string();
        let mut last_check_local = std::time::Instant::now();
        let mut last_timeout_check = std::time::Instant::now();
        let redis_key = format!("nebula:media_stream:{id}");

        // we use a VecDeque to store msgs rather than just process the msg
        // is because we want to bump up the order of processing
        // `StreamSenderMessage::PrepareRtpPacket`.
        // That's the top priority when we deal with audio sending out.
        // The way we do it is to `try_recv` on the channel and if there's PrepareRtpPacket
        // we put it in the front of the queue, so that it will be processed at the next run
        let mut msgs = VecDeque::new();
        loop {
            let result = tokio::select! {
                 _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    if REDIS.exists(&redis_key).await.unwrap_or(false)
                        && !is_channel_hangup(&REDIS.hget::<String>(&redis_key, "channel").await.unwrap_or_default()).await {
                        continue;
                    }
                    return Ok(());
                 }
                 result = receiver.recv() => {
                     result
                 }
            };

            let next_msg = result.ok_or(anyhow!("receiver closed"))?;
            msgs.push_back(next_msg);

            loop {
                while let Ok(m) = receiver.try_recv() {
                    if matches!(m, StreamSenderMessage::PrepareRtpPacket(_)) {
                        msgs.push_front(m);
                    } else {
                        msgs.push_back(m);
                    }
                }

                let Some(msg) = msgs.pop_front() else {
                    break;
                };

                let now = std::time::Instant::now();
                match self {
                    StreamSender::VideoLocal(_) => (),
                    StreamSender::VideoPeer(_) => (),
                    StreamSender::AudioLocal(local) => {
                        if (now - last_check_local).as_millis() > 1000 {
                            last_check_local = now;

                            let channel = local.channel.clone();
                            let redis_key = redis_key.clone();
                            let stream = local.uuid;
                            let sender = local.sender.clone();
                            tokio::spawn(async move {
                                let ip: String = REDIS
                                    .hget(&redis_key, "ip")
                                    .await
                                    .unwrap_or("".to_string());
                                if ip != MEDIA_SERVICE.local_ip {
                                    info!(
                                    channel = channel,
                                    stream = stream.to_string(),
                                    "{channel} local stream sender ip changed from {} to {ip}",
                                    MEDIA_SERVICE.local_ip,
                                );
                                    let _ = sender.send(StreamSenderMessage::Rpc(
                                        RpcMessage {
                                            method: RpcMethod::StopSession,
                                            id: stream.to_string(),
                                            params: serde_json::to_value(
                                                RpcStopSession {
                                                    hangup: true,
                                                    reason: "".to_string(),
                                                },
                                            )
                                            .unwrap_or_default(),
                                        },
                                        RpcSource::Sender,
                                    ));
                                }
                            });
                        }

                        if (now - last_timeout_check).as_secs() > 60 {
                            last_timeout_check = now;

                            let redis_key = redis_key.clone();
                            let stream = local.uuid;
                            let sender = local.sender.clone();
                            tokio::spawn(async move {
                                if !REDIS.exists(&redis_key).await.unwrap_or(false)
                                    || is_channel_hangup(
                                        &REDIS
                                            .hget::<String>(&redis_key, "channel")
                                            .await
                                            .unwrap_or_default(),
                                    )
                                    .await
                                {
                                    let _ = sender.send(StreamSenderMessage::Rpc(
                                        RpcMessage {
                                            method: RpcMethod::StopSession,
                                            id: stream.to_string(),
                                            params: serde_json::to_value(
                                                RpcStopSession {
                                                    hangup: true,
                                                    reason: "".to_string(),
                                                },
                                            )
                                            .unwrap_or_default(),
                                        },
                                        RpcSource::Sender,
                                    ));
                                }
                            });
                        }
                    }
                    StreamSender::AudioRemote(remote) => {
                        if !monitor.is_live(&remote.remote_ip).await {
                            warn!(
                                "detected {} isn't live on {}",
                                remote.remote_ip, MEDIA_SERVICE.local_ip
                            );
                            return Ok(());
                        }
                    }
                    StreamSender::VideoRemote(remote) => {
                        if !monitor.is_live(&remote.remote_ip).await {
                            warn!(
                                "detected {} isn't live on {}",
                                remote.remote_ip, MEDIA_SERVICE.local_ip
                            );
                            return Ok(());
                        }
                    }
                }

                match msg {
                    StreamSenderMessage::RtpPacket(src, packet) => {
                        last_timeout_check = now;
                        let _ = self.receive_rtp(src, packet.to_vec()).await;
                    }
                    StreamSenderMessage::RtcpPacket(packet) => {
                        let _ = self.receive_rtcp(packet).await;
                    }
                    StreamSenderMessage::PrepareRtpPacket(tx) => {
                        if let StreamSender::AudioLocal(local) = self {
                            if let Err(e) = local.prepare_rtp_packets(tx).await {
                                tracing::error!(
                                    channel = local.channel,
                                    stream = local.uuid.to_string(),
                                    "prepare rtp packets error: {e:?}"
                                );
                            }
                        }
                    }
                    StreamSenderMessage::InboundRecording(packet) => {
                        let _ = self.process_inbound_recording(packet).await;
                    }
                    StreamSenderMessage::TransportLayerCc(cc, now) => {
                        let _ = self.process_transportcc(cc, now).await;
                    }
                    StreamSenderMessage::TransportLayerNack(nack, now) => {
                        let _ = self.process_transportnack(nack, now).await;
                    }
                    StreamSenderMessage::VideoFrame(frame) => {
                        if let StreamSender::VideoPeer(peer) = self {
                            let _ = peer.send_frame(frame).await;
                        }
                    }
                    StreamSenderMessage::Rpc(rpc_msg, rpc_source) => {
                        match &rpc_msg.method {
                            RpcMethod::StopSession => {
                                if let StreamSender::AudioLocal(local) = self {
                                    info!(
                                        channel = local.channel,
                                        "local stream sender receive StopSession",
                                    );
                                    local.check_fax().await;
                                }
                                return Ok(());
                            }
                            RpcMethod::SetPeerAddr => {
                                if let Ok(peer) = serde_json::from_value::<StreamPeer>(
                                    rpc_msg.params.clone(),
                                ) {
                                    match self {
                                        StreamSender::AudioLocal(local) => {
                                            if peer.peer_port == LOCAL_RTP_PEER_PORT
                                            {
                                                info!(
                                            channel = local.channel,
                                                        "local stream sender port changed to LOCAL_RTP_PEER_PORT",
                                                    );
                                                return Ok(());
                                            }
                                        }
                                        StreamSender::AudioRemote(_) => {}
                                        StreamSender::VideoLocal(_) => {}
                                        StreamSender::VideoRemote(_) => {}
                                        StreamSender::VideoPeer(_) => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                        match self.process_rpc_msg(rpc_msg.clone(), rpc_source).await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                info!(
                                    "stream sender error process rpc {:?} {}",
                                    rpc_msg, e
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Route RPCs to the appropriate variant (audio/video, local/remote) and apply changes.
    async fn process_rpc_msg(
        &mut self,
        rpc_msg: RpcMessage,
        rpc_source: RpcSource,
    ) -> Result<()> {
        match &mut *self {
            StreamSender::AudioLocal(local) => {
                local.process_rpc_msg(rpc_msg).await?;
            }
            StreamSender::AudioRemote(remote) => {
                match &rpc_msg.method {
                    RpcMethod::SetPeerAddr => {
                        let peer: StreamPeer =
                            serde_json::from_value(rpc_msg.params.clone())?;
                        remote
                            .peer_transport
                            .set_peer_addr(peer.peer_ip, peer.peer_port);
                    }
                    RpcMethod::SetT38Passthrough => {
                        remote.t38_passthrough = true;
                    }
                    _ => {}
                }

                if rpc_source == RpcSource::Pool {
                    remote.process_rpc_msg(rpc_msg).await?;
                }
            }
            StreamSender::VideoLocal(video) => {
                video.process_rpc_msg(rpc_msg).await?;
            }
            StreamSender::VideoRemote(video) => {
                video.process_rpc_msg(rpc_msg).await?;
            }
            StreamSender::VideoPeer(peer) => {
                peer.process_rpc_msg(rpc_msg).await?;
            }
        }
        Ok(())
    }

    /// Handle inbound-recording packets:
    /// - AudioRemote: tag packet (dst UUID in RTP ext) and forward to remote recording UDP.
    /// - AudioLocal: if fax is T.38 and packet isn't RTP → hand to UDPTL; else feed local.receive_rtp.
    ///   Then decode payload to PCM off-thread; fan out to file recorder and stream recording (resample if needed, skip when paused).
    /// - Video*: ignored.
    async fn process_inbound_recording(
        &mut self,
        rtp_packet: Vec<u8>,
    ) -> Result<()> {
        match self {
            StreamSender::AudioRemote(remote) => {
                // Attach destination UUID for demux, then forward to remote recording port.
                let rtp_packet =
                    RtpPacket::set_extension(&rtp_packet, remote.id.as_bytes());
                remote
                    .recording_udp
                    .send_to(&rtp_packet, &remote.remote_recording_addr)
                    .await?;
            }
            StreamSender::AudioLocal(local) => {
                let uuid = local.uuid;
                {
                    let fax = local.state.fax.clone();
                    if let Some(fax) = fax {
                        let local_fax = fax.clone();
                        let is_t38 = nebula_task::spawn_task(move || {
                            local_fax.lock().is_t38()
                        })
                        .await?;
                        let rtp_packet = rtp_packet.clone();
                        if is_t38 && !RtpPacket::is_valid(&rtp_packet) {
                            // UDPTL (fax) path: non-RTP, let fax stack consume it.
                            nebula_task::spawn_task(move || {
                                fax.lock().process_udptl(&rtp_packet)
                            })
                            .await?;
                            return Ok(());
                        } else {
                            // Normal RTP: feed into local audio sender.
                            local.receive_rtp(uuid, rtp_packet.clone()).await?;
                        }
                    }
                }

                let file_recorder_sender =
                    local.state.file_recorder.as_ref().map(|r| r.sender.clone());
                let codec = local.state.inbound_recording_codec.clone();
                let recording = local.state.recording.as_ref().map(|r| {
                    (
                        r.paused,
                        r.inbound_sender.clone(),
                        r.inbound_resampler.clone(),
                    )
                });

                local.recording_runtime.spawn(async move {
                    let _ = nebula_task::spawn_task(move || -> Result<()> {
                        let payload = RtpPacket::get_payload(&rtp_packet).to_vec();
                        let mut buf = [0; 5000];
                        let n = codec.lock().decode(&payload, &mut buf)?;
                        let pcm = buf[..n].to_vec();

                        if let Some(file_recorder) = file_recorder_sender {
                            let _ = file_recorder.send(pcm.clone());
                        }

                        if let Some((paused, sender, resampler)) = recording {
                            if paused {
                                return Ok(());
                            }
                            let pcm = if let Some(resampler) = resampler {
                                resampler.lock().convert(&pcm)
                            } else {
                                pcm
                            };
                            let _ = sender.send(pcm);
                        }

                        Ok(())
                    })
                    .await;
                });
            }
            StreamSender::VideoLocal(_video) => {}
            StreamSender::VideoRemote(_video) => {}
            StreamSender::VideoPeer(_) => {}
        }
        Ok(())
    }

    /// Handle TCC feedback for video: feed acks to GoogCC; if target send rate changes, update Redis and notify switchboard.
    async fn process_transportcc(
        &mut self,
        cc: TransportLayerCc,
        now: Instant,
    ) -> Result<()> {
        match self {
            StreamSender::AudioRemote(_remote) => {}
            StreamSender::AudioLocal(_) => {}
            StreamSender::VideoLocal(video) => {
                video.process_transportcc(cc, now).await?;
            }
            StreamSender::VideoRemote(_) => {}
            StreamSender::VideoPeer(_) => {}
        }
        Ok(())
    }

    async fn process_transportnack(
        &mut self,
        nack: TransportLayerNack,
        now: Instant,
    ) -> Result<()> {
        match self {
            StreamSender::AudioRemote(_remote) => {}
            StreamSender::AudioLocal(_) => {}
            StreamSender::VideoLocal(video) => {
                video.process_transportnack(nack, now).await?;
            }
            StreamSender::VideoRemote(_) => {}
            StreamSender::VideoPeer(_) => {}
        }
        Ok(())
    }

    /// Sender-side RTCP handler (video): ignore empty triggers, SRTP-encrypt if needed, and forward via transport.
    async fn receive_rtcp(&mut self, mut rtcp_packet: Vec<u8>) -> Result<()> {
        match self {
            StreamSender::AudioRemote(_remote) => {}
            StreamSender::AudioLocal(_) => {}
            StreamSender::VideoLocal(video) => {
                if rtcp_packet.is_empty() {
                    // this might be the empty rtcp packet to tigger the creation of
                    // this stream, so just ingore it.
                    return Ok(());
                }
                if let Some(crypto) = video.crypto.as_mut() {
                    let index = crypto.next_rtcp_index().await;
                    let crypto = crypto.state.clone();
                    rtcp_packet = nebula_task::spawn_task(move || {
                        crypto.encrypt_rtcp(&rtcp_packet, index)
                    })
                    .await??;
                }
                video.peer_transport.send(&rtcp_packet).await?;
            }
            StreamSender::VideoRemote(video) => {
                if rtcp_packet.is_empty() {
                    return Ok(());
                }
                let mut packet = video.uuid.as_bytes().to_vec();
                packet.extend_from_slice(rtcp_packet.as_slice());
                video
                    .remote_rtcp_udp
                    .send_to(&packet, &video.remote_rtcp_addr)
                    .await?;
            }
            StreamSender::VideoPeer(_) => {}
        }
        Ok(())
    }

    async fn receive_rtp(
        &mut self,
        source_id: Uuid,
        rtp_packet: Vec<u8>,
    ) -> Result<()> {
        match self {
            StreamSender::AudioRemote(remote) => {
                if remote.t38_passthrough {
                    remote.peer_transport.send(&rtp_packet).await?;
                } else {
                    let mut ext = source_id.as_bytes().to_vec();
                    ext.extend_from_slice(remote.id.as_bytes());
                    let rtp_packet = RtpPacket::set_extension(&rtp_packet, &ext);
                    remote
                        .remote_udp
                        .send_to(&rtp_packet, &remote.remote_addr)
                        .await?;
                }
            }
            StreamSender::AudioLocal(local) => {
                if local.state.t38_passthrough {
                    local.state.peer_transport.send(&rtp_packet).await?;
                } else {
                    local.receive_rtp(source_id, rtp_packet).await?;
                }
            }
            StreamSender::VideoLocal(video) => {
                video.receive_rtp(source_id, rtp_packet).await?;
            }
            StreamSender::VideoRemote(remote) => {
                let mut ext = source_id.as_bytes().to_vec();
                ext.extend_from_slice(remote.uuid.as_bytes());
                let rtp_packet = RtpPacket::set_extension(&rtp_packet, &ext);
                remote
                    .remote_rtp_udp
                    .send_to(&rtp_packet, &remote.remote_rtp_addr)
                    .await?;
            }
            StreamSender::VideoPeer(_) => {}
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ForwardState {
    src: Uuid,
    incoming_ssrc: Ssrc,
    out_rtp_packet: RtpPacket,
    max_incoming: Vp8RewrittenIds,
    first_incoming: Vp8RewrittenIds,
    first_outgoing: Vp8RewrittenIds,
}

impl ForwardState {
    /// Initialize per-source VP8 forwarding state with an RTP template and zeroed rewrite ids.
    fn new(src: Uuid, pt: u8) -> Self {
        let mut rtp_packet = RtpPacket::new();
        rtp_packet.set_ssrc(src.as_fields().0);
        rtp_packet.set_payload_type(pt);
        let mut rtp_packet =
            RtpPacket::set_extension(rtp_packet.data(), &[0x31, 0, 0]);
        let extension_offset = RtpPacket::extension_offset(&rtp_packet);
        rtp_packet[extension_offset] = 0xbe;
        rtp_packet[extension_offset + 1] = 0xde;
        let rtp_packet = RtpPacket::from_vec(rtp_packet);
        Self {
            src,
            incoming_ssrc: 0,
            out_rtp_packet: rtp_packet,
            max_incoming: Vp8RewrittenIds::new(0, 0, 0, 0),
            first_incoming: Vp8RewrittenIds::new(0, 0, 0, 0),
            first_outgoing: Vp8RewrittenIds::new(0, 0, 0, 0),
        }
    }
}

pub struct VideoStreamSenderLocal {
    uuid: Uuid,
    rtpmap: Rtpmap,
    crypto: Option<CryptoContext>,
    peer_transport: PeerTransport,
    forward_states: HashMap<Uuid, ForwardState>,
    max_outgoing: Vp8RewrittenIds,
    tcc_seq: u64,
    tcc_sender: TccSender,
    congestion_control: CongestionController,
    rtx_sender: RtxSender,
    webrtc: bool,
    available_rate: u64,
    room: String,
    channel: String,
}

impl VideoStreamSenderLocal {
    /// Initialize a local video sender: set up GoogCC, RTX, (optional) SRTP, and transport; store room/channel.
    pub async fn new(
        uuid: Uuid,
        channel: String,
        rtpmap: Rtpmap,
        webrtc: bool,
        crypto: Option<CryptoContext>,
        peer_transport: PeerTransport,
    ) -> Self {
        let googcc_config = googcc::Config::default();
        let room = REDIS
            .hget(&format!("nebula:media_stream:{uuid}"), "room")
            .await
            .unwrap_or_default();

        Self {
            uuid,
            rtpmap,
            webrtc,
            crypto,
            peer_transport,
            tcc_seq: 0,
            forward_states: HashMap::new(),
            tcc_sender: TccSender::new(Instant::now()),
            rtx_sender: RtxSender::new(Duration::from_secs(10)),
            congestion_control: CongestionController::new(
                googcc_config,
                Instant::now(),
            ),
            max_outgoing: Vp8RewrittenIds::new(0, 0, 0, 0),
            available_rate: 0,
            room,
            channel,
        }
    }

    /// Rewrite incoming VP8 to our local continuity:
    /// - parse VP8 header (picture_id/tl0)
    /// - expand truncated counters and compute offsets per source
    /// - build outgoing RTP with rewritten seq/timestamp/picture_id/tl0
    /// - remember original for RTX and send
    async fn receive_rtp(
        &mut self,
        src_uuid: Uuid,
        rtp_packet: Vec<u8>,
    ) -> Result<()> {
        let pt = self.rtpmap.type_number;
        let forward_state = self
            .forward_states
            .entry(src_uuid)
            .or_insert_with(|| ForwardState::new(src_uuid, pt));

        let incoming_rtp = RtpPacket::from_vec(rtp_packet);
        let incoming_vp8 = vp8::ParsedHeader::read(incoming_rtp.payload())?;
        let incoming_picture_id = incoming_vp8
            .picture_id
            .ok_or_else(|| anyhow!("no picture id"))?;
        let incoming_tl0_pic_idx = incoming_vp8
            .tl0_pic_idx
            .ok_or_else(|| anyhow!("no t10 pic idx"))?;

        if incoming_rtp.ssrc() != forward_state.incoming_ssrc {
            forward_state.incoming_ssrc = incoming_rtp.ssrc();
            let first_incoming = Vp8RewrittenIds::new(
                incoming_rtp.sequence() as FullSequenceNumber,
                incoming_rtp.timestamp() as FullTimestamp,
                incoming_picture_id as vp8::FullPictureId,
                incoming_tl0_pic_idx as vp8::FullTl0PicIdx,
            );
            let first_outgoing = self
                .max_outgoing
                .checked_add(&Vp8RewrittenIds::new(2, 1, 1, 1))
                .ok_or_else(|| anyhow!("can't add"))?;

            forward_state.first_incoming = first_incoming.clone();
            forward_state.first_outgoing = first_outgoing.clone();
            forward_state.max_incoming = first_incoming;
            self.max_outgoing = first_outgoing;
        }

        let incoming = Vp8RewrittenIds::new(
            expand_truncated_counter(
                incoming_rtp.sequence(),
                &mut forward_state.max_incoming.seqnum,
                16,
            ),
            expand_truncated_counter(
                incoming_rtp.timestamp(),
                &mut forward_state.max_incoming.timestamp,
                32,
            ),
            expand_truncated_counter(
                incoming_picture_id,
                &mut forward_state.max_incoming.picture_id,
                15,
            ),
            expand_truncated_counter(
                incoming_tl0_pic_idx,
                &mut forward_state.max_incoming.tl0_pic_idx,
                8,
            ),
        );
        // If the sub fails, it's because the incoming packet predates the switch (before the key frame)
        let outgoing = forward_state
            .first_outgoing
            .checked_add(
                &incoming
                    .checked_sub(&forward_state.first_incoming)
                    .ok_or_else(|| {
                        anyhow!(
                            "can't {:?} - {:?}",
                            incoming,
                            forward_state.first_incoming
                        )
                    })?,
            )
            .ok_or_else(|| anyhow!("can't add"))?;
        self.max_outgoing = self.max_outgoing.max(&outgoing);

        let mut out_rtp_packet = forward_state.out_rtp_packet.clone();
        out_rtp_packet.set_sequence(outgoing.seqnum as u16);
        out_rtp_packet.set_timestamp(outgoing.timestamp as u32);
        out_rtp_packet.set_marker(incoming_rtp.has_marker());
        out_rtp_packet.set_paylod(incoming_rtp.payload());
        vp8::modify_header(
            out_rtp_packet.payload_mut(),
            outgoing.picture_id as TruncatedPictureId,
            outgoing.tl0_pic_idx as TruncatedTl0PicIdx,
        );

        self
            .rtx_sender
            .remember_sent(Bytes::copy_from_slice(out_rtp_packet.data()), Instant::now());

        self.send_rtp_packet(out_rtp_packet).await?;

        Ok(())
    }

    /// Send a single RTP packet for video. Writes TCC seq, encrypts if SRTP, transmits, and records rate.
    async fn send_rtp_packet(&mut self, mut rtp_packet: RtpPacket) -> Result<()> {
        self.tcc_seq += 1;
        let extension_offset = RtpPacket::extension_offset(rtp_packet.data());
        let seq = self.tcc_seq as u16;
        BigEndian::write_u16(
            &mut rtp_packet.mut_data()[extension_offset + 6..],
            seq,
        );

        let result = if let Some(crypto) = self.crypto.as_mut() {
            let roc = crypto.update_roc(rtp_packet.data()).await;
            let crypto = crypto.state.clone();
            nebula_task::spawn_task(move || -> Result<RtpPacket> {
                crypto.encrypt_rtp(roc, &mut rtp_packet)?;
                Ok(rtp_packet)
            })
            .await?
        } else {
            Ok(rtp_packet)
        };
        let rtp_packet = result?;
        let packet = rtp_packet.data();
        self.peer_transport.send(packet).await?;
        self.tcc_sender.remember_sent(
            self.tcc_seq,
            DataSize::from_bytes(packet.len() as u64),
            Instant::now(),
        );

        Ok(())
    }

    async fn process_transportcc(
        &mut self,
        cc: TransportLayerCc,
        now: Instant,
    ) -> Result<()> {
        let acks = self.tcc_sender.process_feedback(&cc, now);
        if let Some(send_rate) =
            self.congestion_control.recalculate_target_send_rate(acks)
        {
            let rate = send_rate.as_bps();
            if rate != self.available_rate {
                self.available_rate = rate;
                let _ = REDIS
                    .hsetex(
                        &format!("nebula:media_stream:{}", &self.uuid),
                        "available_rate",
                        &rate.to_string(),
                    )
                    .await;
                if !self.room.is_empty() && !self.channel.is_empty() {
                    let _ = MEDIA_SERVICE
                        .switchboard_rpc_client
                        .notify_available_rate(
                            &MEDIA_SERVICE.config.switchboard_host,
                            self.room.clone(),
                            self.channel.clone(),
                            rate,
                        )
                        .await;
                }
            }
        }
        Ok(())
    }

    /// Handle NACKs for missing video packets: use cached originals to build RTX packets and resend.
    async fn process_transportnack(
        &mut self,
        nack: TransportLayerNack,
        _now: Instant,
    ) -> Result<()> {
        let ssrc = nack.media_ssrc;
        for nack in nack.nacks {
            for seqnum in nack.packet_list() {
                if let Some(mut rtx) = self.rtx_sender.resend_as_rtx(ssrc, seqnum) {
                    RtpPacket::set_pt(&mut rtx, self.rtpmap.type_number + 1);
                    let rtp_packet = RtpPacket::from_vec(rtx);
                    let _ = self.send_rtp_packet(rtp_packet).await;
                }
            }
        }

        Ok(())
    }

    /// React to video RPCs: on DtlsDone load SRTP; on SetPeerAddr update transport destination.
    async fn process_rpc_msg(&mut self, rpc_msg: RpcMessage) -> Result<()> {
        match &rpc_msg.method {
            RpcMethod::DtlsDone => {
                if self.webrtc && self.crypto.is_none() {
                    self.crypto =
                        get_crypto(CryptoKind::Local, &self.uuid.to_string()).await;
                }
            }
            RpcMethod::SetPeerAddr => {
                let peer: StreamPeer = serde_json::from_value(rpc_msg.params)?;
                self.peer_transport
                    .set_peer_addr(peer.peer_ip, peer.peer_port);
            }
            _ => {}
        }
        Ok(())
    }
}

pub struct VideoStreamSenderRemote {
    uuid: Uuid,
    rtpmap: Rtpmap,
    crypto: Option<Arc<CryptoContext>>,
    peer_transport: PeerTransport,
    webrtc: bool,
    remote_ip: String,
    remote_rtp_addr: SocketAddr,
    remote_rtcp_addr: SocketAddr,
    remote_rtp_udp: Arc<UdpSocket>,
    remote_rtcp_udp: Arc<UdpSocket>,
}

impl VideoStreamSenderRemote {
    /// Create a remote video sender for the chosen IP. Prepares sockets/addresses and SRTP context.
    pub fn new(
        uuid: Uuid,
        rtpmap: Rtpmap,
        webrtc: bool,
        crypto: Option<Arc<CryptoContext>>,
        peer_transport: PeerTransport,
        remote_ip: String,
        remote_rtp_udp: Arc<UdpSocket>,
        remote_rtcp_udp: Arc<UdpSocket>,
    ) -> Result<Self> {
        let remote_rtp_addr: SocketAddr =
            format!("{}:{}", remote_ip, REMOTE_RTP_PORT).parse()?;
        let remote_rtcp_addr: SocketAddr =
            format!("{}:{}", remote_ip, REMOTE_RTCP_PORT).parse()?;
        Ok(Self {
            uuid,
            rtpmap,
            webrtc,
            crypto,
            peer_transport,
            remote_ip,
            remote_rtp_addr,
            remote_rtcp_addr,
            remote_rtp_udp,
            remote_rtcp_udp,
        })
    }

    async fn process_rpc_msg(&mut self, rpc_msg: RpcMessage) -> Result<()> {
        match &rpc_msg.method {
            RpcMethod::DtlsDone => {
                if self.webrtc && self.crypto.is_none() {
                    self.crypto =
                        get_crypto(CryptoKind::Local, &self.uuid.to_string())
                            .await
                            .map(Arc::new);
                }
            }
            RpcMethod::SetPeerAddr => {
                let peer: StreamPeer = serde_json::from_value(rpc_msg.params)?;
                self.peer_transport
                    .set_peer_addr(peer.peer_ip, peer.peer_port);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct VideoSourceState {
    uuid: Uuid,
    last_key_speaker: u64,
    requested_size: PixelSize,
    layers: VecDeque<(u64, IncomingVideoInfo)>,
    is_screen_share: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocatedVideo {
    uuid: Uuid,
    rid: String,
    ssrc: u32,
    rate: u64,
    size: PixelSize,
}

#[derive(Debug)]
enum VideoSwitchState {
    Switched(String),
    Switching { from: String, to: String },
}

pub struct IncomingVideoState {
    pub ssrc: u32,
    pub size: PixelSize,
    pub last_send_to_destinations: Option<Instant>,
    // the last time sending pli because of zero size
    pub last_zero_size_pli: Option<Instant>,
    pub rate_tracker: DataRateTracker,
    pub last_update: u64,
}

impl IncomingVideoState {
    /// Create a fresh state tracker for an incoming SSRC.
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            size: PixelSize::default(),
            last_send_to_destinations: None,
            last_zero_size_pli: None,
            rate_tracker: DataRateTracker::default(),
            last_update: SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

#[derive(Default)]
pub struct DataRateTracker {
    // Oldest is at the back. The newest is at the front. This makes it easier
    // to do removal using VecDeque::split_off.
    history: VecDeque<(Instant, u64)>,
    accumulated_size: u64,
    rate: Option<u64>,
}

impl DataRateTracker {
    const MAX_DURATION: Duration = Duration::from_millis(5000);
    const MIN_DURATION: Duration = Duration::from_millis(500);

    /// Latest bits/sec estimate over a recent window (if enough data).
    fn rate(&self) -> Option<u64> {
        self.rate
    }

    /// Add a new observation (in bits) at the given time; call update() to refresh the estimate.
    fn push(&mut self, size: u64, time: Instant) {
        self.history.push_front((time, size));
        self.accumulated_size += size;
    }

    /// Drop old samples and recompute the rate over the remaining window.
    fn update(&mut self, now: Instant) {
        let deadline = now - Self::MAX_DURATION;
        let count_to_remove = self
            .history
            .iter()
            .rev()
            .take_while(|(old, _)| *old < deadline)
            .count();
        let removed = self.history.split_off(self.history.len() - count_to_remove);
        for (_, removed_size) in removed {
            self.accumulated_size -= removed_size;
        }

        let duration = if let Some((oldest, _)) = self.history.back() {
            now.saturating_duration_since(*oldest)
        } else {
            Duration::ZERO
        };
        self.rate = if duration >= Self::MIN_DURATION {
            Some((self.accumulated_size as f64 / duration.as_secs_f64()) as u64)
        } else {
            // Wait for more info
            None
        }
    }
}

struct IncomingSsrcState {
    max_seqnum: FullSequenceNumber,
    nack_sender: NackSender,
}

pub struct VideoStreamReceiver {
    id: Uuid,
    channel: String,
    sender: UnboundedSender<StreamReceiverMessage>,
    rtpmap: Rtpmap,
    peer_destinations: Option<Vec<Uuid>>,
    destination_layers: HashMap<Uuid, VideoSwitchState>,
    crypto: Option<CryptoContext>,
    stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    muted: bool,
    rid_ssrcs: HashMap<String, u32>,
    incoming_layers: HashMap<String, IncomingVideoState>,
    last_remb: Option<Instant>,
    tcc_receiver: TccReceiver,
    // For seqnum expanasion of incoming packets
    // and for SRTP replay attack protection
    state_by_incoming_ssrc: HashMap<Ssrc, IncomingSsrcState>,
    /// The last time NACKs were sent.
    nacks_sent: Option<Instant>,

    allocated_ssrcs: HashMap<u32, AllocatedVideoSSrc>,
    allocated_videos: HashMap<Uuid, AllocatedVideo>,
    last_allocated: Option<Instant>,
    room: String,
    codec: Option<Arc<BlockMutex<Box<dyn VideoCodec>>>>,
}

impl VideoStreamReceiver {
    pub async fn new(
        id: Uuid,
        channel: String,
        sender: UnboundedSender<StreamReceiverMessage>,
        ssrc: u32,
        rtpmap: Rtpmap,
        crypto: Option<CryptoContext>,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Self {
        let room: String = REDIS
            .hget(&format!("nebula:media_stream:{id}"), "room")
            .await
            .unwrap_or_default();

        let codec = if room.is_empty() {
            match rtpmap.name {
                PayloadType::H264 => nebula_task::spawn_task(
                    move || -> Result<Box<dyn VideoCodec>> {
                        H264::new()
                            .map(|codec| Box::new(codec) as Box<dyn VideoCodec>)
                    },
                )
                .await
                .ok()
                .and_then(|r| r.ok()),
                PayloadType::VP8 => None,
                _ => None,
            }
        } else {
            None
        };
        let codec = codec.map(|codec| Arc::new(BlockMutex::new(codec)));

        Self {
            id,
            channel,
            sender,
            rtpmap,
            peer_destinations: None,
            destination_layers: HashMap::new(),
            rid_ssrcs: HashMap::new(),
            crypto,
            stream_sender_pool,
            muted: false,
            allocated_ssrcs: HashMap::new(),
            incoming_layers: HashMap::new(),
            last_remb: None,
            tcc_receiver: TccReceiver::new(id.as_fields().0, Instant::now()),
            state_by_incoming_ssrc: HashMap::new(),
            nacks_sent: None,
            allocated_videos: HashMap::new(),
            last_allocated: None,
            room,
            codec,
        }
    }

    async fn receive_rtcp_packet(&mut self, mut rtcp_packet: Vec<u8>) -> Result<()> {
        let now = Instant::now();

        if let Some(crypto) = self.crypto.as_mut() {
            let crypto = crypto.state.clone();
            rtcp_packet =
                nebula_task::spawn_task(move || crypto.decrypt_rtcp(&rtcp_packet))
                    .await??;
        }

        let mut buf = Bytes::from(rtcp_packet);
        while buf.remaining() > 0 {
            let header = rtcp::header::Header::unmarshal(&mut buf.clone())?;
            let len = (header.length as usize) * 4 + RTCP_HEADER_LEN;
            if buf.remaining() < len {
                return Err(anyhow!("rtcp packet too short"));
            }
            let mut in_packet = buf.copy_to_bytes(len);
            let rtcp = RtcpPacket::unmarshal(&mut in_packet)?;
            match &rtcp {
                RtcpPacket::PictureLossIndication(pli) => {
                    if self.codec.is_some() {
                        // peer video
                        let _ = MEDIA_SERVICE
                            .media_rpc_client
                            .request_keyframe(self.id.to_string())
                            .await;
                    } else {
                        // room conference
                        if let Some((uuid, ssrc)) = self
                            .allocated_ssrcs
                            .get(&pli.media_ssrc)
                            .map(|src| (src.uuid, src.ssrc))
                        {
                            let mut pli = pli.clone();
                            pli.media_ssrc = ssrc;
                            let msg = StreamSenderPoolMessage::RtcpPacket(
                                uuid,
                                pli.marshal()?.to_vec(),
                            );
                            let _ = self.stream_sender_pool.send(msg);
                        }
                    }
                }
                RtcpPacket::FullIntraRequest(_fir) => {}
                RtcpPacket::TransportLayerNack(nack) => {
                    let msg = StreamSenderPoolMessage::TransportLayerNack(
                        self.id,
                        nack.clone(),
                        now,
                    );
                    let _ = self.stream_sender_pool.send(msg);
                }
                RtcpPacket::TransportLayerCc(cc) => {
                    let msg = StreamSenderPoolMessage::TransportLayerCc(
                        self.id,
                        cc.clone(),
                        now,
                    );
                    let _ = self.stream_sender_pool.send(msg);
                }
                RtcpPacket::ReceiverEstimatedMaximumBitrate(_remb) => {}
                _ => {}
            }
        }
        Ok(())
    }

    fn get_incoming_ssrc_state_mut(&mut self, ssrc: Ssrc) -> &mut IncomingSsrcState {
        self.state_by_incoming_ssrc
            .entry(ssrc)
            .or_insert(IncomingSsrcState {
                max_seqnum: 0,
                // A 1KB RTCP payload with 4 bytes each would allow only 250
                // in the worst case scenario.
                nack_sender: NackSender::new(250),
            })
    }

    /// Main per-packet video path. Decrypts if needed, updates layers and RID/TCC, handles RTX, and forwards.
    /// Video receiver RTP handler: decrypt (if SRTP), handle RTX recovery and RID/TCC, update layers, and forward.
    async fn receive_rtp_packet(
        &mut self,
        mut rtp_packet: Vec<u8>,
        now: Instant,
    ) -> Result<()> {
        if let Some(crypto) = self.crypto.as_mut() {
            let roc = crypto.update_roc(&rtp_packet).await;
            let crypto = crypto.state.clone();
            rtp_packet = nebula_task::spawn_task(move || {
                crypto.decrypt_rtp(roc, &rtp_packet)
            })
            .await??;
        }

        if let Some(codec) = self.codec.clone() {
            let _ = self.handle_peer_to_peer(codec, rtp_packet).await;
            return Ok(());
        }

        let _ = self.allocate_layers(false).await;

        if self.muted {
            return Ok(());
        }

        let rtp = rtp::packet::Packet::unmarshal(&mut bytes::Bytes::from(
            rtp_packet.clone(),
        ))?;

        let ssrc = rtp.header.ssrc;
        let mut rid = "";
        for ext in rtp.header.extensions.iter() {
            if ext.id == 10 || ext.id == 11 {
                if let Ok(s) = std::str::from_utf8(&ext.payload) {
                    rid = s;
                    if ext.id == 10 {
                        // when ext id is 10, it's VP8 not rtx
                        // we check the stored ssrc for the rid
                        if let Some(existing_ssrc) = self.rid_ssrcs.get_mut(s) {
                            if *existing_ssrc != ssrc {
                                *existing_ssrc = ssrc;
                            }
                        } else {
                            self.rid_ssrcs.insert(s.to_string(), ssrc);
                        }
                    }
                }
            } else if ext.id == 3 {
                let seq = BigEndian::read_u16(&ext.payload);
                self.tcc_receiver.remember_received(seq, now);
                if self.tcc_receiver.should_send_feedback() {
                    let feedbacks =
                        self.tcc_receiver.feedbacks(ssrc).collect::<Vec<_>>();
                    for feedback in feedbacks {
                        if let Ok(pkt) = feedback.marshal() {
                            let msg = StreamSenderPoolMessage::RtcpPacket(
                                self.id,
                                pkt.to_vec(),
                            );
                            let _ = self.stream_sender_pool.send(msg);
                        }
                    }
                }
            }
        }

        let rid = rid.to_string();

        let rtp = if rtp.header.payload_type != self.rtpmap.type_number {
            if rtp.header.payload_type == self.rtpmap.type_number + 1
                && rtp.payload.len() > 2
            {
                let seq = BigEndian::read_u16(&rtp.payload);
                RtpPacket::buffer_set_sequence(&mut rtp_packet, seq);
                RtpPacket::set_pt(&mut rtp_packet, self.rtpmap.type_number);
                RtpPacket::buffer_set_paylod(&mut rtp_packet, &rtp.payload[2..]);
                if let Some(ssrc) = self.rid_ssrcs.get(&rid) {
                    // rewrite the ssrc on the rtx to the actual VP8 rtp packet
                    RtpPacket::set_packet_ssrc(&mut rtp_packet, *ssrc);
                }
                rtp::packet::Packet::unmarshal(&mut bytes::Bytes::from(
                    rtp_packet.clone(),
                ))?
            } else {
                return Err(anyhow!(
                    "not VP8, paylaod type {}",
                    rtp.header.payload_type
                ));
            }
        } else {
            rtp
        };
        let ssrc = rtp.header.ssrc;

        let layer = self
            .incoming_layers
            .entry(rid.to_string())
            .or_insert_with(|| IncomingVideoState::new(ssrc));
        layer.rate_tracker.push(rtp_packet.len() as u64 * 8, now);

        let mut is_key_frame = false;
        if let Ok(header) = vp8::ParsedHeader::read(&rtp.payload) {
            is_key_frame = header.is_key_frame;
            if let Some(res) = header.resolution.as_ref() {
                if &layer.size != res {
                    layer.size = *res;
                    layer.last_send_to_destinations = None;
                }
            }
        }

        if layer.size.height == 0
            && layer
                .last_zero_size_pli
                .as_ref()
                .map(|i| i.elapsed().as_millis() > 1000)
                .unwrap_or(true)
        {
            layer.last_send_to_destinations = Some(now);
            // if we haven't got size for the layer
            // we send a pli to trigger key frame
            let pli = PictureLossIndication {
                sender_ssrc: 1,
                media_ssrc: ssrc,
            };
            if let Ok(packet) = pli.marshal() {
                let msg =
                    StreamSenderPoolMessage::RtcpPacket(self.id, packet.to_vec());
                let _ = self.stream_sender_pool.send(msg);
            }
        }

        let should_send_layer_info = layer
            .last_send_to_destinations
            .as_ref()
            .map(|i| i.elapsed().as_millis() > 1000)
            .unwrap_or(true);
        if should_send_layer_info {
            layer.last_send_to_destinations = Some(now);
            layer.rate_tracker.update(now);
            layer.last_update = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let layers_info: HashMap<String, (u64, IncomingVideoInfo)> = self
                .incoming_layers
                .iter()
                .map(|(rid, state)| {
                    (
                        rid.clone(),
                        (
                            state.last_update,
                            IncomingVideoInfo {
                                uuid: self.id,
                                ssrc,
                                rid: rid.clone(),
                                size: state.size,
                                rate: state.rate_tracker.rate(),
                            },
                        ),
                    )
                })
                .collect();
            if let Ok(layers_info) = serde_json::to_string(&layers_info) {
                let id = self.id;
                tokio::spawn(async move {
                    let _ = REDIS
                        .hsetex(
                            &format!("nebula:media_stream:{id}"),
                            "layers",
                            &layers_info,
                        )
                        .await;
                });
            }
        }

        // if self
        //     .last_remb
        //     .as_ref()
        //     .map(|t| t.elapsed().as_millis() > 1000)
        //     .unwrap_or(true)
        // {
        //     self.last_remb = Some(Instant::now());
        //     let sender_ssrc = self.id.as_fields().0;
        //     let remb = ReceiverEstimatedMaximumBitrate {
        //         sender_ssrc,
        //         bitrate: 2000000,
        //         ssrcs: vec![ssrc],
        //     };
        //     if let Ok(pkt) = remb.marshal() {
        //         let msg = StreamSenderPoolMessage::RtcpPacket(self.id, pkt.to_vec());
        //         let _ = self.stream_sender_pool.send(msg);
        //     }
        // }

        let ssrc_state = self.get_incoming_ssrc_state_mut(ssrc);
        let full_seqnum = expand_truncated_counter(
            rtp.header.sequence_number,
            &mut ssrc_state.max_seqnum,
            16,
        );
        ssrc_state.nack_sender.remember_received(full_seqnum);
        let pkt = Bytes::from(rtp_packet.clone());
        for (destination, state) in self.destination_layers.iter_mut() {
            match state {
                VideoSwitchState::Switched(r) => {
                    if &rid == r {
                        let msg = StreamSenderPoolMessage::RtpPacket(
                            self.id,
                            *destination,
                            pkt.clone(),
                        );
                        let _ = self.stream_sender_pool.send(msg);
                    }
                }
                VideoSwitchState::Switching { from, to } => {
                    if &rid == from {
                        let msg = StreamSenderPoolMessage::RtpPacket(
                            self.id,
                            *destination,
                            pkt.clone(),
                        );
                        let _ = self.stream_sender_pool.send(msg);
                    } else if &rid == to && is_key_frame {
                        let msg = StreamSenderPoolMessage::RtpPacket(
                            self.id,
                            *destination,
                            pkt.clone(),
                        );
                        let _ = self.stream_sender_pool.send(msg);
                        *state = VideoSwitchState::Switched(to.clone());
                    }
                }
            }
        }

        self.send_nacks(now)?;

        Ok(())
    }

    /// Compute and send NACKs for missing sequence numbers, rate-limited by NACK_CALCULATION_INTERVAL.
    fn send_nacks(&mut self, now: Instant) -> Result<()> {
        if let Some(nacks_sent) = self.nacks_sent {
            if now < nacks_sent + NACK_CALCULATION_INTERVAL {
                // We sent NACKs recently. Wait to resend/recalculate them.
                return Ok(());
            }
        }

        for nack in self.state_by_incoming_ssrc.iter_mut().filter_map(
            move |(ssrc, state)| {
                let seqnums = state.nack_sender.send_nacks(now)?;

                let nacks = nack_pairs_from_sequence_numbers(
                    &seqnums.map(|s| s as u16).collect::<Vec<_>>(),
                );
                let nack = TransportLayerNack {
                    sender_ssrc: 1,
                    media_ssrc: *ssrc,
                    nacks,
                };
                Some(nack)
            },
        ) {
            let msg = StreamSenderPoolMessage::RtcpPacket(
                self.id,
                nack.marshal()?.to_vec(),
            );
            let _ = self.stream_sender_pool.send(msg);
        }
        Ok(())
    }

    /// Switch which RID a destination receives. If switching, wait for a keyframe before finalizing.
    fn update_destination_layer(
        &mut self,
        layer: RpcVideoDestinationLayer,
    ) -> Result<()> {
        println!(
            "update destination layer {} {} {:?}",
            layer.src, layer.dst, layer.rid
        );
        let dst = Uuid::from_str(&layer.dst)?;

        let Some(ref new) = layer.rid else {
            self.destination_layers.remove(&dst);
            return Ok(());
        };

        if let Some(layer) = self.destination_layers.get_mut(&dst) {
            match layer {
                VideoSwitchState::Switched(r) => {
                    if r != new {
                        *layer = VideoSwitchState::Switching {
                            from: r.clone(),
                            to: new.clone(),
                        };
                    }
                }
                VideoSwitchState::Switching { from, to } => {
                    if from == new {
                        *layer = VideoSwitchState::Switched(new.clone());
                    } else if to != new {
                        *to = new.clone();
                    }
                }
            }
        } else {
            self.destination_layers
                .insert(dst, VideoSwitchState::Switched(new.clone()));
        };

        if let Some(state) = self.incoming_layers.get(new) {
            println!("request pli ssrc {}", state.ssrc);
            let pli = PictureLossIndication {
                sender_ssrc: 1,
                media_ssrc: state.ssrc,
            };
            if let Ok(packet) = pli.marshal() {
                let msg =
                    StreamSenderPoolMessage::RtcpPacket(self.id, packet.to_vec());
                let _ = self.stream_sender_pool.send(msg);
            }
        }

        Ok(())
    }

    async fn process_rpc_msg(&mut self, rpc_msg: RpcMessage) -> Result<()> {
        match &rpc_msg.method {
            RpcMethod::MuteStream => {
                self.muted = true;
            }
            RpcMethod::UnmuteStream => {
                self.muted = false;
            }
            RpcMethod::UpdateVideoDestinationLayer => {
                let layer: RpcVideoDestinationLayer =
                    serde_json::from_value(rpc_msg.params)?;
                self.update_destination_layer(layer)?;
            }
            RpcMethod::UpdateVideoSources => {
                self.allocate_layers(true).await?;
            }
            RpcMethod::UpdateMediaDestinations => {
                self.peer_destinations = None;
            }
            RpcMethod::SetActiveSpeaker => {
                self.allocate_layers(true).await?;
            }
            _ => (),
        }
        Ok(())
    }

    /// Store current allocations and expose them by SSRC; report to switchboard if in a room.
    fn update_allocated_videos(
        &mut self,
        allocated_videos: HashMap<Uuid, AllocatedVideo>,
    ) {
        self.allocated_videos = allocated_videos;
        self.allocated_ssrcs = self
            .allocated_videos
            .iter()
            .map(|(uuid, video)| {
                (
                    uuid.as_fields().0,
                    AllocatedVideoSSrc {
                        uuid: *uuid,
                        ssrc: video.ssrc,
                        rid: video.rid.clone(),
                    },
                )
            })
            .collect();
        if !self.channel.is_empty() && !self.room.is_empty() {
            let member = self.channel.clone();
            let room = self.room.clone();
            let allocated_videos = self.allocated_videos.clone();
            tokio::spawn(async move {
                report_video_allocations(member, room, allocated_videos).await;
            });
        }
    }

    /// Update which inbound layers are allocated to this destination, optionally forcing a refresh.
    async fn allocate_layers(&mut self, force: bool) -> Result<()> {
        if !force
            && self
                .last_allocated
                .as_ref()
                .map(|t| t.elapsed().as_millis() < 1000)
                .unwrap_or(false)
        {
            return Ok(());
        }
        self.last_allocated = Some(Instant::now());

        let source = VideoStreamSource {
            id: self.id,
            sender: self.sender.clone(),
        };
        let allocated_videos = self.allocated_videos.clone();
        tokio::spawn(async move {
            let _ = source.allocate_layers(allocated_videos).await;
        });

        Ok(())
    }

    async fn handle_peer_to_peer(
        &mut self,
        codec: Arc<BlockMutex<Box<dyn VideoCodec>>>,
        rtp_packet: Vec<u8>,
    ) -> Result<()> {
        let frames = nebula_task::spawn_task(
            move || -> Result<Vec<ffmpeg_next::frame::Video>> {
                let payload = RtpPacket::get_payload(&rtp_packet);
                let timestamp = RtpPacket::get_timestamp(&rtp_packet);
                let marker = RtpPacket::get_marker(&rtp_packet);
                codec.lock().decode(payload, timestamp, marker)
            },
        )
        .await??;

        if self.peer_destinations.is_none() {
            let key = format!("nebula:media_stream:{}:destinations", self.id);
            let destinations: Vec<String> = REDIS.smembers(&key).await?;
            let destinations: Vec<Uuid> = destinations
                .iter()
                .filter_map(|d| Uuid::from_str(d).ok())
                .collect();
            self.peer_destinations = Some(destinations);
        }

        if let Some(dsts) = self.peer_destinations.as_ref() {
            for dst in dsts {
                for frame in frames.clone() {
                    let _ = self
                        .stream_sender_pool
                        .send(StreamSenderPoolMessage::VideoFrame(*dst, frame));
                }
            }
        }

        Ok(())
    }
}

/// VideoStreamSource decides the source video streams that's sending to this destination
/// Think about a video conference, and a participant is subscirbing a few other members
/// of the video conference room, this struct will store the states of those members
/// to decide the which video quality and which member to receive the video stream from
/// based on the available bandwith
pub struct VideoStreamSource {
    // Uuid of the video stream
    id: Uuid,
    sender: UnboundedSender<StreamReceiverMessage>,
}

impl VideoStreamSource {
    /// Decide which layers each source should send to this destination and notify if it changed.
    async fn allocate_layers(
        &self,
        allocated_videos: HashMap<Uuid, AllocatedVideo>,
    ) -> Result<()> {
        let source_states = self.retrieve_sources().await?;
        let new_allocated_videos =
            self.calculate_allocations(source_states.clone()).await;

        let mut allocation_changed = false;
        for (src, current_allocated) in &allocated_videos {
            if let Some(new) = new_allocated_videos.get(src) {
                if new.rid != current_allocated.rid {
                    allocation_changed = true;
                    let _ = MEDIA_SERVICE
                        .media_rpc_client
                        .update_video_destination_layer(
                            src.to_string(),
                            self.id.to_string(),
                            Some(new.rid.clone()),
                        )
                        .await;
                }
            } else {
                allocation_changed = true;
                let _ = MEDIA_SERVICE
                    .media_rpc_client
                    .update_video_destination_layer(
                        src.to_string(),
                        self.id.to_string(),
                        None,
                    )
                    .await;
            }
        }

        for (src, new_allocated) in &new_allocated_videos {
            if !allocated_videos.contains_key(src) {
                allocation_changed = true;
                let _ = MEDIA_SERVICE
                    .media_rpc_client
                    .update_video_destination_layer(
                        src.to_string(),
                        self.id.to_string(),
                        Some(new_allocated.rid.clone()),
                    )
                    .await;
            }
        }

        if allocation_changed {
            let _ = self
                .sender
                .send(StreamReceiverMessage::UpdateAllocatedVideos(
                    new_allocated_videos,
                ));
        }

        Ok(())
    }

    /// Load the current set of candidate sources and their requested sizes and flags.
    async fn retrieve_sources(&self) -> Result<HashMap<Uuid, VideoSourceState>> {
        let key = format!("nebula:media_stream:{}:sources", self.id);
        let sources: Vec<String> = REDIS.smembers(&key).await?;
        let sources = sources
            .iter()
            .filter_map(|s| Uuid::from_str(s).ok())
            .collect::<Vec<_>>();

        let source_info_str: String = REDIS
            .hget(&format!("nebula:media_stream:{}", self.id), "source_info")
            .await
            .unwrap_or_default();
        let source_info: HashMap<String, VideoSourceInfo> =
            serde_json::from_str(&source_info_str).unwrap_or_default();

        let mut source_states = HashMap::new();
        for uuid in sources {
            let mut state = VideoSourceState {
                uuid,
                requested_size: PixelSize {
                    width: 640,
                    height: 360,
                },
                layers: VecDeque::new(),
                is_screen_share: REDIS
                    .hget::<String>(
                        &format!("nebula:media_stream:{uuid}"),
                        "screen_share",
                    )
                    .await
                    .unwrap_or_default()
                    == "yes",
                last_key_speaker: REDIS
                    .hget::<u64>(
                        &format!("nebula:media_stream:{uuid}"),
                        "last_key_speaker",
                    )
                    .await
                    .unwrap_or(0),
            };
            if let Some(info) = source_info.get(&uuid.to_string()) {
                state.requested_size.width = info.width;
                state.requested_size.height = info.height;
            }
            source_states.insert(uuid, state);
        }
        Ok(source_states)
    }

    /// Given live sources and bandwidth, pick the best layer per source.
    async fn calculate_allocations(
        &self,
        source_states: HashMap<Uuid, VideoSourceState>,
    ) -> HashMap<Uuid, AllocatedVideo> {
        let mut sources = source_states.into_iter().collect::<Vec<_>>();
        for (uuid, state) in sources.iter_mut() {
            let layers: String = REDIS
                .hget(&format!("nebula:media_stream:{uuid}"), "layers")
                .await
                .unwrap_or_default();
            let layers: HashMap<String, (u64, IncomingVideoInfo)> =
                serde_json::from_str(&layers).unwrap_or_default();
            let mut layers = layers.into_values().collect::<Vec<_>>();
            layers.sort_by_key(|(_, info)| info.size.height);
            if state.is_screen_share {
                // for screen share, only keep the highest res
                if let Some(layer) = layers.pop() {
                    layers = vec![layer];
                }
            }
            state.layers = VecDeque::from(layers);
        }
        Self::sort_video_sources(&mut sources);

        let available_rate: u64 = REDIS
            .hget(
                &format!("nebula:media_stream:{}", self.id),
                "available_rate",
            )
            .await
            .unwrap_or(5000 * 1000);

        Self::loop_allocatoins(sources, available_rate)
    }

    /// Sort sources by screen-share first, then by key speaker, then by requested size.
    fn sort_video_sources(sources: &mut [(Uuid, VideoSourceState)]) {
        sources.sort_by_key(|(src, state)| {
            std::cmp::Reverse((
                state.is_screen_share,
                state.last_key_speaker,
                state.requested_size.height,
                *src,
            ))
        });
    }

    /// Greedy allocator that respects available_rate and prefers not to downshift quality.
    fn loop_allocatoins(
        mut sources: Vec<(Uuid, VideoSourceState)>,
        available_rate: u64,
    ) -> HashMap<Uuid, AllocatedVideo> {
        let mut allocated: HashMap<Uuid, AllocatedVideo> = HashMap::new();

        let mut total_allocated_rate = 0u64;

        loop {
            let mut layer_allocated = false;
            for (src, state) in sources.iter_mut() {
                let Some((updated, layer)) = state.layers.pop_front() else {
                    continue;
                };
                layer_allocated = true;

                if layer.size.width == 0 {
                    continue;
                }
                if !state.is_screen_share
                    && SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        .saturating_sub(updated)
                        > 3
                {
                    continue;
                }
                let allocated_rate = allocated.get(src).map(|a| a.rate).unwrap_or(0);
                let layer_rate = layer.rate.unwrap_or(match layer.size.height {
                    h if h <= 180 => 200000,
                    h if h <= 360 => 400000,
                    _ => 900000,
                });
                if !state.is_screen_share && layer_rate < allocated_rate {
                    // the rate is lower than previous allocated,
                    // because this size is larger than previous,
                    // it means it's not sending at an ideal frame rate,
                    // so we don't allocate it.
                    continue;
                }
                total_allocated_rate += layer_rate;
                total_allocated_rate -= allocated_rate;
                if total_allocated_rate > available_rate {
                    // check allocated is_empty
                    // if it's empty, that means our bandwidth
                    // is smaller than the lowest requirement
                    // but we'll still allocate it anyways
                    // so that we can receive the transport cc.
                    // if we don't send stream at all,
                    // we won't receive transport cc,
                    // and we'll never calculate new bandwith.
                    if !allocated.is_empty() {
                        return allocated;
                    }
                }

                allocated.insert(
                    *src,
                    AllocatedVideo {
                        rid: layer.rid.clone(),
                        uuid: *src,
                        ssrc: layer.ssrc,
                        rate: layer_rate,
                        size: layer.size,
                    },
                );

                if state.requested_size.height > 0
                    && (layer.size.height as f64 * 1.5)
                        >= state.requested_size.height as f64
                {
                    state.layers.clear();
                    continue;
                }
            }

            if !layer_allocated {
                return allocated;
            }
        }
    }
}

/// Read the chosen IP for sending a video stream (set by the receiver side).
async fn get_video_send_ip(id: &str) -> String {
    let ip: String = REDIS
        .hget(&format!("nebula:media_stream:{}", id), "ip")
        .await
        .unwrap_or("".to_string());
    ip
}

/// Pick a live IP to send from; falls back to our local IP and registers it.
async fn get_send_ip(
    id: &str,
    monitor: &RemoteStreamSenderMonitor,
) -> Result<String> {
    let mutex = DistributedMutex::new(format!("nebula:media_stream:{}:lock", id));
    mutex.lock().await;

    let ip: String = REDIS
        .hget(&format!("nebula:media_stream:{}", id), "ip")
        .await
        .unwrap_or("".to_string());
    if !ip.is_empty() && monitor.is_live(&ip).await {
        return Ok(ip);
    }

    if !REDIS
        .hsetex(
            &format!("nebula:media_stream:{}", id),
            "ip",
            &MEDIA_SERVICE.local_ip,
        )
        .await?
    {
        return Err(anyhow!("session has stopped"));
    }
    Ok(MEDIA_SERVICE.local_ip.clone())
}

/// Try to find a matching SRTP crypto suite between local and remote media.
async fn negotiate_crypto(
    stream_id: &str,
    local_media: &MediaDescription,
    remote_media: &MediaDescription,
) -> Option<(CryptoContext, CryptoContext)> {
    for remote_crypto in &remote_media.cryptos() {
        for local_crypto in &local_media.cryptos() {
            if local_crypto.suite == remote_crypto.suite {
                return Some((
                    CryptoContext::new(
                        stream_id.to_string(),
                        local_crypto.master_key.clone(),
                        local_crypto.salt.clone(),
                        local_crypto.tag_len,
                    )
                    .await
                    .ok()?,
                    CryptoContext::new(
                        stream_id.to_string(),
                        remote_crypto.master_key.clone(),
                        remote_crypto.salt.clone(),
                        remote_crypto.tag_len,
                    )
                    .await
                    .ok()?,
                ));
            }
        }
    }
    None
}

/// Persist SRTP crypto parameters for a stream in Redis.
pub async fn set_crypto(kind: CryptoKind, id: &str, crypto: &Crypto) -> Result<()> {
    REDIS
        .hsetex(
            &format!("nebula:media_stream:{}", id),
            &kind.to_string(),
            &crypto.to_string(),
        )
        .await?;
    Ok(())
}

/// Load SRTP crypto parameters for a stream from Redis.
async fn get_crypto(kind: CryptoKind, id: &str) -> Option<CryptoContext> {
    let crypto: String = REDIS
        .hget(&format!("nebula:media_stream:{}", id), &kind.to_string())
        .await
        .ok()?;
    let crypto = Crypto::from_str(&crypto).ok()?;
    CryptoContext::new(
        id.to_string(),
        crypto.master_key,
        crypto.salt,
        crypto.tag_len,
    )
    .await
    .ok()
}

/// Fetch the local SSRC we assigned for this stream (if any).
async fn get_local_ssrc(id: &str) -> Option<u32> {
    let ssrc: u32 = REDIS
        .hget(&format!("nebula:media_stream:{}", id), "local_ssrc")
        .await
        .ok()?;
    Some(ssrc)
}

/// Fetch the negotiated incoming SSRC for this stream (if any).
async fn get_ssrc(id: &str) -> Option<u32> {
    let ssrc: u32 = REDIS
        .hget(&format!("nebula:media_stream:{}", id), "ssrc")
        .await
        .ok()?;
    Some(ssrc)
}

/// Get the media description by SDP direction (Offer/Answer).
async fn get_media_by_directoin(
    direction: SdpType,
    id: &str,
) -> Option<MediaDescription> {
    let media_string: String = REDIS
        .hget(
            &format!("nebula:media_stream:{}", id),
            &direction.to_string(),
        )
        .await
        .unwrap_or("".to_string());
    MediaDescription::from_str(&media_string).ok()
}

/// Get the media description by kind (Local/Remote) from Redis.
async fn get_media(kind: MediaDescKind, id: &str) -> Option<MediaDescription> {
    let media_string: String = REDIS
        .hget(&format!("nebula:media_stream:{}", id), &kind.to_string())
        .await
        .unwrap_or("".to_string());
    MediaDescription::from_str(&media_string).ok()
}

/// Save and broadcast the updated peer IP:port for a stream.
pub async fn set_peer_addr(
    id: &str,
    peer_ip: Ipv4Addr,
    peer_port: u16,
) -> Result<()> {
    let addr = &format!("{}:{}", peer_ip, peer_port);
    REDIS
        .hsetex(&format!("nebula:media_stream:{}", id), "peer_addr", addr)
        .await?;
    let _ = MEDIA_SERVICE
        .media_rpc_client
        .set_peer_addr(id.to_string(), peer_ip, peer_port)
        .await;
    Ok(())
}

/// Resolve the current peer address for a stream; defaults to 127.0.0.1:50000.
async fn get_peer_addr(id: &str) -> Result<SocketAddr> {
    let addr: String = REDIS
        .hget(&format!("nebula:media_stream:{}", id), "peer_addr")
        .await?;
    addr.parse::<SocketAddr>()
        .or_else(|_| "127.0.0.1:50000".parse::<SocketAddr>())
        .map_err(|e| anyhow!(e))
}

/// Mix two PCM samples with soft clipping to avoid distortion.
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

/// Soft-clip helper for mixing; bends peaks smoothly.
fn mix_normalization(x: f64) -> f64 {
    let a = (1.0 - THRESHOLD) / (1.0 + ALPHA).ln();
    let b = ALPHA / (2.0 - THRESHOLD);
    THRESHOLD + a * (1.0 + b * (x - THRESHOLD)).ln()
}

/// Map a DTMF character to its event code.
fn dtmf_byte(dtmf: &str) -> Option<u8> {
    for (i, v) in DTMF_DIGITS.iter().enumerate() {
        if &dtmf == v {
            return Some(i as u8);
        }
    }
    None
}

/// Report the current video allocations (RID/SSRC/size/rate) to switchboard.
async fn report_video_allocations(
    member: String,
    room: String,
    allocated_videos: HashMap<Uuid, AllocatedVideo>,
) {
    let mut video_allocations = HashMap::new();
    for (uuid, video) in allocated_videos {
        let channel: String = REDIS
            .hget(&format!("nebula:media_stream:{uuid}"), "channel")
            .await
            .unwrap_or_default();
        video_allocations.insert(
            channel,
            VideoAllocation {
                ssrc: video.ssrc,
                rid: video.rid,
                rate: video.rate,
                width: video.size.width,
                height: video.size.height,
            },
        );
    }
    let _ = MEDIA_SERVICE
        .switchboard_rpc_client
        .notify_video_allocations(
            &MEDIA_SERVICE.config.switchboard_host,
            room,
            member,
            video_allocations,
        )
        .await;
}

/// Receive next stream message with a timeout; errors if the channel is gone.
async fn receive_msg(
    receiver: &mut UnboundedReceiver<StreamReceiverMessage>,
    redis_key: &str,
) -> Result<Option<StreamReceiverMessage>> {
    let result = tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(60)) => {
            if REDIS.exists(redis_key).await.unwrap_or(false)
                && !is_channel_hangup(&REDIS.hget::<String>(redis_key, "channel").await.unwrap_or_default()).await {
                return Ok(None);
            }
            return Err(anyhow!("channel is hangup"));
        }
        result = receiver.recv() => {
            result
        }
    };

    let msg = result.ok_or(anyhow!("receiver closed"))?;
    Ok(Some(msg))
}

/// Construct a LAME MP3 encoder with the given sample rate and channels.
async fn new_lame(rate: u32, channel: u8) -> Result<Arc<BlockMutex<Lame>>> {
    nebula_task::spawn_task(move || -> Result<Arc<BlockMutex<Lame>>> {
        let mut lame = Lame::new()?;
        lame.set_sample_rate(rate)?;
        lame.set_quality(1)?;
        lame.set_kilobitrate(16)?;
        lame.set_channels(channel)?;
        lame.init_params()?;
        let lame = Arc::new(BlockMutex::new(lame));
        Ok(lame)
    })
    .await?
}

pub struct VideoStreamDestination {
    uuid: Uuid,
    channel: String,
    webrtc: bool,
    crypto: Option<CryptoContext>,
    peer_transport: PeerTransport,
    rtp_packet: RtpPacket,
    ts_len: u32,
    codec: Option<Arc<BlockMutex<Box<dyn VideoCodec>>>>,
}

impl VideoStreamDestination {
    /// Build a destination peer for video (peer-to-peer). Negotiates RTP map and SRTP, sets up transport.
    async fn new(
        uuid: Uuid,
        channel: String,
        pc_udp: Arc<UdpSocket>,
        stream_sender_pool: UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<Self> {
        let id = uuid.to_string();
        let offer_media = get_media_by_directoin(SdpType::Offer, &id)
            .await
            .ok_or(anyhow!("no local media"))?;
        let answer_media = get_media_by_directoin(SdpType::Answer, &id)
            .await
            .ok_or(anyhow!("no reomte media"))?;
        let rtpmap = offer_media
            .negotiate_rtpmap(&answer_media)
            .ok_or(anyhow!("no rtmap"))?;

        let crypto = if offer_media.is_webrtc() {
            get_crypto(CryptoKind::Local, &id).await
        } else {
            let local_media = get_media(MediaDescKind::Local, &id)
                .await
                .ok_or(anyhow!("no local media"))?;
            let remote_media = get_media(MediaDescKind::Remote, &id)
                .await
                .ok_or(anyhow!("no remote media"))?;
            negotiate_crypto(&id, &local_media, &remote_media)
                .await
                .map(|r| r.0)
        };

        let webrtc = offer_media.is_webrtc();
        let peer_transport =
            new_peer_transport(uuid, pc_udp, stream_sender_pool.clone()).await?;

        let codec = match rtpmap.name {
            PayloadType::H264 => {
                nebula_task::spawn_task(move || -> Result<Box<dyn VideoCodec>> {
                    H264::new().map(|codec| Box::new(codec) as Box<dyn VideoCodec>)
                })
                .await
                .ok()
                .and_then(|r| r.ok())
            }
            PayloadType::VP8 => None,
            _ => None,
        };
        let codec = codec.map(|codec| Arc::new(BlockMutex::new(codec)));

        let mut rtp_packet = RtpPacket::new();
        rtp_packet.set_payload_type(rtpmap.type_number);
        rtp_packet.set_ssrc(uuid.as_fields().0);

        Ok(Self {
            uuid,
            channel,
            webrtc,
            crypto,
            peer_transport,
            rtp_packet,
            codec,
            ts_len: 3000,
        })
    }

    /// Send a decoded video frame: encode (if available), packetize to RTP, mark last, encrypt if SRTP, and transmit.
    async fn send_frame(&mut self, frame: ffmpeg_next::frame::Video) -> Result<()> {
        if self.webrtc && self.crypto.is_none() {
            // if webrtc but crypto hasn't been initiated, we halt the video sending
            return Ok(());
        }
        if let Some(codec) = self.codec.clone() {
            let payloads =
                nebula_task::spawn_task(move || -> Result<Vec<Vec<u8>>> {
                    codec.lock().encode(frame)
                })
                .await??;

            if !payloads.is_empty() {
                self.rtp_packet.next(self.ts_len);

                let mut payloads = payloads.into_iter().peekable();
                while let Some(payload) = payloads.next() {
                    let mut current_rtp_packet = self.rtp_packet.clone();
                    current_rtp_packet.set_paylod(&payload);

                    if payloads.peek().is_none() {
                        // this is the last payload
                        current_rtp_packet.set_marker(true);
                    } else {
                        self.rtp_packet.next_seq();
                    }

                    let current_rtp_packet =
                        self.encrypt_rtp(current_rtp_packet).await?;
                    self.peer_transport.send(current_rtp_packet.data()).await?;
                }
            }
        }
        Ok(())
    }

    async fn encrypt_rtp(&mut self, mut rtp_packet: RtpPacket) -> Result<RtpPacket> {
        if let Some(crypto) = self.crypto.as_mut() {
            let roc = crypto.update_roc(rtp_packet.data()).await;
            let crypto = crypto.state.clone();
            nebula_task::spawn_task(move || -> Result<RtpPacket> {
                crypto.encrypt_rtp(roc, &mut rtp_packet)?;
                Ok(rtp_packet)
            })
            .await?
        } else {
            Ok(rtp_packet)
        }
    }

    /// React to video RPCs for the destination (DTLS done, peer address changes, keyframe requests).
    async fn process_rpc_msg(&mut self, rpc_msg: RpcMessage) -> Result<()> {
        match &rpc_msg.method {
            RpcMethod::DtlsDone => {
                if self.webrtc && self.crypto.is_none() {
                    self.crypto =
                        get_crypto(CryptoKind::Local, &self.uuid.to_string()).await;
                }
            }
            RpcMethod::SetPeerAddr => {
                let peer: StreamPeer = serde_json::from_value(rpc_msg.params)?;
                self.peer_transport
                    .set_peer_addr(peer.peer_ip, peer.peer_port);
            }
            RpcMethod::RequestKeyframe => {
                let codec = self.codec.clone();
                if let Some(codec) = codec {
                    nebula_task::spawn_task(move || {
                        codec.lock().request_keyframe();
                    })
                    .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, time::SystemTime};

    use codec::vp8::PixelSize;
    use nebula_rpc::message::IncomingVideoInfo;
    use uuid::Uuid;

    use crate::stream::{VideoSourceState, VideoStreamSource};

    #[test]
    fn test_sorted_sources() {
        let mut sources = vec![
            (
                Uuid::from_u128(0),
                VideoSourceState {
                    uuid: Uuid::from_u128(0),
                    last_key_speaker: 1,
                    requested_size: PixelSize {
                        width: 600,
                        height: 400,
                    },
                    layers: VecDeque::from([(
                        0,
                        IncomingVideoInfo {
                            uuid: Uuid::from_u128(0),
                            ssrc: 0,
                            size: PixelSize {
                                width: 600,
                                height: 400,
                            },
                            rate: Some(100),
                            rid: "h".to_string(),
                        },
                    )]),
                    is_screen_share: true,
                },
            ),
            (
                Uuid::from_u128(1),
                VideoSourceState {
                    uuid: Uuid::from_u128(1),
                    last_key_speaker: 10,
                    requested_size: PixelSize {
                        width: 600,
                        height: 400,
                    },
                    layers: VecDeque::from([
                        (
                            SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap(),
                            IncomingVideoInfo {
                                uuid: Uuid::from_u128(1),
                                ssrc: 0,
                                size: PixelSize {
                                    width: 300,
                                    height: 200,
                                },
                                rate: Some(500000),
                                rid: "l".to_string(),
                            },
                        ),
                        (
                            SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap(),
                            IncomingVideoInfo {
                                uuid: Uuid::from_u128(1),
                                ssrc: 1,
                                size: PixelSize {
                                    width: 600,
                                    height: 400,
                                },
                                rate: Some(1000000),
                                rid: "h".to_string(),
                            },
                        ),
                    ]),
                    is_screen_share: false,
                },
            ),
        ];
        VideoStreamSource::sort_video_sources(&mut sources);
        assert_eq!(sources[0].0, Uuid::from_u128(0));
        assert_eq!(sources[1].0, Uuid::from_u128(1));
        let allocations = VideoStreamSource::loop_allocatoins(sources, 600000);
        assert_eq!(allocations.get(&Uuid::from_u128(0)).unwrap().rid, "h");
        assert_eq!(allocations.get(&Uuid::from_u128(1)).unwrap().rid, "l");

        let mut sources = vec![
            (
                Uuid::from_u128(0),
                VideoSourceState {
                    uuid: Uuid::from_u128(0),
                    last_key_speaker: 0,
                    requested_size: PixelSize {
                        width: 600,
                        height: 400,
                    },
                    layers: VecDeque::new(),
                    is_screen_share: false,
                },
            ),
            (
                Uuid::from_u128(1),
                VideoSourceState {
                    uuid: Uuid::from_u128(1),
                    last_key_speaker: 0,
                    requested_size: PixelSize {
                        width: 600,
                        height: 500,
                    },
                    layers: VecDeque::new(),
                    is_screen_share: false,
                },
            ),
        ];
        VideoStreamSource::sort_video_sources(&mut sources);
        assert_eq!(sources[0].0, Uuid::from_u128(1));
        assert_eq!(sources[1].0, Uuid::from_u128(0));

        let mut sources = vec![
            (
                Uuid::from_u128(0),
                VideoSourceState {
                    uuid: Uuid::from_u128(0),
                    last_key_speaker: 10,
                    requested_size: PixelSize {
                        width: 600,
                        height: 400,
                    },
                    layers: VecDeque::new(),
                    is_screen_share: false,
                },
            ),
            (
                Uuid::from_u128(1),
                VideoSourceState {
                    uuid: Uuid::from_u128(1),
                    last_key_speaker: 9,
                    requested_size: PixelSize {
                        width: 600,
                        height: 500,
                    },
                    layers: VecDeque::new(),
                    is_screen_share: false,
                },
            ),
        ];
        VideoStreamSource::sort_video_sources(&mut sources);
        assert_eq!(sources[0].0, Uuid::from_u128(0));
        assert_eq!(sources[1].0, Uuid::from_u128(1));
    }
}
