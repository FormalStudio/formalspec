use anyhow::{Result, bail};

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `3d`, `12h`, `30m`, `2w` as seconds.
pub fn parse_age(s: &str) -> Result<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let Ok(n) = n.parse::<i64>() else { bail!("release_age `{s}` is not <number><unit>, e.g. 3d or 12h") };
    let unit = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => bail!("release_age `{s}` needs a unit of m, h, d or w"),
    };
    Ok(n * unit)
}

/// RFC 3339 to unix seconds. Fractions are dropped; offsets are honoured.
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest.as_bytes().first() {
        None | Some(b'Z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let (h, m) = rest[1..].split_once(':')?;
            let secs = h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss - offset)
}

/// RFC 2822, as RSS writes `pubDate` (`Tue, 10 Nov 2026 16:09:07 +0000`), to unix seconds.
/// Zones are numeric offsets or `GMT`, `UT`, `UTC` and `Z`; seconds may be left out.
pub fn parse_rfc2822(s: &str) -> Option<i64> {
    let s = s.split_once(',').map_or(s, |(_, rest)| rest);
    let mut words = s.split_whitespace();
    let d: i64 = words.next()?.parse().ok()?;
    let month = words.next()?;
    let m = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|n| n.eq_ignore_ascii_case(month))? as i64
        + 1;
    let y: i64 = words.next()?.parse().ok()?;
    let mut hms = words.next()?.split(':').map(|n| n.parse::<i64>().ok());
    let (hh, mm, ss) = (hms.next()??, hms.next()??, hms.next().unwrap_or(Some(0))?);
    let offset = match words.next().unwrap_or("Z") {
        "Z" | "GMT" | "UT" | "UTC" => 0,
        zone if zone.len() == 5 && matches!(zone.as_bytes()[0], b'+' | b'-') => {
            let n = |r: std::ops::Range<usize>| zone.get(r)?.parse::<i64>().ok();
            let secs = n(1..3)? * 3600 + n(3..5)? * 60;
            if zone.starts_with('+') { secs } else { -secs }
        }
        _ => return None,
    };
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss - offset)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_and_times() {
        assert_eq!(parse_age("3d").unwrap(), 259_200);
        assert_eq!(parse_age("12h").unwrap(), 43_200);
        assert!(parse_age("3").is_err());
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_time("2024-11-20T12:34:56.789Z"), parse_time("2024-11-20T13:34:56+01:00"));
        assert_eq!(parse_time("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(parse_rfc2822("Thu, 29 Feb 2024 01:00:00 +0100"), Some(1_709_164_800));
        assert_eq!(parse_rfc2822("29 Feb 2024 00:00 GMT"), Some(1_709_164_800));
        assert_eq!(parse_rfc2822("Thu, 29 Feb 2024 00:00:00 EST"), None);
    }
}
