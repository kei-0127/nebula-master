//! # Data Rate Monitoring
//! 
//! Data rate monitoring and bandwidth measurement for media streams.
//! Tracks data usage, calculates transmission rates, and provides bandwidth statistics.

use std::{
    fmt::{self, Display, Formatter},
    iter::Sum,
    ops::{Add, AddAssign, Div, Mul, Sub, SubAssign},
};

use std::time::Duration;

/// Data size in bits (base-10 units). Useful for rate/throughput math.
/// Bits are base-10: 1 Kb = 1,000 bits; 1 Mb = 1,000,000 bits.
#[derive(Copy, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct DataSize {
    bits: u64,  // Size in bits
}

impl Default for DataSize {
    fn default() -> Self {
        Self::ZERO
    }
}

impl std::fmt::Debug for DataSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (unit, bits) in [
            ("gbits", 1_000_000_000),
            ("mbits", 1_000_000),
            ("kbits", 1_000),
        ] {
            if self.bits > bits {
                return write!(f, "{}{}", self.bits / bits, unit);
            }
        }
        write!(f, "{}bits", self.bits)
    }
}

impl DataSize {
    /// Zero data size
    pub const ZERO: Self = Self::from_bits(0);
    
    // Bit conversion constants
    const BITS_PER_BYTE: u64 = 8;                    // 8 bits per byte
    const BITS_PER_KILO_BIT: u64 = 1000;            // 1000 bits per kilobit
    const BITS_PER_MEGA_BIT: u64 = Self::BITS_PER_KILO_BIT * Self::BITS_PER_KILO_BIT;  // 1,000,000 bits per megabit

    /// Construct from raw bit count.
    pub const fn from_bits(bits: u64) -> Self {
        Self { bits }
    }

    /// Return the size in bits.
    pub fn as_bits(&self) -> u64 {
        self.bits
    }

    /// Convert to bytes (integer division; rounds down).
    pub const fn as_bytes(&self) -> u64 {
        self.bits / Self::BITS_PER_BYTE
    }

    /// Construct from bytes (1 byte = 8 bits).
    pub const fn from_bytes(bytes: u64) -> Self {
        Self::from_bits(bytes * Self::BITS_PER_BYTE)
    }

    /// Construct from kilobits (decimal; 1 Kb = 1000 bits).
    pub const fn from_kilobits(kbits: u64) -> Self {
        Self::from_bits(kbits * Self::BITS_PER_KILO_BIT)
    }

    /// Saturating subtraction: returns zero if `other` is larger.
    pub fn saturating_sub(self, other: Self) -> Self {
        if self > other {
            self - other
        } else {
            Self::default()
        }
    }
}

impl Add<DataSize> for DataSize {
    type Output = DataSize;

    fn add(self, other: DataSize) -> DataSize {
        DataSize::from_bits(self.bits + other.bits)
    }
}

impl AddAssign<DataSize> for DataSize {
    fn add_assign(&mut self, rhs: DataSize) {
        *self = *self + rhs;
    }
}

impl Sum for DataSize {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        Self::from_bits(iter.map(|size| size.bits).sum())
    }
}

impl Sub<DataSize> for DataSize {
    type Output = DataSize;

    fn sub(self, other: DataSize) -> DataSize {
        DataSize::from_bits(self.bits - other.bits)
    }
}

impl SubAssign<DataSize> for DataSize {
    fn sub_assign(&mut self, rhs: DataSize) {
        *self = *self - rhs
    }
}

impl Mul<f64> for DataSize {
    type Output = Self;

    /// Scale a `DataSize` by a floating factor (rounds toward zero).
    fn mul(self, x: f64) -> Self {
        Self::from_bits((self.bits as f64 * x as f64) as u64)
    }
}

impl Div<DataSize> for DataSize {
    type Output = f64;

    /// Ratio of two sizes (unitless), as `self.bits / other.bits`.
    fn div(self, other: DataSize) -> f64 {
        self.bits as f64 / other.bits as f64
    }
}

impl Div<f64> for DataSize {
    type Output = Self;

    /// Divide a `DataSize` by a floating factor (rounds toward zero).
    fn div(self, x: f64) -> Self {
        Self::from_bits((self.bits as f64 / x as f64) as u64)
    }
}

#[cfg(test)]
mod data_size_tests {
    use super::{DataRate, DataSize, Duration};

    #[test]
    fn default() {
        assert_eq!(DataSize::from_bits(0), Default::default());
    }

    #[test]
    fn from_bits() {
        assert_eq!(1, DataSize::from_bits(1).as_bits());
        assert_eq!(8, DataSize::from_bits(8).as_bits());
        assert_eq!(16, DataSize::from_bits(16).as_bits());
    }

