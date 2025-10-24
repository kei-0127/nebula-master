use std::time::Duration;

use async_stream::stream;
use futures::{pin_mut, Stream, StreamExt};

use crate::{ring_buffer::RingBuffer, transportcc::Ack};

const FEEDBACK_RTTS_HISTORY_LEN: usize = 32;

/// Estimate mean feedback RTT from batches of ACK reports; emits a running average.
pub fn estimate_feedback_rtts(
    ack_reports: impl Stream<Item = Vec<Ack>>,
) -> impl Stream<Item = Duration> {
    stream! {
        let mut history: RingBuffer<Duration> = RingBuffer::new(FEEDBACK_RTTS_HISTORY_LEN);
        pin_mut!(ack_reports);
        while let Some(acks) = ack_reports.next().await {
            if let Some(max_feedback_rtt) = acks
                .iter()
                .map(|ack| {
                    ack.feedback_arrival
                        .saturating_duration_since(ack.departure)
                })
                .max()
            {
                history.push(max_feedback_rtt);
                let mean_feedback_rtt: Duration =
                    history.iter().sum::<Duration>() / (history.len() as u32);
                yield mean_feedback_rtt;
            }
        }
    }
}
