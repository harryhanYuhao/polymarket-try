//! Table and value formatting for terminal output.

use anyhow::Result;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{CellAlignment, ContentArrangement, Table};
use serde::Serialize;

pub fn table(headers: &[&str]) -> Table {
    let mut t = Table::new();
    t.load_preset(UTF8_FULL_CONDENSED)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(headers.to_vec());
    t
}

pub fn align_right(t: &mut Table, columns: &[usize]) {
    for &c in columns {
        if let Some(col) = t.column_mut(c) {
            col.set_cell_alignment(CellAlignment::Right);
        }
    }
}

pub fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn heading(title: &str) {
    println!("\n{title}");
}

/// `0xabcdef…1234` for long hex identifiers.
pub fn short_id(id: &str) -> String {
    if id.len() > 14 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

pub fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Number with at most `max_decimals` decimals and no trailing zeros.
pub fn num(value: f64, max_decimals: usize) -> String {
    let s = format!("{value:.max_decimals$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    };
    if s == "-0" {
        "0".to_string()
    } else {
        s
    }
}

/// Trim trailing zeros from an API decimal string (`"137.871260000"` →
/// `"137.87126"`); anything that is not a plain decimal is returned as-is.
pub fn dec(text: &str) -> String {
    let t = text.trim();
    let digits = t.strip_prefix('-').unwrap_or(t);
    let plain = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.matches('.').count() <= 1;
    if plain && digits.contains('.') {
        t.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        t.to_string()
    }
}

pub fn signed(value: f64, decimals: usize) -> String {
    format!("{value:+.decimals$}")
}

/// `YYYY-MM-DD HH:MM` (UTC) from Unix seconds.
pub fn unix_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Like [`unix_secs`] but accepts seconds or milliseconds as text.
pub fn timestamp_text(raw: &str) -> String {
    match raw.trim().parse::<u64>() {
        Ok(0) | Err(_) => raw.to_string(),
        Ok(v) if v > 100_000_000_000 => unix_secs(v / 1000),
        Ok(v) => unix_secs(v),
    }
}

/// Days since 1970-01-01 to (year, month, day) — Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates() {
        assert_eq!(unix_secs(0), "1970-01-01 00:00");
        assert_eq!(unix_secs(1_795_100_954), "2026-11-19 15:09");
        assert_eq!(unix_secs(951_782_400), "2000-02-29 00:00");
        assert_eq!(timestamp_text("1767225600000"), "2026-01-01 00:00");
        assert_eq!(timestamp_text("0"), "0");
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(num(2_030_210.0, 2), "2030210");
        assert_eq!(num(0.7335, 4), "0.7335");
        assert_eq!(num(-0.0001, 2), "0");
        assert_eq!(signed(474_054.5, 2), "+474054.50");
        assert_eq!(signed(-2.25, 2), "-2.25");
        assert_eq!(
            short_id("0x74dba1ce1ae9dd535414e85f2d9ab5ea"),
            "0x74dba1…b5ea"
        );
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(dec("137.87126000000000000000000000"), "137.87126");
        assert_eq!(dec("-0.500"), "-0.5");
        assert_eq!(dec("65000.0"), "65000");
        assert_eq!(dec("100"), "100");
        assert_eq!(dec("n/a"), "n/a");
    }
}
