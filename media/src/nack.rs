// Negative Acknowledgment (NACK) Implementation
// 
// This module provides Negative Acknowledgment (NACK) functionality for
// RTP packet loss detection and recovery. NACK allows receivers to request
// retransmission of lost packets, improving media quality and reliability.
// 
// Key Features
// 
// - Packet Loss Detection: Detect missing RTP packets by sequence number
// - NACK Generation: Generate NACK messages for lost packets
// - Retransmission Requests: Request retransmission of lost packets
// - Sequence Tracking: Track received packet sequence numbers
// - Loss Recovery: Improve media quality through packet recovery
// 
// NACK Process
// 
// 1. Packet Reception: Track received packet sequence numbers
// 2. Loss Detection: Identify missing sequence numbers
// 3. NACK Generation: Create NACK messages for lost packets
// 4. Retransmission: Request sender to retransmit lost packets
// 5. Recovery: Receive and process retransmitted packets

use std::time::{Duration, Instant};

use crate::key_sorted_cache::KeySortedCache;

// Tracks whch RTP packets arrived and which went missing and tells you which ones to NACK
pub struct NackSender {
    limit: usize,
    sent_by_seqnum: KeySortedCache<u64, Option<(Instant, Instant)>>,
    max_received: Option<u64>,
}

impl NackSender {

    // Start a NACK tracker; `limit` bounds how many sequence numbers we remember at once
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            sent_by_seqnum: KeySortedCache::new(limit),
            max_received: None,
        }
    }

    // If there are any new unreceived seqnums (the need to send nacks), returns the necessary seqnums to nack.
    pub fn remember_received(&mut self, seqnum: u64) {
        use std::cmp::Ordering::*;

        if let Some(max_received) = &mut self.max_received {
            match seqnum.cmp(max_received) {
                Equal => {
                    // We already received it, so nothing to do.
                }
                Less => {
                    // We likely already sent a NACK, so make sure we don't
                    // send a NACK any more.
                    self.sent_by_seqnum.remove(&seqnum);
                }
                Greater => {
                    let prev_max_received = std::mem::replace(max_received, seqnum);
                    let mut missing_range =
                        prev_max_received.saturating_add(1)..seqnum;
                    let missing_count = missing_range.end - missing_range.start;
                    if missing_count > (self.limit as u64) {
                        // Everything is going to get removed anyway, so this is a bit faster.
                        self.sent_by_seqnum = KeySortedCache::new(self.limit);
                        // Only insert the last ones.  The beginning ones would get pushed out anyway.
                        missing_range = (missing_range.end - (self.limit as u64))
                            ..missing_range.end;
                    }
                    for missing_seqnum in missing_range {
                        // This marks it as needing to be sent the next call to send_nacks()
                        self.sent_by_seqnum.insert(missing_seqnum, None);
                    }
                }
            }
        } else {
            // This is the first seqnum, so there is nothing to NACK and it's the max.
            self.max_received = Some(seqnum);
        }
    }

    // Return the sequence numbers that should be NACKed now, obeying simple rate limits
    // Entries expire after ~2s; if still missing, we ask every ~200ms
    #[allow(clippy::needless_lifetimes)]
    pub fn send_nacks<'sender>(
        &'sender mut self,
        now: Instant,
    ) -> Option<impl Iterator<Item = u64> + 'sender> {
        let mut send_any = false;
        self.sent_by_seqnum.retain(|_seqnum, sent| {
            if let Some((first_sent, last_sent)) = sent {
                if now.saturating_duration_since(*first_sent)
                    >= Duration::from_secs(2)
                {
                    // Expire it.
                    false
                } else if now.saturating_duration_since(*last_sent)
                    >= Duration::from_millis(200)
                {
                    // It has already been sent, but should be sent again.
                    send_any = true;
                    *last_sent = now;
                    true
                } else {
                    // It has already been sent and does not need to be sent again yet.
                    true
                }
            } else {
                // It hasn't been sent yet but should be.
                send_any = true;
                *sent = Some((now, now));
                true
            }
        });

        if send_any {
            Some(
                self.sent_by_seqnum
                    .iter()
                    .filter_map(move |(seqnum, sent)| {
                        if now == sent.unwrap().1 {
                            Some(*seqnum)
                        } else {
                            None
                        }
                    }),
            )
        } else {
            None
        }
    }
}
