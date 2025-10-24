use std::{
    collections::HashMap,
    fmt::{self, Display},
    str::FromStr,
};

use anyhow::{anyhow, Error, Result};
use codec::{pcma::PCMA, pcmu::PCMU, Codec, G722Codec, Opus};
use nebula_redis::{
    redis::{self, RedisError},
    REDIS, DistributedMutex,
};
use nebula_utils::uuid_v5;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use strum_macros::{self, EnumString};
use thiserror::Error;
use uuid::Uuid;

use crate::{sdp::MediaMode, server::MEDIA_SERVICE, stream::LOCAL_RTP_PEER_PORT};

pub const INTERNAL_AUDIO_ORDER: [PayloadType; 4] = [
    PayloadType::Opus,
    PayloadType::G722,
    PayloadType::PCMA,
    PayloadType::PCMU,
];
pub const EXTERNAL_AUDIO_ORDER: [PayloadType; 4] = [
    PayloadType::PCMA,
    PayloadType::PCMU,
    PayloadType::G722,
    PayloadType::Opus,
];
const VIDEO_RTPMAP_ORDER: [PayloadType; 2] = [PayloadType::VP8, PayloadType::H264];

use crate::{
    peer_connection::PEER_CONNECTION_PORT,
    sdp::{PayloadType, Rtpmap},
    session::{get_cert, get_cert_fingerprint},
};

#[derive(
    strum_macros::Display, EnumString, Debug, Eq, PartialEq, Clone, Copy, Hash,
)]
pub enum SessionDescKind {
    #[strum(serialize = "local_session_desciption")]
    Local,
    #[strum(serialize = "remote_session_description")]
    Remote,
}

#[derive(Clone, PartialEq, Debug)]
pub enum SdpType {
    Offer,
    Answer,
}

#[derive(
    strum_macros::Display, EnumString, Debug, Eq, PartialEq, Clone, Copy, Hash,
)]
pub enum MediaDescKind {
    #[strum(serialize = "local_media_desciption")]
    Local,
    #[strum(serialize = "remote_media_description")]
    Remote,
}

#[derive(
    strum_macros::Display, EnumString, Debug, Eq, PartialEq, Clone, Copy, Hash,
)]
pub enum CryptoKind {
    #[strum(serialize = "local_crypto")]
    Local,
    #[strum(serialize = "remote_crypto")]
    Remote,
}

#[derive(strum_macros::Display, EnumString, Debug, Eq, PartialEq, Clone, Hash)]
pub enum MediaType {
    #[strum(serialize = "audio")]
    Audio,
    #[strum(serialize = "video")]
    Video,
}

#[derive(Debug, Error)]
pub enum SdpError {
    #[error("invalid sdp")]
    InvalidSdp,

    #[error("no audio")]
    NoAudio,

    #[error("negotiation failed")]
    NegotiationFailed,
}

pub struct AttrKey {}

impl AttrKey {
    pub const SSRC_GROUP: &'static str = "ssrc-group";
    pub const SSRC: &'static str = "ssrc";
    pub const MSID: &'static str = "msid";
}

impl Default for MediaType {
    fn default() -> Self {
        MediaType::Audio
    }
}

#[derive(Deserialize, Serialize, Default, Debug, Clone)]
pub struct Ice {
    pub ufrag: String,
    pub pwd: String,
    pub candidates: Vec<Candidate>,
}

#[derive(Deserialize, Serialize, Default, Debug, Clone)]
pub struct Dtls {
    pub fingerprint: String,
    pub hash: String,
    pub setup: String,
}

#[derive(Deserialize, Serialize, Default, Debug, Clone)]
pub struct Candidate {
    pub foundation: String,
    pub component: i32,
    pub priority: i32,
    pub addr: String,
    pub port: u16,
    pub kind: String,
}

#[derive(Deserialize, Serialize, Default, Debug, Clone)]
pub struct Crypto {
    pub tag: String,
    pub master_key: Vec<u8>,
    pub salt: Vec<u8>,
    pub suite: String,
    pub tag_len: usize,
}

impl FromStr for Crypto {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(" ").collect();
        if parts.len() != 3 {
            return Err(SdpError::InvalidSdp)?;
        }
        let tag = parts[0].to_string();
        let suite = parts[1].to_string();
        let tag_len =
            parts[1].rsplit("_").next().unwrap_or("").parse::<usize>()? / 8;
        let key = base64::decode(&parts[2].split("|").next().unwrap_or("")[7..])?;
        if key.len() != 30 {
            return Err(SdpError::InvalidSdp)?;
        }
        Ok(Crypto {
            tag,
            suite,
            tag_len,
            master_key: key[..16].to_vec(),
            salt: key[16..].to_vec(),
        })
    }
}

impl Display for Crypto {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} {} inline:{}",
            self.tag,
            self.suite,
            base64::encode([&self.master_key[..], &self.salt[..]].concat())
        )
    }
}

#[derive(Clone, Debug)]
pub struct MediaStreamTrack {
    id: String,
    stream_id: String,
    pub ssrc: u32,
    pub media_type: MediaType,
}

#[derive(Default, Debug, Clone)]
pub struct Origin {
    username: String,
    session_id: String,
    session_version: String,
    network_type: String,
    addr_type: String,
    pub addr: String,
}

impl Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "o={} {} {} {} {} {}",
            &self.username,
            &self.session_id,
            &self.session_version,
            &self.network_type,
            &self.addr_type,
            &self.addr
        )
    }
}

