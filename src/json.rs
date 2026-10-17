//! Shared JSON serialization.
//!
//! Used by the CLI (`json` mode and the `.pretty` node/edge printer) and by
//! the C ABI (`grafile_query_json`, `grafile_meta_json`). Consumers that do
//! not want ANSI codes pass [`JsonOptions::plain`].

use crate::api::Rows;
use crate::engine::Database;
use crate::error::Result;
use crate::record::Value;

/// ANSI color codes for JSON token classes.
#[derive(Debug, Clone, Copy)]
pub struct JsonColors {
    /// Object keys.
    pub key: &'static str,
    /// String values.
    pub string: &'static str,
    /// Numeric values.
    pub number: &'static str,
    /// `null` / `true` / `false`.
    pub literal: &'static str,
    /// Reset sequence.
    pub reset: &'static str,
}

/// JSON rendering options.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonOptions {
    /// Indent nested structures across multiple lines.
    pub pretty: bool,
    /// Optional ANSI color scheme.
    pub colors: Option<JsonColors>,
}

impl JsonOptions {
    /// Plain, uncolored output.
    pub fn plain(pretty: bool) -> Self {
        Self {
            pretty,
            colors: None,
        }
    }

    fn wrap(&self, code: Option<&'static str>, text: &str) -> String {
        match (self.colors, code) {
            (Some(colors), Some(code)) => format!("{code}{text}{}", colors.reset),
            _ => text.to_owned(),
        }
    }

    fn key(&self, text: &str) -> String {
        self.wrap(self.colors.map(|colors| colors.key), text)
    }

    fn string(&self, text: &str) -> String {
        self.wrap(self.colors.map(|colors| colors.string), text)
    }

    fn number(&self, text: &str) -> String {
        self.wrap(self.colors.map(|colors| colors.number), text)
    }

    fn literal(&self, text: &str) -> String {
        self.wrap(self.colors.map(|colors| colors.literal), text)
    }
}

/// Serialize a value as JSON, expanding node and edge references into
/// objects with their attributes.
pub fn value_json(
    db: &mut Database,
    value: &Value,
    options: JsonOptions,
    indent: usize,
) -> Result<String> {
    Ok(match value {
        Value::Null => options.literal("null"),
        Value::Bool(value) => options.literal(if *value { "true" } else { "false" }),
        Value::Int(value) => options.number(&value.to_string()),
        Value::Float(value) => {
            if value.is_finite() {
                let mut buffer = ryu::Buffer::new();
                options.number(buffer.format(*value))
            } else {
                // JSON has no NaN/Infinity.
                options.literal("null")
            }
        }
        Value::Text(text) => options.string(&escape_json(text)),
        Value::Bytes(bytes) => options.string(&escape_json(&format!("0x{}", hex(bytes)))),
        Value::List(items) => {
            if items.is_empty() {
                "[]".to_owned()
            } else {
                let mut out = String::from("[");
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    if options.pretty {
                        out.push('\n');
                        out.push_str(&indent_str(indent + 1));
                    }
                    out.push_str(&value_json(db, item, options, indent + 1)?);
                }
                if options.pretty {
                    out.push('\n');
                    out.push_str(&indent_str(indent));
                }
                out.push(']');
                out
            }
        }
        Value::Map(entries) => {
            let fields: Vec<(String, Value)> = entries
                .iter()
                .map(|(key, item)| (key.to_string(), item.clone()))
                .collect();
            object_json(db, &fields, options, indent)?
        }
        Value::Node(id) => {
            let fields = node_fields(db, *id)?;
            object_json(db, &fields, options, indent)?
        }
        Value::Edge(id) => {
            let fields = edge_fields(db, *id)?;
            object_json(db, &fields, options, indent)?
        }
        // Temporal values render as their ISO 8601 text; points as a small
        // object so the coordinates stay machine readable.
        Value::Date(value) => options.string(&escape_json(&value.to_string())),
        Value::LocalTime(value) => options.string(&escape_json(&value.to_string())),
        Value::Time(value) => options.string(&escape_json(&value.to_string())),
        Value::LocalDateTime(value) => options.string(&escape_json(&value.to_string())),
        Value::DateTime(value) => options.string(&escape_json(&value.to_string())),
        Value::Duration(value) => options.string(&escape_json(&value.to_string())),
        Value::Point(value) => {
            let mut fields = vec![
                ("srid".to_owned(), Value::Int(value.srid() as i64)),
                ("x".to_owned(), Value::Float(value.x())),
                ("y".to_owned(), Value::Float(value.y())),
            ];
            if let Some(z) = value.z() {
                fields.push(("z".to_owned(), Value::Float(z)));
            }
            object_json(db, &fields, options, indent)?
        }
    })
}