    #[test]
    fn from_bytes() {
        assert_eq!(8, DataSize::from_bytes(1).as_bits());
        assert_eq!(16, DataSize::from_bytes(2).as_bits());
    }

    #[test]
    fn from_kilobits() {
        assert_eq!(1_000, DataSize::from_kilobits(1).as_bits());
        assert_eq!(2_000, DataSize::from_kilobits(2).as_bits());
    }

    #[test]
    fn as_bytes_rounds_down() {
        assert_eq!(0, DataSize::from_bits(1).as_bytes());
        assert_eq!(0, DataSize::from_bits(7).as_bytes());
        assert_eq!(1, DataSize::from_bits(8).as_bytes());
        assert_eq!(1, DataSize::from_bits(15).as_bytes());
        assert_eq!(2, DataSize::from_bits(16).as_bytes());
    }

    #[test]
    fn ordinal_comparisons() {
        assert!(DataSize::from_bits(2) > DataSize::from_bits(1));
        assert!(DataSize::from_bits(1) < DataSize::from_bits(2));
        assert!(DataSize::from_bits(2) >= DataSize::from_bits(2));
    }

    #[test]
    fn addition() {
        assert_eq!(
            DataSize::from_bits(1_008),
            DataSize::from_kilobits(1) + DataSize::from_bytes(1)
        );
    }

    #[test]
    fn add_assign() {
        let mut size = DataSize::from_kilobits(1);
        size += DataSize::from_bytes(1);
        assert_eq!(DataSize::from_bits(1_008), size);
    }

    #[test]
    fn subtraction() {
        assert_eq!(
            DataSize::from_bits(992),
            DataSize::from_kilobits(1) - DataSize::from_bytes(1)
        );
    }

    #[test]
    fn sub_assign() {
        let mut size = DataSize::from_kilobits(1);
        size -= DataSize::from_bytes(1);
        assert_eq!(DataSize::from_bits(992), size);
    }

    #[test]
    fn saturating_subtraction() {
        assert_eq!(
            DataSize::from_bits(901),
            DataSize::from_kilobits(1).saturating_sub(DataSize::from_bits(99))
        );
        assert_eq!(
            DataSize::from_bits(1),
            DataSize::from_bits(4).saturating_sub(DataSize::from_bits(3))
        );
        assert_eq!(
            DataSize::from_bits(0),
            DataSize::from_bits(4).saturating_sub(DataSize::from_bits(4))
        );
        assert_eq!(
            DataSize::from_bits(0),
            DataSize::from_bits(4).saturating_sub(DataSize::from_bits(5))
        );
    }

    #[test]
    fn multiplication_by_scalar() {
        assert_eq!(DataSize::from_bytes(56), DataSize::from_bytes(8) * 7.0f64);
        assert_eq!(DataSize::from_bytes(60), DataSize::from_bytes(8) * 7.5f64);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn division_by_data_size() {
        assert_eq!(7.0f64, DataSize::from_bytes(56) / DataSize::from_bytes(8));
        assert_eq!(7.5f64, DataSize::from_bytes(60) / DataSize::from_bytes(8));
    }

    #[test]
    fn division_by_scalar() {
        assert_eq!(DataSize::from_bytes(8), DataSize::from_bytes(56) / 7.0f64);
        assert_eq!(DataSize::from_bytes(8), DataSize::from_bytes(60) / 7.5f64);
    }

    #[test]
    fn division_by_duration() {
        assert_eq!(
            DataRate::from_bps(30),
            DataSize::from_bits(60) / Duration::from_secs(2)
        );
        assert_eq!(
            DataRate::from_bps(u64::MAX),
            DataSize::from_bytes(60) / Duration::ZERO
        );
        assert_eq!(
            DataRate::from_bps(0),
            DataSize::from_bytes(0) / Duration::ZERO
        );
    }

    #[test]
    fn sum() {
        let data_sizes = vec![
            DataSize::from_bits(1),
            DataSize::from_bits(2),
            DataSize::from_bits(5),
        ];
        assert_eq!(DataSize::from_bits(8), data_sizes.into_iter().sum());
    }
}

#[derive(Debug, Copy, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct DataRate {
    size_per_second: DataSize,
}

impl Default for DataRate {
    fn default() -> Self {
        Self::ZERO
    }
}

impl DataRate {
    pub const ZERO: Self = Self::per_second(DataSize::ZERO);

    /// Construct a rate from a `DataSize` per second.
    pub const fn per_second(size_per_second: DataSize) -> Self {
        Self { size_per_second }
    }

