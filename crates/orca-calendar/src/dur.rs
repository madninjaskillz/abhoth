//! Durations as people write them in `calendar.toml`: `90m`, `1h30m`, `15m`, `2d`, `45s`, or a
//! bare number of seconds.

/// Seconds in `s`, or an error naming the text.
pub fn parse(s: &str) -> Result<i64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(n) = t.parse::<i64>() {
        return if n >= 0 {
            Ok(n)
        } else {
            Err(format!("negative duration '{s}'"))
        };
    }
    let mut total: i64 = 0;
    let mut num = String::new();
    let mut any = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        if c == ' ' {
            continue;
        }
        let unit = match c {
            'd' => 86_400,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => return Err(format!("bad duration '{s}' (use e.g. 90m, 1h30m, 2d)")),
        };
        let n: i64 = num
            .parse()
            .map_err(|_| format!("bad duration '{s}' (a number must come before '{c}')"))?;
        total = total
            .checked_add(n.checked_mul(unit).ok_or("duration too long")?)
            .ok_or("duration too long")?;
        num.clear();
        any = true;
    }
    if !num.is_empty() || !any {
        return Err(format!("bad duration '{s}' (a unit is missing: d, h, m or s)"));
    }
    Ok(total)
}

/// `5400` -> `1h30m`, `600` -> `10m`, `0` -> `0m`.
pub fn format(secs: i64) -> String {
    if secs <= 0 {
        return "0m".into();
    }
    let (d, rest) = (secs / 86_400, secs % 86_400);
    let (h, rest) = (rest / 3_600, rest % 3_600);
    let (m, s) = (rest / 60, rest % 60);
    let mut out = String::new();
    for (n, u) in [(d, 'd'), (h, 'h'), (m, 'm'), (s, 's')] {
        if n > 0 {
            out.push_str(&format!("{n}{u}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_and_format() {
        assert_eq!(parse("90m"), Ok(5400));
        assert_eq!(parse("1h30m"), Ok(5400));
        assert_eq!(parse("2d"), Ok(172_800));
        assert_eq!(parse("45s"), Ok(45));
        assert_eq!(parse("600"), Ok(600));
        assert!(parse("90").is_ok());
        assert!(parse("ninety").is_err());
        assert!(parse("90x").is_err());
        assert!(parse("1h30").is_err());
        assert!(parse("").is_err());
        assert_eq!(format(5400), "1h30m");
        assert_eq!(format(600), "10m");
        assert_eq!(format(90_061), "1d1h1m1s");
    }
}
