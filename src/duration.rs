use anyhow::{Result, bail};

/// Parse a human-friendly duration string (e.g. "5m", "2h", "1d") into seconds.
pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();

    // Split off the trailing *character*, not the trailing byte. Inputs like
    // "5日" are 4 bytes long but `split_at(len - 1)` would land mid-codepoint
    // and panic with "byte index is not a char boundary" — turning a typo
    // into a runtime crash.
    let unit = s
        .chars()
        .next_back()
        .ok_or_else(|| anyhow::anyhow!("invalid duration: '' — use format like 5m, 2h, 1d"))?;
    let num_str = &s[..s.len() - unit.len_utf8()];
    if num_str.is_empty() {
        bail!("invalid duration: '{}' — use format like 5m, 2h, 1d", s);
    }

    let num: i64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration number: '{}'", num_str))?;

    if num <= 0 {
        bail!("duration must be positive");
    }

    // Use checked multiplication: a large value like "999999999999999d"
    // parses cleanly as an i64 but `num * 86400` overflows. In debug builds
    // that panics (crashing the CLI / daemon auto-prune); in release builds it
    // wraps silently to a NEGATIVE value, which in `prune` becomes a cutoff
    // FAR in the future and deletes the entire history. Reject the overflow.
    let factor: i64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => bail!("unknown duration unit '{}' — use s, m, h, or d", unit),
    };
    num.checked_mul(factor)
        .ok_or_else(|| anyhow::anyhow!("duration '{}' is too large", s))
}

/// Format an elapsed duration in seconds into a human-readable string.
pub fn format_elapsed(seconds: i64) -> String {
    if seconds < 0 {
        return "in the future".to_string();
    }

    let (value, unit) = if seconds < 60 {
        (seconds, "second")
    } else if seconds < 3600 {
        (seconds / 60, "minute")
    } else if seconds < 86400 {
        (seconds / 3600, "hour")
    } else {
        (seconds / 86400, "day")
    };
    let suffix = if value == 1 { "" } else { "s" };
    format!("{} {}{} ago", value, unit, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 's' unit converts directly to raw seconds.
    #[test]
    fn parse_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
    }

    /// The 'm' unit multiplies the number by 60.
    #[test]
    fn parse_minutes() {
        assert_eq!(parse_duration("5m").unwrap(), 300);
    }

    /// The 'h' unit multiplies the number by 3600.
    #[test]
    fn parse_hours() {
        assert_eq!(parse_duration("2h").unwrap(), 7200);
    }

    /// The 'd' unit multiplies the number by 86400.
    #[test]
    fn parse_days() {
        assert_eq!(parse_duration("1d").unwrap(), 86400);
    }

    /// An unrecognised unit suffix must return an error.
    #[test]
    fn parse_unknown_unit_is_error() {
        assert!(parse_duration("5x").is_err());
    }

    /// Zero and negative durations are invalid and must be rejected.
    #[test]
    fn parse_zero_is_rejected() {
        assert!(parse_duration("0m").is_err());
    }

    /// A multi-byte trailing character must produce a clean error rather than
    /// panic. `s.split_at(s.len() - 1)` lands mid-codepoint for inputs like
    /// "5日" (4 bytes, last char starts at byte 1) and used to crash the CLI.
    #[test]
    fn parse_multibyte_unit_does_not_panic() {
        let result = parse_duration("5日");
        assert!(
            result.is_err(),
            "non-ASCII unit must produce an error, not a panic"
        );
    }

    /// A bare multi-byte character (no number) must error cleanly — same
    /// char-boundary trap as above, with no digits to anchor to.
    #[test]
    fn parse_lone_multibyte_does_not_panic() {
        let result = parse_duration("日");
        assert!(result.is_err(), "lone non-ASCII input must error cleanly");
    }

    /// An empty string must produce a clean error rather than panic.
    #[test]
    fn parse_empty_string_does_not_panic() {
        assert!(parse_duration("").is_err());
    }

    /// A value whose `number * unit_factor` overflows i64 must be rejected
    /// cleanly. "999999999999999d" parses as a valid i64 but `* 86400`
    /// overflows: in debug it would panic, in release it wraps to a negative
    /// value that makes `prune` compute a future cutoff and wipe all history.
    /// (Red before the `checked_mul` fix: the multiply panicked under the
    /// debug overflow checks that tests run with.)
    #[test]
    fn parse_overflowing_duration_is_rejected() {
        let result = parse_duration("999999999999999d");
        assert!(
            result.is_err(),
            "an overflowing duration must error, not panic or wrap; got {:?}",
            result
        );
        assert!(
            result.unwrap_err().to_string().contains("too large"),
            "error should explain the value is too large"
        );
    }

    /// The largest non-overflowing second-count is accepted unchanged, proving
    /// the overflow guard does not reject ordinary in-range values.
    #[test]
    fn parse_max_in_range_seconds_is_accepted() {
        let s = format!("{}s", i64::MAX);
        assert_eq!(parse_duration(&s).unwrap(), i64::MAX);
    }

    /// Durations under 60 seconds are shown as seconds.
    #[test]
    fn format_elapsed_under_a_minute() {
        assert_eq!(format_elapsed(45), "45 seconds ago");
        assert_eq!(format_elapsed(1), "1 second ago");
    }

    /// Durations between 60 s and 3600 s are shown as minutes.
    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed(120), "2 minutes ago");
        assert_eq!(format_elapsed(60), "1 minute ago");
    }

    /// Durations between 3600 s and 86400 s are shown as hours.
    #[test]
    fn format_elapsed_hours() {
        assert_eq!(format_elapsed(7200), "2 hours ago");
        assert_eq!(format_elapsed(3600), "1 hour ago");
    }

    /// Durations of 86400 s or more are shown as days.
    #[test]
    fn format_elapsed_days() {
        assert_eq!(format_elapsed(172_800), "2 days ago");
        assert_eq!(format_elapsed(86_400), "1 day ago");
    }

    /// Clock skew or future-dated records are described without a negative age.
    #[test]
    fn format_elapsed_future_timestamp() {
        assert_eq!(format_elapsed(-1), "in the future");
    }
}
