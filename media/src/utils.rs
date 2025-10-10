use std::ops::{Add, Mul};

pub fn exponential_moving_average<T: Mul<f64, Output = T> + Add<T, Output = T>>(
    average: T,
    alpha: f64,
    update: T,
) -> T {
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
