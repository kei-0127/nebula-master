use anyhow::{anyhow, Result};
use bytes::Buf;
use rtcp::{
    goodbye::Goodbye,
    header::{
        Header, PacketType, FORMAT_FIR, FORMAT_PLI, FORMAT_REMB, FORMAT_RRR,
        FORMAT_SLI, FORMAT_TCC, FORMAT_TLN,
    },
    payload_feedbacks::{
        full_intra_request::FullIntraRequest,
        picture_loss_indication::PictureLossIndication,
        receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate,
        slice_loss_indication::SliceLossIndication,
    },
    receiver_report::ReceiverReport,
    sender_report::SenderReport,
    source_description::SourceDescription,
    transport_feedbacks::{
        rapid_resynchronization_request::RapidResynchronizationRequest,
        transport_layer_cc::TransportLayerCc,
        transport_layer_nack::TransportLayerNack,
    },
};
use webrtc_util::Unmarshal;

pub const RTCP_HEADER_LEN: usize = 4;
pub const RTCP_SSRC_LEN: usize = 4;

#[derive(Clone)]
pub enum TransportSpecificFeedback {
    TransportLayerNack(TransportLayerNack),
}

#[derive(Clone)]
pub enum PayloadSpecificFeedback {
    FullIntraRequest(FullIntraRequest),
}

#[derive(Clone, Debug)]
pub enum RtcpPacket {
    SenderReport(SenderReport),
    ReceiverReport(ReceiverReport),
    SourceDescription(SourceDescription),
    Goodbye(Goodbye),
    TransportLayerNack(TransportLayerNack),
    TransportLayerCc(TransportLayerCc),
    RapidResynchronizationRequest(RapidResynchronizationRequest),
    FullIntraRequest(FullIntraRequest),
    SliceLossIndication(SliceLossIndication),
    PictureLossIndication(PictureLossIndication),
    ReceiverEstimatedMaximumBitrate(ReceiverEstimatedMaximumBitrate),
}

impl RtcpPacket {
    pub fn get_ssrc(buf: &[u8]) -> u32 {
        let mut ssrc = (buf[4] as u32) << 24;
        ssrc += (buf[5] as u32) << 16;
        ssrc += (buf[6] as u32) << 8;
        ssrc += buf[7] as u32;
        ssrc
    }

    pub fn payload_offset() -> usize {
        RTCP_HEADER_LEN + RTCP_SSRC_LEN
    }

    pub fn unmarshal<B: Buf + Clone>(buf: &mut B) -> Result<RtcpPacket> {
        let header = Header::unmarshal(&mut buf.clone())?;
        let rtcp = match header.packet_type {
            PacketType::Unsupported => {
                return Err(anyhow!("unsupported rtcp type"));
            }
            PacketType::SenderReport => {
                let sr = SenderReport::unmarshal(buf)?;
                RtcpPacket::SenderReport(sr)
            }
            PacketType::ReceiverReport => {
                let rr = ReceiverReport::unmarshal(buf)?;
                RtcpPacket::ReceiverReport(rr)
            }
            PacketType::SourceDescription => {
                let content = SourceDescription::unmarshal(buf)?;
                RtcpPacket::SourceDescription(content)
            }
            PacketType::Goodbye => {
                let content = Goodbye::unmarshal(buf)?;
                RtcpPacket::Goodbye(content)
            }
            PacketType::ApplicationDefined => {
                return Err(anyhow!("unsupported rtcp type"));
            }
            PacketType::TransportSpecificFeedback => match header.count {
                FORMAT_TLN => {
                    let content = TransportLayerNack::unmarshal(buf)?;
                    RtcpPacket::TransportLayerNack(content)
                }
                FORMAT_TCC => {
                    let content = TransportLayerCc::unmarshal(buf)?;
                    RtcpPacket::TransportLayerCc(content)
                }
                FORMAT_RRR => {
                    let content = RapidResynchronizationRequest::unmarshal(buf)?;
                    RtcpPacket::RapidResynchronizationRequest(content)
                }
                _ => {
                    return Err(anyhow!("unsupported rtcp type"));
                }
            },

            PacketType::PayloadSpecificFeedback => match header.count {
                FORMAT_FIR => {
                    let content = FullIntraRequest::unmarshal(buf)?;
                    RtcpPacket::FullIntraRequest(content)
                }
                FORMAT_SLI => {
                    let content = SliceLossIndication::unmarshal(buf)?;
                    RtcpPacket::SliceLossIndication(content)
                }
                FORMAT_PLI => {
                    let content = PictureLossIndication::unmarshal(buf)?;
                    RtcpPacket::PictureLossIndication(content)
                }
                FORMAT_REMB => {
                    let content = ReceiverEstimatedMaximumBitrate::unmarshal(buf)?;
                    RtcpPacket::ReceiverEstimatedMaximumBitrate(content)
                }
                _ => {
                    return Err(anyhow!("unsupported rtcp type"));
                }
            },
        };
        Ok(rtcp)
    }
}
