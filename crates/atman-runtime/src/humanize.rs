pub fn format_count(n: u64) -> String {
    if n < 10_000 {
        return n.to_string();
    }
    let (v, unit) = if n >= 1_000_000_000 {
        (n as f64 / 1_000_000_000.0, "G")
    } else if n >= 1_000_000 {
        (n as f64 / 1_000_000.0, "M")
    } else {
        (n as f64 / 1_000.0, "K")
    };
    if v >= 100.0 {
        format!("{:.0}{unit}", v)
    } else if v >= 10.0 {
        format!("{:.1}{unit}", v)
    } else {
        format!("{:.2}{unit}", v)
    }
}

pub fn format_secs(secs: i64) -> String {
    if secs < 0 {
        return "0s".into();
    }
    let s = secs as u64;
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3_600 {
        return format!("{}m{}s", s / 60, s % 60);
    }
    if s < 86_400 {
        return format!("{}h{}m", s / 3_600, (s % 3_600) / 60);
    }
    format!("{}d{}h", s / 86_400, (s % 86_400) / 3_600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_counts_stay_raw() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(9_999), "9999");
    }

    #[test]
    fn thousands_use_k() {
        assert_eq!(format_count(10_000), "10.0K");
        assert_eq!(format_count(12_345), "12.3K");
        assert_eq!(format_count(999_000), "999K");
    }

    #[test]
    fn millions_use_m() {
        assert_eq!(format_count(1_000_000), "1.00M");
        assert_eq!(format_count(224_694), "225K");
        assert_eq!(format_count(1_500_000), "1.50M");
    }

    #[test]
    fn billions_use_g() {
        assert_eq!(format_count(1_000_000_000), "1.00G");
        assert_eq!(format_count(12_345_678_901), "12.3G");
    }

    #[test]
    fn seconds_under_minute_stay_raw() {
        assert_eq!(format_secs(0), "0s");
        assert_eq!(format_secs(45), "45s");
    }

    #[test]
    fn seconds_convert_to_minutes() {
        assert_eq!(format_secs(60), "1m0s");
        assert_eq!(format_secs(125), "2m5s");
        assert_eq!(format_secs(2_220), "37m0s");
    }

    #[test]
    fn seconds_convert_to_hours() {
        assert_eq!(format_secs(3_600), "1h0m");
        assert_eq!(format_secs(7_500), "2h5m");
    }

    #[test]
    fn seconds_convert_to_days() {
        assert_eq!(format_secs(86_400), "1d0h");
        assert_eq!(format_secs(90_061), "1d1h");
    }

    #[test]
    fn negative_seconds_clamp_to_zero() {
        assert_eq!(format_secs(-42), "0s");
    }
}
