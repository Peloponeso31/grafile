//! Record and value encoding.
//!
//! Two encodings live here:
//!
//! * **Records** (node/edge payloads) use compact LEB128 varints and typed
//!   values, optimized for size and fast decoding.
//! * **Index keys** are order-preserving: `memcmp` on encoded bytes yields the
//!   same order as comparing the original values, so B+tree index scans need
//!   no value decoding.
//!
//! Layouts are specified in `docs/format.md`.

use std::sync::Arc;

use crate::error::{Error, Result};

/// A dynamically typed property or expression value.
///
/// Strings, byte arrays and lists are reference-counted so cloning a value
/// (for registers, result rows, or index entries) is cheap.
#[derive(Debug, Clone)]
pub enum Value {
    /// The absence of a value.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit floating point.
    Float(f64),
    /// UTF-8 text.
    Text(Arc<str>),
    /// Raw bytes.
    Bytes(Arc<[u8]>),
    /// Ordered list of values.
    List(Arc<[Value]>),
    /// Map of string keys to values (engine-internal; not storable).
    Map(Arc<[(Arc<str>, Value)]>),
    /// Reference to a node by id (engine-internal; not storable).
    Node(u64),
    /// Reference to an edge by id (engine-internal; not storable).
    Edge(u64),
}

impl Value {
    /// Human-readable type name used in error messages and `type()`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Int(_) => "integer",
            Self::Float(_) => "float",
            Self::Text(_) => "string",
            Self::Bytes(_) => "bytes",
            Self::List(_) => "list",
            Self::Map(_) => "map",
            Self::Node(_) => "node",
            Self::Edge(_) => "relationship",
        }
    }

    /// Interpret the value as an integer, if possible.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// Interpret the value as text, if possible.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }

    /// Whether the value is [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Borrow the value as a boolean, following Cypher truthiness rules
    /// (`null` is unknown).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// Numeric value as `f64` for arithmetic (integers included).
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Int(value) => Some(*value as f64),
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }
}

impl PartialEq for Value {
    /// Equality with NaN equal to itself so that `Eq` and `Ord` are
    /// consistent (needed for sorting and grouping).
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::List(a), Self::List(b)) => a == b,
            (Self::Map(a), Self::Map(b)) => a == b,
            (Self::Node(a), Self::Node(b)) => a == b,
            (Self::Edge(a), Self::Edge(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    /// Total order used for sorting, grouping and index-style comparisons.
    ///
    /// Across types the order is `null < boolean < number < string < bytes <
    /// list < node < edge`; numbers compare numerically across integers and
    /// floats; NaN sorts consistently via `total_cmp`.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            (Self::Int(a), Self::Int(b)) => a.cmp(b),
            (Self::Float(a), Self::Float(b)) => float_cmp(*a, *b),
            (Self::Int(a), Self::Float(b)) => float_cmp(*a as f64, *b),
            (Self::Float(a), Self::Int(b)) => float_cmp(*a, *b as f64),
            (Self::Text(a), Self::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Self::Bytes(a), Self::Bytes(b)) => a.cmp(b),
            (Self::List(a), Self::List(b)) => a.cmp(b),
            (Self::Map(a), Self::Map(b)) => a.cmp(b),
            (Self::Node(a), Self::Node(b)) => a.cmp(b),
            (Self::Edge(a), Self::Edge(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

fn float_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    if a == b {
        // Treat -0.0 and 0.0 as equal, matching `PartialEq`.
        return std::cmp::Ordering::Equal;
    }
    a.total_cmp(&b)
}

fn rank(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 2,
        Value::Text(_) => 3,
        Value::Bytes(_) => 4,
        Value::List(_) => 5,
        Value::Map(_) => 6,
        Value::Node(_) => 7,
        Value::Edge(_) => 8,
    }
}

/// Cypher equality: `None` when either side is null or the types are
/// incomparable; integers and floats compare numerically.
pub fn value_equals(a: &Value, b: &Value) -> Option<bool> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x == y),
        (Value::Float(x), Value::Float(y)) => Some(x == y),
        (Value::Int(x), Value::Float(y)) => Some(*x as f64 == *y),
        (Value::Float(x), Value::Int(y)) => Some(*x == *y as f64),
        (Value::Bool(x), Value::Bool(y)) => Some(x == y),
        (Value::Text(x), Value::Text(y)) => Some(x == y),
        (Value::Bytes(x), Value::Bytes(y)) => Some(x == y),
        (Value::Node(x), Value::Node(y)) => Some(x == y),
        (Value::Edge(x), Value::Edge(y)) => Some(x == y),
        (Value::List(x), Value::List(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (item_a, item_b) in x.iter().zip(y.iter()) {
                match value_equals(item_a, item_b) {
                    Some(true) => {}
                    Some(false) => return Some(false),
                    None => unknown = true,
                }
            }
            if unknown { None } else { Some(true) }
        }
        _ => None,
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(Arc::from(value))
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(Arc::from(value.as_str()))
    }
}

