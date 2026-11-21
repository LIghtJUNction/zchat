use crate::util::*;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, protocol::WebSocketConfig};
use zchat_core::{ChatError, Value, parse};

pub(crate) struct Simplex {
    url: String,
    address: SocketAddr,
    user_id: u64,
    websocket: Option<WebSocket<TcpStream>>,
    next_id: u64,
    events: EventQueue,
}

impl Simplex {
    pub fn new(config: &Value) -> Result<Self, ChatError> {
        let url = config
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("ws://127.0.0.1:5225")
            .to_owned();
        let uri: tungstenite::http::Uri = url
            .parse()
            .map_err(|_| ChatError::new("invalid_config", "invalid SimpleX WebSocket URL"))?;
        let ip = match uri.host() {
            Some("localhost" | "127.0.0.1") => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Some("::1" | "[::1]") => IpAddr::V6(Ipv6Addr::LOCALHOST),
            _ => {
                return Err(ChatError::new(
                    "invalid_config",
                    "SimpleX's unauthenticated WebSocket API is restricted to loopback",
                ));
            }
        };
        if uri.scheme_str() != Some("ws")
            || uri.authority().is_some_and(|v| v.as_str().contains('@'))
        {
            return Err(ChatError::new(
                "invalid_config",
                "SimpleX requires a ws:// loopback URL without credentials",
            ));
        }
        let port = uri.port_u16().unwrap_or(5225);
        Ok(Self {
            url,
            address: SocketAddr::new(ip, port),
            user_id: u64_param(config, "user_id", 0),
            websocket: None,
            next_id: 1,
            events: EventQueue::default(),
        })
    }