impl FromStr for Origin {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(" ").collect();
        if parts.len() != 6 {
            Err(anyhow!("invalid sdp"))?;
        }
        Ok(Origin {
            username: parts[0].to_string(),
            session_id: parts[1].to_string(),
            session_version: parts[2].to_string(),
            network_type: parts[3].to_string(),
            addr_type: parts[4].to_string(),
            addr: parts[5].to_string(),
        })
    }
}

#[derive(Default, Debug, Clone)]
pub struct Connection {
    network_type: String,
    addr_type: String,
    pub addr: String,
}

impl Display for Connection {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "c={} {} {}",
            &self.network_type, &self.addr_type, &self.addr
        )
    }
}

#[derive(Debug, Clone)]
pub struct Attribute {
    key: String,
    value: Option<String>,
}

impl Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(value) = self.value.as_ref() {
            write!(f, "a={}:{}\r\n", self.key, value)
        } else {
            write!(f, "a={}\r\n", self.key)
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct MediaDescription {
    pub media_type: MediaType,
    pub port: u16,
    pub num_ports: u32,
    pub proto: String,
    pub payloads: Vec<String>,
    pub attributes: Vec<Attribute>,
}

impl Display for MediaDescription {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "m={} ", &self.media_type)?;
        write!(f, "{}", self.port)?;
        if self.num_ports > 0 {
            write!(f, "/{}", self.num_ports)?;
        }
        write!(f, " {} {}\r\n", &self.proto, self.payloads.join(" "))?;
        for attr in &self.attributes {
            write!(f, "{}", attr)?;
        }
        Ok(())
    }
}

impl FromStr for MediaDescription {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut split = s.split("\r\n");
        let first_line = split.next().ok_or(anyhow!("invalid media description"))?;
        if !first_line.starts_with("m=") {
            return Err(SdpError::InvalidSdp)?;
        }
        let mut media = MediaDescription::parse_media(&first_line[2..])?;
        for line in split {
            if line.len() < 2 {
                continue;
            }
            let desc_type = &line[0..1];
            let data = &line[2..];
            match desc_type.to_lowercase().as_ref() {
                "a" => {
                    let parts: Vec<&str> = data.splitn(2, ":").collect();
                    let key = parts[0].to_string();
                    let value = parts.get(1).map(|v| v.to_string());
                    let attr = Attribute { key, value };
                    media.attributes.push(attr);
                }
                _ => {}
            }
        }
        Ok(media)
    }
}

impl MediaDescription {
    /// Parse an SDP `m=` section into a `MediaDescription` (media kind, port, proto, payloads, attrs).
    pub fn parse_media(s: &str) -> Result<MediaDescription> {
        let parts: Vec<&str> = s.splitn(4, " ").collect();
        if parts.len() < 3 {
            Err(SdpError::InvalidSdp)?;
        }
        let mut media = MediaDescription {
            media_type: MediaType::from_str(parts[0].to_lowercase().as_ref())?,
            proto: parts[2].to_string(),
            ..Default::default()
        };
        if parts.len() == 4 {
            media.payloads = parts[3]
                .split(" ")
                .collect::<Vec<&str>>()
                .iter()
                .map(|s| s.to_string())
                .collect();
        }

        let port_parts: Vec<&str> = parts[1].split("/").collect();
        media.port = port_parts[0].parse()?;
        if port_parts.len() == 2 {
            media.num_ports = port_parts[1].parse()?;
        }

        Ok(media)
    }

    /// Build a minimal RTP/AVP audio description with optional `mid` and provided `rtpmaps`.
    fn create_basic_audio(
        port: u16,
        mid: Option<String>,
        rtpmaps: Vec<Rtpmap>,
    ) -> MediaDescription {
        let mut media = MediaDescription {
            media_type: MediaType::Audio,
            port,
            num_ports: 0,
            proto: "RTP/AVP".to_string(),
            payloads: Vec::new(),
            attributes: Vec::new(),
        };
        for rtpmap in rtpmaps {
            media.insert_rtpmap(&rtpmap);
        }
        if let Some(mid) = mid {
            media.insert_mid(mid);
        }
        media
    }

    /// Create a WebRTC video `MediaDescription` (SAVPF) with SSRC, MID, msid, and rtpmap lines.
    pub fn create_video(
        rtpmaps: Vec<Rtpmap>,
        ssrc: u32,
        mid: String,
        track_id: String,
    ) -> MediaDescription {
        let port = PEER_CONNECTION_PORT;
        let proto = "UDP/TLS/RTP/SAVPF".to_string();
        let mut media = MediaDescription {
            media_type: MediaType::Video,
            port,
            num_ports: 0,
            proto,
            payloads: Vec::new(),
            attributes: Vec::new(),
        };
        for rtpmap in rtpmaps {
            media.insert_rtpmap(&rtpmap);
        }
        media.insert_mid(mid);
        media.insert_attribute("rtcp-mux".to_string(), None);
        // media.insert_attribute("rtcp-rsize".to_string(), None);
        media.insert_msid("-".to_string(), track_id.clone());

        media.insert_ssrc(ssrc, "-".to_string(), track_id);
        media
    }