/// Encode an unsigned LEB128 varint.
pub fn encode_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Decode an unsigned LEB128 varint, advancing `pos`.
pub fn decode_varint(input: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *input
            .get(*pos)
            .ok_or_else(|| Error::corrupt("varint runs past end of record"))?;
        *pos += 1;
        if shift >= 64 || (shift == 63 && byte > 1) {
            return Err(Error::corrupt("varint overflows 64 bits"));
        }
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

/// Zigzag-encode a signed integer for compact varint storage.
pub const fn zigzag_encode(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// Decode a zigzag-encoded signed integer.
pub const fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Tag bytes for record values.
mod tag {
    pub const NULL: u8 = 0;
    pub const FALSE: u8 = 1;
    pub const TRUE: u8 = 2;
    pub const INT: u8 = 3;
    pub const FLOAT: u8 = 4;
    pub const TEXT: u8 = 5;
    pub const BYTES: u8 = 6;
    pub const LIST: u8 = 7;
}

/// Encode a value into a record buffer.
pub fn encode_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(tag::NULL),
        Value::Bool(false) => out.push(tag::FALSE),
        Value::Bool(true) => out.push(tag::TRUE),
        Value::Int(value) => {
            out.push(tag::INT);
            encode_varint(out, zigzag_encode(*value));
        }
        Value::Float(value) => {
            out.push(tag::FLOAT);
            out.extend_from_slice(&value.to_bits().to_be_bytes());
        }
        Value::Text(value) => {
            out.push(tag::TEXT);
            encode_varint(out, value.len() as u64);
            out.extend_from_slice(value.as_bytes());
        }
        Value::Bytes(value) => {
            out.push(tag::BYTES);
            encode_varint(out, value.len() as u64);
            out.extend_from_slice(value);
        }
        Value::List(values) => {
            out.push(tag::LIST);
            encode_varint(out, values.len() as u64);
            for item in values.iter() {
                encode_value(out, item);
            }
        }
        // Maps and entity references are engine-internal and never stored;
        // the engine validates property values before encoding.
        Value::Map(_) | Value::Node(_) | Value::Edge(_) => {
            debug_assert!(false, "attempted to encode an unstorable value");
            out.push(tag::NULL);
        }
    }
}

/// Decode a value from a record buffer, advancing `pos`.
pub fn decode_value(input: &[u8], pos: &mut usize) -> Result<Value> {
    let byte = *input
        .get(*pos)
        .ok_or_else(|| Error::corrupt("value tag runs past end of record"))?;
    *pos += 1;
    match byte {
        tag::NULL => Ok(Value::Null),
        tag::FALSE => Ok(Value::Bool(false)),
        tag::TRUE => Ok(Value::Bool(true)),
        tag::INT => {
            let raw = decode_varint(input, pos)?;
            Ok(Value::Int(zigzag_decode(raw)))
        }
        tag::FLOAT => {
            let bytes = take(input, pos, 8)?;
            let bits = u64::from_be_bytes(bytes.try_into().expect("slice length"));
            Ok(Value::Float(f64::from_bits(bits)))
        }
        tag::TEXT => {
            let len = decode_varint(input, pos)? as usize;
            let bytes = take(input, pos, len)?;
            let text = std::str::from_utf8(bytes)
                .map_err(|_| Error::corrupt("invalid UTF-8 in text value"))?;
            Ok(Value::Text(Arc::from(text)))
        }
        tag::BYTES => {
            let len = decode_varint(input, pos)? as usize;
            let bytes = take(input, pos, len)?;
            Ok(Value::Bytes(Arc::from(bytes)))
        }
        tag::LIST => {
            let count = decode_varint(input, pos)? as usize;
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(decode_value(input, pos)?);
            }
            Ok(Value::List(Arc::from(values)))
        }
        other => Err(Error::corrupt(format!("unknown value tag {other}"))),
    }
}

