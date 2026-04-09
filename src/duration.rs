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
