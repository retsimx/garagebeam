//! Latency instrumentation for the GB-6 control path.
//!
//! The feature is opt-in via the `GARAGE_BEAM_LATENCY` environment variable and
//! is pure policy elsewhere: when disabled, no clock is read and no samples are
//! recorded, so the production control path is unchanged.

/// Environment variable that enables latency instrumentation.
pub const ENV: &str = "GARAGE_BEAM_LATENCY";

/// Whether latency instrumentation is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instrumentation {
    enabled: bool,
}

impl Instrumentation {
    /// Reads [`ENV`] from the process environment.
    pub fn from_env() -> Self {
        Self::from_value(std::env::var(ENV).ok().as_deref())
    }

    /// Pure parse of an optional raw value. Whitespace is trimmed and the
    /// comparison is ASCII case-insensitive. The truthy set is
    /// `{"1","true","yes","on"}`; everything else (including `None`, empty and
    /// unknown values) disables instrumentation.
    pub fn from_value(value: Option<&str>) -> Self {
        let enabled = value.map(str::trim).is_some_and(|v| {
            matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        });
        Self { enabled }
    }

    /// Returns whether instrumentation is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// Reads the monotonic clock in nanoseconds.
///
/// Returns `0` if `clock_gettime` fails. That cannot happen for
/// `CLOCK_MONOTONIC` with a valid pointer, so the fallback exists only to keep
/// this function total without unwinding.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `timespec`, and `CLOCK_MONOTONIC` is a
    // valid clock id, so the call writes a well-defined timestamp on success.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// A single latency observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Nanoseconds from beam edge to BLE write.
    pub edge_to_write_ns: u64,
    /// Nanoseconds from BLE write to read-back, when observed.
    pub write_to_read_ns: Option<u64>,
}

/// Nearest-rank percentile over an ascending-sorted slice.
///
/// `rank = ceil(p / 100 * N)` clamped into `1..=N`, using integer maths only
/// (the deployment host is a 32-bit armv6 Pi, so no floats). An empty slice
/// returns `0`. By construction `p = 0` yields the first element and
/// `p = 100` the last.
pub fn percentile(sorted_asc: &[u64], p: u8) -> u64 {
    let n = sorted_asc.len();
    if n == 0 {
        return 0;
    }
    let rank = (p as u64 * n as u64).div_ceil(100).clamp(1, n as u64);
    sorted_asc[rank as usize - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_value_accepts_truthy_in_any_case() {
        for value in ["1", "true", "yes", "on", "TRUE", "Yes", "ON", "TrUe"] {
            assert!(
                Instrumentation::from_value(Some(value)).enabled(),
                "expected {value:?} to enable instrumentation"
            );
        }
    }

    #[test]
    fn from_value_trims_surrounding_whitespace() {
        assert!(Instrumentation::from_value(Some("  true  ")).enabled());
        assert!(Instrumentation::from_value(Some("\tON\n")).enabled());
    }

    #[test]
    fn from_value_rejects_falsy_empty_and_unknown() {
        for value in [
            "0", "false", "no", "off", "", "  ", "garbage", "2", "trueish",
        ] {
            assert!(
                !Instrumentation::from_value(Some(value)).enabled(),
                "expected {value:?} to disable instrumentation"
            );
        }
        assert!(!Instrumentation::from_value(None).enabled());
    }

    #[test]
    fn percentile_single_element() {
        let values = [42];
        assert_eq!(percentile(&values, 0), 42);
        assert_eq!(percentile(&values, 50), 42);
        assert_eq!(percentile(&values, 95), 42);
        assert_eq!(percentile(&values, 99), 42);
        assert_eq!(percentile(&values, 100), 42);
    }

    #[test]
    fn percentile_four_elements() {
        let values = [10, 20, 30, 40];
        assert_eq!(percentile(&values, 0), 10);
        assert_eq!(percentile(&values, 50), 20);
        assert_eq!(percentile(&values, 95), 40);
        assert_eq!(percentile(&values, 99), 40);
        assert_eq!(percentile(&values, 100), 40);
    }

    #[test]
    fn percentile_hundred_elements() {
        let values: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&values, 0), 1);
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 95);
        assert_eq!(percentile(&values, 99), 99);
        assert_eq!(percentile(&values, 100), 100);
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 0), 0);
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[], 100), 0);
    }

    #[test]
    fn monotonic_ns_is_non_decreasing() {
        let first = monotonic_ns();
        let mut spin = 0u64;
        for i in 0..100_000u64 {
            spin = spin.wrapping_add(i);
        }
        std::hint::black_box(spin);
        let second = monotonic_ns();
        assert!(second >= first, "clock went backwards: {first} -> {second}");
    }
}
