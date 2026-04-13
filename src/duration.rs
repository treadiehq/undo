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

    #[test]
    fn parse_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
    }

    #[test]
    fn parse_minutes() {
        assert_eq!(parse_duration("5m").unwrap(), 300);
    }

    #[test]
    fn parse_hours() {
        assert_eq!(parse_duration("2h").unwrap(), 7200);
    }

    #[test]
    fn parse_days() {
        assert_eq!(parse_duration("1d").unwrap(), 86400);
    }

    #[test]
    fn parse_unknown_unit_is_error() {
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn parse_zero_is_rejected() {
        assert!(parse_duration("0m").is_err());
    }
}