fn take<'a>(input: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| Error::corrupt("record field length overflow"))?;
    let slice = input
        .get(*pos..end)
        .ok_or_else(|| Error::corrupt("record field runs past end of record"))?;
    *pos = end;
    Ok(slice)
}

/// A node record: label ids plus properties sorted by key id.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodeRecord {
    /// Sorted, deduplicated label ids.
    pub labels: Vec<u32>,
    /// Properties sorted by key id.
    pub props: Vec<(u32, Value)>,
}

impl NodeRecord {
    /// Encode into a record buffer.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.labels.len() * 2 + self.props.len() * 6);
        encode_varint(&mut out, self.labels.len() as u64);
        let mut prev = 0u32;
        for label in &self.labels {
            encode_varint(&mut out, u64::from(label - prev));
            prev = *label;
        }
        encode_props(&mut out, &self.props);
        out
    }

    /// Decode from a record buffer.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut pos = 0usize;
        let label_count = decode_varint(bytes, &mut pos)? as usize;
        let mut labels = Vec::with_capacity(label_count);
        let mut prev = 0u32;
        for _ in 0..label_count {
            let delta = decode_varint(bytes, &mut pos)?;
            let delta =
                u32::try_from(delta).map_err(|_| Error::corrupt("label delta out of range"))?;
            let label = prev
                .checked_add(delta)
                .ok_or_else(|| Error::corrupt("label id overflow"))?;
            labels.push(label);
            prev = label;
        }
        let props = decode_props(bytes, &mut pos)?;
        if pos != bytes.len() {
            return Err(Error::corrupt("trailing bytes after node record"));
        }
        Ok(Self { labels, props })
    }

    /// Property lookup by key id.
    pub fn get(&self, key_id: u32) -> Option<&Value> {
        self.props
            .binary_search_by_key(&key_id, |(key, _)| *key)
            .ok()
            .map(|idx| &self.props[idx].1)
    }

    /// Insert or replace a property, keeping the map sorted.
    pub fn set(&mut self, key_id: u32, value: Value) {
        match self.props.binary_search_by_key(&key_id, |(key, _)| *key) {
            Ok(idx) => self.props[idx].1 = value,
            Err(idx) => self.props.insert(idx, (key_id, value)),
        }
    }

    /// Remove a property, returning whether it existed.
    pub fn remove(&mut self, key_id: u32) -> bool {
        match self.props.binary_search_by_key(&key_id, |(key, _)| *key) {
            Ok(idx) => {
                self.props.remove(idx);
                true
            }
            Err(_) => false,
        }
    }
}

/// An edge record: type, endpoints, and properties sorted by key id.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EdgeRecord {
    /// Edge type id (never 0).
    pub edge_type: u32,
    /// Source node id.
    pub src: u64,
    /// Destination node id.
    pub dst: u64,
    /// Properties sorted by key id.
    pub props: Vec<(u32, Value)>,
}

impl EdgeRecord {
    /// Encode into a record buffer.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.props.len() * 6);
        encode_varint(&mut out, u64::from(self.edge_type));
        encode_varint(&mut out, self.src);
        encode_varint(&mut out, self.dst);
        encode_props(&mut out, &self.props);
        out
    }

    /// Decode from a record buffer.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut pos = 0usize;
        let edge_type = u32::try_from(decode_varint(bytes, &mut pos)?)
            .map_err(|_| Error::corrupt("edge type out of range"))?;
        let src = decode_varint(bytes, &mut pos)?;
        let dst = decode_varint(bytes, &mut pos)?;
        let props = decode_props(bytes, &mut pos)?;
        if pos != bytes.len() {
            return Err(Error::corrupt("trailing bytes after edge record"));
        }
        Ok(Self {
            edge_type,
            src,
            dst,
            props,
        })
    }

    /// Property lookup by key id.
    pub fn get(&self, key_id: u32) -> Option<&Value> {
        self.props
            .binary_search_by_key(&key_id, |(key, _)| *key)
            .ok()
            .map(|idx| &self.props[idx].1)
    }

    /// Insert or replace a property, keeping the map sorted.
    pub fn set(&mut self, key_id: u32, value: Value) {
        match self.props.binary_search_by_key(&key_id, |(key, _)| *key) {
            Ok(idx) => self.props[idx].1 = value,
            Err(idx) => self.props.insert(idx, (key_id, value)),
        }
    }

    /// Remove a property, returning whether it existed.
    pub fn remove(&mut self, key_id: u32) -> bool {
        match self.props.binary_search_by_key(&key_id, |(key, _)| *key) {
            Ok(idx) => {
                self.props.remove(idx);
                true
            }
            Err(_) => false,
        }
    }
}

