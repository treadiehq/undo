use anyhow::{bail, Result};

/// Parse a human-friendly duration string (e.g. "5m", "2h", "1d") into seconds.
pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    if s.len() < 2 {
        bail!("invalid duration: '{}' — use format like 5m, 2h, 1d", s);
    }

    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: i64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration number: '{}'", num_str))?;

    if num <= 0 {
        bail!("duration must be positive");
    }

    match unit {
        "s" => Ok(num),
        "m" => Ok(num * 60),
        "h" => Ok(num * 3600),
        "d" => Ok(num * 86400),
        _ => bail!("unknown duration unit '{}' — use s, m, h, or d", unit),
    }
}

/// Format an elapsed duration in seconds into a human-readable string.
pub fn format_elapsed(seconds: i64) -> String {
    if seconds < 60 {
        format!("{} second(s) ago", seconds)
    } else if seconds < 3600 {
        format!("{} minute(s) ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{} hour(s) ago", seconds / 3600)
    } else {
        format!("{} day(s) ago", seconds / 86400)
    }
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

    /// Durations under 60 seconds are shown as seconds.
    #[test]
    fn format_elapsed_under_a_minute() {
        assert_eq!(format_elapsed(45), "45 second(s) ago");
    }

    /// Durations between 60 s and 3600 s are shown as minutes.
    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed(120), "2 minute(s) ago");
    }

    /// Durations between 3600 s and 86400 s are shown as hours.
    #[test]
    fn format_elapsed_hours() {
        assert_eq!(format_elapsed(7200), "2 hour(s) ago");
    }

    /// Durations of 86400 s or more are shown as days.
    #[test]
    fn format_elapsed_days() {
        assert_eq!(format_elapsed(172_800), "2 day(s) ago");
    }
}