    /// Create an audio `MediaDescription` for WebRTC or plain RTP, picking port/proto and crypto as needed.
    async fn create_audio(
        channel_id: &str,
        webrtc: bool,
        rtpmaps: Vec<Rtpmap>,
        encryption: bool,
    ) -> Result<MediaDescription> {
        let port = if webrtc {
            PEER_CONNECTION_PORT
        } else {
            new_session_port(channel_id).await?
        };
        let proto = if webrtc {
            "UDP/TLS/RTP/SAVPF".to_string()
        } else if encryption {
            "RTP/SAVP".to_string()
        } else {
            "RTP/AVP".to_string()
        };
        let mut media = MediaDescription {
            media_type: MediaType::Audio,
            port,
            num_ports: 0,
            proto,
            payloads: Vec::new(),
            attributes: Vec::new(),
        };
        for rtpmap in rtpmaps {
            media.insert_rtpmap(&rtpmap);
        }
        if webrtc {
            media.insert_mid("audio".to_string());
            media.insert_attribute("rtcp-mux".to_string(), None);
            let stream_id = "-".to_string();
            let track_id = nebula_utils::uuid();
            media.insert_msid(stream_id.clone(), track_id.clone());

            let ssrc = new_ssrc();
            media.insert_ssrc(ssrc, stream_id, track_id);
        }
        if encryption && !webrtc {
            media.insert_attribute(
                "crypto".to_string(),
                Some(format!(
                    "1 AES_CM_128_HMAC_SHA1_80 inline:{}",
                    base64::encode(nebula_utils::rand_bytes(30))
                )),
            );
        }
        Ok(media)
    }

    /// Set audio packetization time (in ms) as an SDP `ptime` attribute.
    pub fn set_ptime(&mut self, ptime: usize) {
        self.insert_attribute("ptime".to_string(), Some(ptime.to_string()));
    }

    /// Get `ptime` if present, defaulting to 20ms when missing or invalid.
    pub fn ptime(&self) -> usize {
        self.attribute("ptime")
            .and_then(|ptime| ptime.parse::<usize>().ok())
            .unwrap_or(20)
    }

    /// Return the value of the first attribute with `key` (session-local to this media).
    pub fn attribute(&self, key: &str) -> Option<String> {
        attribute(&self.attributes, key)
    }

    /// True if this media has any `a={key}` attribute, regardless of value.
    pub fn has_attribute(&self, key: &str) -> bool {
        for attr in &self.attributes {
            if &attr.key == key {
                return true;
            }
        }
        false
    }

    /// Extract minimal ICE credentials from media attributes (`ice-ufrag`/`ice-pwd`).
    pub fn ice(&self) -> Option<Ice> {
        extract_ice(&self.attributes)
    }

    /// Extract DTLS info (`setup`, `fingerprint`) from media attributes.
    pub fn dtls(&self) -> Option<Dtls> {
        extract_dtls(&self.attributes)
    }

    /// Derive a stable UUID for this media using `channel:(mid|port)` via `uuid_v5`.
    pub fn uuid(&self, channel: &str) -> Uuid {
        let id = if let Some(mid) = self.mid() {
            format!("{}:{}", &channel, mid)
        } else {
            format!("{}:{}", &channel, self.port)
        };
        uuid_v5(&id)
    }

    /// Return SDP `mid` if set for this media.
    pub fn mid(&self) -> Option<String> {
        self.attribute("mid")
    }

    /// Parse all `a=crypto` lines into `Crypto` entries (ignoring invalid lines).
    pub fn cryptos(&self) -> Vec<Crypto> {
        let mut cryptos = Vec::new();
        for attr in &self.attributes {
            match attr.key.as_str() {
                "crypto" => {
                    let value = attr.value.clone().unwrap_or("".to_string());
                    if let Ok(crypto) = Crypto::from_str(&value) {
                        cryptos.push(crypto);
                    }
                }
                _ => {}
            }
        }
        cryptos
    }

    /// Find an `Rtpmap` by payload type name (e.g., Opus, G722) using payloads + attributes.
    pub fn get_rtpmap(&self, name: &PayloadType) -> Option<Rtpmap> {
        for pt in &self.payloads {
            if let Some(rtpmap) = self.pt_rtpmap(pt) {
                if &rtpmap.name == name {
                    return Some(rtpmap);
                }
            }
        }
        None
    }

    /// Return the DTMF `telephone-event` rtpmap if present.
    pub fn dtmf_rtpmap(&self) -> Option<Rtpmap> {
        for pt in &self.payloads {
            if let Some(rtpmap) = self.pt_rtpmap(&pt) {
                if rtpmap.name == PayloadType::TelephoneEvent {
                    return Some(rtpmap);
                }
            }
        }
        None
    }

    /// Resolve an `Rtpmap` by dynamic PT number, using defaults and matching rtpmap/fmtp/rtcp-fb lines.
    pub fn pt_rtpmap(&self, pt: &str) -> Option<Rtpmap> {
        Rtpmap::pt_default(pt).or_else(|| {
            let mut rtpmap = None;
            for attr in &self.attributes {
                match attr.key.as_str() {
                    "rtpmap" => {
                        let value = attr.value.clone().unwrap_or("".to_string());
                        let parts: Vec<&str> = value.splitn(2, " ").collect();
                        if parts.len() == 2 && parts[0] == pt {
                            if let Ok(r) = Rtpmap::from_str(&value) {
                                if r.type_number.to_string() == pt {
                                    rtpmap = Some(r);
                                }
                            }
                        }
                    }
                    "fmtp" => {
                        let value = attr.value.clone().unwrap_or("".to_string());
                        let parts: Vec<&str> = value.splitn(2, " ").collect();
                        if parts.len() == 2 && parts[0] == pt {
                            if let Some(rtpmap) = rtpmap.as_mut() {
                                rtpmap.fmtp = parts[1].to_string();
                            }
                        }
                    }
                    "rtcp-fb" => {
                        let value = attr.value.clone().unwrap_or("".to_string());
                        let parts: Vec<&str> = value.splitn(2, " ").collect();
                        if parts.len() == 2 && parts[0] == pt {
                            if let Some(rtpmap) = rtpmap.as_mut() {
                                rtpmap.rtcp_feedbacks.push(parts[1].to_string());
                            }
                        }
                    }
                    _ => {}
                };
            }
            rtpmap
        })
    }