fn encode_props(out: &mut Vec<u8>, props: &[(u32, Value)]) {
    encode_varint(out, props.len() as u64);
    let mut prev = 0u32;
    for (key, value) in props {
        encode_varint(out, u64::from(key - prev));
        prev = *key;
        encode_value(out, value);
    }
}

fn decode_props(bytes: &[u8], pos: &mut usize) -> Result<Vec<(u32, Value)>> {
    let count = decode_varint(bytes, pos)? as usize;
    let mut props = Vec::with_capacity(count);
    let mut prev = 0u32;
    for _ in 0..count {
        let delta = decode_varint(bytes, pos)?;
        let delta = u32::try_from(delta).map_err(|_| Error::corrupt("key delta out of range"))?;
        let key = prev
            .checked_add(delta)
            .ok_or_else(|| Error::corrupt("key id overflow"))?;
        let value = decode_value(bytes, pos)?;
        props.push((key, value));
        prev = key;
    }
    Ok(props)
}

/// Order-preserving type tags for index keys.
mod index_tag {
    pub const NULL: u8 = 0;
    pub const FALSE: u8 = 1;
    pub const TRUE: u8 = 2;
    pub const INT: u8 = 3;
    pub const FLOAT: u8 = 4;
    pub const TEXT: u8 = 5;
    pub const BYTES: u8 = 6;
    pub const LIST: u8 = 7;
}

/// Encode a value so that byte order equals value order.
///
/// Text and byte strings escape `0x00` as `0x00 0xFF` and terminate with
/// `0x00 0x01`, which keeps prefix-free ordering for composite keys.
pub fn encode_index_key(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(index_tag::NULL),
        Value::Bool(false) => out.push(index_tag::FALSE),
        Value::Bool(true) => out.push(index_tag::TRUE),
        Value::Int(value) => {
            out.push(index_tag::INT);
            let flipped = (*value as u64) ^ (1u64 << 63);
            out.extend_from_slice(&flipped.to_be_bytes());
        }
        Value::Float(value) => {
            out.push(index_tag::FLOAT);
            let bits = value.to_bits();
            let flipped = if bits >> 63 == 1 {
                !bits
            } else {
                bits ^ (1u64 << 63)
            };
            out.extend_from_slice(&flipped.to_be_bytes());
        }
        Value::Text(value) => {
            out.push(index_tag::TEXT);
            encode_escaped(out, value.as_bytes());
        }
        Value::Bytes(value) => {
            out.push(index_tag::BYTES);
            encode_escaped(out, value);
        }
        Value::List(values) => {
            out.push(index_tag::LIST);
            for item in values.iter() {
                encode_index_key(out, item);
            }
            out.extend_from_slice(&[0x00, 0x01]);
        }
        Value::Map(_) | Value::Node(_) | Value::Edge(_) => {
            debug_assert!(false, "attempted to index an unstorable value");
            out.push(index_tag::NULL);
        }
    }
}

