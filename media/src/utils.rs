//! # Media Utilities
//!
//! Utility functions for media processing calculations.

use std::ops::{Add, Mul};

/// Smooth a signal with an exponential moving average.
/// `alpha` in [0,1]: 0 keeps the old value; 1 jumps to the new value.
pub fn exponential_moving_average<T: Mul<f64, Output = T> + Add<T, Output = T>>(
    average: T,    // Current average
    alpha: f64,    // Smoothing factor
    update: T,     // New value
) -> T {
    // Weighted average: new_value * alpha + old_average * (1 - alpha)
    (update * alpha) + (average * (1.0 - alpha))
}

#[cfg(test)]
mod exponential_moving_average_tests {
    use super::exponential_moving_average;

    #[test]
    #[allow(clippy::float_cmp)]
    fn interpolation() {
        assert_eq!(10.0, exponential_moving_average(10.0, 0.0, 20.0));
        assert_eq!(15.0, exponential_moving_average(10.0, 0.5, 20.0));
        assert_eq!(20.0, exponential_moving_average(10.0, 1.0, 20.0));
    }
}
