//! TDLib's current C JSON interface. One process-wide receive mutex is required
//! by TDLib and routes updates for every account without stealing other clients'
//! messages. Native code remains pinned for TDLib's worker-thread lifetime.
use crate::util::*;
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, c_char},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use zchat_core::{ChatError, Value, parse};
use zsys::dylib::Library;

type Create = unsafe extern "C" fn() -> i32;
type Send = unsafe extern "C" fn(i32, *const c_char);
type Receive = unsafe extern "C" fn(f64) -> *const c_char;

struct TdApi {
    create: Create,
    send: Send,
    receive: Receive,
}
struct ClientState {
    events: EventQueue,
    authorization: Value,
    chats: BTreeMap<String, Value>,
}
struct Runtime {
    api: TdApi,
    library: String,
    clients: BTreeMap<i32, ClientState>,
    next_request: u64,
}
static RUNTIME: OnceLock<Mutex<Option<Runtime>>> = OnceLock::new();

pub(crate) struct Telegram {
    config: Value,
    client: Option<i32>,
}

impl Telegram {
    pub fn new(config: &Value) -> Result<Self, ChatError> {
        if u64_param(config, "api_id", 0) == 0 || u64_param(config, "api_id", 0) > i32::MAX as u64 {
            return Err(ChatError::new(
                "invalid_config",
                "Telegram needs your application's positive api_id",
            ));
        }
        required(config, "api_hash")?;
        required(config, "database_directory")?;
        Ok(Self {
            config: config.clone(),
            client: None,
        })
    }

