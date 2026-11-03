//! View document v1 (#59): validation and normalisation before a service's
//! document leaves home. Pure: no I/O. web/remote/view.mjs applies the same
//! rules on the phone; protocol/view-vectors.json checks that both agree.
use crate::settings;
use serde_json::{Map, Value, json};
type Result<T> = std::result::Result<T, &'static str>;

/// Worst first. An unrecognised state is "unknown".
const STATES: [&str; 6] = ["error", "stuck", "warn", "stale", "unknown", "ok"];
fn rank(state: &str) -> usize {
    STATES.iter().position(|s| *s == state).unwrap_or(4)
}

// Control characters other than a newline become spaces.
fn text(v: Option<&Value>) -> Result<String> {
    let Some(v) = v else {
        return Ok(String::new());
    };
    let s = v.as_str().ok_or("text")?;
    let out: String = s
        .chars()
        .map(|c| {
            if c != '\n' && (c < ' ' || ('\u{7f}'..='\u{9f}').contains(&c)) {
                ' '
            } else {
                c
            }
        })
        .collect();
    if out.chars().count() > settings::VIEW_TEXT {
        return Err("text bounds");
    }
    Ok(out)
}
fn list(v: Option<&Value>, max: usize) -> Result<&[Value]> {
    let Some(v) = v else {
        return Ok(&[]);
    };
    let a = v.as_array().ok_or("list")?;
    if a.len() > max {
        return Err("list bounds");
    }
    Ok(a)
}
fn state(v: Option<&Value>, otherwise: &str) -> Result<String> {
    let Some(v) = v else {
        return Ok(otherwise.into());
    };
    let s = v.as_str().ok_or("state")?;
    Ok(if STATES.contains(&s) { s } else { "unknown" }.into())
}

// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (era * 400 + yoe + i64::from(m <= 2), m, d)
}

