use std::str::FromStr;

use anyhow::{Error, Result};
use codec::{pcma::PCMA, pcmu::PCMU, Codec, G722Codec, Opus};
use serde::{Deserialize, Serialize};
use strum_macros::{self, EnumString};
use thiserror::Error;

use crate::sdp::H264_FMTP;

#[derive(Debug, Error)]
pub enum SdpError {
    #[error("invalid sdp")]
    InvalidSdp,

    #[error("no audio")]
    NoAudio,

    #[error("negotiation failed")]
    NegotiationFailed,
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
    #[strum(serialize = "VP8")]
    VP8,
    #[strum(serialize = "VP9")]
    VP9,
    #[strum(serialize = "rtx")]
    Rtx,
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
                fmtp: H264_FMTP.to_string(),
                ..Default::default()
            },
            PayloadType::VP8 => Rtpmap {
                name: PayloadType::VP8,
                rate: 90000,
                type_number: 96,
                ..Default::default()
            },
            PayloadType::Rtx => Rtpmap {
                name: PayloadType::Rtx,
                rate: 90000,
                type_number: 97,
                fmtp: "apt=96".to_string(),
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

    pub async fn get_codec(&self) -> Result<Box<dyn Codec>> {
        let pt = self.clone();
        nebula_task::spawn_task(move || -> Box<dyn Codec> {
            match pt {
                PayloadType::PCMA => Box::new(PCMA::new()),
                PayloadType::PCMU => Box::new(PCMU::new()),
                PayloadType::G722 => Box::new(G722Codec::new()),
                PayloadType::Opus => Box::new(Opus::new()),
                PayloadType::H264 => Box::new(PCMA::new()),
                PayloadType::VP8 => Box::new(PCMA::new()),
                PayloadType::VP9 => Box::new(PCMA::new()),
                PayloadType::Rtx => Box::new(PCMA::new()),
                PayloadType::TelephoneEvent => Box::new(PCMA::new()),
            }
        })
        .await
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
