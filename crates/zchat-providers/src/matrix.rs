use crate::{http, util::*};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zchat_core::{ChatError, Value};

pub(crate) struct Matrix {
    base: String,
    token: String,
    cursor: String,
    names: BTreeMap<String, String>,
    user_id: Option<String>,
}

impl Matrix {
    pub fn new(config: &Value) -> Result<Self, ChatError> {
        let base = required(config, "homeserver")?
            .trim_end_matches('/')
            .to_owned();
        http::validate_url(&base)?;
        Ok(Self {
            base,
            token: text(config, "access_token").to_owned(),
            cursor: text(config, "cursor").to_owned(),
            names: BTreeMap::new(),
            user_id: None,
        })
    }

    fn raw(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<http::HttpResponse, ChatError> {
        if !path.starts_with("/_matrix/") || path.contains(['\r', '\n', '#']) {
            return Err(ChatError::new(
                "invalid_params",
                "Matrix paths must start with /_matrix/ and contain no fragment or control characters",
            ));
        }
        let body = body.map(Value::try_stringify).transpose()?;
        let authorization = format!("Bearer {}", self.token);
        let mut headers = vec![("Content-Type", "application/json")];
        if !self.token.is_empty() {
            headers.push(("Authorization", authorization.as_str()));
        }
        http::request(
            method,
            &format!("{}{path}", self.base),
            &headers,
            body.as_deref(),
            timeout,
        )
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ChatError> {
        self.raw(method, path, body, Duration::from_secs(30))?
            .json()
    }

    fn plaintext_allowed(&self, room: &str) -> Result<(), ChatError> {
        let response = self.raw(
            "GET",
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.encryption/",
                percent_encode(room)
            ),
            None,
            Duration::from_secs(20),
        )?;
        if response.status == 404 {
            let value = zchat_core::parse(&response.body)?;
            if text(&value, "errcode") == "M_NOT_FOUND" {
                return Ok(());
            }
        }
        if (200..300).contains(&response.status) {
            return Err(ChatError::new(
                "encryption_required",
                "This room requires Matrix end-to-end encryption; this HTTP adapter does not implement a crypto device. Use a crypto-capable Matrix plugin.",
            ));
        }
        response.json()?;
        Err(ChatError::new(
            "encryption_state_unknown",
            "Cannot establish the room encryption state",
        ))
    }

    fn native(&self, params: &Value) -> Result<Value, ChatError> {
        let method = required(params, "method")?;
        let path = required(params, "path")?;
        // A raw API path must not accidentally bypass the plaintext send guard.
        let decoded = percent_decode(path.split('?').next().unwrap_or(path))?;
        let parts: Vec<&str> = decoded.split('/').collect();
        if let Some(index) = parts.iter().position(|part| *part == "send")
            && index >= 2
            && parts.get(index - 2) == Some(&"rooms")
            && parts.get(index + 1) != Some(&"m.room.encrypted")
        {
            self.plaintext_allowed(parts[index - 1])?;
        }
        self.call(method, path, params.get("body"))
    }

    pub fn invoke(&mut self, operation: &str, params: &Value) -> Result<Value, ChatError> {
        match operation {
            "account.status" | "account.whoami" => {
                let response = self.call("GET", "/_matrix/client/v3/account/whoami", None)?;
                self.user_id = response
                    .get("user_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Ok(response)
            }
            "server.info" => self.call("GET", "/_matrix/client/versions", None),
            "capabilities" => Ok(Self::capabilities()),
            "native.invoke" => self.native(params),
            "conversation.list" => {
                let joined = self.call("GET", "/_matrix/client/v3/joined_rooms", None)?;
                let mut ids = list(value_at(&joined, "joined_rooms"))
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                ids.sort_unstable();
                let skip = page_offset(params, 0)?.min(ids.len());
                let limit = u64_param(params, "limit", 50).clamp(1, 200) as usize;
                let selected = ids
                    .iter()
                    .skip(skip)
                    .take(limit)
                    .copied()
                    .collect::<Vec<_>>();
                if selected.is_empty() {
                    return Ok(object([
                        ("conversations", array([])),
                        ("cursor", Value::Null),
                    ]));
                }
                // Constrain the sync to this page so accounts with many rooms
                // do not expand an unbounded state response for every page.
                let filter = object([
                    (
                        "room",
                        object([
                            ("rooms", array(selected.iter().map(|id| string(*id)))),
                            ("timeline", object([("limit", number(0))])),
                            (
                                "state",
                                object([(
                                    "types",
                                    array([
                                        string("m.room.name"),
                                        string("m.room.canonical_alias"),
                                    ]),
                                )]),
                            ),
                            ("ephemeral", object([("types", array([]))])),
                            ("account_data", object([("types", array([]))])),
                        ]),
                    ),
                    ("presence", object([("types", array([]))])),
                    ("account_data", object([("types", array([]))])),
                ]);
                let raw = self.call(
                    "GET",
                    &format!(
                        "/_matrix/client/v3/sync?timeout=0&filter={}",
                        percent_encode(&filter.try_stringify()?)
                    ),
                    None,
                )?;
                self.remember_names(&raw);
                let mut conversations = Vec::new();
                for id in &selected {
                    let room = raw
                        .get("rooms")
                        .and_then(|v| v.get("join"))
                        .and_then(|v| v.get(id))
                        .unwrap_or(&Value::Null);
                    conversations.push(object([
                        ("id", string(*id)),
                        (
                            "title",
                            string(self.names.get(*id).map(String::as_str).unwrap_or(id)),
                        ),
                        (
                            "unread",
                            room.get("unread_notifications")
                                .and_then(|v| v.get("notification_count"))
                                .cloned()
                                .unwrap_or(number(0)),
                        ),
                    ]));
                }
                let next = skip + selected.len();
                Ok(object([
                    ("conversations", array(conversations)),
                    (
                        "cursor",
                        if next < ids.len() {
                            string(next.to_string())
                        } else {
                            Value::Null
                        },
                    ),
                ]))
            }
            "conversation.join" => {
                let room = identifier(params, "conversation_id")?;
                self.call(
                    "POST",
                    &format!("/_matrix/client/v3/join/{}", percent_encode(&room)),
                    Some(&empty()),
                )
            }
            "conversation.leave" => {
                let room = identifier(params, "conversation_id")?;
                self.call(
                    "POST",
                    &format!("/_matrix/client/v3/rooms/{}/leave", percent_encode(&room)),
                    Some(&empty()),
                )
            }
            "conversation.create" => {
                self.call("POST", "/_matrix/client/v3/createRoom", Some(params))
            }
            "message.list" => {
                let room = identifier(params, "conversation_id")?;
                let limit = u64_param(params, "limit", 50).clamp(1, 200);
                let mut path = format!(
                    "/_matrix/client/v3/rooms/{}/messages?dir=b&limit={limit}",
                    percent_encode(&room)
                );
                if !text(params, "cursor").is_empty() {
                    path.push_str(&format!("&from={}", percent_encode(text(params, "cursor"))));
                }
                let raw = self.call("GET", &path, None)?;
                let messages = list(value_at(&raw, "chunk"))
                    .iter()
                    .map(|event| self.normalize_message(&room, event))
                    .collect::<Vec<_>>();
                Ok(object([
                    ("messages", array(messages)),
                    ("cursor", value_at(&raw, "end").clone()),
                    ("order", string("newest_first")),
                ]))
            }
            "message.send" | "message.edit" => {
                let room = identifier(params, "conversation_id")?;
                self.plaintext_allowed(&room)?;
                let content = object([
                    ("msgtype", string("m.text")),
                    ("body", string(required(params, "text")?)),
                ]);
                let mut body = content.clone();
                if operation == "message.edit" {
                    set(&mut body, "m.new_content", content);
                    set(
                        &mut body,
                        "body",
                        string(format!("* {}", text(params, "text"))),
                    );
                    set(
                        &mut body,
                        "m.relates_to",
                        object([
                            ("rel_type", string("m.replace")),
                            ("event_id", string(identifier(params, "message_id")?)),
                        ]),
                    );
                } else if let Some(reply) = params.get("reply_to").and_then(Value::as_str) {
                    set(
                        &mut body,
                        "m.relates_to",
                        object([("m.in_reply_to", object([("event_id", string(reply))]))]),
                    );
                }
                let txn = transaction(params);
                let raw = self.call(
                    "PUT",
                    &format!(
                        "/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
                        percent_encode(&room),
                        percent_encode(&txn)
                    ),
                    Some(&body),
                )?;
                Ok(object([
                    ("id", value_at(&raw, "event_id").clone()),
                    ("conversation_id", string(room)),
                    ("status", string("accepted")),
                    ("raw", raw),
                ]))
            }
            "message.delete" => {
                let room = identifier(params, "conversation_id")?;
                let event = identifier(params, "message_id")?;
                self.call(
                    "PUT",
                    &format!(
                        "/_matrix/client/v3/rooms/{}/redact/{}/{}",
                        percent_encode(&room),
                        percent_encode(&event),
                        percent_encode(&transaction(params))
                    ),
                    Some(&object([("reason", string(text(params, "reason")))])),
                )
            }
            "message.react" => {
                let room = identifier(params, "conversation_id")?;
                let body = object([(
                    "m.relates_to",
                    object([
                        ("rel_type", string("m.annotation")),
                        ("event_id", string(identifier(params, "message_id")?)),
                        ("key", string(required(params, "reaction")?)),
                    ]),
                )]);
                self.call(
                    "PUT",
                    &format!(
                        "/_matrix/client/v3/rooms/{}/send/m.reaction/{}",
                        percent_encode(&room),
                        percent_encode(&transaction(params))
                    ),
                    Some(&body),
                )
            }
            "message.read" => {
                let room = identifier(params, "conversation_id")?;
                let event = identifier(params, "message_id")?;
                self.call(
                    "POST",
                    &format!(
                        "/_matrix/client/v3/rooms/{}/receipt/m.read/{}",
                        percent_encode(&room),
                        percent_encode(&event)
                    ),
                    Some(&empty()),
                )
            }
            "typing.set" => {
                let room = identifier(params, "conversation_id")?;
                let user = if let Some(user) = params.get("user_id").and_then(Value::as_str) {
                    user.to_owned()
                } else {
                    identifier(
                        &self.call("GET", "/_matrix/client/v3/account/whoami", None)?,
                        "user_id",
                    )?
                };
                self.call(
                    "PUT",
                    &format!(
                        "/_matrix/client/v3/rooms/{}/typing/{}",
                        percent_encode(&room),
                        percent_encode(&user)
                    ),
                    Some(&object([
                        ("typing", Value::Bool(bool_param(params, "typing", true))),
                        (
                            "timeout",
                            number(u64_param(params, "timeout_ms", 10_000).min(30_000)),
                        ),
                    ])),
                )
            }
            "events.poll" => {
                let timeout = u64_param(params, "timeout_ms", 0).min(30_000);
                let cursor = params
                    .get("cursor")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.cursor);
                let mut path = format!("/_matrix/client/v3/sync?timeout={timeout}");
                if !cursor.is_empty() {
                    path.push_str(&format!("&since={}", percent_encode(cursor)));
                }
                let raw = self
                    .raw("GET", &path, None, Duration::from_millis(timeout + 15_000))?
                    .json()?;
                self.remember_names(&raw);
                self.cursor = text(&raw, "next_batch").to_owned();
                let mut events = EventQueue::default();
                let mut limited = false;
                if let Some(rooms) = raw
                    .get("rooms")
                    .and_then(|v| v.get("join"))
                    .and_then(Value::as_object)
                {
                    for (room_id, room) in rooms {
                        limited |= room
                            .get("timeline")
                            .and_then(|v| v.get("limited"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        for event in list(
                            room.get("timeline")
                                .and_then(|v| v.get("events"))
                                .unwrap_or(&Value::Null),
                        ) {
                            let mut normalized = self.normalize_message(room_id, event);
                            if !bool_param(params, "include_raw", false) {
                                crate::remove_raw(&mut normalized);
                            }
                            events.push(object([
                                ("kind", string(text(event, "type"))),
                                ("conversation_id", string(room_id)),
                                ("message", normalized),
                            ]));
                        }
                    }
                }
                let mut output = events.drain();
                set(&mut output, "cursor", string(&self.cursor));
                if limited {
                    set(&mut output, "resync_required", Value::Bool(true));
                }
                Ok(output)
            }
            _ => Err(unsupported("matrix", operation)),
        }
    }

    fn remember_names(&mut self, sync: &Value) {
        if let Some(rooms) = sync
            .get("rooms")
            .and_then(|v| v.get("join"))
            .and_then(Value::as_object)
        {
            for (room_id, room) in rooms {
                for event in list(
                    room.get("state")
                        .and_then(|v| v.get("events"))
                        .unwrap_or(&Value::Null),
                ) {
                    let name = match text(event, "type") {
                        "m.room.name" => text(value_at(event, "content"), "name"),
                        "m.room.canonical_alias" => text(value_at(event, "content"), "alias"),
                        _ => "",
                    };
                    if !name.is_empty()
                        && (text(event, "type") == "m.room.name"
                            || !self.names.contains_key(room_id))
                        && (self.names.len() < 8192 || self.names.contains_key(room_id))
                    {
                        self.names.insert(room_id.clone(), name.to_owned());
                    }
                }
            }
        }
    }

    fn normalize_message(&self, room: &str, event: &Value) -> Value {
        let mut message = normalize_message(room, event);
        if let Some(user) = &self.user_id {
            set(
                &mut message,
                "outgoing",
                Value::Bool(text(event, "sender") == user),
            );
        }
        message
    }

    pub fn capabilities() -> Value {
        capability(
            "matrix",
            &[
                "account.status",
                "server.info",
                "conversation.list",
                "conversation.join",
                "conversation.leave",
                "conversation.create",
                "message.list",
                "message.send",
                "message.edit",
                "message.delete",
                "message.react",
                "message.read",
                "typing.set",
                "events.poll",
                "native.invoke",
            ],
            "plaintext_rooms_only",
            &[
                "Encrypted rooms are read as encrypted events; plaintext sends into encrypted rooms are rejected.",
                "Native methods use the Matrix Client-Server API; media upload and calls need a separate crypto/media-capable plugin.",
                "Normalized events cover joined-room timelines; limited timelines require message.list resynchronization. Use native /sync for other event streams.",
            ],
        )
    }
}

fn transaction(params: &Value) -> String {
    if let Some(value) = params.get("idempotency_key").and_then(Value::as_str) {
        return value.to_owned();
    }
    if let Some(value) = params.get("transaction_id").and_then(Value::as_str) {
        return value.to_owned();
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("zchat-{time}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn normalize_message(room: &str, event: &Value) -> Value {
    let content = value_at(event, "content");
    object([
        ("id", value_at(event, "event_id").clone()),
        ("conversation_id", string(room)),
        ("sender", value_at(event, "sender").clone()),
        ("text", value_at(content, "body").clone()),
        ("timestamp_ms", value_at(event, "origin_server_ts").clone()),
        (
            "encrypted",
            Value::Bool(text(event, "type") == "m.room.encrypted"),
        ),
        ("type", value_at(event, "type").clone()),
        ("raw", event.clone()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_normalization_preserves_encryption_and_raw() {
        let event = object([
            ("event_id", string("$one")),
            ("type", string("m.room.encrypted")),
            ("sender", string("@a:test")),
            ("content", object([("ciphertext", string("opaque"))])),
        ]);
        let normalized = normalize_message("!room:test", &event);
        assert_eq!(
            normalized.get("encrypted").and_then(Value::as_bool),
            Some(true)
        );
        assert!(matches!(normalized.get("text"), Some(Value::Null)));
        assert_eq!(normalized.get("raw"), Some(&event));
    }

    #[test]
    fn transaction_id_is_stable_when_supplied() {
        assert_eq!(
            transaction(&object([
                ("idempotency_key", string("across-restarts")),
                ("transaction_id", string("legacy"))
            ])),
            "across-restarts"
        );
        assert_eq!(
            transaction(&object([("transaction_id", string("retry-me"))])),
            "retry-me"
        );
        assert_ne!(transaction(&empty()), transaction(&empty()));
    }
}
