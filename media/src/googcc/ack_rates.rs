use std::time::Duration;

use async_stream::stream;
use futures::{pin_mut, Stream, StreamExt};

use crate::{
    date_rate::{DataRate, DataSize},
    math::{AbsDiff, Square},
    transportcc::Ack,
};

fn accumulate_acked_sizes(
    acks: impl Stream<Item = Ack>,
) -> impl Stream<Item = (DataSize, Duration)> {
    // TODO: Maybe make some of these configurable
    let initial_ack_group_duration = Duration::from_millis(500);
    let subsequent_ack_group_duration = Duration::from_millis(150);

    stream! {
        pin_mut!(acks);
        if let Some(mut ack1) = acks.next().await {
            let mut accumulated_size = ack1.size;
            let mut accumulated_duration = Duration::ZERO;
            let mut target_ack_group_duration = initial_ack_group_duration;
            while let Some(ack2) = acks.next().await {
                if ack2.arrival < ack1.arrival {
                    // Reset when we hit out-of-order packets
                    accumulated_size = DataSize::ZERO;
                    accumulated_duration = Duration::ZERO;
                } else {
                    let arrival_delta = ack2.arrival.saturating_duration_since(ack1.arrival);
                    accumulated_duration += arrival_delta;
                    if arrival_delta > target_ack_group_duration {
                        // Reset if it's been too long since we've received an ACK
                        accumulated_size = DataSize::ZERO;
                        accumulated_duration = Duration::from_micros(
                            accumulated_duration.as_micros() as u64
                                % target_ack_group_duration.as_micros() as u64,
                        );
                    } else if accumulated_duration >= target_ack_group_duration {
                        yield (accumulated_size, target_ack_group_duration);

                        // Use what's "left over" for the next group.
                        accumulated_size = Default::default();
                        accumulated_duration =
                            accumulated_duration.saturating_sub(target_ack_group_duration);

                        // Now that we have a group, we can use a smaller window.
                        target_ack_group_duration = subsequent_ack_group_duration;
                    }
                }
                accumulated_size += ack2.size;
                ack1 = ack2;
            }
        }
    }
}

fn estimate_acked_rates_from_groups(
    ack_groups: impl Stream<Item = (DataSize, Duration)>,
) -> impl Stream<Item = DataRate> {
    stream! {
        pin_mut!(ack_groups);
        if let Some((size, duration)) = ack_groups.next().await {
            let mut estimate: DataRate = size / duration;
            let mut variance: f64 = 50.0;

            yield estimate;

            while let Some((size, duration)) = ack_groups.next().await {
                let sample: DataRate = size / duration;
                let sample_variance = ((sample.abs_diff(estimate) / estimate) * 10.0).square();
                let pred_variance = variance + 5.0;
                estimate = ((estimate * sample_variance) + (sample * pred_variance))
                    / (sample_variance + pred_variance);
                variance = (sample_variance * pred_variance) / (sample_variance + pred_variance);

                yield estimate;
            }
        }
    }
}

pub fn estimate_acked_rates(
    acks: impl Stream<Item = Ack>,
) -> impl Stream<Item = DataRate> {
    estimate_acked_rates_from_groups(accumulate_acked_sizes(acks))
}
