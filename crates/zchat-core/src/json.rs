//! Owned JSON values backed by zconv's strict, duplicate-rejecting validator.
use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::fmt::Write;
use zconv::json::{self, Kind, Token};

use crate::{ChatError, Result};

/// Resource limits apply before token allocation and before owned conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonLimits {
    pub max_bytes: usize,
    pub max_tokens: usize,
    pub max_depth: usize,
    pub max_object_fields: usize,
}

impl Default for JsonLimits {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            max_tokens: 65_536,
            max_depth: 32,
            max_object_fields: 4096,
        }
    }
}

/// Numbers retain their exact validated lexical representation, including
/// integers larger than JavaScript's safe integer range.
///
/// Prefer `From` for constructing numbers. `try_stringify` rejects manually
/// constructed `Number` values that are not a single valid JSON number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub fn object() -> Self {
        Self::Object(BTreeMap::new())
    }
    pub fn array() -> Self {
        Self::Array(Vec::new())
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
    pub fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(value) = self {
            Some(*value)
        } else {
            None
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        if let Self::Number(value) = self
            && !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return value.parse().ok();
        }
        None
    }
    pub fn as_i64(&self) -> Option<i64> {
        if let Self::Number(value) = self {
            let digits = value.strip_prefix('-').unwrap_or(value);
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return value.parse().ok();
            }
        }
        None
    }
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        if let Self::Array(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        if let Self::Array(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        if let Self::Object(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_object_mut(&mut self) -> Option<&mut BTreeMap<String, Value>> {
        if let Self::Object(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.as_object_mut()?.get_mut(key)
    }
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key)?.as_u64()
    }
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    /// Serialize a value built from validated JSON or the provided constructors.
    /// Use `try_stringify` when accepting manually constructed values from a
    /// caller, since an invalid Number or an excessive tree is a programming error.
    pub fn stringify(&self) -> String {
        self.try_stringify()
            .expect("JSON value must be valid and bounded")
    }

    pub fn try_stringify(&self) -> Result<String> {
        self.stringify_with_limits(JsonLimits::default())
    }

    pub fn stringify_with_limits(&self, limits: JsonLimits) -> Result<String> {
        validate_limits(limits)?;
        let mut output = String::new();
        let mut values = 0;
        write_value(self, &mut output, limits, 0, &mut values)?;
        Ok(output)
    }

    pub fn number(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        if valid_number(&raw) {
            Ok(Self::Number(raw))
        } else {
            Err(ChatError::new(
                "invalid_number",
                "expected one finite JSON number",
            ))
        }
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}
impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}
impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
macro_rules! integers {
    ($($kind:ty),* $(,)?) => {$(
        impl From<$kind> for Value {
            fn from(value: $kind) -> Self { Self::Number(value.to_string()) }
        }
    )*};
}
integers!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize
);
impl From<Vec<Value>> for Value {
    fn from(value: Vec<Value>) -> Self {
        Self::Array(value)
    }
}

