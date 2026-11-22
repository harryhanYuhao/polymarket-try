//! Human-readable table rendering helpers.

use chrono::{DateTime, SecondsFormat, Utc};
use comfy_table::{presets::UTF8_FULL, Cell, ContentArrangement, Table};

pub fn table(headers: &[&str], rows: Vec<Vec<String>>) -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(headers.iter().map(Cell::new));
    for row in rows {
        table.add_row(row);
    }
    table
}

pub fn short(value: &str, max: usize) -> String {
    let count = value.chars().count();
    if count <= max {
        return value.to_owned();
    }
    let kept: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

pub fn format_unix_seconds(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| timestamp.to_string())
}

pub fn format_unix_millis(timestamp: u64) -> String {
    let seconds = (timestamp / 1_000) as i64;
    let nanos = ((timestamp % 1_000) * 1_000_000) as u32;
    DateTime::<Utc>::from_timestamp(seconds, nanos)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| timestamp.to_string())
}

pub fn option(value: Option<&str>) -> String {
    value.unwrap_or("-").to_owned()
}
