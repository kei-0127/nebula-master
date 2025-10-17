//! # Google Congestion Control (GoogCC)
//! 
//! Google's congestion control algorithm for WebRTC.
//! Dynamically adjusts media bitrate based on network conditions.

use std::cmp::{max, min};
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::{
    stream::{Stream, StreamExt},
    FutureExt,
};

use crate::date_rate::DataSize;
use crate::math::Square;
use crate::utils::exponential_moving_average;
use crate::{date_rate::DataRate, transportcc::Ack};

mod delay_directions;
use delay_directions::*;

mod ack_rates;
use ack_rates::*;

mod feedback_rtts;
use feedback_rtts::*;

mod stream;
use stream::StreamExt as OurStreamExt;

/// Configuration for Google Congestion Control
/// 
/// This struct defines the parameters for the GoogCC algorithm,
/// including target bitrate ranges and initial settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub initial_target_send_rate: DataRate,  // Starting bitrate
    pub min_target_send_rate: DataRate,       // Minimum allowed bitrate
    pub max_target_send_rate: DataRate,       // Maximum allowed bitrate
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Default bitrate settings for optimal WebRTC performance
            // These values are tuned for typical network conditions
            initial_target_send_rate: DataRate::from_kbps(5000),  // 5 Mbps initial rate
            min_target_send_rate: DataRate::from_kbps(100),       // 100 kbps minimum
            max_target_send_rate: DataRate::from_kbps(30000),     // 30 Mbps maximum
        }
    }
}

impl Config {
    fn clamp_target_send_rate(&self, target_send_rate: DataRate) -> DataRate {
        target_send_rate.clamp(self.min_target_send_rate, self.max_target_send_rate)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Baseline we want to at least reach quickly when ramping up.
    // If we're below this, try to ramp up to it quickly.
    pub base: DataRate,
    /// Soft cap we try not to exceed; keeps headroom to avoid oscillations.
    // There's no point going much above this.
    // It's effectively a max, but we have to leave some
    // headroom above it so that fluctuations in the
    // rate won't cause us to send below rate when
    // we otherwise would be able to.  Doing so can cause
    // annoying flucutations in which video layer
    // is forwarded.
    pub ideal: DataRate,
}

/// Maintains GoogCC signal streams and computes the target send rate.
pub struct CongestionController {
    current_request: Option<Request>,
    acks_sender1: UnboundedSender<Vec<Ack>>,
    acks_sender2: UnboundedSender<Vec<Ack>>,
    acks_sender3: UnboundedSender<Vec<Ack>>,
    feedback_rtts: Pin<Box<dyn Stream<Item = Duration> + Send>>,
    acked_rates: Pin<Box<dyn Stream<Item = DataRate> + Send>>,
    delay_directions: Pin<Box<dyn Stream<Item = (Instant, DelayDirection)> + Send>>,
    calculator: TargetCalculator,
}

impl CongestionController {
    /// Wire up the GoogCC signal pipeline (RTT, acked rate, delay direction) and target calculator.
    pub fn new(config: Config, now: Instant) -> Self {
        //                                       Acks                                    Latest Request
        //                                        |                                            |
        //             +--------------------------+--------------------------+                 |
        //             |                          |                          |                 |
        // +-----------v------------+ +-----------v------------+ +-----------v-----------+     |
        // | estimate_feedback_rtts | | accumulate_acked_sizes | | accumulate_ack_groups |     |
        // +-----------+------------+ +-----------+------------+ +-----------+-----------+     |
        //             |                          |                          |                 |
        //             |               +----------v-----------+   +----------v-----------+     |
        //             |               | estimate_acked_rates |   | calculate_ack_deltas |     |
        //             |               +----------+-----------+   +----------+-----------+     |
        //             |                          |                          |                 |
        //             |                          |             +------------v-----------+     |
        //             |                          |             | calculate_delay_slopes |     |
        //             |                          |             +------------+-----------+     |
        //             +---------------+          |                          |                 |
        //                             |          |         +----------------v-----------+     |
        //                             |          |         | calculate_delay_directions |     |
        //                             |          |         +----------------+-----------+     |
        //                             |          |                          |                 |
        //                             |          |        +-----------------+                 |
        //                             |          |        |                                   |
        //                        +----v----------v--------v----+                              |
        //                        | calculate_target_send_rates <------------------------------+
        //                        +-----------------------------+
        let (acks_sender1, ack_reports1) = unbounded();
        let (acks_sender2, ack_reports2) = unbounded();
        let (acks_sender3, ack_reports3) = unbounded();
        let feedback_rtts = estimate_feedback_rtts(ack_reports1).latest_only();
        let acked_rates =
            estimate_acked_rates(ack_reports2.flat_map(futures::stream::iter))
                .latest_only();
        let delay_directions =
            calculate_delay_directions(ack_reports3.flat_map(futures::stream::iter))
                .latest_only();
        let calculator = TargetCalculator::new(config, now);

        Self {
            current_request: None,
            acks_sender1,
            acks_sender2,
            acks_sender3,
            feedback_rtts: Box::pin(feedback_rtts),
            acked_rates: Box::pin(acked_rates),
            delay_directions: Box::pin(delay_directions),
            calculator,
        }
    }

