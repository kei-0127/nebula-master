use std::{
    collections::{btree_map, BTreeMap, HashMap},
    convert::{TryFrom, TryInto},
    ops::{Add, AddAssign, Sub},
    time::{Duration, Instant},
};

use rtcp::transport_feedbacks::transport_layer_cc::{
    PacketStatusChunk, RecvDelta, StatusChunkTypeTcc, StatusVectorChunk,
    SymbolSizeTypeTcc, SymbolTypeTcc, TransportLayerCc,
};
use webrtc_util::{Marshal, MarshalSize};

use crate::{date_rate::DataSize, two_generation_cache::TwoGenerationCache};

const MICROS_PER_REFERENCE_TICK: u16 = 250 << 8;
const MICROS_PER_DELTA_TICK: u16 = 250;

pub struct TccReceiver {
    sender_ssrc: u32,
    // The timestamps of ACKs are all based off of this time.
    epoch: Instant,
    last_feedback: Instant,

    feedback_seqnum: u8,
    unacked_arrival: BTreeMap<u64, Instant>,
    max_received_seqnum: u64,
}

impl TccReceiver {
    pub fn new(sender_ssrc: u32, epoch: Instant) -> Self {
        Self {
            sender_ssrc,
            epoch,
            last_feedback: epoch,
            feedback_seqnum: 1,
            unacked_arrival: BTreeMap::new(),
            max_received_seqnum: 0,
        }
    }

    pub fn remember_received(&mut self, seq: u16, arrival: Instant) {
        let tcc = expand_truncated_counter(seq, &mut self.max_received_seqnum, 16);
        if let btree_map::Entry::Vacant(entry) = self.unacked_arrival.entry(tcc) {
            entry.insert(arrival);
        }
    }

    pub fn should_send_feedback(&mut self) -> bool {
        if self.unacked_arrival.len() > 5
            || self.last_feedback.elapsed().as_millis() > 100
        {
            self.last_feedback = Instant::now();
            true
        } else {
            false
        }
    }