    /// Get media direction (`sendrecv` by default) from attributes like `sendonly`/`recvonly`.
    pub fn mode(&self) -> MediaMode {
        for attribute in &self.attributes {
            if let Ok(mode) = MediaMode::from_str(&attribute.key) {
                return mode;
            }
        }
        MediaMode::Sendrecv
    }

    /// True if this media uses WebRTC profile (`UDP/TLS/RTP/SAVPF`).
    pub fn is_webrtc(&self) -> bool {
        self.proto == "UDP/TLS/RTP/SAVPF"
    }

    /// Build an answer `MediaDescription` for this offer, choosing ports, MID, (D)TLS/ICE, and negotiated codecs.
    pub async fn create_answer(
        &self,
        channel_id: &str,
        peer_rtpmaps: Option<Vec<Rtpmap>>,
        internal_sdp: bool,
    ) -> Result<MediaDescription> {
        let port = if self.port == LOCAL_RTP_PEER_PORT {
            LOCAL_RTP_PEER_PORT
        } else if self.proto == "UDP/TLS/RTP/SAVPF" {
            PEER_CONNECTION_PORT
        } else {
            new_session_port(channel_id).await?
        };
        let mut media = MediaDescription {
            media_type: self.media_type.clone(),
            port,
            num_ports: 0,
            proto: self.proto.clone(),
            payloads: Vec::new(),
            attributes: Vec::new(),
        };
        if let Some(mid) = self.mid() {
            media.attributes.push(Attribute {
                key: "mid".to_string(),
                value: Some(mid),
            });
        }
        if self.proto == "UDP/TLS/RTP/SAVPF" {
            media.insert_attribute("rtcp-mux".to_string(), None);
            // if self.has_attribute("rtcp-rsize") {
            //     media.insert_attribute("rtcp-resize".to_string(), None);
            // }
            let stream_id = "-".to_string();
            let track_id = nebula_utils::uuid();
            media.insert_msid(stream_id.clone(), track_id.clone());

            let ssrc = new_ssrc();
            media.insert_ssrc(ssrc, stream_id, track_id);
        }

        let mut rtpmaps = Vec::new();
        if let Some(rtpmap) = self.answer_rtpmap(peer_rtpmaps, internal_sdp) {
            rtpmaps.push(rtpmap);
        }
        if let Some(rtpmap) = self.dtmf_rtpmap() {
            rtpmaps.push(rtpmap);
        }

        for rtpmap in &rtpmaps {
            media.insert_rtpmap(rtpmap);
        }

        for attr in self.attributes.iter() {
            if attr.key == "crypto" {
                let value = attr.value.clone().unwrap_or("".to_string());
                let parts: Vec<&str> = value.split(" ").collect();
                if parts.len() == 3 {
                    if parts[1] == "AES_CM_128_HMAC_SHA1_80" {
                        media.insert_attribute(
                            "crypto".to_string(),
                            Some(format!(
                                "{} AES_CM_128_HMAC_SHA1_80 inline:{}",
                                parts[0],
                                base64::encode(nebula_utils::rand_bytes(30))
                            )),
                        );
                        break;
                    }
                }
            }
        }

        if self.media_type == MediaType::Audio {
            let ptime = self.ptime();
            if ptime != 20 {
                media.set_ptime(ptime);
            }
        }

        if self.media_type == MediaType::Video {
            media.insert_attribute("recvonly".to_string(), None);
        }

        Ok(media)
    }

    /// Add ICE `ufrag`/`pwd` and host candidates with this media's port.
    pub fn insert_ice(&mut self, ice: &Ice) {
        self.insert_attribute("ice-ufrag".to_string(), Some(ice.ufrag.clone()));
        self.insert_attribute("ice-pwd".to_string(), Some(ice.pwd.clone()));
        for candidate in &ice.candidates {
            self.insert_attribute(
                "candidate".to_string(),
                Some(format!(
                    "{} {} udp {} {} {} typ {}",
                    candidate.foundation,
                    candidate.component,
                    candidate.priority,
                    candidate.addr,
                    self.port,
                    candidate.kind,
                )),
            );
        }
    }

    /// Populate `a=ssrc` attributes (cname, msid, labels) for a track.
    fn insert_ssrc(&mut self, ssrc: u32, stream_id: String, track_id: String) {
        self.insert_attribute(
            "ssrc".to_string(),
            Some(format!("{} cname:{}", ssrc, nebula_utils::rand_string(16))),
        );
        self.insert_attribute(
            "ssrc".to_string(),
            Some(format!("{} msid:{} {}", ssrc, stream_id, track_id)),
        );
        self.insert_attribute(
            "ssrc".to_string(),
            Some(format!("{} mslabel:{}", ssrc, stream_id)),
        );
        self.insert_attribute(
            "ssrc".to_string(),
            Some(format!("{} label:{}", ssrc, track_id)),
        );
    }

    /// Set the `mid` for this media.
    pub fn insert_mid(&mut self, mid: String) {
        self.insert_attribute("mid".to_string(), Some(mid));
    }

    /// Add `msid` linking stream and track identifiers.
    fn insert_msid(&mut self, stream_id: String, track_id: String) {
        self.insert_attribute(
            "msid".to_string(),
            Some(format!("{} {}", stream_id, track_id)),
        );
    }

    /// Add DTLS fingerprint and setup role attributes.
    pub fn insert_dtls(&mut self, dtls: &Dtls) {
        self.insert_attribute(
            "fingerprint".to_string(),
            Some(format!("sha-256 {}", dtls.fingerprint.clone())),
        );
        self.insert_attribute("setup".to_string(), Some(dtls.setup.clone()));
    }