    /// Update the external request (baseline/ideal) that guides the target rate.
    pub fn request(&mut self, request: Request) {
        self.current_request = Some(request);
    }

    /// Feed fresh ACKs and produce a new target send rate if something changed.
    /// Internally updates RTT, acked_rate, and delay trend, then adjusts the target.
    pub fn recalculate_target_send_rate(
        &mut self,
        mut acks: Vec<Ack>,
    ) -> Option<DataRate> {
        if acks.is_empty() {
            return None;
        }

        // TODO: See if we can get rid of these clones.
        acks.sort_by_key(|ack| ack.arrival);
        let _ = self.acks_sender1.unbounded_send(acks.clone());
        let _ = self.acks_sender2.unbounded_send(acks.clone());
        let _ = self.acks_sender3.unbounded_send(acks);

        let rtt = self
            .feedback_rtts
            .next()
            .now_or_never()
            .map(|next| next.expect("stream should never end"));
        let acked_rate = self
            .acked_rates
            .next()
            .now_or_never()
            .map(|next| next.expect("stream should never end"));

        let delay_direction = self
            .delay_directions
            .next()
            .now_or_never()
            .map(|next| next.expect("stream should never end"));

        self.calculator.next(
            &mut self.current_request,
            delay_direction,
            rtt,
            acked_rate,
        )
    }
}

/// Internal calculator that folds in delay trend, RTT, and acked rate
/// to produce the next target send rate within configured bounds.
struct TargetCalculator {
    acked_rate: Option<DataRate>,
    acked_rate_when_overusing: RateAverager,
    config: Config,
    previous_direction: Option<DelayDirection>,
    requested: Request,
    rtt: Duration,
    target_send_rate: DataRate,
    target_send_rate_updated: Instant,
}

impl TargetCalculator {
    fn new(config: Config, start_time: Instant) -> Self {
        Self {
            acked_rate: None,
            acked_rate_when_overusing: RateAverager::new(),
            previous_direction: None,
            requested: Request {
                // Basically use the config.initial_target_send_rate instead.
                base: config.initial_target_send_rate,
                // Basically uncapped until we receive a request.
                ideal: config.max_target_send_rate,
            },
            rtt: Duration::from_millis(100),
            target_send_rate: config.initial_target_send_rate,
            target_send_rate_updated: start_time,
            config,
        }
    }