/// Serialize one row as a JSON object keyed by column name.
pub fn row_object_json(
    db: &mut Database,
    columns: &[String],
    row: &[Value],
    options: JsonOptions,
    indent: usize,
) -> Result<String> {
    let fields: Vec<(String, Value)> = columns.iter().cloned().zip(row.iter().cloned()).collect();
    object_json(db, &fields, options, indent)
}

/// Serialize all rows as a single JSON array of objects.
pub fn rows_json_array(db: &mut Database, rows: &Rows, options: JsonOptions) -> Result<String> {
    if rows.is_empty() {
        return Ok("[]".to_owned());
    }
    let mut out = String::from("[");
    for (index, row) in rows.rows().iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        if options.pretty {
            out.push('\n');
            out.push_str(&indent_str(1));
        }
        out.push_str(&row_object_json(db, rows.columns(), row, options, 1)?);
    }
    if options.pretty {
        out.push('\n');
    }
    out.push(']');
    Ok(out)
}

/// Serialize `(key, value)` pairs as a JSON object.
pub fn object_json(
    db: &mut Database,
    fields: &[(String, Value)],
    options: JsonOptions,
    indent: usize,
) -> Result<String> {
    if fields.is_empty() {
        return Ok("{}".to_owned());
    }
    let mut out = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        if options.pretty {
            out.push('\n');
            out.push_str(&indent_str(indent + 1));
        }
        out.push_str(&options.key(&escape_json(key)));
        out.push(':');
        if options.pretty {
            out.push(' ');
        }
        out.push_str(&value_json(db, value, options, indent + 1)?);
    }
    if options.pretty {
        out.push('\n');
        out.push_str(&indent_str(indent));
    }
    out.push('}');
    Ok(out)
}

fn node_fields(db: &mut Database, id: u64) -> Result<Vec<(String, Value)>> {
    let record = db.get_node(id)?;
    let mut labels = Vec::new();
    let mut properties: Vec<(std::sync::Arc<str>, Value)> = Vec::new();
    if let Some(record) = &record {
        for label in &record.labels {
            if let Some(name) = db.schema().label_name(*label) {
                labels.push(Value::from(name));
            }
        }
        for (key, value) in &record.props {
            if let Some(name) = db.schema().property_key_name(*key) {
                properties.push((std::sync::Arc::from(name), value.clone()));
            }
        }
    }
    Ok(vec![
        ("id".to_owned(), Value::Int(id as i64)),
        ("labels".to_owned(), Value::List(labels.into())),
        ("properties".to_owned(), Value::Map(properties.into())),
    ])
}

fn edge_fields(db: &mut Database, id: u64) -> Result<Vec<(String, Value)>> {
    let record = db.get_edge(id)?;
    let mut edge_type = Value::Null;
    let mut start = Value::Null;
    let mut end = Value::Null;
    let mut properties: Vec<(std::sync::Arc<str>, Value)> = Vec::new();
    if let Some(record) = &record {
        edge_type = db
            .schema()
            .edge_type_name(record.edge_type)
            .map(Value::from)
            .unwrap_or(Value::Null);
        start = Value::Int(record.src as i64);
        end = Value::Int(record.dst as i64);
        for (key, value) in &record.props {
            if let Some(name) = db.schema().property_key_name(*key) {
                properties.push((std::sync::Arc::from(name), value.clone()));
            }
        }
    }
    Ok(vec![
        ("id".to_owned(), Value::Int(id as i64)),
        ("type".to_owned(), edge_type),
        ("start".to_owned(), start),
        ("end".to_owned(), end),
        ("properties".to_owned(), Value::Map(properties.into())),
    ])
}

