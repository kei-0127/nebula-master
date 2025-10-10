use super::server::MEDIA_SERVICE;
use crate::session::get_cert;
use anyhow::{anyhow, Error, Result};
use base64;
use codec::{pcma::PCMA, pcmu::PCMU, Codec, G722Codec, Opus};
use indexmap::IndexMap;
use nebula_redis::REDIS;
use serde::{Deserialize, Serialize};
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::str::FromStr;
use strum_macros;
use strum_macros::EnumString;
use thiserror::Error;

const INTERNAL_RTPMAP_ORDER: [PayloadType; 5] = [
    PayloadType::Opus,
    PayloadType::G722,
    PayloadType::PCMA,
    PayloadType::PCMU,
    PayloadType::H264,
];
const EXTERNAL_RTPMAP_ORDER: [PayloadType; 5] = [
    PayloadType::PCMA,
    PayloadType::PCMU,
    PayloadType::G722,
    PayloadType::Opus,
    PayloadType::H264,
];

#[derive(Debug, Error)]
pub enum SdpError {
    #[error("invalid sdp")]
    InvalidSdp,

    #[error("no audio")]
    NoAudio,

    #[error("negotiation failed")]
    NegotiationFailed,
}

#[derive(strum_macros::Display, EnumString, Debug, Eq, PartialEq, Clone, Hash)]
pub enum MediaType {
    #[strum(serialize = "audio")]
    Audio,
    #[strum(serialize = "video")]
    Video,
}

impl Default for MediaType {
    fn default() -> Self {
        MediaType::Audio
    }
}

#[derive(
    strum_macros::Display,
    EnumString,
    Deserialize,
    Serialize,
    Debug,
    Eq,
    PartialEq,
    Clone,
    Hash,
)]
pub enum MediaMode {
    #[strum(serialize = "inactive")]
    Inactive,
    #[strum(serialize = "sendonly")]
    Sendonly,
    #[strum(serialize = "recvonly")]
    Recvonly,
    #[strum(serialize = "sendrecv")]
    Sendrecv,
}

impl Default for MediaMode {
    fn default() -> Self {
        MediaMode::Sendrecv
    }
}

#[derive(Default, Debug, Clone)]
pub struct Media {
    pub media_type: MediaType,
    pub port: u16,
    pub num_ports: u32,
    pub proto: String,
    pub payloads: Vec<String>,
    pub rtpmaps: HashMap<String, Rtpmap>,
    pub cryptos: IndexMap<String, Crypto>,
    pub attributes: Vec<String>,
    pub mode: MediaMode,
    pub ice: Option<Ice>,
    pub dtls: Option<Dtls>,
    pub rtcp_mux: bool,
    pub msid: Option<String>,
}

impl Display for Media {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "m={} ", &self.media_type)?;
        write!(f, "{}", self.port)?;
        if self.num_ports > 0 {
            write!(f, "/{}", self.num_ports)?;
        }
        write!(f, " {} {}\r\n", &self.proto, self.payloads.join(" "))?;

        if let Some(msid) = &self.msid {
            write!(f, "a=msid:- {}\r\n", msid)?;
        }

        for payload in &self.payloads {
            if let Some(rtpmap) = self.rtpmaps.get(payload) {
                write!(
                    f,
                    "a=rtpmap:{} {}/{}{}\r\n",
                    rtpmap.type_number,
                    rtpmap.name,
                    rtpmap.rate,
                    if rtpmap.params != "" {
                        format!("/{}", rtpmap.params)
                    } else {
                        "".to_string()
                    }
                )?;
                if rtpmap.fmtp != "" {
                    write!(f, "a=fmtp:{} {}\r\n", payload, rtpmap.fmtp)?;
                }
            }
        }

        if !self.cryptos.is_empty() {
            for (_tag, crypto) in &self.cryptos {
                write!(f, "a=crypto:{}\r\n", crypto)?;
            }
        }

        if let Some(dtls) = &self.dtls {
            write!(f, "a=fingerprint:{} {}\r\n", &dtls.hash, &dtls.fingerprint)?;
            write!(f, "a=setup:{}\r\n", &dtls.setup)?;
        }

        if let Some(ice) = &self.ice {
            write!(f, "a=ice-ufrag:{}\r\n", ice.ufrag)?;
            write!(f, "a=ice-pwd:{}\r\n", ice.pwd)?;
            write!(f, "a=ice-options:trickle renomination\r\n")?;
            for c in &ice.candidates {
                write!(
                    f,
                    "a=candidate:{} {} udp {} {} {} typ {} generation 0\r\n",
                    c.foundation, c.component, c.priority, c.addr, c.port, c.kind,
                )?;
            }
        }

        if self.rtcp_mux {
            write!(f, "a=rtcp-mux\r\n")?;
        }

        write!(f, "a={}\r\n", &self.mode)?;

        for attr in &self.attributes {
            write!(f, "a={}\r\n", attr)?;
        }

        Ok(())
    }
}