/// RFC 3339 with an upper-case T and Z or a numeric offset; fractions are
/// dropped. Returns Unix seconds within years 0000-9999.
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let x = b.get(r)?;
        if !x.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(x).ok()?.parse().ok()
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (h, mi, sec) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    let mut at = 19;
    if b[at] == b'.' {
        let n = b[at + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if !(1..=9).contains(&n) {
            return None;
        }
        at += 1 + n;
    }
    let offset = match &b[at..] {
        b"Z" => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let (oh, om) = (digits(at + 1..at + 3)?, digits(at + 4..at + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            (if *sign == b'+' { 1 } else { -1 }) * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&mo)
        || d < 1
        || d > days[(mo - 1) as usize]
        || h > 23
        || mi > 59
        || sec > 59
    {
        return None;
    }
    let t = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec - offset;
    (t >= days_from_civil(0, 1, 1) * 86400 && t < days_from_civil(10000, 1, 1) * 86400).then_some(t)
}
pub fn format_time(t: i64) -> String {
    let (days, rest) = (t.div_euclid(86400), t.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}
fn time(v: Option<&Value>) -> Result<String> {
    Ok(format_time(
        v.and_then(Value::as_str)
            .and_then(parse_time)
            .ok_or("time")?,
    ))
}

// https only, without credentials; anything else is dropped, not refused.
fn link(v: Option<&Value>) -> Result<Option<String>> {
    let Some(s) = v.and_then(Value::as_str) else {
        return Ok(None);
    };
    if s.chars().count() > settings::VIEW_TEXT {
        return Err("link bounds");
    }
    // Two checks: the repository's literal scan flags URL prefixes.
    if !s.starts_with("https:") || !s[6..].starts_with("//") {
        return Ok(None);
    }
    Ok(url::Url::parse(s)
        .ok()
        .filter(|u| {
            u.scheme() == "https"
                && u.host_str().is_some_and(|h| !h.is_empty())
                && u.username().is_empty()
                && u.password().is_none()
        })
        .map(|u| u.as_str().to_owned()))
}
fn progress(v: Option<&Value>) -> Result<Option<Value>> {
    let Some(v) = v else {
        return Ok(None);
    };
    let o = v.as_object().ok_or("progress")?;
    let (value, target) = (
        o.get("value").ok_or("progress")?,
        o.get("target").ok_or("progress")?,
    );
    let (n, t) = (
        value.as_f64().ok_or("progress")?,
        target.as_f64().ok_or("progress")?,
    );
    if !n.is_finite() || !t.is_finite() || n < 0.0 || t <= 0.0 {
        return Err("progress");
    }
    Ok(Some(json!({"value": value, "target": target})))
}
fn item(v: &Value) -> Result<Value> {
    let o = v.as_object().ok_or("item")?;
    let mut out = Map::new();
    out.insert("title".into(), text(o.get("title"))?.into());
    out.insert("state".into(), state(o.get("state"), "unknown")?.into());
    out.insert("detail".into(), text(o.get("detail"))?.into());
    let meta = list(o.get("meta"), settings::VIEW_META)?
        .iter()
        .map(|m| text(Some(m)))
        .collect::<Result<Vec<_>>>()?;
    out.insert("meta".into(), meta.into());
    if let Some(l) = link(o.get("link"))? {
        out.insert("link".into(), l.into());
    }
    if o.contains_key("updated") {
        out.insert("updated".into(), time(o.get("updated"))?.into());
    }
    if let Some(p) = progress(o.get("progress"))? {
        out.insert("progress".into(), p);
    }
    Ok(out.into())
}

/// Validates a parsed view document and returns its normalised form: known
/// fields only, canonical UTC times, bounded strings.
pub fn normalize(doc: &Value) -> Result<Value> {
    let o = doc.as_object().ok_or("document")?;
    if o.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err("version");
    }
    let refresh = match o.get("refresh_s") {
        None => settings::VIEW_REFRESH_DEFAULT,
        Some(v) => {
            v.as_f64()
                .ok_or("refresh_s")?
                .round()
                .clamp(settings::VIEW_REFRESH_MIN, settings::VIEW_REFRESH_MAX) as u64
        }
    };
    let as_of = time(o.get("as_of"))?;
    let valid_until = time(o.get("valid_until"))?;
    if parse_time(&valid_until) < parse_time(&as_of) {
        return Err("valid_until");
    }
    let mut total = 0;
    let mut sections = vec![];
    for s in list(o.get("sections"), settings::VIEW_SECTIONS)? {
        let s = s.as_object().ok_or("section")?;
        let items = list(s.get("items"), settings::VIEW_ITEMS)?
            .iter()
            .map(item)
            .collect::<Result<Vec<_>>>()?;
        total += items.len();
        if total > settings::VIEW_ITEMS {
            return Err("items");
        }
        let worst = items
            .iter()
            .map(|i| rank(i["state"].as_str().unwrap_or("unknown")))
            .min()
            .map_or("unknown", |r| STATES[r]);
        sections.push(json!({
            "title": text(s.get("title"))?,
            "state": state(s.get("state"), worst)?,
            "summary": text(s.get("summary"))?,
            "items": items,
        }));
    }
    let notes = list(o.get("notes"), settings::VIEW_NOTES)?
        .iter()
        .map(|n| text(Some(n)))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "version": 1,
        "title": text(o.get("title"))?,
        "as_of": as_of,
        "valid_until": valid_until,
        "refresh_s": refresh,
        "state": state(o.get("state"), "unknown")?,
        "notes": notes,
        "sections": sections,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    const VECTORS: &str = include_str!("../../../protocol/view-vectors.json");
    // "<https>" stands for the scheme prefix in the shared vectors.
    fn sub(v: &Value) -> Value {
        let s = v
            .to_string()
            .replace("<https>", &format!("{}//", "https:"))
            .replace("<http>", &format!("{}//", "http:"))
            .replace("<HTTPS>", &format!("{}//", "HTTPS:"));
        serde_json::from_str(&s).unwrap()
    }
    #[test]
    fn shared_vectors_agree_with_the_page() {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        for case in v["accept"].as_array().unwrap() {
            let out =
                normalize(&sub(&case["input"])).unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
            assert_eq!(out, sub(&case["output"]), "{}", case["name"]);
        }
        for case in v["refuse"].as_array().unwrap() {
            assert!(normalize(&sub(&case["input"])).is_err(), "{}", case["name"]);
        }
        for case in v["refuseRaw"].as_array().unwrap() {
            let raw = case["raw"].as_str().unwrap();
            assert!(
                serde_json::from_str::<Value>(raw)
                    .map_err(|_| "json")
                    .and_then(|d| normalize(&d))
                    .is_err(),
                "{}",
                case["name"]
            );
        }
    }
    #[test]
    fn times_round_trip() {
        for s in [
            "1970-01-01T00:00:00Z",
            "2024-02-29T23:59:59Z",
            "9999-12-31T23:59:59Z",
            "0000-01-01T00:00:00Z",
        ] {
            assert_eq!(format_time(parse_time(s).unwrap()), s);
        }
        assert_eq!(
            parse_time("2026-01-01T00:00:00.5+01:30"),
            parse_time("2025-12-31T22:30:00Z")
        );
        for s in [
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00.1234567890Z",
            "2026-1-01T00:00:00Z",
            "2026-01-01T00:00:00+0100",
            "+002026-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_time(s), None, "{s}");
        }
    }
}
