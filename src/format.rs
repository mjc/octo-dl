//! Formatting helpers for human-readable byte sizes and durations.

use std::fmt;
use std::time::Duration;

/// Display wrapper for a human-readable byte count (B, KB, MB, GB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(u64);

#[must_use]
pub fn format_bytes(bytes: u64) -> ByteSize {
    ByteSize(bytes)
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_formatted_bytes(f, self.0)
    }
}

/// Appends a human-readable byte count to an existing string without creating
/// a temporary formatted byte string.
pub fn push_formatted_bytes(out: &mut String, bytes: u64) {
    write_formatted_bytes(out, bytes).expect("writing formatted bytes to a string should not fail");
}

fn write_formatted_bytes(out: &mut impl fmt::Write, bytes: u64) -> fmt::Result {
    match bytes {
        b if b >= GB => push_scaled_bytes(out, b, GB, "GB"),
        b if b >= MB => push_scaled_bytes(out, b, MB, "MB"),
        b if b >= KB => push_scaled_bytes(out, b, KB, "KB"),
        b => {
            push_decimal(out, b)?;
            out.write_str(" B")
        }
    }
}

const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const GB: u64 = MB * 1024;

fn push_scaled_bytes(
    out: &mut impl fmt::Write,
    bytes: u64,
    unit_bytes: u64,
    unit: &str,
) -> fmt::Result {
    let scaled =
        ((u128::from(bytes) * 100) + (u128::from(unit_bytes) / 2)) / u128::from(unit_bytes);
    push_decimal(out, (scaled / 100) as u64)?;
    out.write_char('.')?;
    let frac = (scaled % 100) as u8;
    out.write_char(char::from(b'0' + (frac / 10)))?;
    out.write_char(char::from(b'0' + (frac % 10)))?;
    out.write_char(' ')?;
    out.write_str(unit)
}

fn push_decimal(out: &mut impl fmt::Write, value: u64) -> fmt::Result {
    let mut buf = [0_u8; 20];
    let mut n = value;
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.write_str(std::str::from_utf8(&buf[i..]).expect("digits are utf-8"))
}

/// Formats a duration as a human-readable string (e.g. "5.0s", "1m 05s", "1h 01m 05s").
#[must_use]
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:01}s", secs, d.subsec_millis() / 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_formats_units() {
        assert_eq!(format_bytes(500).to_string(), "500 B");
        assert_eq!(format_bytes(1024).to_string(), "1.00 KB");
        assert_eq!(format_bytes(1536).to_string(), "1.50 KB");
        assert_eq!(format_bytes(1_048_576).to_string(), "1.00 MB");
        assert_eq!(format_bytes(1_073_741_824).to_string(), "1.00 GB");
    }

    #[test]
    fn format_duration_units() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5.0s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3665)), "1h 01m 05s");
    }

    #[test]
    fn format_bytes_formats_zero() {
        assert_eq!(format_bytes(0).to_string(), "0 B");
    }

    #[test]
    fn push_formatted_bytes_appends_without_replacing_prefix() {
        let mut out = String::from("Verified: ");

        push_formatted_bytes(&mut out, 1536);

        assert_eq!(out, "Verified: 1.50 KB");
    }

    #[test]
    fn format_bytes_returns_display_wrapper() {
        fn assert_display(_: impl std::fmt::Display) {}

        let label = format_bytes(42);

        assert_display(label);
        assert_eq!(format!("{label}/s"), "42 B/s");
    }

    #[test]
    fn format_bytes_formats_exact_boundaries() {
        assert_eq!(format_bytes(1024).to_string(), "1.00 KB");
        assert_eq!(format_bytes(1_048_576).to_string(), "1.00 MB");
        assert_eq!(format_bytes(1_073_741_824).to_string(), "1.00 GB");
    }

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(Duration::ZERO), "0.0s");
    }

    #[test]
    fn format_duration_subsecond() {
        assert_eq!(format_duration(Duration::from_millis(500)), "0.5s");
    }

    mod property_tests {
        use super::*;
        use proptest::{collection::vec, prelude::*};

        fn short_ascii_string() -> impl Strategy<Value = String> {
            vec(proptest::char::range(' ', '~'), 0..24)
                .prop_map(|chars| chars.into_iter().collect())
        }

        proptest! {
            #[test]
            fn push_formatted_bytes_preserves_prefix_and_matches_display(
                prefix in short_ascii_string(),
                bytes in any::<u64>(),
            ) {
                let mut out = prefix.clone();
                push_formatted_bytes(&mut out, bytes);

                assert_eq!(out, format!("{prefix}{}", format_bytes(bytes)));
            }

            #[test]
            fn format_bytes_selects_expected_unit_suffix(bytes in any::<u64>()) {
                let formatted = format_bytes(bytes).to_string();
                let suffix = match bytes {
                    b if b >= GB => " GB",
                    b if b >= MB => " MB",
                    b if b >= KB => " KB",
                    _ => " B",
                };

                assert!(formatted.ends_with(suffix), "{formatted} should end with {suffix}");
            }

            #[test]
            fn format_bytes_below_kilobyte_stays_exact(bytes in 0u64..KB) {
                assert_eq!(format_bytes(bytes).to_string(), format!("{bytes} B"));
            }

            #[test]
            fn format_duration_sub_minute_matches_tenths(
                secs in 0u64..60,
                millis in 0u32..1000,
            ) {
                let duration = Duration::from_secs(secs) + Duration::from_millis(u64::from(millis));
                assert_eq!(
                    format_duration(duration),
                    format!("{secs}.{:01}s", millis / 100),
                );
            }

            #[test]
            fn format_duration_minutes_match_expected_pattern(
                minutes in 1u64..60,
                seconds in 0u64..60,
            ) {
                let duration = Duration::from_secs((minutes * 60) + seconds);
                assert_eq!(
                    format_duration(duration),
                    format!("{minutes}m {seconds:02}s"),
                );
            }

            #[test]
            fn format_duration_hours_match_expected_pattern(
                hours in 1u64..1000,
                minutes in 0u64..60,
                seconds in 0u64..60,
            ) {
                let duration =
                    Duration::from_secs((hours * 3600) + (minutes * 60) + seconds);
                assert_eq!(
                    format_duration(duration),
                    format!("{hours}h {minutes:02}m {seconds:02}s"),
                );
            }
        }
    }
}