/// Serialize the database header as JSON.
///
/// With `raw` the exact stored values are emitted (epoch seconds, hex UUID);
/// otherwise a human-friendly interpretation is produced.
pub fn header_json(db: &mut Database, raw: bool) -> Result<String> {
    let header = db.header();
    let mut fields: Vec<(String, Value)> = Vec::new();
    if raw {
        fields.push(("magic".to_owned(), Value::from("GRAFILE\\0")));
        fields.push((
            "format_major".to_owned(),
            Value::Int(header.format_major.into()),
        ));
        fields.push((
            "format_minor".to_owned(),
            Value::Int(header.format_minor.into()),
        ));
        fields.push(("page_size".to_owned(), Value::Int(header.page_size.into())));
        fields.push((
            "reserved_per_page".to_owned(),
            Value::Int(header.reserved_per_page.into()),
        ));
        fields.push(("encoding".to_owned(), Value::Int(header.encoding.into())));
        fields.push((
            "page_count".to_owned(),
            Value::Int(header.page_count as i64),
        ));
        fields.push((
            "freelist_trunk".to_owned(),
            Value::Int(header.freelist_trunk.into()),
        ));
        fields.push((
            "freelist_count".to_owned(),
            Value::Int(header.freelist_count.into()),
        ));
        fields.push((
            "catalog_root".to_owned(),
            Value::Int(header.catalog_root.into()),
        ));
        fields.push(("node_root".to_owned(), Value::Int(header.node_root.into())));
        fields.push(("edge_root".to_owned(), Value::Int(header.edge_root.into())));
        fields.push((
            "fragment_root".to_owned(),
            Value::Int(header.fragment_root.into()),
        ));
        fields.push((
            "next_node_id".to_owned(),
            Value::Int(header.next_node_id as i64),
        ));
        fields.push((
            "next_edge_id".to_owned(),
            Value::Int(header.next_edge_id as i64),
        ));
        fields.push((
            "next_label_id".to_owned(),
            Value::Int(header.next_label_id.into()),
        ));
        fields.push((
            "next_type_id".to_owned(),
            Value::Int(header.next_type_id.into()),
        ));
        fields.push((
            "next_key_id".to_owned(),
            Value::Int(header.next_key_id.into()),
        ));
        fields.push((
            "change_counter".to_owned(),
            Value::Int(header.change_counter.into()),
        ));
        fields.push((
            "schema_generation".to_owned(),
            Value::Int(header.schema_generation.into()),
        ));
        fields.push(("created_at".to_owned(), Value::Int(header.created_at)));
        fields.push(("uuid".to_owned(), Value::from(hex(&header.uuid))));
        fields.push((
            "user_version".to_owned(),
            Value::Int(header.user_version.into()),
        ));
        fields.push((
            "application_id".to_owned(),
            Value::Int(header.application_id.into()),
        ));
    } else {
        let uuid = uuid::Uuid::from_bytes(header.uuid);
        let logical_size = header.page_count * u64::from(header.page_size);
        fields.push((
            "file".to_owned(),
            Value::from(db.path().display().to_string()),
        ));
        fields.push((
            "format_version".to_owned(),
            Value::from(format!("{}.{}", header.format_major, header.format_minor)),
        ));
        fields.push((
            "encoding".to_owned(),
            Value::from(match header.encoding {
                0 => "UTF-8".to_owned(),
                other => format!("unknown ({other})"),
            }),
        ));
        fields.push(("page_size".to_owned(), Value::Int(header.page_size.into())));
        fields.push((
            "page_count".to_owned(),
            Value::Int(header.page_count as i64),
        ));
        fields.push(("logical_size".to_owned(), Value::Int(logical_size as i64)));
        fields.push((
            "created".to_owned(),
            Value::from(format_unix_utc(header.created_at)),
        ));
        fields.push(("uuid".to_owned(), Value::from(uuid.to_string())));
        fields.push((
            "change_counter".to_owned(),
            Value::Int(header.change_counter.into()),
        ));
        fields.push((
            "schema_generation".to_owned(),
            Value::Int(header.schema_generation.into()),
        ));
        fields.push((
            "user_version".to_owned(),
            Value::Int(header.user_version.into()),
        ));
        fields.push((
            "application_id".to_owned(),
            Value::Int(header.application_id.into()),
        ));
        fields.push((
            "freelist".to_owned(),
            Value::Map(
                vec![
                    ("count".to_owned(), Value::Int(header.freelist_count.into())),
                    ("trunk".to_owned(), Value::Int(header.freelist_trunk.into())),
                ]
                .into_iter()
                .map(|(key, value)| (std::sync::Arc::<str>::from(key), value))
                .collect::<Vec<_>>()
                .into(),
            ),
        ));
        let roots: Vec<(std::sync::Arc<str>, Value)> = vec![
            ("catalog".to_owned(), Value::Int(header.catalog_root.into())),
            ("nodes".to_owned(), Value::Int(header.node_root.into())),
            ("edges".to_owned(), Value::Int(header.edge_root.into())),
            (
                "fragments".to_owned(),
                Value::Int(header.fragment_root.into()),
            ),
        ]
        .into_iter()
        .map(|(key, value)| (std::sync::Arc::from(key), value))
        .collect();
        fields.push(("roots".to_owned(), Value::Map(roots.into())));
        let allocated: Vec<(std::sync::Arc<str>, Value)> = vec![
            (
                "nodes".to_owned(),
                Value::Int(header.next_node_id.saturating_sub(1) as i64),
            ),
            (
                "edges".to_owned(),
                Value::Int(header.next_edge_id.saturating_sub(1) as i64),
            ),
            (
                "labels".to_owned(),
                Value::Int(header.next_label_id.saturating_sub(1).into()),
            ),
            (
                "types".to_owned(),
                Value::Int(header.next_type_id.saturating_sub(1).into()),
            ),
            (
                "keys".to_owned(),
                Value::Int(header.next_key_id.saturating_sub(1).into()),
            ),
        ]
        .into_iter()
        .map(|(key, value)| (std::sync::Arc::from(key), value))
        .collect();
        fields.push(("allocated".to_owned(), Value::Map(allocated.into())));
        fields.push((
            "reserved_per_page".to_owned(),
            Value::Int(header.reserved_per_page.into()),
        ));
    }
    // The header view is plain text; no colors, compact single line.
    object_json(db, &fields, JsonOptions::plain(false), 0)
}

fn indent_str(level: usize) -> String {
    "  ".repeat(level)
}

/// Escape a string for inclusion in a JSON document.
pub fn escape_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Format a Unix timestamp as UTC without external date libraries.
pub fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let remainder = secs.rem_euclid(86_400);
    let (hour, minute, second) = (remainder / 3600, (remainder % 3600) / 60, remainder % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Days since the Unix epoch to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_control_characters_and_quotes() {
        assert_eq!(escape_json("plain"), "\"plain\"");
        assert_eq!(escape_json("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(escape_json("a\nb\tc"), "\"a\\nb\\tc\"");
        assert_eq!(escape_json("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn unix_utc_formatting_handles_epoch_leap_day_and_negative() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_unix_utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_unix_utc(1_709_164_800), "2024-02-29 00:00:00 UTC");
        assert_eq!(format_unix_utc(-1), "1969-12-31 23:59:59 UTC");
    }
}