    pub fn invoke(&mut self, operation: &str, params: &Value) -> Result<Value, ChatError> {
        if operation == "capabilities" {
            return Ok(Self::capabilities());
        }
        let library = self
            .config
            .get("library_path")
            .and_then(Value::as_str)
            .unwrap_or(default_library());
        let mut guard = RUNTIME
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| {
                ChatError::new("runtime_poisoned", "TDLib receive runtime was poisoned")
            })?;
        if guard.is_none() {
            *guard = Some(Runtime::load(library)?);
        }
        let runtime = guard
            .as_mut()
            .ok_or_else(|| ChatError::new("backend_unavailable", "TDLib runtime unavailable"))?;
        if runtime.library != library {
            return Err(ChatError::new(
                "invalid_config",
                "All Telegram accounts in one process must use the same TDLib library_path",
            ));
        }
        let client = match self.client {
            Some(client) => client,
            None => {
                let client = runtime.create()?;
                self.client = Some(client);
                runtime.initialize(client, &self.config)?;
                client
            }
        };
        match operation {
            "account.status" | "account.login" => runtime.authorization_status(
                client,
                &self.config,
                params,
                operation == "account.login",
            ),
            "native.invoke" => {
                let mut request = if let Some(request) = params.get("request") {
                    request.clone()
                } else {
                    let method = required(params, "method")?;
                    let mut request = params
                        .get("params")
                        .cloned()
                        .unwrap_or_else(|| params.clone());
                    if let Value::Object(map) = &mut request {
                        map.remove("method");
                    }
                    set(&mut request, "@type", string(method));
                    request
                };
                required(&request, "@type")?;
                if let Value::Object(map) = &mut request {
                    map.remove("@client_id");
                    map.remove("@extra");
                }
                runtime.call(client, request)
            }
            "conversation.list" => {
                let limit = u64_param(params, "limit", 50).clamp(1, 200) as usize;
                let skip = page_offset(params, 0)?;
                if skip >= 8192 {
                    return Err(ChatError::new(
                        "capacity",
                        "Telegram list cursor exceeds the 8192-chat normalized window; use native TDLib chat-list updates",
                    ));
                }
                let wanted = (skip + limit + 1).min(8192);
                let chat_list = object([("@type", string("chatListMain"))]);
                let loaded = runtime.request(
                    client,
                    object([
                        ("@type", string("loadChats")),
                        ("chat_list", chat_list.clone()),
                        ("limit", number(wanted)),
                    ]),
                    Duration::from_secs(20),
                )?;
                if text(&loaded, "@type") == "error"
                    && loaded.get("code").and_then(Value::as_u64) != Some(404)
                {
                    td_result(loaded)?;
                }
                let raw = runtime.call(
                    client,
                    object([
                        ("@type", string("getChats")),
                        ("chat_list", chat_list),
                        ("limit", number(wanted)),
                    ]),
                )?;
                let mut conversations = Vec::new();
                let ids = list(value_at(&raw, "chat_ids"));
                for id in ids.iter().skip(skip).take(limit) {
                    let key = scalar_string(id);
                    let cached = runtime
                        .clients
                        .get(&client)
                        .and_then(|state| state.chats.get(&key))
                        .cloned();
                    let chat = match cached {
                        Some(chat) => chat,
                        None => runtime.call(
                            client,
                            object([("@type", string("getChat")), ("chat_id", id.clone())]),
                        )?,
                    };
                    conversations.push(object([
                        ("id", string(key)),
                        ("title", value_at(&chat, "title").clone()),
                        ("unread", value_at(&chat, "unread_count").clone()),
                        ("raw", chat),
                    ]));
                }
                let next = skip + conversations.len();
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
            "message.list" => {
                let room = integer_id(params, "conversation_id")?;
                let cursor = text(params, "cursor");
                let from = if cursor.is_empty() {
                    number(0)
                } else {
                    cursor.parse::<i64>().map_err(|_| {
                        ChatError::new("invalid_params", "Telegram cursor must be a message ID")
                    })?;
                    Value::Number(cursor.to_owned())
                };
                let raw = runtime.call(
                    client,
                    object([
                        ("@type", string("getChatHistory")),
                        ("chat_id", room.clone()),
                        ("from_message_id", from),
                        ("offset", number(0)),
                        (
                            "limit",
                            number(u64_param(params, "limit", 50).clamp(1, 100)),
                        ),
                        ("only_local", Value::Bool(false)),
                    ]),
                )?;
                let messages = list(value_at(&raw, "messages"))
                    .iter()
                    .map(normalize_message)
                    .collect::<Vec<_>>();
                let cursor = messages
                    .last()
                    .and_then(|m| m.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let chat = runtime.call(
                    client,
                    object([("@type", string("getChat")), ("chat_id", room.clone())]),
                )?;
                let channel = value_at(&chat, "type")
                    .get("is_channel")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let sponsored = if channel {
                    runtime.call(
                        client,
                        object([
                            ("@type", string("getChatSponsoredMessages")),
                            ("chat_id", room),
                        ]),
                    )?
                } else {
                    Value::Null
                };
                Ok(object([
                    ("messages", array(messages)),
                    ("cursor", cursor),
                    ("order", string("newest_first")),
                    ("sponsored_messages", sponsored),
                ]))
            }
            "message.send" => {
                let room = integer_id(params, "conversation_id")?;
                let input = object([
                    ("@type", string("inputMessageText")),
                    (
                        "text",
                        object([
                            ("@type", string("formattedText")),
                            ("text", string(required(params, "text")?)),
                            ("entities", array([])),
                        ]),
                    ),
                    ("clear_draft", Value::Bool(false)),
                ]);
                let mut request = object([
                    ("@type", string("sendMessage")),
                    ("chat_id", room),
                    ("input_message_content", input),
                ]);
                if params.get("reply_to").is_some() {
                    set(
                        &mut request,
                        "reply_to",
                        object([
                            ("@type", string("inputMessageReplyToMessage")),
                            ("message_id", integer_id(params, "reply_to")?),
                        ]),
                    );
                }
                let raw = runtime.call(client, request)?;
                let mut normalized = normalize_message(&raw);
                set(&mut normalized, "status", string("queued"));
                Ok(normalized)
            }
            "message.edit" => runtime.call(
                client,
                object([
                    ("@type", string("editMessageText")),
                    ("chat_id", integer_id(params, "conversation_id")?),
                    ("message_id", integer_id(params, "message_id")?),
                    (
                        "input_message_content",
                        object([
                            ("@type", string("inputMessageText")),
                            (
                                "text",
                                object([
                                    ("@type", string("formattedText")),
                                    ("text", string(required(params, "text")?)),
                                    ("entities", array([])),
                                ]),
                            ),
                        ]),
                    ),
                ]),
            ),
            "message.delete" => runtime.call(
                client,
                object([
                    ("@type", string("deleteMessages")),
                    ("chat_id", integer_id(params, "conversation_id")?),
                    ("message_ids", array([integer_id(params, "message_id")?])),
                    (
                        "revoke",
                        Value::Bool(bool_param(params, "for_everyone", false)),
                    ),
                ]),
            ),
            "message.read" => runtime.call(
                client,
                object([
                    ("@type", string("viewMessages")),
                    ("chat_id", integer_id(params, "conversation_id")?),
                    ("message_ids", array([integer_id(params, "message_id")?])),
                    (
                        "source",
                        object([("@type", string("messageSourceChatHistory"))]),
                    ),
                    ("force_read", Value::Bool(true)),
                ]),
            ),
            "message.react" => runtime.call(
                client,
                object([
                    (
                        "@type",
                        string(if bool_param(params, "remove", false) {
                            "removeMessageReaction"
                        } else {
                            "addMessageReaction"
                        }),
                    ),
                    ("chat_id", integer_id(params, "conversation_id")?),
                    ("message_id", integer_id(params, "message_id")?),
                    (
                        "reaction_type",
                        object([
                            ("@type", string("reactionTypeEmoji")),
                            ("emoji", string(required(params, "reaction")?)),
                        ]),
                    ),
                ]),
            ),
            "typing.set" => runtime.call(
                client,
                object([
                    ("@type", string("sendChatAction")),
                    ("chat_id", integer_id(params, "conversation_id")?),
                    (
                        "action",
                        object([(
                            "@type",
                            string(if bool_param(params, "typing", true) {
                                "chatActionTyping"
                            } else {
                                "chatActionCancel"
                            }),
                        )]),
                    ),
                ]),
            ),
            "events.poll" => {
                let deadline = Instant::now()
                    + Duration::from_millis(u64_param(params, "timeout_ms", 0).min(30_000));
                for _ in 0..MAX_EVENTS {
                    let wait = deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(100));
                    if let Some(update) = runtime.receive(wait)? {
                        runtime.route(update);
                    } else if Instant::now() >= deadline {
                        break;
                    }
                    if runtime
                        .clients
                        .get(&client)
                        .is_some_and(|state| !state.events.is_empty())
                        || Instant::now() >= deadline
                    {
                        break;
                    }
                }
                Ok(runtime
                    .clients
                    .get_mut(&client)
                    .ok_or_else(|| ChatError::new("transport_closed", "TDLib client unavailable"))?
                    .events
                    .drain())
            }
            _ => Err(unsupported("telegram", operation)),
        }
    }

