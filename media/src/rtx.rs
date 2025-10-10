use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use byteorder::{BigEndian, ByteOrder};

use crate::{
    packet::RtpPacket,
    stream::{FullSequenceNumber, Ssrc, TruncatedSequenceNumber},
    two_generation_cache::TwoGenerationCache,
};

const RTX_SSRC_OFFSET: Ssrc = 1;

// Keeps a cache of previously sent packets over a limited time window
// and can be asked to create an RTX packet from one of those packets
// based on SSRC and seqnum.  The cache is across SSRCs, not per SSRC.
pub struct RtxSender {
    // The key includes an SSRC because we send packets with many SSRCs
    // and a truncated seqnum because we need to look them up by
    // seqnums in NACKs which are truncated.
    previously_sent_by_seqnum:
        TwoGenerationCache<(Ssrc, TruncatedSequenceNumber), Vec<u8>>,
    next_outgoing_seqnum_by_ssrc: HashMap<Ssrc, FullSequenceNumber>,
}

impl RtxSender {
    pub fn new(limit: Duration) -> Self {
        Self {
            previously_sent_by_seqnum: TwoGenerationCache::new(
                limit,
                Instant::now(),
            ),
            next_outgoing_seqnum_by_ssrc: HashMap::new(),
        }
    }

    fn get_next_seqnum_mut(&mut self, rtx_ssrc: Ssrc) -> &mut FullSequenceNumber {
        self.next_outgoing_seqnum_by_ssrc
            .entry(rtx_ssrc)
            .or_insert(1)
    }

    fn increment_seqnum(&mut self, rtx_ssrc: Ssrc) -> FullSequenceNumber {
        let next_seqnum = self.get_next_seqnum_mut(rtx_ssrc);
        let seqnum = *next_seqnum;
        *next_seqnum += 1;
        seqnum
    }

    pub fn remember_sent(&mut self, outgoing: Vec<u8>, departed: Instant) {
        let ssrc = RtpPacket::get_ssrc(&outgoing);
        let seq = RtpPacket::get_sequence(&outgoing);
        self.previously_sent_by_seqnum.insert(
            (ssrc, seq as TruncatedSequenceNumber),
            outgoing,
            departed,
        );
    }

    pub fn resend_as_rtx(
        &mut self,
        ssrc: Ssrc,
        seqnum: TruncatedSequenceNumber,
    ) -> Option<Vec<u8>> {
        let rtx_ssrc = to_rtx_ssrc(ssrc);
        let rtx_seqnum = *self.get_next_seqnum_mut(rtx_ssrc);

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

    fn remembered_packet_stats(&self) -> (usize, usize) {
        let mut count = 0usize;
        let mut sum_of_packets = 0usize;
        self.previously_sent_by_seqnum
            .iter()
            .for_each(|(_, packet)| {
                count += 1;
                sum_of_packets += packet.len()
            });
        (count, sum_of_packets)
    }
}

fn to_rtx_ssrc(ssrc: Ssrc) -> Ssrc {
    ssrc.wrapping_add(RTX_SSRC_OFFSET)
}

fn packet_to_rtx(packet: &[u8], ssrc: u32, seq: u16) -> Vec<u8> {
    let original_seq = RtpPacket::get_sequence(packet);
    let mut new_payload = vec![0, 0];
    BigEndian::write_u16(&mut new_payload, original_seq);
    new_payload.extend_from_slice(RtpPacket::get_payload(packet));

    let mut new_packet = packet.to_vec();
    RtpPacket::buffer_set_sequence(&mut new_packet, seq);
    RtpPacket::set_packet_ssrc(&mut new_packet, ssrc);
    RtpPacket::buffer_set_paylod(&mut new_packet, &new_payload);
    new_packet
}