    fn next(
        &mut self,
        request: &mut Option<Request>,
        delay_directions: Option<(Instant, DelayDirection)>,
        rtt: Option<Duration>,
        acked_rate: Option<DataRate>,
    ) -> Option<DataRate> {
        const MULTIPLICATIVE_INCREASE_PER_SECOND: f64 = 0.08;
        const MIN_MULTIPLICATIVE_INCREASE: DataRate = DataRate::from_kbps(1);
        const ADDITIVE_INCREASE_PER_RTT: DataSize = DataSize::from_bytes(1200);
        const ADDITIVE_INCREASE_RTT_PAD: Duration = Duration::from_millis(100);
        const MIN_ADDITIVE_INCREASE_PER_SECOND: DataRate = DataRate::from_kbps(4);
        const MIN_RTT_FOR_DECREASE: Duration = Duration::from_millis(10);
        const MAX_RTT_FOR_DECREASE: Duration = Duration::from_millis(200);
        const DECREASE_FROM_ACKED_RATE_MULTIPLIER: f64 = 0.85;

        if let Some(rtt) = rtt {
            self.rtt = rtt;
        }

        if let Some(acked_rate) = acked_rate {
            self.acked_rate = Some(acked_rate);
        }

        if let Some((now, direction)) = delay_directions {
            let mut reset_to_initial = false;
            if let Some(request) = request.take() {
                let previously_requested =
                    std::mem::replace(&mut self.requested, request);
                // We pick a value that is a common threshold for a client requesting very little,
                // such as one video at the lowest resolution.  Anything smaller than that is
                // considered low enough that googcc can't operate well (at least until we add probing)
                // So when we transition out of such a state, we reset googcc.
                let tiny_send_rate: DataRate = DataRate::from_kbps(150);
                let previous_ideal_was_tiny =
                    previously_requested.ideal <= tiny_send_rate;
                let current_ideal_is_tiny = self.requested.ideal <= tiny_send_rate;
                let initial_target_send_rate = max(
                    self.config.initial_target_send_rate,
                    self.requested.base * 0.5,
                );
                let target_below_initial =
                    self.target_send_rate < initial_target_send_rate;
                if previous_ideal_was_tiny
                    && !current_ideal_is_tiny
                    && target_below_initial
                {
                    // We were previously limited by a tiny ideal send rate,
                    // but are no longer, and we're below the initial send
                    // rate, so it's like we've started all over.
                    // Might as well just jump up to the initial rate.
                    self.target_send_rate = initial_target_send_rate;
                    reset_to_initial = true;
                }
            }

            let changed_target_send_rate: Option<DataRate> = match direction {
                DelayDirection::Decreasing => {
                    // While the delay is decreasing, hold the target rate to let the queues drain.
                    // The non-update of target_send_rate_updated is intentional.
                    None
                }
                DelayDirection::Steady => {
                    // While delay is steady, increase the target rate.
                    if let Some(acked_rate) = self.acked_rate {
                        self.acked_rate_when_overusing
                            .reset_if_sample_out_of_bounds(acked_rate);
                    }

                    let increase_duration = if self.previous_direction
                        != Some(DelayDirection::Steady)
                    {
                        // This is a strange thing where the first "steady" we have after an
                        // increase/decrease basically only increases the rate a little or not at
                        // all. This is because we don't know how long it's been steady.
                        Duration::ZERO
                    } else {
                        now.saturating_duration_since(self.target_send_rate_updated)
                    };
                    // If we don't have a good average acked_rate when overusing,
                    // use a faster increase (8%-16% per second)
                    // Otherwise, use a slower increase (1200 bytes per RTT).
                    let should_do_multiplicative_increase =
                        self.acked_rate_when_overusing.average().is_none();
                    let increase = if should_do_multiplicative_increase {
                        let multiplicative_increase_per_second =
                            if self.target_send_rate < self.requested.base {
                                // If we're below the requested base rate, increase more aggressively
                                MULTIPLICATIVE_INCREASE_PER_SECOND * 2.0
                            } else {
                                MULTIPLICATIVE_INCREASE_PER_SECOND
                            };
                        let multiplier = (1.0 + multiplicative_increase_per_second)
                            .powf(increase_duration.as_secs_f64().min(1.0))
                            - 1.0;
                        max(
                            MIN_MULTIPLICATIVE_INCREASE,
                            self.target_send_rate * multiplier,
                        )
                    } else {
                        let padded_rtt = self.rtt + ADDITIVE_INCREASE_RTT_PAD;
                        let increase_per_second = max(
                            MIN_ADDITIVE_INCREASE_PER_SECOND,
                            ADDITIVE_INCREASE_PER_RTT / padded_rtt,
                        );
                        increase_per_second * increase_duration.as_secs_f64()
                    };
                    let mut increased_rate = self.target_send_rate + increase;
                    // If we have an acked_rate, never increase over 200% + 5Mb of it.
                    if let Some(acked_rate) = self.acked_rate {
                        let acked_rate_based_limit =
                            (acked_rate * 2.0) + DataRate::from_kbps(5000);
                        increased_rate = min(acked_rate_based_limit, increased_rate);
                    }
                    // Don't end up decreasing when we were supposed to increase.
                    let increased_rate = max(self.target_send_rate, increased_rate);
                    Some(increased_rate)
                }
                DelayDirection::Increasing => {
                    // If the delay is increasing, decrease the rate.
                    if let Some(acked_rate) = self.acked_rate {
                        // We have an acked rate, so reduce based on that.
                        if (now
                            >= self.target_send_rate_updated
                                + self.rtt.clamp(
                                    MIN_RTT_FOR_DECREASE,
                                    MAX_RTT_FOR_DECREASE,
                                ))
                            || (acked_rate <= self.target_send_rate * 0.5)
                        {
                            let mut decreased_rate =
                                acked_rate * DECREASE_FROM_ACKED_RATE_MULTIPLIER;
                            if decreased_rate > self.target_send_rate {
                                if let Some(average_acked_rate_when_overusing) =
                                    self.acked_rate_when_overusing.average()
                                {
                                    decreased_rate =
                                        average_acked_rate_when_overusing
                                            * DECREASE_FROM_ACKED_RATE_MULTIPLIER;
                                }
                            }
                            self.acked_rate_when_overusing
                                .reset_if_sample_out_of_bounds(acked_rate);
                            self.acked_rate_when_overusing.add_sample(acked_rate);

                            // Don't accidentally increase the estimated rate!
                            let decreased_rate =
                                min(self.target_send_rate, decreased_rate);
                            Some(decreased_rate)
                        } else {
                            // Wait until the next period that we're increasing to decrease (or until the acked_rate drops).
                            // The non-update of the target_send_rate_updated is intentional.
                            None
                        }
                    } else {
                        // We're increasing before we even have an acked rate.  Aggressively reduce.
                        let decreased_rate = self.target_send_rate / 2.0;
                        Some(decreased_rate)
                    }
                }
            };

            // Apply clamping to any change, including a change because of resetting to initial.
            self.previous_direction = Some(direction);

            if changed_target_send_rate.is_some() || reset_to_initial {
                self.target_send_rate = self.config.clamp_target_send_rate(
                    changed_target_send_rate.unwrap_or(self.target_send_rate),
                );
                self.target_send_rate_updated = now;
                return Some(self.target_send_rate);
            }
        }
        None
    }
}

struct RateAverager {
    average: Option<DataRate>,
    // Note that this is the *relative* variance, equal to the variance divided by the mean.
    // The values for the initial, min, and max are calibrated for kbps.
    variance: DataRate,
}

// Maybe make these things configurable: alpha, initial/min/max variance, number
// of standard deviations for the "reset bounds"
impl RateAverager {
    fn new() -> Self {
        Self {
            average: None,
            variance: DataRate::from_bps(400),
        }
    }