fn encode_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        if *byte == 0x00 {
            out.extend_from_slice(&[0x00, 0xFF]);
        } else {
            out.push(*byte);
        }
    }
    out.extend_from_slice(&[0x00, 0x01]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn value_strategy() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::Int),
            any::<f64>().prop_map(Value::Float),
            "\\PC{0,20}".prop_map(|s| Value::Text(Arc::from(s.as_str()))),
            prop::collection::vec(any::<u8>(), 0..20).prop_map(|b| Value::Bytes(Arc::from(b))),
        ];
        leaf.prop_recursive(2, 16, 4, |inner| {
            prop::collection::vec(inner, 0..4).prop_map(|values| Value::List(Arc::from(values)))
        })
    }

    proptest! {
        #[test]
        fn value_roundtrip(value in value_strategy()) {
            let mut buf = Vec::new();
            encode_value(&mut buf, &value);
            let mut pos = 0;
            let decoded = decode_value(&buf, &mut pos).expect("decode");
            prop_assert_eq!(pos, buf.len());
            // Compare re-encoded bytes: NaN floats are not `PartialEq`.
            let mut reencoded = Vec::new();
            encode_value(&mut reencoded, &decoded);
            prop_assert_eq!(reencoded, buf);
        }

        #[test]
        fn varint_roundtrip(value in any::<u64>()) {
            let mut buf = Vec::new();
            encode_varint(&mut buf, value);
            let mut pos = 0;
            prop_assert_eq!(decode_varint(&buf, &mut pos).expect("decode"), value);
            prop_assert_eq!(pos, buf.len());
        }

        #[test]
        fn node_record_roundtrip(
            mut labels in prop::collection::vec(1u32..50, 0..5),
            props in prop::collection::vec((1u32..50, value_strategy()), 0..6),
        ) {
            labels.sort_unstable();
            labels.dedup();
            let mut map: std::collections::BTreeMap<u32, Value> = props.into_iter().collect();
            // Null-valued properties are not stored.
            map.retain(|_, value| !value.is_null());
            let record = NodeRecord {
                labels,
                props: map.into_iter().collect(),
            };
            let encoded = record.encode();
            let decoded = NodeRecord::decode(&encoded).expect("decode");
            prop_assert_eq!(decoded.labels.clone(), record.labels);
            prop_assert_eq!(decoded.encode(), encoded);
        }

        #[test]
        fn index_keys_are_order_preserving(
            a in prop_oneof![
                any::<i64>().prop_map(Value::Int),
                any::<f64>().prop_map(Value::Float),
                "\\PC{0,8}".prop_map(|s| Value::Text(Arc::from(s.as_str()))),
            ],
            b in prop_oneof![
                any::<i64>().prop_map(Value::Int),
                any::<f64>().prop_map(Value::Float),
                "\\PC{0,8}".prop_map(|s| Value::Text(Arc::from(s.as_str()))),
            ],
        ) {
            let mut ka = Vec::new();
            let mut kb = Vec::new();
            encode_index_key(&mut ka, &a);
            encode_index_key(&mut kb, &b);
            let ordering = compare_values(&a, &b);
            prop_assert_eq!(ka.cmp(&kb), ordering);
        }
    }

    /// Total order used by the index encoding, for test comparison.
    ///
    /// Within a type, values compare naturally (floats by `total_cmp`, which
    /// matches the bit transform). Across types, tag order decides.
    fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(y),
            (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
            (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
            _ => index_tag_of(a).cmp(&index_tag_of(b)),
        }
    }

    fn index_tag_of(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::Bool(false) => 1,
            Value::Bool(true) => 2,
            Value::Int(_) => 3,
            Value::Float(_) => 4,
            Value::Text(_) => 5,
            Value::Bytes(_) => 6,
            Value::List(_) => 7,
            Value::Map(_) | Value::Node(_) | Value::Edge(_) => 8,
        }
    }

    #[test]
    fn negative_ints_sort_before_positive() {
        let mut neg = Vec::new();
        let mut zero = Vec::new();
        let mut pos = Vec::new();
        encode_index_key(&mut neg, &Value::Int(-5));
        encode_index_key(&mut zero, &Value::Int(0));
        encode_index_key(&mut pos, &Value::Int(5));
        assert!(neg < zero && zero < pos);
    }

    #[test]
    fn negative_floats_sort_before_positive() {
        let mut neg = Vec::new();
        let mut pos = Vec::new();
        encode_index_key(&mut neg, &Value::Float(-1.5));
        encode_index_key(&mut pos, &Value::Float(1.5));
        assert!(neg < pos);
    }

    #[test]
    fn embedded_zero_text_orders_before_terminated() {
        let mut with_zero = Vec::new();
        let mut plain = Vec::new();
        encode_index_key(&mut with_zero, &Value::Text(Arc::from("a\0b")));
        encode_index_key(&mut plain, &Value::Text(Arc::from("ab")));
        // "a" < "a\0b" < "ab": escaped bytes preserve the raw comparison.
        assert!(with_zero < plain);
    }

    #[test]
    fn null_property_is_absent() {
        let mut record = NodeRecord::default();
        record.set(3, Value::Int(1));
        record.set(1, Value::Text(Arc::from("x")));
        assert_eq!(record.props[0].0, 1);
        assert_eq!(record.props[1].0, 3);
        assert!(record.remove(3));
        assert!(!record.remove(3));
    }
}