    /// Construct from bits per second (bps).
    pub const fn from_bps(bps: u64) -> Self {
        Self::per_second(DataSize::from_bits(bps))
    }

    /// Construct from kilobits per second (Kbps, decimal 1000-based).
    pub const fn from_kbps(kbps: u64) -> Self {
        Self::per_second(DataSize::from_kilobits(kbps))
    }

    /// Return bits per second.
    pub fn as_bps(&self) -> u64 {
        self.size_per_second.as_bits()
    }

    /// Return kilobits per second (decimal; rounds down).
    pub fn as_kbps(&self) -> u64 {
        self.as_bps() / DataSize::BITS_PER_KILO_BIT
    }

    /// Saturating subtraction of two rates (floors at zero).
    pub fn saturating_sub(self, other: Self) -> Self {
        Self::per_second(self.size_per_second.saturating_sub(other.size_per_second))
    }
}

impl Display for DataRate {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let bits = self.size_per_second.bits;
        // Human-friendly formatting: bps < 1000; Kbps up to < 1,000,000; else Mbps.
        // One decimal place using integer math; rounds down.
        if bits < DataSize::BITS_PER_KILO_BIT {
            write!(f, "{} bps", bits)
        } else if bits < DataSize::BITS_PER_MEGA_BIT {
            write!(
                f,
                "{:.1} Kbps",
                (bits * 10 / DataSize::BITS_PER_KILO_BIT) as f64 / 10f64
            )
        } else {
            write!(
                f,
                "{:.1} Mbps",
                (bits * 10 / DataSize::BITS_PER_MEGA_BIT) as f64 / 10f64
            )
        }
    }
}

impl Add<DataRate> for DataRate {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        DataRate::per_second(self.size_per_second + other.size_per_second)
    }
}

impl Sum for DataRate {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        Self::per_second(iter.map(|rate| rate.size_per_second).sum())
    }
}

impl Sub<DataRate> for DataRate {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        DataRate::per_second(self.size_per_second - other.size_per_second)
    }
}

impl Mul<f64> for DataRate {
    type Output = Self;

    fn mul(self, x: f64) -> Self {
        Self::per_second(self.size_per_second * x)
    }
}

impl Div<DataRate> for DataRate {
    type Output = f64;

    fn div(self, other: Self) -> f64 {
        self.size_per_second / other.size_per_second
    }
}

impl Div<f64> for DataRate {
    type Output = Self;

    fn div(self, x: f64) -> Self {
        Self::per_second(self.size_per_second / x)
    }
}

#[cfg(test)]
mod data_rate_tests {
    use super::DataRate;

    #[test]
    fn default() {
        assert_eq!(DataRate::from_bps(0), Default::default());
    }

    #[test]
    fn from_bps() {
        assert_eq!(1, DataRate::from_bps(1).as_bps());
        assert_eq!(8, DataRate::from_bps(8).as_bps());
        assert_eq!(16, DataRate::from_bps(16).as_bps());
    }

    #[test]
    fn from_kbps() {
        assert_eq!(1_000, DataRate::from_kbps(1).as_bps());
        assert_eq!(8_000, DataRate::from_kbps(8).as_bps());
        assert_eq!(16_000, DataRate::from_kbps(16).as_bps());
    }

    #[test]
    fn as_kbps_rounds_down() {
        assert_eq!(0, DataRate::from_bps(1).as_kbps());
        assert_eq!(0, DataRate::from_bps(999).as_kbps());
        assert_eq!(1, DataRate::from_bps(1_000).as_kbps());
        assert_eq!(2, DataRate::from_bps(2_999).as_kbps());
    }

    #[test]
    fn ordinal_comparisons() {
        assert!(DataRate::from_bps(2) > DataRate::from_bps(1));
        assert!(DataRate::from_bps(1) < DataRate::from_bps(2));
        assert!(DataRate::from_bps(2) >= DataRate::from_bps(2));
    }

    #[test]
    fn addition() {
        assert_eq!(
            DataRate::from_bps(1_099),
            DataRate::from_kbps(1) + DataRate::from_bps(99)
        );
    }

    #[test]
    fn subtraction() {
        assert_eq!(
            DataRate::from_bps(901),
            DataRate::from_kbps(1) - DataRate::from_bps(99)
        );
    }

    #[test]
    fn saturating_subtraction() {
        assert_eq!(
            DataRate::from_bps(901),
            DataRate::from_kbps(1).saturating_sub(DataRate::from_bps(99))
        );
        assert_eq!(
            DataRate::from_bps(1),
            DataRate::from_bps(4).saturating_sub(DataRate::from_bps(3))
        );
        assert_eq!(
            DataRate::from_bps(0),
            DataRate::from_bps(4).saturating_sub(DataRate::from_bps(4))
        );
        assert_eq!(
            DataRate::from_bps(0),
            DataRate::from_bps(4).saturating_sub(DataRate::from_bps(5))
        );
    }

