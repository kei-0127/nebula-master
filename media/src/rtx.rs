// RTP Retransmission (RTX)
//
// This module implements RTP retransmission (RTX) functionality for reliable media delivery
// RTX allows the sender to retransmit lost RTP packets by wrapping them in RTX packets
// with a different SSRC and sequence number
//
// Key Features
//
// - Packet Caching: Maintains a cache of recently sent packets for retransmission
// - RTX Packet Generation: Creates RTX packets from cached original packets
// - Sequence Number Management: Manages RTX sequence numbers per SSRC
// - Automatic Cleanup: Automatically removes old packets from cache
//
// RTX Protocol
//
// RTX packets follow RFC 4588:
// 1. Different SSRC: RTX packets use a different SSRC than original packets
// 2. Original Sequence: Original sequence number is embedded in payload
// 3. New Sequence: RTX packets get new sequence numbers
// 4. Payload Wrapping: Original payload is wrapped with sequence number
//
// Usage
//
// use nebula_media::rtx::RtxSender;
// use std::time::Duration;
//
// // Create RTX sender with 30-second cache
// let mut rtx_sender = RtxSender::new(Duration::from_secs(30));
//
// // Remember sent packet
// rtx_sender.remember_sent(packet_data, Instant::now());
//
// // Retransmit packet
// if let Some(rtx_packet) = rtx_sender.resend_as_rtx(ssrc, seqnum) {
//     send_packet(rtx_packet);
// }

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use byteorder::{BigEndian, ByteOrder};
use bytes::Bytes;

use crate::{
    packet::RtpPacket,
    stream::{FullSequenceNumber, Ssrc, TruncatedSequenceNumber},
    two_generation_cache::TwoGenerationCache,
};

// Offset added to original SSRC to create RTX SSRC
// Note: This is a project-specific mapping; some deployments negotiate a distinct SSRC/PT
const RTX_SSRC_OFFSET: Ssrc = 1;

// Keeps a cache of previously sent packets over a limited time window
// and can be asked to create an RTX packet from one of those packets
// based on SSRC and seqnum.  The cache is across SSRCs, not per SSRC.
pub struct RtxSender {
    // The key includes an SSRC because we send packets with many SSRCs
    // and a truncated seqnum because we need to look them up by
    // seqnums in NACKs which are truncated.
    previously_sent_by_seqnum:
        TwoGenerationCache<(Ssrc, TruncatedSequenceNumber), Bytes>,
    next_outgoing_seqnum_by_ssrc: HashMap<Ssrc, FullSequenceNumber>,
}

impl RtxSender {

    // Create a new RTX sender with specified cach duration
    // The cach duration determines how long packets are kept for potential retransmission
    // Longer durations allow for more retransmissions but use more memory
    pub fn new(limit: Duration) -> Self {
        Self {
            previously_sent_by_seqnum: TwoGenerationCache::new(
                limit,
                Instant::now(),
            ),
            next_outgoing_seqnum_by_ssrc: HashMap::new(),
        }
    }

    // Get mutable reference to next sequence number for RTX SSRC
    // This method ensures each RTX SSRC has its own sequence number counter,
    // starting from 1 if this is the first time we see this SSRC
    fn get_next_seqnum_mut(&mut self, rtx_ssrc: Ssrc) -> &mut FullSequenceNumber {
        self.next_outgoing_seqnum_by_ssrc
            .entry(rtx_ssrc)
            .or_insert(1)
    }

    // Increment and return the next sequence number for RTX SSRC
    // This method atomically increments the sequence number and returns the previous value,
    // ensureing each RTX packet gets a unique sequence number
    fn increment_seqnum(&mut self, rtx_ssrc: Ssrc) -> FullSequenceNumber {
        let next_seqnum = self.get_next_seqnum_mut(rtx_ssrc);
        let seqnum = *next_seqnum;
        *next_seqnum += 1;
        seqnum
    }