    /// Replace any existing direction with the provided mode (e.g., `sendrecv`).
    pub fn set_mode(&mut self, mode: MediaMode) {
        let mut new_attributes = Vec::new();
        for attribute in &self.attributes {
            if let Ok(mode) = MediaMode::from_str(&attribute.key) {
                continue;
            }
            new_attributes.push(attribute.clone());
        }
        self.attributes = new_attributes;
        self.insert_attribute(mode.to_string(), None);
    }

    /// Remove all rtpmap/fmtp pairs and reset payloads, preserving only non-codec attributes.
    pub fn clear_rtpmap(&mut self) {
        let mut new_attributes = Vec::new();
        for attribute in &self.attributes {
            if attribute.key == "rtpmap" || attribute.key == "fmtp" {
                continue;
            }
            new_attributes.push(attribute.clone());
        }
        self.payloads = Vec::new();
        self.attributes = new_attributes;
    }

    /// Append rtpmap/fmtp/rtcp-fb lines and payload type for a codec.
    pub fn insert_rtpmap(&mut self, rtpmap: &Rtpmap) {
        self.payloads.push(rtpmap.type_number.to_string());
        self.attributes.push(Attribute {
            key: "rtpmap".to_string(),
            value: Some(format!(
                "{} {}/{}{}",
                rtpmap.type_number,
                rtpmap.name,
                rtpmap.rate,
                if rtpmap.params != "" {
                    format!("/{}", rtpmap.params)
                } else {
                    "".to_string()
                }
            )),
        });
        if rtpmap.fmtp != "" {
            self.attributes.push(Attribute {
                key: "fmtp".to_string(),
                value: Some(format!("{} {}", rtpmap.type_number, rtpmap.fmtp)),
            });
        }
        for fb in &rtpmap.rtcp_feedbacks {
            self.attributes.push(Attribute {
                key: "rtcp-fb".to_string(),
                value: Some(format!("{} {}", rtpmap.type_number, fb)),
            });
        }
    }

    /// Append an arbitrary SDP attribute to this media.
    pub fn insert_attribute(&mut self, key: String, value: Option<String>) {
        self.attributes.push(Attribute { key, value });
    }

    /// Choose the first mutually supported codec between two media sections (by name).
    pub fn negotiate_rtpmap(
        &self,
        other_media: &MediaDescription,
    ) -> Option<Rtpmap> {
        for other_pt in &other_media.payloads {
            for pt in &self.payloads {
                if let Some(local_rtpmap) = self.pt_rtpmap(&pt) {
                    if let Some(remote_rtpmap) = other_media.pt_rtpmap(&other_pt) {
                        if local_rtpmap.name == remote_rtpmap.name {
                            return Some(local_rtpmap.clone());
                        }
                    }
                }
            }
        }
        None
    }

    /// Pick codec for answer: prefer peer's order, otherwise our preferred order per media type.
    fn answer_rtpmap(
        &self,
        peer_rtpmaps: Option<Vec<Rtpmap>>,
        internal_sdp: bool,
    ) -> Option<Rtpmap> {
        if let Some(rtpmaps) = peer_rtpmaps {
            for rtpmap in rtpmaps {
                if rtpmap.name != PayloadType::TelephoneEvent {
                    if let Some(rtpmap) = self.get_rtpmap(&rtpmap.name) {
                        return Some(rtpmap.clone());
                    }
                }
            }
        }
        let rtpmap_order = if self.media_type == MediaType::Audio {
            if internal_sdp {
                INTERNAL_AUDIO_ORDER.iter()
            } else {
                EXTERNAL_AUDIO_ORDER.iter()
            }
        } else {
            VIDEO_RTPMAP_ORDER.iter()
        };
        for rtpmap_name in rtpmap_order {
            if let Some(rtpmap) = self.get_rtpmap(rtpmap_name) {
                return Some(rtpmap.clone());
            }
        }
        None
    }
}

#[derive(Default, Debug, Clone)]
pub struct SessionDescription {
    pub version: String,
    pub origin: Origin,
    pub session_name: String,
    pub connection: Connection,
    pub time_start: i32,
    pub time_stop: i32,
    attributes: Vec<Attribute>,
    pub media_descriptions: Vec<MediaDescription>,
}

impl SessionDescription {
    /// Create a base SDP with local media IP, empty attributes, and no media lines.
    pub fn new() -> SessionDescription {
        SessionDescription {
            version: "0".to_string(),
            origin: Origin {
                username: "Nebula".to_string(),
                session_id: "14824".to_string(),
                session_version: "1".to_string(),
                network_type: "IN".to_string(),
                addr_type: "IP4".to_string(),
                addr: MEDIA_SERVICE.config.media_ip.to_string(),
            },
            session_name: "Nebula".to_string(),
            connection: Connection {
                network_type: "IN".to_string(),
                addr_type: "IP4".to_string(),
                addr: MEDIA_SERVICE.config.media_ip.to_string(),
            },
            time_start: 0,
            time_stop: 0,
            attributes: Vec::new(),
            media_descriptions: Vec::new(),
        }
    }

    /// Return the MID list from a `group:BUNDLE` attribute if present.
    pub fn bundle(&self) -> Option<Vec<String>> {
        for attr in &self.attributes {
            if &attr.key == "group"
                && attr
                    .value
                    .as_ref()
                    .map(|v| v.starts_with("BUNDLE "))
                    .unwrap_or(false)
            {
                return attr
                    .value
                    .as_ref()
                    .map(|v| v[7..].split(" ").map(|s| s.to_string()).collect());
            }
        }
        None
    }