    pub fn capabilities() -> Value {
        let mut capabilities = capability(
            "telegram",
            &[
                "account.status",
                "account.login",
                "conversation.list",
                "message.list",
                "message.send",
                "message.edit",
                "message.delete",
                "message.read",
                "message.react",
                "typing.set",
                "events.poll",
                "native.invoke",
            ],
            "managed_by_tdlib",
            &[
                "Requires official TDLib and an independently registered api_id/api_hash.",
                "AI content access is disabled by default in accordance with Telegram API terms; human UI/CLI access remains available.",
                "Channel history includes sponsored_messages; the caller must render and report views through TDLib.",
                "Calls and platform-specific features use TDLib native methods; a call media engine is not included.",
                "account.status never requests an authentication code; explicit account.login advances phone/code/password challenges.",
            ],
        );
        set(&mut capabilities, "ai_content_access", Value::Bool(false));
        capabilities
    }
}

impl Drop for Telegram {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let Some(mutex) = RUNTIME.get() else {
            return;
        };
        if let Ok(mut guard) = mutex.lock()
            && let Some(runtime) = guard.as_mut()
        {
            let _ = runtime.request(
                client,
                object([("@type", string("close"))]),
                Duration::from_secs(1),
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if runtime.clients.get(&client).is_some_and(|state| {
                    text(&state.authorization, "@type") == "authorizationStateClosed"
                }) {
                    break;
                }
                match runtime.receive(Duration::from_millis(50)) {
                    Ok(Some(update)) => runtime.route(update),
                    Ok(None) => (),
                    Err(_) => break,
                }
            }
            runtime.clients.remove(&client);
        }
    }
}