    fn add_sample(&mut self, sample: DataRate) {
        let alpha = 0.05;
        let average = if let Some(average) = self.average {
            exponential_moving_average(average, alpha, sample)
        } else {
            sample
        };

        self.average = Some(average);
        self.variance = {
            // Do the calculation in bps f64 space.
            let var = self.variance.as_bps() as f64;
            let avg = average.as_bps() as f64;
            let sample = sample.as_bps() as f64;
            let var = (((1.0 - alpha) * var)
                + (alpha * (avg - sample).square() / avg.max(1.0)))
            .clamp(400.0, 2500.0);
            DataRate::from_bps(var as u64)
        };
    }

    // Does not add sample.
    fn reset_if_sample_out_of_bounds(&mut self, sample: DataRate) {
        if let Some(average) = self.average {
            let relative_bound = {
                // Do the calculation in bps f64 space.
                let var = self.variance.as_bps() as f64;
                let avg = average.as_bps() as f64;
                let bound = 3.0 * (var * avg).sqrt();
                DataRate::from_bps(bound as u64)
            };
            if sample > (average + relative_bound)
                || sample < average.saturating_sub(relative_bound)
            {
                *self = Self::new();
            }
        }
    }

    fn average(&self) -> Option<DataRate> {
        self.average
    }
}

#[cfg(test)]
mod rate_averager_tests {
    use super::*;

    #[test]
    fn new_has_no_average() {
        let averager = RateAverager::new();
        assert_eq!(None, averager.average());
    }

    #[test]
    fn first_sample_sets_baseline() {
        let mut averager = RateAverager::new();
        let sample = DataRate::from_bps(1_234_567);
        averager.add_sample(sample);
        assert_eq!(Some(sample), averager.average());
    }

    #[test]
    fn further_samples_converge() {
        let mut averager = RateAverager::new();

        averager.add_sample(DataRate::from_bps(1_000));
        averager.add_sample(DataRate::from_bps(2_000));
        let average = averager.average().unwrap();
        assert!(average > DataRate::from_bps(1_000), "{:?}", average);
        // Assume we never want to bias towards new samples over our running average.
        assert!(average < DataRate::from_bps(1_500), "{:?}", average);

        // Continue sampling 2,000 until we get as close as interpolation will let us.
        let mut i = 0;
        while averager.average().unwrap() < DataRate::from_bps(1_980) {
            assert!(i < 10_000, "not converging fast enough");
            averager.add_sample(DataRate::from_bps(2_000));
            i += 1;
        }
    }

    #[test]
    fn non_outliers_do_not_reset() {
        let mut averager = RateAverager::new();
        averager.add_sample(DataRate::from_bps(10_000));
        assert!(averager.average().is_some());

        averager.reset_if_sample_out_of_bounds(DataRate::from_bps(10_000));
        assert!(averager.average().is_some());
        averager.reset_if_sample_out_of_bounds(DataRate::from_bps(9_000));
        assert!(averager.average().is_some());
        averager.reset_if_sample_out_of_bounds(DataRate::from_bps(11_000));
        assert!(averager.average().is_some());
    }

    #[test]
    fn obvious_outliers_cause_reset() {
        let mut averager = RateAverager::new();

        averager.add_sample(DataRate::from_bps(10_000));
        assert!(averager.average().is_some());
        averager.reset_if_sample_out_of_bounds(DataRate::from_bps(1_000));
        assert_eq!(None, averager.average());

        averager.add_sample(DataRate::from_bps(10_000));
        assert!(averager.average().is_some());
        averager.reset_if_sample_out_of_bounds(DataRate::from_bps(100_000));
        assert_eq!(None, averager.average());
    }
}