pub fn obj<K: Into<String>>(values: impl IntoIterator<Item = (K, Value)>) -> Value {
    Value::Object(
        values
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
pub fn arr(values: impl IntoIterator<Item = Value>) -> Value {
    Value::Array(values.into_iter().collect())
}

pub fn parse(input: &str) -> Result<Value> {
    parse_with_limits(input, JsonLimits::default())
}

pub fn parse_with_limits(input: &str, limits: JsonLimits) -> Result<Value> {
    validate_limits(limits)?;
    if input.len() > limits.max_bytes {
        return Err(ChatError::new(
            "input_too_large",
            format!("JSON exceeds {} bytes", limits.max_bytes),
        ));
    }
    let native_limits = json::Limits {
        max_depth: limits.max_depth,
        max_input: limits.max_bytes,
        max_tokens: limits.max_tokens,
        max_object_fields: limits.max_object_fields,
    };
    let initial = input.len().clamp(1, 64).min(limits.max_tokens);
    let mut tokens = vec![Token::EMPTY; initial];
    loop {
        let token_capacity = tokens.len();
        match json::parse(input.as_bytes(), &mut tokens, native_limits) {
            Ok(document) => return own(document.root()),
            Err(zconv::Error::WorkspaceTooSmall { needed })
                if token_capacity < limits.max_tokens =>
            {
                let next = token_capacity
                    .saturating_mul(2)
                    .max(needed)
                    .min(limits.max_tokens);
                tokens.resize(next, Token::EMPTY);
            }
            Err(error) => return Err(ChatError::new("invalid_json", format!("{error:?}"))),
        }
    }
}

fn validate_limits(limits: JsonLimits) -> Result<()> {
    // A caller can lower these bounds, but cannot request an unbounded allocation.
    if limits.max_bytes == 0
        || limits.max_bytes > 16 * 1024 * 1024
        || limits.max_tokens == 0
        || limits.max_tokens > 262_144
        || limits.max_depth == 0
        || limits.max_depth > json::MAX_DEPTH
        || limits.max_object_fields == 0
        || limits.max_object_fields > 16_384
    {
        return Err(ChatError::new(
            "invalid_limits",
            "JSON limits exceed the supported bounded range",
        ));
    }
    Ok(())
}

fn own(value: json::Value<'_, '_>) -> Result<Value> {
    Ok(match value.kind() {
        Kind::Null => Value::Null,
        Kind::Bool => Value::Bool(value.as_bool().unwrap_or(false)),
        Kind::Number => Value::Number(
            core::str::from_utf8(value.raw())
                .map_err(|_| ChatError::new("invalid_json", "invalid numeric UTF-8"))?
                .into(),
        ),
        Kind::String => {
            Value::String(decode(value.as_string().ok_or_else(|| {
                ChatError::new("invalid_json", "expected validated string")
            })?)?)
        }
        Kind::Array => Value::Array(value.array().map(own).collect::<Result<Vec<_>>>()?),
        Kind::Object => Value::Object(
            value
                .object()
                .map(|(key, value)| Ok((decode(key)?, own(value)?)))
                .collect::<Result<BTreeMap<_, _>>>()?,
        ),
    })
}

fn decode(value: json::JsonString<'_>) -> Result<String> {
    if let Some(plain) = value.as_str() {
        return Ok(plain.into());
    }
    String::from_utf8(value.bytes().collect())
        .map_err(|_| ChatError::new("invalid_json", "invalid decoded UTF-8"))
}

fn valid_number(value: &str) -> bool {
    if value.is_empty() || value.len() > 16 * 1024 * 1024 || value.trim() != value {
        return false;
    }
    let mut token = [Token::EMPTY; 1];
    json::parse(value.as_bytes(), &mut token, json::Limits::default())
        .is_ok_and(|document| document.root().kind() == Kind::Number)
}

fn check_output(output: &str, limits: JsonLimits) -> Result<()> {
    if output.len() > limits.max_bytes {
        return Err(ChatError::new(
            "output_too_large",
            format!("JSON exceeds {} bytes", limits.max_bytes),
        ));
    }
    Ok(())
}

fn write_string(value: &str, output: &mut String, limits: JsonLimits) -> Result<()> {
    if value.len() > limits.max_bytes.saturating_sub(output.len()) {
        return Err(ChatError::new(
            "output_too_large",
            "JSON string exceeds the output bound",
        ));
    }
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{0008}' => output.push_str("\\b"),
            '\u{000c}' => output.push_str("\\f"),
            c if c <= '\u{001f}' => {
                write!(output, "\\u{:04x}", c as u32).expect("String write");
            }
            c => output.push(c),
        }
        check_output(output, limits)?;
    }
    output.push('"');
    check_output(output, limits)
}