impl Runtime {
    fn load(path: &str) -> Result<Self, ChatError> {
        let path_c = CString::new(path)
            .map_err(|_| ChatError::new("invalid_config", "TDLib library path contains NUL"))?;
        // SAFETY: the user-selected backend is trusted native code. zsys retains
        // its native reference on Drop; we intentionally never unload TDLib.
        let library = unsafe { Library::open(&path_c) }.map_err(|_| {
            ChatError::new(
                "backend_unavailable",
                "Cannot load TDLib; install the official tdjson library or set library_path",
            )
        })?;
        let symbol = |name: &'static CStr| {
            library.symbol(name).map_err(|_| {
                ChatError::new("backend_incompatible", "TDLib lacks the current C JSON API")
            })
        };
        // SAFETY: signatures match td/telegram/td_json_client.h exactly.
        let api = unsafe {
            TdApi {
                create: std::mem::transmute::<*mut std::ffi::c_void, Create>(symbol(
                    c"td_create_client_id",
                )?),
                send: std::mem::transmute::<*mut std::ffi::c_void, Send>(symbol(c"td_send")?),
                receive: std::mem::transmute::<*mut std::ffi::c_void, Receive>(symbol(
                    c"td_receive",
                )?),
            }
        };
        Ok(Self {
            api,
            library: path.to_owned(),
            clients: BTreeMap::new(),
            next_request: 1,
        })
    }

    fn create(&mut self) -> Result<i32, ChatError> {
        // SAFETY: api's image remains pinned and this is its declared C ABI.
        let client = unsafe { (self.api.create)() };
        if client <= 0 {
            return Err(ChatError::new(
                "backend_error",
                "TDLib did not allocate a client",
            ));
        }
        self.clients.insert(
            client,
            ClientState {
                events: EventQueue::default(),
                authorization: Value::Null,
                chats: BTreeMap::new(),
            },
        );
        Ok(client)
    }

    fn initialize(&mut self, client: i32, config: &Value) -> Result<(), ChatError> {
        let state = self.call(client, object([("@type", string("getAuthorizationState"))]))?;
        if text(&state, "@type") == "authorizationStateWaitTdlibParameters" {
            self.call(
                client,
                object([
                    ("@type", string("setTdlibParameters")),
                    ("use_test_dc", Value::Bool(false)),
                    (
                        "database_directory",
                        string(required(config, "database_directory")?),
                    ),
                    ("files_directory", string(text(config, "files_directory"))),
                    (
                        "database_encryption_key",
                        string(text(config, "database_encryption_key")),
                    ),
                    ("use_file_database", Value::Bool(true)),
                    ("use_chat_info_database", Value::Bool(true)),
                    ("use_message_database", Value::Bool(true)),
                    ("use_secret_chats", Value::Bool(true)),
                    ("api_id", number(u64_param(config, "api_id", 0))),
                    ("api_hash", string(required(config, "api_hash")?)),
                    ("system_language_code", string("en")),
                    ("device_model", string("zchat")),
                    ("system_version", string(std::env::consts::OS)),
                    ("application_version", string(env!("CARGO_PKG_VERSION"))),
                ]),
            )?;
        }
        Ok(())
    }

    fn call(&mut self, client: i32, request: Value) -> Result<Value, ChatError> {
        td_result(self.request(client, request, Duration::from_secs(30))?)
    }

    fn authorization_status(
        &mut self,
        client: i32,
        config: &Value,
        params: &Value,
        advance: bool,
    ) -> Result<Value, ChatError> {
        let mut state = self.call(client, object([("@type", string("getAuthorizationState"))]))?;
        if text(&state, "@type") == "authorizationStateWaitTdlibParameters" {
            self.initialize(client, config)?;
            state = self.call(client, object([("@type", string("getAuthorizationState"))]))?;
        }
        if advance {
            let command = match text(&state, "@type") {
                "authorizationStateWaitPhoneNumber" => {
                    let phone = params
                        .get("phone_number")
                        .and_then(Value::as_str)
                        .or_else(|| config.get("phone_number").and_then(Value::as_str))
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| {
                            ChatError::new("invalid_params", "account.login requires phone_number")
                        })?;
                    Some(object([
                        ("@type", string("setAuthenticationPhoneNumber")),
                        ("phone_number", string(phone)),
                        ("settings", Value::Null),
                    ]))
                }
                "authorizationStateWaitCode" => Some(object([
                    ("@type", string("checkAuthenticationCode")),
                    ("code", string(required(params, "code")?)),
                ])),
                "authorizationStateWaitPassword" => Some(object([
                    ("@type", string("checkAuthenticationPassword")),
                    ("password", string(required(params, "password")?)),
                ])),
                "authorizationStateReady" => None,
                _ => {
                    return Err(ChatError::new(
                        "account_not_ready",
                        "Use the displayed TDLib authorization state and native.invoke for this login challenge",
                    ));
                }
            };
            if let Some(command) = command {
                self.call(client, command)?;
            }
            state = self.call(client, object([("@type", string("getAuthorizationState"))]))?;
        }
        Ok(object([
            (
                "connected",
                Value::Bool(text(&state, "@type") == "authorizationStateReady"),
            ),
            ("authorization_state", state),
            ("client_id", number(client)),
        ]))
    }

    fn request(
        &mut self,
        client: i32,
        mut request: Value,
        timeout: Duration,
    ) -> Result<Value, ChatError> {
        let id = format!("zchat-{}", self.next_request);
        self.next_request = self
            .next_request
            .checked_add(1)
            .ok_or_else(|| ChatError::new("capacity", "TDLib request counter exhausted"))?;
        set(&mut request, "@extra", string(&id));
        let json = request.try_stringify()?;
        let json = CString::new(json)
            .map_err(|_| ChatError::new("invalid_params", "TDLib request contains NUL"))?;
        // SAFETY: client is owned by this runtime; TDLib copies the input before
        // returning. The process mutex serializes all receive calls.
        unsafe {
            (self.api.send)(client, json.as_ptr());
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(response) = self.receive(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100)),
            )? {
                if response.get("@client_id").and_then(Value::as_u64) == Some(client as u64)
                    && text(&response, "@extra") == id
                {
                    return Ok(response);
                }
                self.route(response);
            }
            if Instant::now() >= deadline {
                return Err(ChatError::new(
                    "timeout",
                    "TDLib request timed out; inspect state before retrying a mutation",
                ));
            }
        }
    }

    fn receive(&self, wait: Duration) -> Result<Option<Value>, ChatError> {
        // SAFETY: receive is protected by the process-wide mutex. Copy its
        // temporary string before any further TDLib receive call.
        let pointer = unsafe { (self.api.receive)(wait.as_secs_f64()) };
        if pointer.is_null() {
            return Ok(None);
        }
        let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
        if bytes.len() > MAX_WIRE_BYTES {
            return Err(ChatError::new(
                "payload_too_large",
                "TDLib response exceeds 1 MiB",
            ));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| ChatError::new("invalid_response", "TDLib returned invalid UTF-8"))?;
        parse(text).map(Some)
    }

    fn route(&mut self, response: Value) {
        let Some(id) = response.get("@client_id").and_then(Value::as_u64) else {
            return;
        };
        let Some(state) = self.clients.get_mut(&(id as i32)) else {
            return;
        };
        match text(&response, "@type") {
            "updateAuthorizationState" => {
                state.authorization = value_at(&response, "authorization_state").clone()
            }
            "updateNewChat" => {
                let chat = value_at(&response, "chat");
                if let Some(id) = chat.get("id") {
                    if state.chats.len() >= 8192 {
                        state.chats.pop_first();
                    }
                    // Keep only list metadata, not unbounded message histories.
                    state.chats.insert(
                        scalar_string(id),
                        object([
                            ("id", id.clone()),
                            ("title", value_at(chat, "title").clone()),
                            ("unread_count", value_at(chat, "unread_count").clone()),
                            ("type", value_at(chat, "type").clone()),
                        ]),
                    );
                }
            }
            "updateChatTitle" => {
                if let Some(chat) = state
                    .chats
                    .get_mut(&scalar_string(value_at(&response, "chat_id")))
                {
                    set(chat, "title", value_at(&response, "title").clone());
                }
            }
            "updateChatReadInbox" => {
                if let Some(chat) = state
                    .chats
                    .get_mut(&scalar_string(value_at(&response, "chat_id")))
                {
                    set(
                        chat,
                        "unread_count",
                        value_at(&response, "unread_count").clone(),
                    );
                }
            }
            _ => (),
        }
        if text(&response, "@type").starts_with("update") {
            let mut event = object([
                ("kind", string(text(&response, "@type"))),
                ("data", response.clone()),
            ]);
            if let Some(message) = response.get("message") {
                let mut normalized = normalize_message(message);
                crate::remove_raw(&mut normalized);
                set(&mut event, "message", normalized);
            }
            state.events.push(event);
        }
    }
}