    // Remember a sent packet for potential retransmission
    // This method caches a packet that was just sent, allowing it to be retransmitted later if needed.
    // The packet is indexed by its SSRC and sequence number for efficient lookup during retransmission
    pub fn remember_sent(&mut self, outgoing: Bytes, departed: Instant) {
        let ssrc = RtpPacket::get_ssrc(&outgoing);
        let seq = RtpPacket::get_sequence(&outgoing);
        self.previously_sent_by_seqnum.insert(
            (ssrc, seq as TruncatedSequenceNumber),
            outgoing,
            departed,
        );
    }

    //  Create an RTX packet for retransmission
//
//  This method creates a retransmission packet from a previously sent packet.
//  The RTX packet uses a different SSRC and sequence number, and wraps the
//  original payload with the original sequence number.
//
//  Arguments
//  `ssrc` - The original SSRC of the packet to retransmit
//  `seqnum` - The sequence number of the packet to retransmit
//
//  Returns
//  `Some(Vec<u8>)` - The RTX packet data if the original packet was found
//  `None` - If the original packet is not in the cache
    pub fn resend_as_rtx(
        &mut self,
        ssrc: Ssrc,
        seqnum: TruncatedSequenceNumber,
    ) -> Option<Vec<u8>> {
        let rtx_ssrc = to_rtx_ssrc(ssrc);
        let rtx_seqnum = *self.get_next_seqnum_mut(rtx_ssrc);

        // Look up the original packet in the cach
        let previously_sent = self.previously_sent_by_seqnum.get(&(ssrc, seqnum))?;
        let mut rtx = packet_to_rtx(previously_sent, rtx_ssrc, rtx_seqnum as u16);
        // This has to go after the use of previously_sent.to_rtx because previously_sent
        // has a ref to self.previously_sent_by_seqnum until then, and so we can't
        // get a mut ref to self.next_outgoing_seqnum until after we release that.
        // But we don't want to call self.increment_seqnum() before we call self.previously_sent_by_seqnum.get
        // because it might return None, in which case we don't want to waste a seqnum.
        self.increment_seqnum(rtx_ssrc);
        Some(rtx)
    }
}

// Convert original SSRC to RTX SSRC
// RTX packets use a different SSRC than the original packets to distinguish them from the original strea.
// This function adds a fixed offset to create the RTX SSRC
fn to_rtx_ssrc(ssrc: Ssrc) -> Ssrc {
    ssrc.wrapping_add(RTX_SSRC_OFFSET)
}

// Convert an RTP packet to an RTX packet
//
// This function creates an RTX packet by:
// 1. Extracting the original sequence number and payload
// 2. Creating a new payload with the original sequence number prepended
// 3. Setting the RTX SSRC and new sequence number
// 4. Replacing the payload in the packet
//
// Arguments
// `packet` - The original RTP packet data
// `ssrc` - The RTX SSRC to use
// `seq` - The new sequence number for the RTX packet
//
// Returns
// `Vec<u8>` - The RTX packet data
fn packet_to_rtx(packet: &[u8], ssrc: u32, seq: u16) -> Vec<u8> {
    // Compute lengths
    let payload = RtpPacket::get_payload(packet);
    let payload_offset = RtpPacket::payload_offset(packet);
    let new_payload_len = 2 + payload.len();
    let mut new_packet = Vec::with_capacity(payload_offset + new_payload_len);

    // Copy header+extensions as-is
    new_packet.extend_from_slice(&packet[..payload_offset]);

    // Write new sequence and SSRC directly into header region
    RtpPacket::buffer_set_sequence(&mut new_packet, seq);
    RtpPacket::set_packet_ssrc(&mut new_packet, ssrc);

    // Build new payload with original seq prefix (RFC 4588)
    let original_seq = RtpPacket::get_sequence(packet);
    new_packet.extend_from_slice(&[0, 0]);
    let end = new_packet.len();
    BigEndian::write_u16(&mut new_packet[end - 2..end], original_seq);
    new_packet.extend_from_slice(payload);

    new_packet
}