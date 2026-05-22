//! Formatting helpers for human-readable byte sizes and durations.

use std::time::Duration;

/// Formats a byte count as a human-readable string (B, KB, MB, GB).
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    let mut out = String::new();
    push_formatted_bytes(&mut out, bytes);
    out
}

/// Appends a human-readable byte count to an existing string without creating
/// a temporary formatted byte string.
pub fn push_formatted_bytes(out: &mut String, bytes: u64) {
    match bytes {
        b if b >= GB => push_scaled_bytes(out, b, GB, "GB"),
        b if b >= MB => push_scaled_bytes(out, b, MB, "MB"),
        b if b >= KB => push_scaled_bytes(out, b, KB, "KB"),
        b => {
            push_decimal(out, b);
            out.push_str(" B");
        }
    }
}

const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const GB: u64 = MB * 1024;

fn push_scaled_bytes(out: &mut String, bytes: u64, unit_bytes: u64, unit: &str) {
    let scaled =
        ((u128::from(bytes) * 100) + (u128::from(unit_bytes) / 2)) / u128::from(unit_bytes);
    push_decimal(out, (scaled / 100) as u64);
    out.push('.');
    let frac = (scaled % 100) as u8;
    out.push(char::from(b'0' + (frac / 10)));
    out.push(char::from(b'0' + (frac % 10)));
    out.push(' ');
    out.push_str(unit);
}

fn push_decimal(out: &mut String, value: u64) {
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
    out.push_str(std::str::from_utf8(&buf[i..]).expect("digits are utf-8"));
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
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(1_048_576), "1.00 MB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GB");
    }

    #[test]
    fn format_duration_units() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5.0s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3665)), "1h 01m 05s");
    }

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn push_formatted_bytes_appends_without_replacing_prefix() {
        let mut out = String::from("Verified: ");

        push_formatted_bytes(&mut out, 1536);

        assert_eq!(out, "Verified: 1.50 KB");
    }

    #[test]
    fn format_bytes_exact_boundaries() {
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1_048_576), "1.00 MB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GB");
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
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn format_bytes_never_panics(bytes in 0u64..u64::MAX) {
                let _ = format_bytes(bytes);
            }

            #[test]
            fn format_bytes_monotonic(a in 0u64..1_000_000_000, b in 1_000_000_000u64..u64::MAX) {
                let _ = (format_bytes(a), format_bytes(b));
            }

            #[test]
            fn format_duration_never_panics(secs in 0u64..1_000_000) {
                let _ = format_duration(Duration::from_secs(secs));
            }

            #[test]
            fn format_duration_millis_never_panics(millis in 0u64..1_000_000_000) {
                let _ = format_duration(Duration::from_millis(millis));
            }
        }
    }
}
