use std::time::Duration;

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_MONTH: u64 = 30 * SECONDS_PER_DAY;
const SECONDS_PER_YEAR: u64 = 365 * SECONDS_PER_DAY;

#[must_use]
pub fn format_compact_latency(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds == 0 {
        return format!("{}ms", duration.subsec_millis());
    }
    if seconds < SECONDS_PER_MINUTE {
        return format!("{}.{:01}s", seconds, duration.subsec_millis() / 100);
    }

    let minutes = seconds / SECONDS_PER_MINUTE;
    if minutes < SECONDS_PER_MINUTE {
        return format!("{minutes}m");
    }

    let hours = seconds / SECONDS_PER_HOUR;
    if hours < 100 {
        return format!("{hours}h");
    }
    "99+h".to_owned()
}

#[must_use]
pub fn format_compact_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < SECONDS_PER_MINUTE {
        return format!("{seconds}s");
    }
    if seconds < SECONDS_PER_HOUR {
        return format!("{}m", seconds / SECONDS_PER_MINUTE);
    }
    if seconds < 3 * SECONDS_PER_HOUR {
        return format_parts(
            seconds / SECONDS_PER_HOUR,
            "h",
            seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE,
            "m",
        );
    }
    if seconds < SECONDS_PER_DAY {
        return format!("{}h", seconds / SECONDS_PER_HOUR);
    }
    if seconds < 3 * SECONDS_PER_DAY {
        return format_parts(
            seconds / SECONDS_PER_DAY,
            "d",
            seconds % SECONDS_PER_DAY / SECONDS_PER_HOUR,
            "h",
        );
    }
    if seconds < SECONDS_PER_MONTH {
        return format!("{}d", seconds / SECONDS_PER_DAY);
    }
    if seconds < 3 * SECONDS_PER_MONTH {
        return format_parts(
            seconds / SECONDS_PER_MONTH,
            "mo",
            seconds % SECONDS_PER_MONTH / SECONDS_PER_DAY,
            "d",
        );
    }
    if seconds < SECONDS_PER_YEAR {
        return format!("{}mo", seconds / SECONDS_PER_MONTH);
    }

    let years = seconds / SECONDS_PER_YEAR;
    if years >= 100 {
        return "99+y".to_owned();
    }
    if years < 3 {
        return format_parts(
            years,
            "y",
            seconds % SECONDS_PER_YEAR / SECONDS_PER_MONTH,
            "mo",
        );
    }
    format!("{years}y")
}

#[must_use]
pub fn format_relative_age(age: Duration) -> String {
    format!("{} ago", format_compact_duration(age))
}

