//! Human-readable formatting helpers.

const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

pub fn bytes(n: u64) -> String {
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn rate(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        "-".to_string()
    } else {
        format!("{}/s", bytes(bytes_per_sec))
    }
}

/// Like [`rate`] but shows `0 B/s` instead of a dash (for totals).
pub fn rate_or_zero(bytes_per_sec: u64) -> String {
    format!("{}/s", bytes(bytes_per_sec))
}

pub fn percent(done: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

/// Text progress bar of `width` cells, e.g. `████░░░░`.
pub fn bar(pct: f64, width: usize) -> String {
    let filled = ((pct / 100.0) * width as f64).floor() as usize;
    let filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// `45s`, `3m 05s`, `2h 10m`, `4d 3h`.
pub fn duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        3600..=86_399 => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d {}h", secs / 86_400, secs % 86_400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn formats_rate() {
        assert_eq!(rate(0), "-");
        assert_eq!(rate(2048), "2.0 KiB/s");
    }

    #[test]
    fn percent_is_safe() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(50, 100), 50.0);
        assert_eq!(percent(200, 100), 100.0);
    }

    #[test]
    fn bar_width_is_constant() {
        for pct in [0.0, 0.1, 50.0, 99.9, 100.0] {
            assert_eq!(bar(pct, 10).chars().count(), 10);
        }
        assert_eq!(bar(100.0, 4), "████");
        assert_eq!(bar(0.0, 4), "░░░░");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(185), "3m 05s");
        assert_eq!(duration(7800), "2h 10m");
        assert_eq!(duration(90_000), "1d 1h");
    }
}