fn write_value(
    value: &Value,
    output: &mut String,
    limits: JsonLimits,
    depth: usize,
    count: &mut usize,
) -> Result<()> {
    *count += 1;
    if *count > limits.max_tokens {
        return Err(ChatError::new(
            "too_many_values",
            "JSON value count exceeds the bound",
        ));
    }
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => {
            if !valid_number(value) {
                return Err(ChatError::new("invalid_number", "invalid JSON number"));
            }
            if value.len() > limits.max_bytes.saturating_sub(output.len()) {
                return Err(ChatError::new(
                    "output_too_large",
                    "JSON number exceeds the output bound",
                ));
            }
            output.push_str(value);
        }
        Value::String(value) => write_string(value, output, limits)?,
        Value::Array(values) => {
            if depth >= limits.max_depth {
                return Err(ChatError::new(
                    "depth_exceeded",
                    "JSON nesting exceeds the bound",
                ));
            }
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_value(value, output, limits, depth + 1, count)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            if depth >= limits.max_depth {
                return Err(ChatError::new(
                    "depth_exceeded",
                    "JSON nesting exceeds the bound",
                ));
            }
            if values.len() > limits.max_object_fields {
                return Err(ChatError::new(
                    "too_many_fields",
                    "JSON object fields exceed the bound",
                ));
            }
            output.push('{');
            for (index, (key, value)) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                *count += 1; // Keys consume tokens in zconv's representation.
                write_string(key, output, limits)?;
                output.push(':');
                write_value(value, output, limits, depth + 1, count)?;
            }
            output.push('}');
        }
    }
    check_output(output, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_large_ids_and_decodes_unicode() {
        let value = parse(r#"{"id":18446744073709551616,"text":"\ud83d\ude80\n汉字","null":null}"#)
            .unwrap();
        assert_eq!(
            value.get("id"),
            Some(&Value::Number("18446744073709551616".into()))
        );
        assert_eq!(value.get_str("text"), Some("🚀\n汉字"));
        assert_eq!(parse(&value.stringify()).unwrap(), value);
    }

    #[test]
    fn rejects_duplicate_decoded_keys_and_invalid_surrogates() {
        for text in [
            r#"{"a":1,"\u0061":2}"#,
            r#""\ud800""#,
            r#""\udc00""#,
            "1 trailing",
            "01",
            "NaN",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn enforces_input_depth_token_and_field_limits() {
        let limits = JsonLimits {
            max_bytes: 4,
            ..JsonLimits::default()
        };
        assert!(parse_with_limits("[1,2]", limits).is_err());
        let limits = JsonLimits {
            max_depth: 1,
            ..JsonLimits::default()
        };
        assert!(parse_with_limits("[[]]", limits).is_err());
        let limits = JsonLimits {
            max_tokens: 2,
            ..JsonLimits::default()
        };
        assert!(parse_with_limits("[1,2]", limits).is_err());
        let limits = JsonLimits {
            max_object_fields: 1,
            ..JsonLimits::default()
        };
        assert!(parse_with_limits(r#"{"a":1,"b":2}"#, limits).is_err());
    }

    #[test]
    fn grows_token_tape_for_large_valid_arrays() {
        let value = arr((0..200).map(Value::from));
        assert_eq!(parse(&value.stringify()).unwrap(), value);
    }

    #[test]
    fn controls_and_quotes_round_trip() {
        let source: String = (0..32).filter_map(char::from_u32).collect();
        let value = Value::from(format!("{source}\"\\雪🚀"));
        assert_eq!(parse(&value.stringify()).unwrap(), value);
    }

    #[test]
    fn serialization_rejects_number_injection_and_overflow() {
        assert!(
            Value::Number("0,\"injected\":true".into())
                .try_stringify()
                .is_err()
        );
        assert!(Value::Number("NaN".into()).try_stringify().is_err());
        let limits = JsonLimits {
            max_bytes: 5,
            ..JsonLimits::default()
        };
        assert!(Value::from("\n\n\n").stringify_with_limits(limits).is_err());
        assert_eq!(Value::from(-1i64).as_u64(), None);
        assert_eq!(Value::from(i64::MIN).as_i64(), Some(i64::MIN));
    }
}