    /// Convenience accessor for the media descriptions.
    pub fn medias(&self) -> &Vec<MediaDescription> {
        &self.media_descriptions
    }

    /// Return the first video media, if any.
    pub fn video(&self) -> Option<&MediaDescription> {
        for media in self.medias() {
            if media.media_type == MediaType::Video {
                return Some(media);
            }
        }
        None
    }

    /// Return the first audio media, if any.
    pub fn audio(&self) -> Option<&MediaDescription> {
        for media in self.medias() {
            if media.media_type == MediaType::Audio {
                return Some(media);
            }
        }
        None
    }

    /// Mutable access to the first audio media, if any.
    pub fn audio_mut(&mut self) -> Option<&mut MediaDescription> {
        for media in self.media_descriptions.iter_mut() {
            if media.media_type == MediaType::Audio {
                return Some(media);
            }
        }
        None
    }

    /// Build a MID->track map by reading `ssrc-group`, `msid`, and `ssrc` lines (skip RTX repair flows).
    pub fn media_stream_tracks(&self) -> HashMap<String, MediaStreamTrack> {
        let mut rtx_repair_flows = HashMap::new();
        let mut tracks = HashMap::new();

        for media in &self.media_descriptions {
            let mid = media.mid().unwrap_or("".to_string());
            if mid == "" {
                continue;
            }

            for attr in &media.attributes {
                let mut stream_id = "".to_string();
                let mut track_id = "".to_string();

                match attr.key.as_str() {
                    AttrKey::SSRC_GROUP => {
                        let value = attr.value.clone().unwrap_or("".to_string());
                        let parts: Vec<&str> = value.split(" ").collect();
                        if parts[0] == "FID" {
                            if parts.len() == 3 {
                                if let Ok(rtx_repair_flow) = parts[2].parse::<u32>()
                                {
                                    rtx_repair_flows.insert(rtx_repair_flow, true);
                                } else {
                                    continue;
                                }
                            }
                        }
                    }
                    AttrKey::MSID => {
                        let value = attr.value.clone().unwrap_or("".to_string());
                        let parts: Vec<&str> = value.split(" ").collect();
                        if parts.len() == 2 {
                            stream_id = parts[0].to_string();
                            track_id = parts[1].to_string();
                        }
                    }
                    AttrKey::SSRC => {
                        let value = attr.value.clone().unwrap_or("".to_string());
                        let parts: Vec<&str> = value.split(" ").collect();
                        let ssrc = if let Ok(ssrc) = parts[0].parse::<u32>() {
                            ssrc
                        } else {
                            continue;
                        };

                        if rtx_repair_flows.get(&ssrc).map(|v| *v).unwrap_or(false) {
                            continue;
                        }

                        if parts.len() == 3 && parts[1].starts_with("msid:") {
                            stream_id = parts[1]["msid:".len()..].to_string();
                            track_id = parts[2].to_string();
                        }

                        let track = MediaStreamTrack {
                            id: track_id,
                            stream_id,
                            ssrc,
                            media_type: media.media_type.clone(),
                        };
                        tracks.insert(mid.clone(), track);
                    }
                    _ => (),
                }
            }
        }
        tracks
    }

    /// Look up a media section by its `mid`.
    pub fn mid(&self, mid: String) -> Option<&MediaDescription> {
        for media in self.medias() {
            if media.mid().as_ref() == Some(&mid) {
                return Some(media);
            }
        }
        None
    }

    /// Get media by index.
    pub fn media(&self, i: usize) -> Option<&MediaDescription> {
        self.medias().get(i)
    }

    /// Create a one-media (audio) SDP with provided port, optional MID, and codecs.
    pub fn create_basic(
        audio_port: u16,
        audio_mid: Option<String>,
        audio_rtpmaps: Vec<Rtpmap>,
    ) -> SessionDescription {
        let mut medias = Vec::new();
        let audio = MediaDescription::create_basic_audio(
            audio_port,
            audio_mid,
            audio_rtpmaps,
        );
        medias.push(audio);
        Self::with_medias(medias, None)
    }

    /// Build an SDP offer: fills audio media, and for WebRTC adds DTLS/ICE and BUNDLE.
    pub async fn create_offer(
        channel_id: &str,
        webrtc: bool,
        audio_rtpmaps: Vec<Rtpmap>,
        encryption: bool,
    ) -> Result<SessionDescription> {
        let dtls_ice = if webrtc {
            Some((new_dtls("actpass".to_string()).await?, new_ice()))
        } else {
            None
        };

        let mut medias = Vec::new();
        let audio = MediaDescription::create_audio(
            channel_id,
            webrtc,
            audio_rtpmaps,
            encryption,
        )
        .await?;
        medias.push(audio);

        Ok(Self::with_medias(medias, dtls_ice))
    }

    /// Assemble the final SDP from medias; optionally inject DTLS/ICE and BUNDLE group.
    fn with_medias(
        mut medias: Vec<MediaDescription>,
        dtls_ice: Option<(Dtls, Ice)>,
    ) -> SessionDescription {
        let mut bundle = Vec::new();
        for media in medias.iter_mut() {
            if let Some((dtls, ice)) = dtls_ice.as_ref() {
                media.insert_dtls(dtls);
                media.insert_ice(ice);
            }

            if let Some(mid) = media.mid() {
                bundle.push(mid);
            }
        }

        let mut sdp = SessionDescription::new();
        sdp.media_descriptions = medias;
        if bundle.len() > 0 {
            sdp.insert_attribute(
                "group".to_string(),
                Some(format!("BUNDLE {}", bundle.join(" "))),
            );
        }
        sdp
    }