fn td_result(value: Value) -> Result<Value, ChatError> {
    if text(&value, "@type") == "error" {
        return Err(ChatError::new(
            "upstream_error",
            format!(
                "TDLib error {}: {}",
                scalar_string(value_at(&value, "code")),
                text(&value, "message")
                    .chars()
                    .take(160)
                    .collect::<String>()
            ),
        ));
    }
    Ok(value)
}
fn scalar_string(value: &Value) -> String {
    match value {
        Value::String(v) | Value::Number(v) => v.clone(),
        _ => String::new(),
    }
}
fn normalize_message(message: &Value) -> Value {
    let content = value_at(message, "content");
    let formatted = content
        .get("text")
        .or_else(|| content.get("caption"))
        .unwrap_or(&Value::Null);
    let sender = value_at(message, "sender_id");
    object([
        ("id", string(scalar_string(value_at(message, "id")))),
        (
            "conversation_id",
            string(scalar_string(value_at(message, "chat_id"))),
        ),
        (
            "sender",
            string(scalar_string(
                sender
                    .get("user_id")
                    .or_else(|| sender.get("chat_id"))
                    .unwrap_or(&Value::Null),
            )),
        ),
        ("text", value_at(formatted, "text").clone()),
        ("timestamp", value_at(message, "date").clone()),
        (
            "outgoing",
            Value::Bool(bool_param(message, "is_outgoing", false)),
        ),
        ("type", value_at(content, "@type").clone()),
        ("raw", message.clone()),
    ])
}
fn default_library() -> &'static str {
    if cfg!(target_os = "windows") {
        "tdjson.dll"
    } else if cfg!(target_os = "macos") {
        "libtdjson.dylib"
    } else {
        "libtdjson.so"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicI32, Ordering},
    };
    struct MockTd {
        queue: VecDeque<CString>,
        last: Option<CString>,
        calls: Vec<String>,
        authorization: &'static str,
    }
    static MOCK: OnceLock<Mutex<MockTd>> = OnceLock::new();
    fn mock() -> &'static Mutex<MockTd> {
        MOCK.get_or_init(|| {
            Mutex::new(MockTd {
                queue: VecDeque::new(),
                last: None,
                calls: Vec::new(),
                authorization: "authorizationStateWaitPhoneNumber",
            })
        })
    }
    unsafe extern "C" fn mock_create() -> i32 {
        static NEXT: AtomicI32 = AtomicI32::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }
    unsafe extern "C" fn mock_send(client: i32, request: *const c_char) {
        let request = parse(unsafe { CStr::from_ptr(request) }.to_str().unwrap()).unwrap();
        let mut mock = mock().lock().unwrap();
        let method = text(&request, "@type");
        mock.calls.push(method.to_owned());
        let response_type = match method {
            "getAuthorizationState" => mock.authorization,
            "setAuthenticationPhoneNumber" => {
                mock.authorization = "authorizationStateWaitCode";
                "ok"
            }
            "checkAuthenticationCode" => {
                mock.authorization = "authorizationStateReady";
                "ok"
            }
            _ => "ok",
        };
        let response = object([
            ("@type", string(response_type)),
            ("@client_id", number(client)),
            ("@extra", value_at(&request, "@extra").clone()),
        ]);
        mock.queue
            .push_back(CString::new(response.try_stringify().unwrap()).unwrap());
    }
    unsafe extern "C" fn mock_receive(_timeout: f64) -> *const c_char {
        let mut mock = mock().lock().unwrap();
        mock.last = mock.queue.pop_front();
        mock.last.as_ref().map_or(std::ptr::null(), |s| s.as_ptr())
    }

    #[test]
    fn json_abi_routes_accounts_and_status_never_requests_a_code() {
        let mut runtime = Runtime {
            api: TdApi {
                create: mock_create,
                send: mock_send,
                receive: mock_receive,
            },
            library: "mock".to_owned(),
            clients: BTreeMap::new(),
            next_request: 1,
        };
        let first = runtime.create().unwrap();
        let other = runtime.create().unwrap();
        let update = object([
            ("@type", string("updateChatTitle")),
            ("@client_id", number(other)),
            ("chat_id", number(9)),
            ("title", string("another account")),
        ]);
        mock()
            .lock()
            .unwrap()
            .queue
            .push_back(CString::new(update.try_stringify().unwrap()).unwrap());
        let config = object([("phone_number", string("+10000000000"))]);
        let state = runtime
            .authorization_status(first, &config, &empty(), false)
            .unwrap();
        assert_eq!(
            text(value_at(&state, "authorization_state"), "@type"),
            "authorizationStateWaitPhoneNumber"
        );
        assert!(
            mock()
                .lock()
                .unwrap()
                .calls
                .iter()
                .all(|name| name == "getAuthorizationState")
        );
        assert!(!runtime.clients.get(&other).unwrap().events.is_empty());
        assert!(runtime.clients.get(&first).unwrap().events.is_empty());
        let state = runtime
            .authorization_status(first, &config, &empty(), true)
            .unwrap();
        assert_eq!(
            text(value_at(&state, "authorization_state"), "@type"),
            "authorizationStateWaitCode"
        );
        assert_eq!(
            mock()
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|name| *name == "setAuthenticationPhoneNumber")
                .count(),
            1
        );
        let state = runtime
            .authorization_status(first, &config, &object([("code", string("12345"))]), true)
            .unwrap();
        assert_eq!(state.get("connected").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn preserves_64_bit_identifiers_and_media_captions() {
        let raw = parse(r#"{"id":9007199254740993,"chat_id":-1001234567890,"sender_id":{"@type":"messageSenderUser","user_id":9007199254740995},"content":{"@type":"messagePhoto","caption":{"text":"photo","entities":[]}}}"#).unwrap();
        let normalized = normalize_message(&raw);
        assert_eq!(text(&normalized, "id"), "9007199254740993");
        assert_eq!(text(&normalized, "conversation_id"), "-1001234567890");
        assert_eq!(text(&normalized, "text"), "photo");
    }
}