    #[test]
    fn multiplication_by_scalar() {
        assert_eq!(DataRate::from_bps(56), DataRate::from_bps(8) * 7.0f64);
        assert_eq!(DataRate::from_bps(60), DataRate::from_bps(8) * 7.5f64);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn division_by_data_rate() {
        assert_eq!(7.0f64, DataRate::from_bps(56) / DataRate::from_bps(8));
        assert_eq!(7.5f64, DataRate::from_bps(60) / DataRate::from_bps(8));
    }

    #[test]
    fn division_by_scalar() {
        assert_eq!(DataRate::from_bps(8), DataRate::from_bps(56) / 7.0f64);
        assert_eq!(DataRate::from_bps(8), DataRate::from_bps(60) / 7.5f64);
    }

    #[test]
    fn sum() {
        let data_rates = vec![
            DataRate::from_bps(1),
            DataRate::from_bps(2),
            DataRate::from_bps(5),
        ];
        assert_eq!(DataRate::from_bps(8), data_rates.into_iter().sum());
    }

    #[test]
    fn display_rounds_down_to_1_decimal_point() {
        assert_eq!("0 bps", format!("{}", DataRate::ZERO));
        assert_eq!("1 bps", format!("{}", DataRate::from_bps(1)));
        assert_eq!("999 bps", format!("{}", DataRate::from_bps(999)));
        assert_eq!("1.0 Kbps", format!("{}", DataRate::from_bps(1_000)));
        assert_eq!("1.5 Kbps", format!("{}", DataRate::from_bps(1_550)));
        assert_eq!("1.9 Kbps", format!("{}", DataRate::from_bps(1_999)));
        assert_eq!("999.9 Kbps", format!("{}", DataRate::from_bps(999_999)));
        assert_eq!("1.0 Mbps", format!("{}", DataRate::from_bps(1_000_000)));
        assert_eq!("2.3 Mbps", format!("{}", DataRate::from_bps(2_350_000)));
    }
}

impl Mul<Duration> for DataRate {
    type Output = DataSize;

    /// Rate × duration → size (in bits), using floating seconds; rounds toward zero.
    fn mul(self, duration: Duration) -> DataSize {
        DataSize::from_bits(((self.as_bps() as f64) * duration.as_secs_f64()) as u64)
    }
}

impl Div<Duration> for DataSize {
    type Output = DataRate;

    /// Size ÷ duration → rate (bits per second), using floating seconds; rounds toward zero.
    fn div(self, duration: Duration) -> DataRate {
        DataRate::from_bps((self.as_bits() as f64 / duration.as_secs_f64()) as u64)
    }
}

impl Div<DataRate> for DataSize {
    type Output = Duration;

    /// Size ÷ rate → duration (seconds), using floating seconds.
    fn div(self, rate: DataRate) -> Duration {
        Duration::from_secs_f64((self.as_bits() as f64) / (rate.as_bps() as f64))
    }
}

#[cfg(test)]
mod data_rate_and_data_size_interaction_tests {
    use super::{DataRate, DataSize};
    use std::time::Duration;

    #[test]
    fn per_second() {
        assert_eq!(
            DataRate::from_bps(8),
            DataRate::per_second(DataSize::from_bytes(1))
        );
    }

    #[test]
    fn data_rate_multiplication_by_duration_gives_data_size() {
        assert_eq!(
            DataSize::from_bits(56),
            DataRate::from_bps(8) * Duration::from_secs(7)
        );
        assert_eq!(
            DataSize::from_bits(61_455),
            DataRate::from_bps(8_194) * Duration::from_secs_f64(7.5f64)
        );
    }

    #[test]
    fn data_size_division_by_duration_gives_data_rate() {
        assert_eq!(
            DataRate::from_bps(8),
            DataSize::from_bits(56) / Duration::from_secs(7)
        );
        assert_eq!(
            DataRate::from_bps(8_194),
            DataSize::from_bits(61_455) / Duration::from_secs_f64(7.5f64)
        );
    }

    #[test]
    fn data_size_division_by_data_rate_gives_duration() {
        assert_eq!(
            Duration::from_secs(7),
            DataSize::from_bits(56) / DataRate::from_bps(8)
        );
        assert_eq!(
            Duration::from_secs_f64(7.5f64),
            DataSize::from_bits(61_455) / DataRate::from_bps(8_194)
        );
    }
}