    /// Reset peer state to plain RTP/AVP and optional local RTP port, preserving only rtpmap/fmtp/mid.
    pub fn reset_peer(&mut self, reset_rtp_port: bool) {
        self.attributes = Vec::new();
        for media in self.media_descriptions.iter_mut() {
            if reset_rtp_port {
                media.port = LOCAL_RTP_PEER_PORT;
            }
            media.proto = "RTP/AVP".to_string();
            let attributes = media.attributes.clone();
            media.attributes = Vec::new();
            for attr in attributes {
                if attr.key == "rtpmap" || attr.key == "fmtp" || attr.key == "mid" {
                    media.attributes.push(attr);
                }
            }
        }
    }

    /// Create a minimal local peer SDP (audio on local RTP peer port with one codec).
    pub fn create_local_peer(rtpmap: Rtpmap) -> SessionDescription {
        let mut media = MediaDescription {
            media_type: MediaType::Audio,
            port: LOCAL_RTP_PEER_PORT,
            num_ports: 0,
            proto: "RTP/AVP".to_string(),
            payloads: Vec::new(),
            attributes: Vec::new(),
        };
        media.attributes.push(Attribute {
            key: "mid".to_string(),
            value: Some("audio".to_string()),
        });
        media.insert_rtpmap(&rtpmap);

        let mut sdp = SessionDescription::new();
        sdp.media_descriptions = vec![media];
        sdp
    }

    /// Build an SDP answer for all medias, negotiating codecs and adding DTLS/ICE when present.
    pub async fn create_answer(
        &self,
        channel_id: &str,
        peer_sdp: Option<&SessionDescription>,
        internal_sdp: bool,
    ) -> Result<SessionDescription> {
        let mut answer_medias = Vec::new();

        let dtls = if self.dtls().is_some() {
            let fingerprint = get_cert_fingerprint().await?;
            Some(Dtls {
                fingerprint,
                hash: "sha-256".to_string(),
                setup: "active".to_string(),
            })
        } else {
            None
        };

        let ice = self.ice().map(|_| new_ice());

        let mut bundle = Vec::new();

        for media in self.medias() {
            let peer_rtpmaps =
                peer_sdp.and_then(|sdp| sdp.get_rtpmaps(&media.media_type));
            if let Ok(mut answer_media) = media
                .create_answer(channel_id, peer_rtpmaps, internal_sdp)
                .await
            {
                if let Some(dtls) = dtls.as_ref() {
                    answer_media.insert_dtls(dtls);
                }

                if let Some(ice) = ice.as_ref() {
                    answer_media.insert_ice(ice);
                }

                if let Some(mid) = answer_media.mid() {
                    bundle.push(mid);
                }
                answer_medias.push(answer_media);
            }
        }

        let mut answer_sdp = SessionDescription::new();
        answer_sdp.media_descriptions = answer_medias;
        if bundle.len() > 0 {
            answer_sdp.insert_attribute(
                "group".to_string(),
                Some(format!("BUNDLE {}", bundle.join(" "))),
            );
        }

        Ok(answer_sdp)
    }

    /// Collect all `Rtpmap`s for a given `MediaType` from this SDP.
    pub fn get_rtpmaps(&self, media_type: &MediaType) -> Option<Vec<Rtpmap>> {
        for media in self.medias() {
            if &media.media_type == media_type {
                return Some(
                    media
                        .payloads
                        .iter()
                        .filter_map(|p| media.pt_rtpmap(p).map(|r| r.clone()))
                        .collect(),
                );
            }
        }
        None
    }

    /// Append a session-level SDP attribute.
    pub fn insert_attribute(&mut self, key: String, value: Option<String>) {
        self.attributes.push(Attribute { key, value });
    }
}

impl Display for SessionDescription {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "v={}\r\n", &self.version)?;
        write!(f, "{}\r\n", &self.origin)?;
        write!(f, "s={}\r\n", &self.session_name)?;
        write!(f, "{}\r\n", &self.connection)?;
        write!(f, "t={} {}\r\n", self.time_start, self.time_stop)?;

        for attr in &self.attributes {
            write!(f, "{}", attr)?;
        }

        for m in &self.media_descriptions {
            write!(f, "{}", m)?;
        }

        Ok(())
    }
}

impl FromStr for SessionDescription {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut sdp = SessionDescription::default();
        for line in s.split("\r\n") {
            if line.len() < 2 {
                continue;
            }
            let desc_type = &line[0..1];
            let data = &line[2..];
            match desc_type.to_lowercase().as_ref() {
                "v" => {
                    sdp.version = data.to_string();
                }
                "s" => {
                    sdp.session_name = data.to_string();
                }
                "o" => {
                    sdp.origin = Origin::from_str(data)?;
                }
                "t" => {
                    let parts: Vec<&str> = data.split(" ").collect();
                    if parts.len() != 2 {
                        Err(anyhow!("invalid sdp"))?;
                    }
                    sdp.time_start = parts[0].parse::<i32>()?;
                    sdp.time_stop = parts[1].parse::<i32>()?;
                }
                "m" => {
                    if let Ok(media) = MediaDescription::parse_media(data) {
                        sdp.media_descriptions.push(media);
                    }
                }
                "c" => {
                    let parts: Vec<&str> = data.split(" ").collect();
                    if parts.len() != 3 {
                        Err(SdpError::InvalidSdp)?;
                    }
                    let c = Connection {
                        network_type: parts[0].to_string(),
                        addr_type: parts[1].to_string(),
                        addr: parts[2].to_string(),
                    };
                    sdp.connection = c;
                }
                "a" => {
                    let parts: Vec<&str> = data.splitn(2, ":").collect();
                    let key = parts[0].to_string();
                    let value = parts.get(1).map(|v| v.to_string());
                    let attr = Attribute { key, value };
                    if let Some(media) = sdp.media_descriptions.last_mut() {
                        media.attributes.push(attr);
                    } else {
                        sdp.attributes.push(attr);
                    }
                }
                _ => (),
            }
        }
        Ok(sdp)
    }
}