    #[allow(clippy::needless_lifetimes)]
    pub fn feedbacks<'a>(
        &'a mut self,
        media_ssrc: u32,
    ) -> impl Iterator<Item = TransportLayerCc> + 'a {
        let sender_ssrc = self.sender_ssrc;
        let epoch = self.epoch;
        let next_feedback_seqnum = &mut self.feedback_seqnum;
        let arrivals = std::mem::take(&mut self.unacked_arrival);
        let mut arrivals = arrivals.into_iter().peekable();
        std::iter::from_fn(move || {
            let (first_seqnum, first_arrival) = *arrivals.peek()?;
            let reference_time_ticks = ticks_from_duration(
                first_arrival.saturating_duration_since(epoch),
                MICROS_PER_REFERENCE_TICK,
            );
            let reference_time = epoch
                + duration_from_ticks(
                    reference_time_ticks,
                    MICROS_PER_REFERENCE_TICK,
                );

            let mut prev_seqnum = first_seqnum;
            let mut prev_arrival = reference_time;

            let mut recv_deltas = Vec::new();
            let mut symbol_lists = Vec::new();
            while let Some((seqnum, arrival)) = arrivals.peek() {
                let (seqnum, arrival) = (*seqnum, *arrival);
                if seqnum > (first_seqnum + (u16::MAX as u64)) {
                    // This seqnum can't fit into the packet, so we need to return this packet and then process another one.
                    break;
                }
                if recv_deltas
                    .iter()
                    .map(|d: &RecvDelta| d.marshal_size())
                    .sum::<usize>()
                    + symbol_lists.len()
                    > 1100
                {
                    // The RTCP packet is getting too big.  Let's cap it off and send another one.
                    break;
                }
                for _ in (prev_seqnum + 1)..seqnum {
                    symbol_lists.push(SymbolTypeTcc::PacketNotReceived);
                }
                let delta_ticks = ticks_from_before_and_after(
                    prev_arrival,
                    arrival,
                    MICROS_PER_DELTA_TICK,
                );
                const MAX_SMALL_DELTA_TICKS: i64 = u8::MAX as i64;
                const MAX_LARGE_DELTA_TICKS: i64 = i16::MAX as i64;
                const MIN_LARGE_DELTA_TICKS: i64 = i16::MIN as i64;

                // Overlapping here is intentional.
                #[allow(clippy::match_overlapping_arm)]
                match delta_ticks {
                    0..=MAX_SMALL_DELTA_TICKS => {
                        recv_deltas.push(RecvDelta {
                            type_tcc_packet: SymbolTypeTcc::PacketReceivedSmallDelta,
                            delta: delta_ticks * MICROS_PER_DELTA_TICK as i64,
                        });
                        symbol_lists.push(SymbolTypeTcc::PacketReceivedSmallDelta);
                    }
                    MIN_LARGE_DELTA_TICKS..=MAX_LARGE_DELTA_TICKS => {
                        recv_deltas.push(RecvDelta {
                            type_tcc_packet: SymbolTypeTcc::PacketReceivedLargeDelta,
                            delta: delta_ticks * MICROS_PER_DELTA_TICK as i64,
                        });
                        symbol_lists.push(SymbolTypeTcc::PacketReceivedLargeDelta);
                    }
                    _ => {
                        // This delta can't fit into the packet, so we need to return this packet and then process another one.
                        break;
                    }
                };

                // Consume what we were peeking because it fit into the packet.
                arrivals.next();

                prev_seqnum = seqnum;
                prev_arrival = arrival;
            }

            let feedback_seqnum = std::mem::replace(
                next_feedback_seqnum,
                next_feedback_seqnum.wrapping_add(1),
            );
            let last_seqnum = prev_seqnum;
            let status_count = (last_seqnum - first_seqnum + 1) as u16;
            let pkt = TransportLayerCc {
                sender_ssrc,
                media_ssrc,
                base_sequence_number: first_seqnum as u16,
                packet_status_count: status_count,
                reference_time: reference_time_ticks as u32,
                fb_pkt_count: feedback_seqnum,
                packet_chunks: vec![PacketStatusChunk::StatusVectorChunk(
                    StatusVectorChunk {
                        type_tcc: StatusChunkTypeTcc::StatusVectorChunk,
                        symbol_size: SymbolSizeTypeTcc::TwoBit,
                        symbol_list: symbol_lists,
                    },
                )],
                recv_deltas,
            };
            Some(pkt)
        })
    }
}

/// A remote instant, internally represented as a duration since a remote-chosen epoch.
///
/// RemoteInstants can only meaningfully be compared if they come from the same connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemoteInstant(Duration);

impl RemoteInstant {
    pub fn from_micros(micros: u64) -> Self {
        Self(Duration::from_micros(micros))
    }

    pub fn from_millis(millis: u64) -> Self {
        Self(Duration::from_millis(millis))
    }

    pub fn saturating_duration_since(self, other: RemoteInstant) -> Duration {
        self.0.saturating_sub(other.0)
    }

    pub fn checked_sub(self, offset: Duration) -> Option<RemoteInstant> {
        self.0.checked_sub(offset).map(RemoteInstant)
    }
}

impl Add<Duration> for RemoteInstant {
    type Output = Self;

    fn add(self, offset: Duration) -> Self {
        Self(self.0 + offset)
    }
}