    fn connect(&mut self) -> Result<&mut WebSocket<TcpStream>, ChatError> {
        if self.websocket.is_none() {
            let stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(5))
                .map_err(|e| io_error("connect SimpleX CLI", e))?;
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .map_err(|e| io_error("configure SimpleX socket", e))?;
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .map_err(|e| io_error("configure SimpleX socket", e))?;
            let config = WebSocketConfig::default()
                .read_buffer_size(8192)
                .write_buffer_size(0)
                .max_write_buffer_size(MAX_WIRE_BYTES)
                .max_message_size(Some(MAX_WIRE_BYTES))
                .max_frame_size(Some(MAX_WIRE_BYTES));
            let (websocket, _) =
                tungstenite::client::client_with_config(self.url.as_str(), stream, Some(config))
                    .map_err(|_| {
                        ChatError::new("transport_error", "SimpleX WebSocket handshake failed")
                    })?;
            self.websocket = Some(websocket);
        }
        self.websocket
            .as_mut()
            .ok_or_else(|| ChatError::new("transport_closed", "SimpleX socket unavailable"))
    }

    fn read(&mut self, wait: Duration) -> Result<Option<Value>, ChatError> {
        let websocket = self.connect()?;
        websocket
            .get_mut()
            .set_read_timeout(Some(
                wait.clamp(Duration::from_millis(1), Duration::from_millis(100)),
            ))
            .map_err(|e| io_error("configure SimpleX poll timeout", e))?;
        match websocket.read() {
            Ok(Message::Text(text)) => parse(text.as_str()).map(Some),
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                let _ = websocket.flush();
                Ok(None)
            }
            Ok(Message::Close(_)) => {
                self.websocket = None;
                Err(ChatError::new(
                    "transport_closed",
                    "SimpleX CLI closed the connection",
                ))
            }
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(_) => {
                self.websocket = None;
                Err(ChatError::new(
                    "transport_error",
                    "SimpleX WebSocket failed",
                ))
            }
        }
    }

    fn command(&mut self, command: &str) -> Result<Value, ChatError> {
        if command.len() > MAX_WIRE_BYTES / 2 || command.contains('\0') {
            return Err(ChatError::new(
                "invalid_params",
                "SimpleX command is too large or contains NUL",
            ));
        }
        let id = self.next_id.to_string();
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| ChatError::new("capacity", "SimpleX correlation counter exhausted"))?;
        let message =
            object([("corrId", string(&id)), ("cmd", string(command))]).try_stringify()?;
        self.connect()?
            .send(Message::Text(message.into()))
            .map_err(|_| ChatError::new("transport_error", "Could not send SimpleX command"))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(value) = self.read(deadline.saturating_duration_since(Instant::now()))? {
                if text(&value, "corrId") == id {
                    let response = value_at(&value, "resp").clone();
                    if matches!(text(&response, "type"), "chatCmdError" | "chatError") {
                        let detail = text(value_at(&response, "chatError"), "type");
                        return Err(ChatError::new(
                            "upstream_error",
                            format!("SimpleX command failed ({detail})"),
                        ));
                    }
                    return Ok(response);
                }
                if value.get("corrId").and_then(Value::as_str).is_none() {
                    self.events.push(value_at(&value, "resp").clone());
                }
            }
            if Instant::now() >= deadline {
                return Err(ChatError::new(
                    "timeout",
                    "SimpleX command timed out; inspect state before retrying a mutation",
                ));
            }
        }
    }

    fn active_user(&mut self) -> Result<Value, ChatError> {
        let response = self.command("/u")?;
        let actual = response
            .get("user")
            .and_then(|v| v.get("userId"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ChatError::new(
                    "account_not_ready",
                    "Create or select a user in the SimpleX CLI first",
                )
            })?;
        if self.user_id != 0 && self.user_id != actual {
            return Err(ChatError::new(
                "account_mismatch",
                "The SimpleX CLI active user differs from the configured user_id",
            ));
        }
        self.user_id = actual;
        Ok(response)
    }

    pub fn invoke(&mut self, operation: &str, params: &Value) -> Result<Value, ChatError> {
        if operation == "capabilities" {
            return Ok(Self::capabilities());
        }
        if operation == "native.invoke" {
            let command = params.get("command").and_then(Value::as_str).or_else(|| params.get("method").and_then(Value::as_str).filter(|v| v.starts_with('/'))).ok_or_else(|| ChatError::new("invalid_params", "SimpleX native.invoke requires command containing an official CLI API command"))?;
            return self.command(command);
        }
        if operation == "events.poll" {
            let deadline = Instant::now()
                + Duration::from_millis(u64_param(params, "timeout_ms", 0).min(30_000));
            for _ in 0..MAX_EVENTS {
                if let Some(value) =
                    self.read(deadline.saturating_duration_since(Instant::now()))?
                {
                    if value.get("corrId").and_then(Value::as_str).is_none() {
                        self.events.push(value_at(&value, "resp").clone());
                    }
                } else if !self.events.is_empty() || Instant::now() >= deadline {
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            return Ok(self.events.drain());
        }
        if operation == "account.status" {
            return self.active_user();
        }
        self.active_user()?;
        match operation {
            "conversation.list" => {
                if !text(params, "cursor").is_empty() {
                    return Err(ChatError::new(
                        "unsupported_operation",
                        "This SimpleX API preview adapter supports a bounded latest-chat window; a conversation-list cursor is not implemented",
                    ));
                }
                let limit = u64_param(params, "limit", 100).clamp(1, 200);
                let query = if let Some(query) = params.get("query").and_then(Value::as_str) {
                    object([("type", string("search")), ("search", string(query))])
                } else {
                    object([
                        ("type", string("filters")),
                        ("favorite", Value::Bool(false)),
                        ("unread", Value::Bool(false)),
                    ])
                };
                let raw = self.command(&format!(
                    "/_get chats {} count={limit} {}",
                    self.user_id,
                    query.try_stringify()?
                ))?;
                let conversations = list(value_at(&raw, "chats"))
                    .iter()
                    .filter_map(|chat| {
                        let (id, title) = chat_identity(value_at(chat, "chatInfo"))?;
                        Some(object([
                            ("id", string(id)),
                            ("title", string(title)),
                            (
                                "unread",
                                value_at(value_at(chat, "chatStats"), "unreadCount").clone(),
                            ),
                            ("raw", chat.clone()),
                        ]))
                    })
                    .collect::<Vec<_>>();
                Ok(object([
                    ("conversations", array(conversations)),
                    ("cursor", Value::Null),
                    ("window", string("latest")),
                ]))
            }
            "message.list" => {
                let room = chat_ref(params)?;
                let limit = u64_param(params, "limit", 50).clamp(1, 200);
                let page = if let Some(cursor) = params
                    .get("cursor")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                {
                    cursor.parse::<u64>().map_err(|_| {
                        ChatError::new("invalid_params", "SimpleX cursor must be a message ID")
                    })?;
                    format!("before={cursor} count={limit}")
                } else {
                    format!("count={limit}")
                };
                let raw = self.command(&format!("/_get chat {room} {page}"))?;
                let messages = list(value_at(value_at(&raw, "chat"), "chatItems"))
                    .iter()
                    .map(|item| normalize_message(&room, item))
                    .collect::<Vec<_>>();
                let cursor = messages
                    .first()
                    .and_then(|v| v.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(object([
                    ("messages", array(messages)),
                    ("cursor", cursor),
                    ("order", string("oldest_first")),
                ]))
            }
            "message.send" => {
                let room = chat_ref(params)?;
                let mut message = object([
                    (
                        "msgContent",
                        object([
                            ("type", string("text")),
                            ("text", string(required(params, "text")?)),
                        ]),
                    ),
                    ("mentions", empty()),
                ]);
                if params.get("reply_to").is_some() {
                    set(
                        &mut message,
                        "quotedItemId",
                        integer_id(params, "reply_to")?,
                    );
                }
                let raw = self.command(&format!(
                    "/_send {room} json {}",
                    array([message]).try_stringify()?
                ))?;
                let item = list(value_at(&raw, "chatItems"))
                    .first()
                    .map(|v| value_at(v, "chatItem"))
                    .unwrap_or(&Value::Null);
                let id = value_at(value_at(item, "meta"), "itemId");
                Ok(object([
                    ("id", scalar_id(id)),
                    ("conversation_id", string(room)),
                    ("status", string("queued")),
                    ("raw", raw),
                ]))
            }
            "message.edit" => {
                let room = chat_ref(params)?;
                let id = integer_id(params, "message_id")?;
                let body = object([
                    (
                        "msgContent",
                        object([
                            ("type", string("text")),
                            ("text", string(required(params, "text")?)),
                        ]),
                    ),
                    ("mentions", empty()),
                ]);
                self.command(&format!(
                    "/_update item {room} {} json {}",
                    id.try_stringify()?,
                    body.try_stringify()?
                ))
            }
            "message.delete" => {
                let room = chat_ref(params)?;
                self.command(&format!(
                    "/_delete item {room} {} {}",
                    integer_id(params, "message_id")?.try_stringify()?,
                    if bool_param(params, "for_everyone", false) {
                        "broadcast"
                    } else {
                        "internal"
                    }
                ))
            }
            "message.read" => {
                let room = chat_ref(params)?;
                self.command(&format!(
                    "/_read chat items {room} {}",
                    integer_id(params, "message_id")?.try_stringify()?
                ))
            }
            "message.react" => {
                let room = chat_ref(params)?;
                let reaction = object([
                    ("type", string("emoji")),
                    ("emoji", string(required(params, "reaction")?)),
                ]);
                self.command(&format!(
                    "/_reaction {room} {} {} {}",
                    integer_id(params, "message_id")?.try_stringify()?,
                    if bool_param(params, "remove", false) {
                        "off"
                    } else {
                        "on"
                    },
                    reaction.try_stringify()?
                ))
            }
            _ => Err(unsupported("simplex", operation)),
        }
    }

    pub fn capabilities() -> Value {
        capability(
            "simplex",
            &[
                "account.status",
                "conversation.list",
                "message.list",
                "message.send",
                "message.edit",
                "message.delete",
                "message.read",
                "message.react",
                "events.poll",
                "native.invoke",
            ],
            "managed_by_simplex_core",
            &[
                "Requires a current SimpleX CLI running its localhost WebSocket API.",
                "Use a dedicated CLI database/profile for zchat; native commands can access the CLI's account state.",
                "Conversation IDs are official @contact_id, #group_id or *local_id references.",
                "Calls and file-specific commands are accessible through native.invoke; zchat does not claim a media engine.",
            ],
        )
    }
}

fn chat_ref(params: &Value) -> Result<String, ChatError> {
    let value = required(params, "conversation_id")?;
    let bytes = value.as_bytes();
    if bytes.len() < 2
        || !matches!(bytes[0], b'@' | b'#' | b'*')
        || !bytes[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(ChatError::new(
            "invalid_params",
            "SimpleX conversation_id must be @number, #number, or *number",
        ));
    }
    Ok(value.to_owned())
}

fn chat_identity(info: &Value) -> Option<(String, String)> {
    let (prefix, record, id_key) = match text(info, "type") {
        "direct" => ("@", value_at(info, "contact"), "contactId"),
        "group" => ("#", value_at(info, "groupInfo"), "groupId"),
        "local" => ("*", value_at(info, "noteFolder"), "noteFolderId"),
        _ => return None,
    };
    let id = record.get(id_key)?.as_u64()?;
    let name = record
        .get("localDisplayName")
        .and_then(Value::as_str)
        .or_else(|| {
            record
                .get("groupProfile")
                .and_then(|v| v.get("displayName"))
                .and_then(Value::as_str)
        })
        .unwrap_or("Notes");
    Some((format!("{prefix}{id}"), name.to_owned()))
}

fn normalize_message(room: &str, item: &Value) -> Value {
    let meta = value_at(item, "meta");
    let content = item
        .get("content")
        .and_then(|v| v.get("msgContent"))
        .unwrap_or(&Value::Null);
    object([
        ("id", scalar_id(value_at(meta, "itemId"))),
        ("conversation_id", string(room)),
        (
            "sender",
            item.get("chatDir")
                .and_then(|v| v.get("groupMember"))
                .and_then(|v| v.get("localDisplayName"))
                .cloned()
                .unwrap_or_else(|| string(text(value_at(item, "chatDir"), "type"))),
        ),
        (
            "text",
            meta.get("itemText")
                .or_else(|| content.get("text"))
                .cloned()
                .unwrap_or(Value::Null),
        ),
        (
            "outgoing",
            Value::Bool(matches!(
                text(value_at(item, "chatDir"), "type"),
                "directSnd" | "groupSnd" | "localSnd"
            )),
        ),
        ("timestamp", value_at(meta, "itemTs").clone()),
        ("raw", item.clone()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    #[test]
    fn rejects_public_unauthenticated_endpoints_and_command_injection() {
        assert!(Simplex::new(&object([("url", string("ws://example.org:5225"))])).is_err());
        assert!(chat_ref(&object([("conversation_id", string("@1 /_delete user 2"))])).is_err());
        assert_eq!(
            chat_ref(&object([("conversation_id", string("#42"))])).unwrap(),
            "#42"
        );
    }

    #[test]
    fn websocket_routes_events_while_matching_command() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut websocket = tungstenite::accept(stream).unwrap();
            let request = websocket.read().unwrap().into_text().unwrap();
            let request = parse(&request).unwrap();
            websocket
                .send(Message::Text(
                    object([("resp", object([("type", string("newChatItems"))]))])
                        .stringify()
                        .into(),
                ))
                .unwrap();
            websocket
                .send(Message::Text(
                    object([
                        ("corrId", value_at(&request, "corrId").clone()),
                        (
                            "resp",
                            object([
                                ("type", string("activeUser")),
                                ("user", object([("userId", number(7))])),
                            ]),
                        ),
                    ])
                    .stringify()
                    .into(),
                ))
                .unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let mut simplex = Simplex::new(&object([
            ("url", string(format!("ws://{address}"))),
            ("user_id", number(7)),
        ]))
        .unwrap();
        assert_eq!(
            text(&simplex.invoke("account.status", &empty()).unwrap(), "type"),
            "activeUser"
        );
        assert!(!simplex.events.is_empty());
        server.join().unwrap();
    }
}