fn attribute(attrs: &[Attribute], key: &str) -> Option<String> {
    for attr in attrs {
        if &attr.key == key {
            return attr.value.clone();
        }
    }
    None
}

fn extract_ice(attrs: &[Attribute]) -> Option<Ice> {
    let ufrag = attribute(attrs, "ice-ufrag")?;
    let pwd = attribute(attrs, "ice-pwd")?;
    Some(Ice {
        ufrag,
        pwd,
        candidates: Vec::new(),
    })
}

fn extract_dtls(attrs: &[Attribute]) -> Option<Dtls> {
    let setup = attribute(attrs, "setup")?;
    let fingerprint = attribute(attrs, "fingerprint")?;
    let mut parts = fingerprint.splitn(2, " ");
    let hash = parts.next()?.to_string();
    let fingerprint = parts.next()?.to_string();
    Some(Dtls {
        fingerprint,
        hash,
        setup,
    })
}

impl SessionDescription {
    pub fn ice(&self) -> Option<Ice> {
        if let Some(ice) = extract_ice(&self.attributes) {
            return Some(ice);
        }

        for media in &self.media_descriptions {
            if let Some(ice) = media.ice() {
                return Some(ice);
            }
        }

        None
    }

    pub fn dtls(&self) -> Option<Dtls> {
        if let Some(dtls) = extract_dtls(&self.attributes) {
            return Some(dtls);
        }

        for media in &self.media_descriptions {
            if let Some(dtls) = media.dtls() {
                return Some(dtls);
            }
        }

        None
    }

    pub fn attribute(&self, key: &str) -> Option<String> {
        attribute(&self.attributes, key)
    }
}

/// Generate DTLS parameters using the cached certificate fingerprint and provided setup role.
async fn new_dtls(setup: String) -> Result<Dtls> {
    let fingerprint = get_cert_fingerprint().await?;
    Ok(Dtls {
        fingerprint,
        hash: "sha-256".to_string(),
        setup,
    })
}

/// Create a simple host-only ICE block using the local media IP (with dual-stack address forms).
fn new_ice() -> Ice {
    let foundation = nebula_utils::rand_number(10);
    let octets = MEDIA_SERVICE.config.media_ip.octets();
    let ipv6 = format!(
        "64:ff9b::{:02x?}{:02x?}:{:02x?}{:02x?}",
        octets[0], octets[1], octets[2], octets[3]
    );
    Ice {
        ufrag: nebula_utils::rand_string(16),
        pwd: nebula_utils::rand_string(24),
        candidates: vec![
            Candidate {
                foundation: foundation.clone(),
                component: 1,
                priority: 355321,
                addr: MEDIA_SERVICE.config.media_ip.to_string(),
                port: 0,
                kind: "host".to_string(),
            },
            Candidate {
                foundation: foundation.clone(),
                component: 1,
                priority: 355321,
                addr: ipv6,
                port: 0,
                kind: "host".to_string(),
            },
        ],
    }
}

/// Allocate a new RTP port in [10000,40000] with a Redis lock, even across processes.
async fn new_session_port(channel_id: &str) -> Result<u16> {
    let key = "nebula:media:port";
    loop {
        let port = REDIS.incr_by(&key, 2).await? as u16;
        if port < 10000 || port > 40000 {
            REDIS.set(&key, &10000.to_string()).await?;
            continue;
        }
        let port_key = format!("nebula:session:{}:lock", port);
        let mutex = DistributedMutex::new(port_key);
        mutex.lock().await;

        let success = REDIS
            .setexnx(&format!("nebula:session:{}", port), channel_id, 60 * 5)
            .await;
        if success {
            return Ok(port);
        }
    }
}

/// Generate a random 32-bit SSRC value.
fn new_ssrc() -> u32 {
    let mut buffer = [0; 4];
    rand::thread_rng().fill_bytes(&mut buffer);
    let mut ssrc = buffer[0] as u32;
    ssrc |= (buffer[1] as u32) << 8;
    ssrc |= (buffer[2] as u32) << 16;
    ssrc |= (buffer[3] as u32) << 24;
    ssrc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_from_string() {
        let sdp_str = [
            "v=0",
            "o=Galaxy 14824 1 IN IP4 127.0.0.1",
            "s=Galaxy",
            "c=IN IP4 127.0.0.1",
            "t=0 0",
            "a=ice-ufrag:074c6550",
            "a=ice-pwd:a28a397a4c3f31747d1ee3474af08a068",
            "a=fingerprint:sha-1 99:41:49:83:4a:97:0e:1f:ef:6d:f7:c9:c7:70:9d:1f:66:79:a8:07",
            "m=audio 19886 RTP/AVP 9 8 0 101 13",
            "a=rtpmap:9 G722/8000",
            "a=rtpmap:101 telephone-event/8000",
            "a=fmtp:101 0-15",
            "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:gnSzGnf4d9ORNarMnaVWRLzu+CqlkhAGdSWhfTaZ",
            "a=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:gnSzGnf4d9ORNarMnaVWRLzu+CqlkhAGdSWhfTaZ",
            "a=sendonly",
            "",
        ]
        .join("\r\n");
        let sdp = SessionDescription::from_str(&sdp_str).unwrap();
        assert_eq!(sdp_str, sdp.to_string());
    }
}
