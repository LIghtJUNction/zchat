use std::collections::BTreeMap;
use zchat_core::{ChatError, Value};

pub(crate) const MAX_WIRE_BYTES: usize = 1024 * 1024;
const MAX_QUEUED_BYTES: usize = MAX_WIRE_BYTES / 2;
pub(crate) const MAX_EVENTS: usize = 256;

pub(crate) fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

pub(crate) fn string(value: impl Into<String>) -> Value {
    Value::String(value.into())
}
pub(crate) fn number(value: impl ToString) -> Value {
    Value::Number(value.to_string())
}
pub(crate) fn array(values: impl IntoIterator<Item = Value>) -> Value {
    Value::Array(values.into_iter().collect())
}
pub(crate) fn empty() -> Value {
    Value::Object(BTreeMap::new())
}
pub(crate) fn value_at<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}
pub(crate) fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, ChatError> {
    let result = text(value, key);
    if result.is_empty() {
        Err(ChatError::new(
            "invalid_params",
            format!("{key} must be a nonempty string"),
        ))
    } else {
        Ok(result)
    }
}
pub(crate) fn identifier(value: &Value, key: &str) -> Result<String, ChatError> {
    match value.get(key) {
        Some(Value::String(v)) if !v.is_empty() => Ok(v.clone()),
        Some(Value::Number(v)) => Ok(v.clone()),
        _ => Err(ChatError::new(
            "invalid_params",
            format!("{key} must be an identifier"),
        )),
    }
}
pub(crate) fn scalar_id(value: &Value) -> Value {
    match value {
        Value::String(v) | Value::Number(v) => string(v),
        _ => Value::Null,
    }
}
pub(crate) fn page_offset(params: &Value, default: usize) -> Result<usize, ChatError> {
    match params.get("cursor") {
        Some(Value::String(value)) if !value.is_empty() => value.parse().map_err(|_| {
            ChatError::new("invalid_params", "cursor must be a nonnegative page offset")
        }),
        Some(Value::Number(value)) => value.parse().map_err(|_| {
            ChatError::new("invalid_params", "cursor must be a nonnegative page offset")
        }),
        Some(Value::Null) | None => Ok(default),
        Some(Value::String(value)) if value.is_empty() => Ok(default),
        _ => Err(ChatError::new(
            "invalid_params",
            "cursor must be a nonnegative page offset",
        )),
    }
}
pub(crate) fn u64_param(value: &Value, key: &str, default: u64) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(default)
}
pub(crate) fn bool_param(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}
pub(crate) fn list(value: &Value) -> &[Value] {
    value.as_array().map(|v| v.as_slice()).unwrap_or(&[])
}
pub(crate) fn unsupported(provider: &str, operation: &str) -> ChatError {
    ChatError::new(
        "unsupported_operation",
        format!(
            "{provider} does not normalize {operation}; inspect capabilities and use native.invoke for supported native operations"
        ),
    )
}
pub(crate) fn io_error(context: &str, error: std::io::Error) -> ChatError {
    ChatError::new("transport_error", format!("{context}: {}", error.kind()))
}
pub(crate) fn percent_encode(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            result.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(result, "%{byte:02X}");
        }
    }
    result
}
pub(crate) fn percent_decode(value: &str) -> Result<String, ChatError> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = bytes
                .get(i + 1..i + 3)
                .ok_or_else(|| ChatError::new("invalid_params", "invalid percent escape"))?;
            let hex = std::str::from_utf8(pair)
                .map_err(|_| ChatError::new("invalid_params", "invalid percent escape"))?;
            output.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| ChatError::new("invalid_params", "invalid percent escape"))?,
            );
            i += 3;
        } else {
            output.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(output)
        .map_err(|_| ChatError::new("invalid_params", "invalid UTF-8 identifier"))
}
pub(crate) fn integer_id(value: &Value, key: &str) -> Result<Value, ChatError> {
    let id = identifier(value, key)?;
    id.parse::<i64>().map_err(|_| {
        ChatError::new(
            "invalid_params",
            format!("{key} must be a signed 64-bit integer"),
        )
    })?;
    Ok(Value::Number(id))
}
pub(crate) fn set(value: &mut Value, key: &str, item: Value) {
    if let Value::Object(map) = value {
        map.insert(key.to_owned(), item);
    }
}
pub(crate) fn capability(
    provider: &str,
    operations: &[&str],
    encryption: &str,
    limitations: &[&str],
) -> Value {
    object([
        ("provider", string(provider)),
        ("operations", array(operations.iter().map(|v| string(*v)))),
        ("encryption", string(encryption)),
        ("limitations", array(limitations.iter().map(|v| string(*v)))),
    ])
}

#[derive(Default)]
pub(crate) struct EventQueue {
    entries: std::collections::VecDeque<(Value, usize)>,
    bytes: usize,
    dropped: u64,
}

impl EventQueue {
    pub fn push(&mut self, value: Value) {
        let Ok(encoded) = value.try_stringify() else {
            self.dropped += 1;
            return;
        };
        let size = encoded.len();
        if size > MAX_QUEUED_BYTES {
            self.dropped += 1;
            return;
        }
        while self.entries.len() >= MAX_EVENTS || self.bytes + size > MAX_QUEUED_BYTES {
            if let Some((_, size)) = self.entries.pop_front() {
                self.bytes -= size;
                self.dropped += 1;
            } else {
                break;
            }
        }
        self.bytes += size;
        self.entries.push_back((value, size));
    }
    pub fn drain(&mut self) -> Value {
        let events = self
            .entries
            .drain(..)
            .map(|(value, _)| value)
            .collect::<Vec<_>>();
        self.bytes = 0;
        let dropped = std::mem::take(&mut self.dropped);
        object([
            ("events", array(events)),
            ("dropped_events", number(dropped)),
            ("resync_required", Value::Bool(dropped > 0)),
        ])
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