impl AddAssign<Duration> for RemoteInstant {
    fn add_assign(&mut self, offset: Duration) {
        self.0 += offset;
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Ack {
    pub size: DataSize,
    pub departure: Instant,
    pub arrival: RemoteInstant,
    pub feedback_arrival: Instant,
}

pub struct TccSender {
    next_send_seqnum: u64,
    max_received_seqnum: u64,
    size_by_seqnum: TwoGenerationCache<u64, (DataSize, Instant)>,
}

// The state for sending transport-cc, which keeps track of packets sent and
// feedback received.  It also updates outgoing packets to have a different
// transport-cc seqnum.
impl TccSender {
    pub fn new(now: Instant) -> Self {
        Self {
            next_send_seqnum: 1,
            max_received_seqnum: 0,
            // WebRTC limits this to 60 seconds, and Jitsi to 1000 packets.
            // At 20mbps (the max rate), 1000 packets can be used in 60ms, which doesn't seem long enough.
            // A feedback message might take longer than that.
            // So we'll go with the WebRTC way.  However, 60 seconds seems a bit long, so let's go with 10 seconds.
            // TODO: Consider making this configurable
            size_by_seqnum: TwoGenerationCache::new(Duration::from_secs(10), now),
        }
    }

    pub fn increment_seqnum(&mut self) -> u64 {
        let seqnum = self.next_send_seqnum;
        self.next_send_seqnum += 1;
        seqnum
    }

    pub fn remember_sent(&mut self, seqnum: u64, size: DataSize, now: Instant) {
        let departure = now;
        self.size_by_seqnum.insert(seqnum, (size, departure), now);
    }

    pub fn process_feedback(
        &mut self,
        feedback: &TransportLayerCc,
        feedback_arrival: Instant,
    ) -> Vec<Ack> {
        let mut acks = Vec::new();
        let arrivals = self.read_feedback(feedback);
        for (seqnum, arrival) in arrivals {
            if let Some((size, departure)) = self.size_by_seqnum.remove(&seqnum) {
                acks.push(Ack {
                    // seqnum,
                    size,
                    departure,
                    arrival,
                    feedback_arrival,
                });
            }
        }

        acks
    }

    fn read_feedback(&mut self, cc: &TransportLayerCc) -> Vec<(u64, RemoteInstant)> {
        let chunks = cc.packet_chunks.iter().flat_map(|chunk| match chunk {
            PacketStatusChunk::RunLengthChunk(c) => {
                vec![c.packet_status_symbol; c.run_length as usize]
            }
            PacketStatusChunk::StatusVectorChunk(c) => c.symbol_list.clone(),
        });

        let reference_time =
            MICROS_PER_REFERENCE_TICK as i64 * u64::from(cc.reference_time) as i64;

        let mut arrivals = Vec::new();
        let mut arrivals_sum_delta_ticks: i64 = 0;

        let mut seq = expand_truncated_counter(
            cc.base_sequence_number,
            &mut self.max_received_seqnum,
            16,
        );
        let mut i = 0;
        for status in chunks {
            match status {
                SymbolTypeTcc::PacketReceivedSmallDelta
                | SymbolTypeTcc::PacketReceivedLargeDelta => {
                    if let Some(delta) = cc.recv_deltas.get(i) {
                        arrivals_sum_delta_ticks += delta.delta;
                        let arrival_micros =
                            (reference_time + arrivals_sum_delta_ticks) as u64;
                        arrivals
                            .push((seq, RemoteInstant::from_micros(arrival_micros)));
                    }
                    i += 1;
                }
                SymbolTypeTcc::PacketNotReceived
                | SymbolTypeTcc::PacketReceivedWithoutDelta => {}
            }
            seq += 1;
        }
        arrivals
    }
}

// pub fn read_feedback(
//     mut payload: &[u8],
//     max_seqnum: &mut FullSequenceNumber,
// ) -> Option<(u8, Vec<(FullSequenceNumber, RemoteInstant)>)> {
//     let _ssrc = payload.read_u32::<BE>().ok()?;
//     let base_seqnum = payload.read_u16::<BE>().ok()?;
//     let base_seqnum: FullSequenceNumber = expand_seqnum(base_seqnum, max_seqnum);
//     let status_count = payload.read_u16::<BE>().ok()?;
//     let reference_time_ticks = payload.read_u24::<BE>().ok()?;
//     let feedback_seqnum = payload.read_u8().ok()?;

//     let mut status_chunks = Vec::new();
//     let mut status_chunks_sum_count = 0;
//     while status_chunks_sum_count < (status_count as usize) {
//         let encoded_status_chunk = payload.read_u16::<BE>().ok()?;
//         let status_chunk = PacketStatusChunk::from_u16(encoded_status_chunk)?;
//         status_chunks.push(status_chunk);
//         status_chunks_sum_count += status_chunk.len();
//     }
//     let mut arrivals = Vec::new();
//     let mut arrivals_sum_delta_ticks: i32 = 0;
//     for (seqnum, status) in (base_seqnum..(base_seqnum + status_count as u64))
//         .zip(status_chunks.iter().copied().flatten())
//     {
//         if let Some(delta_ticks) = match status? {
//             PacketStatus::NotReceived => None,
//             PacketStatus::ReceivedSmallDelta => {
//                 let delta_ticks = payload.read_u8().ok()?;
//                 Some(delta_ticks as i32)
//             }
//             PacketStatus::ReceivedLargeOrNegativeDelta => {
//                 let delta_ticks = payload.read_i16::<BE>().ok()?;
//                 Some(delta_ticks as i32)
//             }
//         } {
//             arrivals_sum_delta_ticks += delta_ticks;
//             let arrival_micros = ((MICROS_PER_REFERENCE_TICK as i64
//                 * u64::from(reference_time_ticks) as i64)
//                 + (MICROS_PER_DELTA_TICK as i64 * arrivals_sum_delta_ticks as i64))
//                 as u64;
//             arrivals.push((seqnum, RemoteInstant::from_micros(arrival_micros)));
//         }
//     }
//     Some((feedback_seqnum, arrivals))
// }

fn ticks_from_duration(duration: Duration, micros_per_tick: u16) -> u64 {
    // Round down.  If we round up the reference time, that will force the first
    // ack to be the 2-byte variety, which is less efficient to encode.
    (duration.as_micros() as u64) / (micros_per_tick as u64)
}

fn duration_from_ticks(ticks: u64, micros_per_tick: u16) -> Duration {
    Duration::from_micros(ticks * micros_per_tick as u64)
}

fn ticks_from_before_and_after(
    before: Instant,
    after: Instant,
    micros_per_tick: u16,
) -> i64 {
    if after > before {
        ticks_from_duration(
            after.checked_duration_since(before).unwrap(),
            micros_per_tick,
        ) as i64
    } else {
        // Negative!
        -(ticks_from_duration(
            before.checked_duration_since(after).unwrap(),
            micros_per_tick,
        ) as i64)
    }
}

pub fn expand_truncated_counter<Truncated>(
    truncated: Truncated,
    max: &mut u64,
    width: usize,
) -> u64
where
    Truncated:
        TryFrom<u64> + Into<u64> + Sub<Truncated, Output = Truncated> + Ord + Copy,
    <Truncated as TryFrom<u64>>::Error: std::fmt::Debug,
{
    let mask: u64 = (1 << width) - 1;
    let really_big: Truncated = (1 << (width - 1)).try_into().unwrap();

    let truncated_max = (*max & mask).try_into().unwrap();
    let max_roc = *max >> width;
    let roc: u64 =
        if truncated_max > truncated && truncated_max - truncated > really_big {
            // Truncated is a lot smaller than the max;  It's likely a rollover.
            max_roc + 1
        } else if max_roc > 0
            && truncated > truncated_max
            && truncated - truncated_max > really_big
        {
            // Truncated is a lot bigger than the max;  It's likely a rollunder.
            max_roc - 1
        } else {
            // Truncated is close to the max, so it's neither rollover nor rollunder.
            max_roc
        };
    let full = (roc << width) | (truncated.into() & mask);
    if full > *max {
        *max = full;
    }
    full
}