#[derive(
    Deserialize,
    Serialize,
    Debug,
    Clone,
    strum_macros::Display,
    EnumString,
    PartialEq,
)]
pub enum PayloadType {
    #[strum(serialize = "PCMA")]
    PCMA,
    #[strum(serialize = "PCMU")]
    PCMU,
    #[strum(serialize = "G722")]
    G722,
    #[strum(serialize = "opus")]
    Opus,
    #[strum(serialize = "H264")]
    H264,
    #[strum(serialize = "VP9")]
    VP9,
    #[strum(serialize = "telephone-event")]
    TelephoneEvent,
}

impl Default for PayloadType {
    fn default() -> Self {
        PayloadType::PCMA
    }
}

impl PayloadType {
    pub fn get_rtpmap(&self) -> Rtpmap {
        match self {
            PayloadType::PCMA => Rtpmap {
                name: PayloadType::PCMA,
                rate: 8000,
                type_number: 8,
                ..Default::default()
            },
            PayloadType::PCMU => Rtpmap {
                name: PayloadType::PCMU,
                rate: 8000,
                type_number: 0,
                ..Default::default()
            },
            PayloadType::G722 => Rtpmap {
                name: PayloadType::G722,
                rate: 8000,
                type_number: 9,
                ..Default::default()
            },
            PayloadType::Opus => Rtpmap {
                name: PayloadType::Opus,
                rate: 48000,
                type_number: 111,
                params: "2".to_string(),
                fmtp: "minptime=10;useinbandfec=1".to_string(),
                ..Default::default()
            },
            PayloadType::H264 => Rtpmap {
                name: PayloadType::H264,
                rate: 90000,
                type_number: 127,
                fmtp: "packetization-mode=1;profile-level-id=42001f".to_string(),
                ..Default::default()
            },
            PayloadType::VP9 => Rtpmap {
                name: PayloadType::VP9,
                rate: 90000,
                type_number: 98,
                fmtp: "profile-id=0".to_string(),
                ..Default::default()
            },
            PayloadType::TelephoneEvent => Rtpmap {
                name: PayloadType::TelephoneEvent,
                rate: 8000,
                type_number: 101,
                fmtp: "0-15".to_string(),
                ..Default::default()
            },
        }
    }

    pub fn get_codec(&self) -> Box<dyn Codec> {
        match self {
            PayloadType::PCMA => Box::new(PCMA::new()),
            PayloadType::PCMU => Box::new(PCMU::new()),
            PayloadType::G722 => Box::new(G722Codec::new()),
            PayloadType::Opus => Box::new(Opus::new()),
            PayloadType::H264 => Box::new(PCMA::new()),
            PayloadType::VP8 => Box::new(PCMA::new()),
            PayloadType::VP9 => Box::new(PCMA::new()),
            PayloadType::TelephoneEvent => Box::new(PCMA::new()),
        }
    }
}

#[derive(Deserialize, Serialize, Default, Debug, Clone)]
pub struct Rtpmap {
    pub name: PayloadType,
    pub rate: u32,
    pub type_number: u8,
    pub params: String,
    pub fmtp: String,
    pub rtcp_feedbacks: Vec<String>,
}

impl FromStr for Rtpmap {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.splitn(2, " ").collect();
        if parts.len() != 2 {
            Err(SdpError::InvalidSdp)?;
        }
        let pt = parts[0].parse::<u8>()?;

        let parts: Vec<&str> = parts[1].split("/").collect();
        if parts.len() < 2 {
            Err(SdpError::InvalidSdp)?;
        }
        let rate: u32 = parts[1].parse()?;
        let name = PayloadType::from_str(parts[0])?;
        let mut params = "".to_string();
        if parts.len() == 3 {
            params = parts[2].to_string();
        }
        Ok(Rtpmap {
            name,
            rate,
            type_number: pt,
            params,
            fmtp: "".to_string(),
            rtcp_feedbacks: Vec::new(),
        })
    }
}

impl Rtpmap {
    pub fn get_rate(&self) -> u32 {
        if self.name == PayloadType::G722 {
            16000
        } else {
            self.rate
        }
    }

    pub fn pt_default(pt: &str) -> Option<Rtpmap> {
        match pt {
            "0" => Some(Rtpmap {
                name: PayloadType::PCMU,
                rate: 8000,
                type_number: 0,
                ..Default::default()
            }),
            "8" => Some(Rtpmap {
                name: PayloadType::PCMA,
                rate: 8000,
                type_number: 8,
                ..Default::default()
            }),
            "9" => Some(Rtpmap {
                name: PayloadType::G722,
                rate: 8000,
                type_number: 9,
                ..Default::default()
            }),
            _ => None,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct Dtls {
    pub fingerprint: String,
    pub hash: String,
    pub setup: String,
}

#[derive(Deserialize, Serialize, Default, Debug, Clone)]
pub struct Ice {
    pub ufrag: String,
    pub pwd: String,
    pub candidates: Vec<Candidate>,
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

#[derive(Default, Debug)]
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
            Err(SdpError::InvalidSdp)?;
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

#[derive(Default, Debug)]
pub struct Connection {
    network_type: String,
    addr_type: String,
    addr: String,
}

impl Display for Connection {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "c={} {} {}",
            &self.network_type, &self.addr_type, &self.addr
        );
        Ok(())
    }
}