fn format_parts(primary: u64, primary_unit: &str, secondary: u64, secondary_unit: &str) -> String {
    if secondary == 0 {
        format!("{primary}{primary_unit}")
    } else {
        format!("{primary}{primary_unit} {secondary}{secondary_unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_duration_covers_adaptive_boundaries_and_caps() {
        let minute = Duration::from_secs(SECONDS_PER_MINUTE);
        let hour = Duration::from_secs(SECONDS_PER_HOUR);
        let day = Duration::from_secs(SECONDS_PER_DAY);
        let month = Duration::from_secs(SECONDS_PER_MONTH);
        let year = Duration::from_secs(SECONDS_PER_YEAR);

        assert_eq!(format_compact_duration(Duration::ZERO), "0s");
        assert_eq!(format_compact_duration(Duration::from_secs(59)), "59s");
        assert_eq!(format_compact_duration(minute), "1m");
        assert_eq!(
            format_compact_duration(Duration::from_secs(59 * 60 + 59)),
            "59m"
        );
        assert_eq!(format_compact_duration(hour), "1h");
        assert_eq!(format_compact_duration(Duration::from_mins(134)), "2h 14m");
        assert_eq!(format_compact_duration(Duration::from_hours(3)), "3h");
        assert_eq!(format_compact_duration(Duration::from_mins(1_439)), "23h");
        assert_eq!(format_compact_duration(day), "1d");
        assert_eq!(format_compact_duration(Duration::from_hours(51)), "2d 3h");
        assert_eq!(format_compact_duration(day * 3), "3d");
        assert_eq!(format_compact_duration(day * 29), "29d");
        assert_eq!(format_compact_duration(month), "1mo");
        assert_eq!(
            format_compact_duration(Duration::from_secs(2 * SECONDS_PER_MONTH + 4 * 86_400)),
            "2mo 4d"
        );
        assert_eq!(
            format_compact_duration(Duration::from_secs(3 * SECONDS_PER_MONTH)),
            "3mo"
        );
        assert_eq!(
            format_compact_duration(Duration::from_secs(11 * SECONDS_PER_MONTH)),
            "11mo"
        );
        assert_eq!(format_compact_duration(year), "1y");
        assert_eq!(
            format_compact_duration(Duration::from_secs(
                2 * SECONDS_PER_YEAR + 3 * SECONDS_PER_MONTH
            )),
            "2y 3mo"
        );
        assert_eq!(
            format_compact_duration(Duration::from_secs(3 * SECONDS_PER_YEAR)),
            "3y"
        );
        assert_eq!(
            format_compact_duration(Duration::from_secs(99 * SECONDS_PER_YEAR)),
            "99y"
        );
        assert_eq!(
            format_compact_duration(Duration::from_secs(100 * SECONDS_PER_YEAR)),
            "99+y"
        );
        assert_eq!(format_compact_duration(Duration::MAX), "99+y");
    }

    #[test]
    fn compact_latency_covers_boundaries_and_caps() {
        assert_eq!(format_compact_latency(Duration::from_millis(999)), "999ms");
        assert_eq!(format_compact_latency(Duration::from_millis(1_250)), "1.2s");
        assert_eq!(format_compact_latency(Duration::from_mins(1)), "1m");
        assert_eq!(
            format_compact_latency(Duration::from_secs(SECONDS_PER_HOUR)),
            "1h"
        );
        assert_eq!(
            format_compact_latency(Duration::from_secs(100 * SECONDS_PER_HOUR)),
            "99+h"
        );
        assert_eq!(format_compact_latency(Duration::MAX), "99+h");
    }

    #[test]
    fn relative_age_appends_suffix_and_remains_bounded() {
        assert_eq!(format_relative_age(Duration::from_mins(2)), "2m ago");
        assert_eq!(format_relative_age(Duration::MAX), "99+y ago");
    }
}

#[derive(Clone, Copy)]
enum ByteScale {
    Decimal,
    Binary,
}

impl ByteScale {
    const fn base(self) -> u64 {
        match self {
            Self::Decimal => 1_000,
            Self::Binary => 1_024,
        }
    }

    const fn unit(self, index: usize) -> &'static str {
        match (self, index) {
            (Self::Decimal | Self::Binary, 0) => "B",
            (Self::Decimal, 1) => "kB",
            (Self::Decimal, 2) => "MB",
            (Self::Decimal, 3) => "GB",
            (Self::Decimal, 4) => "TB",
            (Self::Decimal, 5) => "PB",
            (Self::Decimal, _) => "EB",
            (Self::Binary, 1) => "KiB",
            (Self::Binary, 2) => "MiB",
            (Self::Binary, 3) => "GiB",
            (Self::Binary, 4) => "TiB",
            (Self::Binary, 5) => "PiB",
            (Self::Binary, _) => "EiB",
        }
    }

    const fn max_unit() -> usize {
        6
    }
}

struct ScaledBytes {
    value: f64,
    whole_value: u64,
    unit_index: usize,
}

#[allow(clippy::cast_precision_loss)]
fn scale_bytes(bytes: u64, scale: ByteScale) -> ScaledBytes {
    let base = scale.base();
    let max_unit = ByteScale::max_unit();
    let mut unit_factor: u64 = 1;
    let mut unit_index = 0;
    while bytes >= unit_factor.saturating_mul(base) && unit_index < max_unit {
        unit_factor *= base;
        unit_index += 1;
    }
    let mut value = bytes as f64 / unit_factor as f64;
    if matches!(scale, ByteScale::Decimal)
        && unit_index > 0
        && value >= 999.95
        && unit_index < max_unit
    {
        unit_factor *= base;
        unit_index += 1;
        value = bytes as f64 / unit_factor as f64;
    }
    ScaledBytes {
        value,
        whole_value: bytes / unit_factor,
        unit_index,
    }
}

#[must_use]
pub fn format_decimal_bytes(bytes: u64) -> String {
    let scaled = scale_bytes(bytes, ByteScale::Decimal);
    if scaled.unit_index == 0 {
        format!("{bytes} B")
    } else {
        format!(
            "{:.1} {}",
            scaled.value,
            ByteScale::Decimal.unit(scaled.unit_index)
        )
    }
}

#[must_use]
pub fn format_decimal_byte_rate(rate: Option<u64>) -> String {
    rate.map_or_else(
        || "--".to_owned(),
        |rate| format!("{}/s", format_decimal_bytes(rate)),
    )
}

#[must_use]
pub fn format_binary_bytes(bytes: u64) -> String {
    let scaled = scale_bytes(bytes, ByteScale::Binary);
    if scaled.unit_index == 0 {
        format!("{bytes} B")
    } else {
        format!(
            "{} {}",
            scaled.whole_value,
            ByteScale::Binary.unit(scaled.unit_index)
        )
    }
}
