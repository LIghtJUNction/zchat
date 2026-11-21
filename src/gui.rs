//! Native desktop UI. Account engines and network I/O stay on one worker thread.
use crate::{
    config::{Account, default_path},
    engine::{Access, Engine, Origin, request},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zchat_core::{ChatError, Value, obj, parse};
use zgpui::native::{ButtonStyle, Canvas, Event, Palette, Rect, Theme, Ui, Window, colors};

const RAIL: f32 = 76.;
const SIDEBAR: f32 = 288.;
const ACCENT: u32 = 0xd59a7bff;
const ACCENT_SOFT: u32 = 0x3b2b22ff;
const POLL_INTERVAL: Duration = Duration::from_secs(8);

enum Job {
    Call {
        id: u64,
        request: Value,
    },
    Add {
        id: u64,
        account: Account,
    },
    Permissions {
        id: u64,
        account: String,
        read: bool,
        write: bool,
    },
    OpenSponsor {
        id: u64,
        account: String,
        conversation: String,
        message: String,
        url: String,
    },
}
struct Reply {
    id: u64,
    result: Result<Value, ChatError>,
}
struct Worker {
    input: Option<SyncSender<Job>>,
    output: Receiver<Reply>,
    stopping: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    fn start(path: Option<PathBuf>) -> Result<Self, ChatError> {
        let (input, jobs) = mpsc::sync_channel::<Job>(8);
        let (responses, output) = mpsc::sync_channel::<Reply>(16);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let handle = thread::Builder::new()
            .name("zchat-accounts".into())
            .spawn(move || {
                let mut engine = match Engine::new(path, Access::default()) {
                    Ok(engine) => engine,
                    Err(error) => {
                        let _ = responses.send(Reply {
                            id: 0,
                            result: Err(error),
                        });
                        return;
                    }
                };
                if responses
                    .send(Reply {
                        id: 0,
                        result: Ok(snapshot(&engine)),
                    })
                    .is_err()
                {
                    return;
                }
                while !stop.load(Ordering::Relaxed) {
                    let Ok(job) = jobs.recv() else {
                        break;
                    };
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let (id, result) = match job {
                        Job::Call { id, request } => (id, engine.call(request, Origin::Human)),
                        Job::Add { id, account } => {
                            let before = engine.store.clone();
                            let result = engine
                                .store
                                .add_account(account)
                                .map(|()| snapshot(&engine));
                            if result.is_err() {
                                engine.store = before;
                            }
                            (id, result)
                        }
                        Job::Permissions {
                            id,
                            account,
                            read,
                            write,
                        } => {
                            let before = engine.store.clone();
                            let result = (|| {
                                let account =
                                    engine.store.accounts.get_mut(&account).ok_or_else(|| {
                                        ChatError::new(
                                            "account_not_found",
                                            "Select an existing account",
                                        )
                                    })?;
                                account.ai_read = read && account.provider != "telegram";
                                account.ai_write = write && account.ai_read;
                                engine.store.save()?;
                                Ok(snapshot(&engine))
                            })();
                            if result.is_err() {
                                engine.store = before;
                            }
                            (id, result)
                        }
                        Job::OpenSponsor {
                            id,
                            account,
                            conversation,
                            message,
                            url,
                        } => (
                            id,
                            open_sponsor(&mut engine, &account, &conversation, &message, &url),
                        ),
                    };
                    if responses.send(Reply { id, result }).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| ChatError::new("worker_start", error.to_string()))?;
        Ok(Self {
            input: Some(input),
            output,
            stopping,
            handle: Some(handle),
        })
    }

    fn submit(&self, job: Job) -> Result<(), ChatError> {
        self.input
            .as_ref()
            .ok_or_else(|| ChatError::new("worker_closed", "Account worker stopped"))?
            .try_send(job)
            .map_err(|error| match error {
                TrySendError::Full(_) => ChatError::new(
                    "busy",
                    "Account worker is busy; wait for the current request",
                ),
                TrySendError::Disconnected(_) => {
                    ChatError::new("worker_closed", "Account worker stopped")
                }
            })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.input.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        // The native window is dropped first by run(). An in-flight call has its
        // own timeout; joining lets backend databases and TDLib close cleanly.
    }
}

fn snapshot(engine: &Engine) -> Value {
    obj([
        ("accounts", engine.store.public_accounts()),
        ("plugins", engine.plugins()),
        ("catalog", engine.catalog()),
    ])
}

#[derive(Clone, Debug)]
struct AccountView {
    id: String,
    label: String,
    provider: String,
    ai_read: bool,
    ai_write: bool,
}
#[derive(Clone, Debug)]
struct Conversation {
    id: String,
    title: String,
}
#[derive(Clone, Debug)]
struct Sponsor {
    label: String,
    button: String,
    url: String,
}
#[derive(Clone, Debug)]
struct Message {
    id: String,
    sender: String,
    text: String,
    outgoing: bool,
    sponsor: Option<Sponsor>,
}
struct MessageLayout {
    id: String,
    sender: String,
    lines: Vec<String>,
    outgoing: bool,
    height: f32,
    sponsor: Option<Sponsor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Inbox,
    Connections,
    Tools,
}
#[derive(Clone, Debug)]
enum Purpose {
    Status,
    Conversations,
    Messages,
    Read,
    Typing,
    Background,
    Send { text: String },
    Events,
    Native,
    Add,
    Permissions,
    OpenSponsor,
}
struct Pending {
    account: String,
    conversation: String,
    purpose: Purpose,
}
struct LinkPrompt {
    message: String,
    url: String,
}
struct AuthPrompt {
    method: &'static str,
    key: &'static str,
    label: &'static str,
    secret: bool,
}

#[derive(Clone, Copy)]
struct Field {
    key: &'static str,
    label: &'static str,
    placeholder: &'static str,
    secret: bool,
    required: bool,
    numeric: bool,
}
#[derive(Clone, Copy)]
struct Provider {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    fields: &'static [Field],
}

const PROVIDERS: &[Provider] = &[
    Provider {
        id: "matrix",
        label: "Matrix",
        description: "Connect with a homeserver access token. Encrypted rooms need a crypto-capable plugin.",
        fields: &[
            Field {
                key: "homeserver",
                label: "Homeserver",
                placeholder: "https://matrix.org",
                secret: false,
                required: true,
                numeric: false,
            },
            Field {
                key: "access_token",
                label: "Access token",
                placeholder: "Your account token",
                secret: true,
                required: true,
                numeric: false,
            },
        ],
    },
    Provider {
        id: "telegram",
        label: "Telegram",
        description: "Uses your own Telegram API credentials and an installed TDLib JSON library.",
        fields: &[
            Field {
                key: "api_id",
                label: "API ID",
                placeholder: "Application API ID",
                secret: false,
                required: true,
                numeric: true,
            },
            Field {
                key: "api_hash",
                label: "API hash",
                placeholder: "Application API hash",
                secret: true,
                required: true,
                numeric: false,
            },
            Field {
                key: "phone_number",
                label: "Phone number",
                placeholder: "+ country code · optional",
                secret: false,
                required: false,
                numeric: false,
            },
            Field {
                key: "database_directory",
                label: "TDLib data directory",
                placeholder: "Automatic account directory",
                secret: false,
                required: false,
                numeric: false,
            },
            Field {
                key: "library_path",
                label: "TDLib library",
                placeholder: "System library · optional path",
                secret: false,
                required: false,
                numeric: false,
            },
        ],
    },
    Provider {
        id: "xmpp",
        label: "XMPP",
        description: "Use your Jabber address and a TLS-enabled server. Requires libstrophe.",
        fields: &[
            Field {
                key: "jid",
                label: "Jabber address",
                placeholder: "you@example.org",
                secret: false,
                required: true,
                numeric: false,
            },
            Field {
                key: "password",
                label: "Password",
                placeholder: "Account password",
                secret: true,
                required: true,
                numeric: false,
            },
            Field {
                key: "host",
                label: "Server override",
                placeholder: "Automatic discovery",
                secret: false,
                required: false,
                numeric: false,
            },
            Field {
                key: "port",
                label: "Port",
                placeholder: "5222",
                secret: false,
                required: false,
                numeric: true,
            },
            Field {
                key: "library_path",
                label: "libstrophe library",
                placeholder: "System library · optional path",
                secret: false,
                required: false,
                numeric: false,
            },
        ],
    },
    Provider {
        id: "simplex",
        label: "SimpleX",
        description: "Connect to the local SimpleX Chat API. Start simplex-chat -p 5225 first.",
        fields: &[
            Field {
                key: "url",
                label: "Local WebSocket",
                placeholder: "ws://127.0.0.1:5225",
                secret: false,
                required: true,
                numeric: false,
            },
            Field {
                key: "user_id",
                label: "User ID",
                placeholder: "1",
                secret: false,
                required: true,
                numeric: true,
            },
        ],
    },
    Provider {
        id: "deltachat",
        label: "Delta Chat",
        description: "Uses an installed deltachat-rpc-server and its local account database.",
        fields: &[
            Field {
                key: "command",
                label: "RPC executable",
                placeholder: "deltachat-rpc-server",
                secret: false,
                required: false,
                numeric: false,
            },
            Field {
                key: "account_id",
                label: "Account ID",
                placeholder: "1",
                secret: false,
                required: true,
                numeric: true,
            },
            Field {
                key: "accounts_path",
                label: "Accounts directory",
                placeholder: "Use backend default",
                secret: false,
                required: false,
                numeric: false,
            },
        ],
    },
    Provider {
        id: "threema-gateway",
        label: "Threema",
        description: "Uses a dedicated Threema Gateway identity. This adapter uses Gateway Basic mode.",
        fields: &[
            Field {
                key: "identity",
                label: "Gateway identity",
                placeholder: "*EXAMPLE",
                secret: false,
                required: true,
                numeric: false,
            },
            Field {
                key: "secret",
                label: "Gateway secret",
                placeholder: "API secret",
                secret: true,
                required: true,
                numeric: false,
            },
        ],
    },
    Provider {
        id: "session",
        label: "Session",
        description: "No Session engine is bundled. Register a compatible native provider plugin to connect.",
        fields: &[],
    },
];

struct ConnectionForm {
    provider: usize,
    id: String,
    label: String,
    fields: BTreeMap<String, String>,
    ai_read: bool,
    ai_write: bool,
}
impl Default for ConnectionForm {
    fn default() -> Self {
        let mut form = Self {
            provider: 0,
            id: String::new(),
            label: String::new(),
            fields: BTreeMap::new(),
            ai_read: false,
            ai_write: false,
        };
        form.select_provider(0);
        form
    }
}
impl ConnectionForm {
    fn select_provider(&mut self, provider: usize) {
        self.provider = provider;
        self.fields.clear();
        self.ai_read = false;
        self.ai_write = false;
        match PROVIDERS[provider].id {
            "matrix" => {
                self.fields
                    .insert("homeserver".into(), "https://matrix.org".into());
            }
            "simplex" => {
                self.fields
                    .insert("url".into(), "ws://127.0.0.1:5225".into());
                self.fields.insert("user_id".into(), "1".into());
            }
            "deltachat" => {
                self.fields.insert("account_id".into(), "1".into());
            }
            "xmpp" => {
                self.fields.insert("port".into(), "5222".into());
            }
            _ => (),
        }
    }

    fn account(&self, path: Option<&PathBuf>) -> Result<Account, ChatError> {
        let provider = PROVIDERS[self.provider];
        if provider.id == "session" {
            return Err(ChatError::new(
                "backend_unavailable",
                "Session requires a compatible external provider plugin",
            ));
        }
        let mut config = BTreeMap::new();
        for field in provider.fields {
            let raw = self.fields.get(field.key).map(String::as_str).unwrap_or("");
            let value = if field.secret { raw } else { raw.trim() };
            if value.is_empty() {
                if field.required {
                    return Err(ChatError::new(
                        "missing_field",
                        format!("Enter {}", field.label),
                    ));
                }
                continue;
            }
            let value = if field.numeric {
                let number = value.parse::<u64>().map_err(|_| {
                    ChatError::new(
                        "invalid_field",
                        format!("{} must be a positive integer", field.label),
                    )
                })?;
                let maximum = match field.key {
                    "port" => u16::MAX as u64,
                    "api_id" => i32::MAX as u64,
                    _ => u32::MAX as u64,
                };
                if number == 0 || number > maximum {
                    return Err(ChatError::new(
                        "invalid_field",
                        format!("{} is outside the supported range", field.label),
                    ));
                }
                Value::from(number)
            } else {
                Value::from(value)
            };
            config.insert(field.key.into(), value);
        }
        if provider.id == "telegram" && !config.contains_key("database_directory") {
            let path = path.cloned().map(Ok).unwrap_or_else(default_path)?;
            let directory = path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(format!("telegram-{}", self.id.trim()));
            config.insert(
                "database_directory".into(),
                directory.to_string_lossy().as_ref().into(),
            );
        }
        if provider.id == "threema-gateway" {
            config.insert("mode".into(), "basic".into());
        }
        Account::from_value(&obj([
            ("id", self.id.trim().into()),
            (
                "label",
                if self.label.trim().is_empty() {
                    self.id.trim()
                } else {
                    self.label.trim()
                }
                .into(),
            ),
            ("provider", provider.id.into()),
            ("plugin", "builtin".into()),
            ("config", Value::Object(config)),
            (
                "ai_read",
                (self.ai_read && provider.id != "telegram").into(),
            ),
            (
                "ai_write",
                (self.ai_write && self.ai_read && provider.id != "telegram").into(),
            ),
        ]))
    }
}

struct App {
    page: Page,
    path: Option<PathBuf>,
    accounts: Vec<AccountView>,
    selected_account: String,
    selected_conversation: String,
    conversation_title: String,
    conversations: Vec<Conversation>,
    messages: Vec<Message>,
    message_layout: Vec<MessageLayout>,
    layout_width: f32,
    layout_dirty: bool,
    message_scroll: f32,
    conversation_scroll: usize,
    account_scroll: usize,
    management_scroll: usize,
    pointer: (f32, f32),
    search: String,
    draft: String,
    draft_key: Option<(String, String, String, String)>,
    account_menu: bool,
    account_status: String,
    pending: BTreeMap<u64, Pending>,
    next_id: u64,
    notice: String,
    notice_error: bool,
    form: ConnectionForm,
    manage_account: String,
    manage_read: bool,
    manage_write: bool,
    native_operation: String,
    native_method: String,
    native_params: String,
    native_output: String,
    native_scroll: usize,
    auth_prompt: Option<AuthPrompt>,
    auth_value: String,
    link_prompt: Option<LinkPrompt>,
    seen: BTreeSet<String>,
    visible_receipts: Vec<String>,
    typing_active: bool,
    last_typing: Instant,
    last_edit: Instant,
    plugins: Value,
    catalog: Value,
    last_poll: Instant,
    poll_enabled: bool,
}

impl App {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            page: Page::Inbox,
            path,
            accounts: Vec::new(),
            selected_account: String::new(),
            selected_conversation: String::new(),
            conversation_title: String::new(),
            conversations: Vec::new(),
            messages: Vec::new(),
            message_layout: Vec::new(),
            layout_width: 0.,
            layout_dirty: true,
            message_scroll: 0.,
            conversation_scroll: 0,
            account_scroll: 0,
            management_scroll: 0,
            pointer: (0., 0.),
            search: String::new(),
            draft: String::new(),
            draft_key: None,
            account_menu: false,
            account_status: "Local workspace".into(),
            pending: BTreeMap::new(),
            next_id: 1,
            notice: String::new(),
            notice_error: false,
            form: ConnectionForm::default(),
            manage_account: String::new(),
            manage_read: false,
            manage_write: false,
            native_operation: "native.invoke".into(),
            native_method: String::new(),
            native_params: "{}".into(),
            native_output: String::new(),
            native_scroll: 0,
            auth_prompt: None,
            auth_value: String::new(),
            link_prompt: None,
            seen: BTreeSet::new(),
            visible_receipts: Vec::new(),
            typing_active: false,
            last_typing: Instant::now()
                .checked_sub(Duration::from_secs(3))
                .unwrap_or_else(Instant::now),
            last_edit: Instant::now(),
            plugins: Value::array(),
            catalog: Value::Null,
            last_poll: Instant::now(),
            poll_enabled: false,
        }
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = text.into();
        self.notice_error = error;
    }
    fn busy(&self) -> bool {
        !self.pending.is_empty()
    }
    fn sending(&self) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending.purpose, Purpose::Send { .. }))
    }
    fn selected(&self) -> Option<&AccountView> {
        self.accounts
            .iter()
            .find(|account| account.id == self.selected_account)
    }

    fn call(&mut self, worker: &Worker, purpose: Purpose, operation: &str, params: Value) -> bool {
        if self.selected_account.is_empty() {
            self.notify("Connect and select an account first", true);
            return false;
        }
        let id = self.next_id;
        self.next_id += 1;
        let pending = Pending {
            account: self.selected_account.clone(),
            conversation: self.selected_conversation.clone(),
            purpose,
        };
        match worker.submit(Job::Call {
            id,
            request: request(&self.selected_account, operation, params),
        }) {
            Ok(()) => {
                self.pending.insert(id, pending);
                true
            }
            Err(error) => {
                self.notify(error.message, true);
                false
            }
        }
    }

    fn select_account(&mut self, index: usize, worker: &Worker) {
        self.set_typing(worker, false);
        let Some(account) = self.accounts.get(index) else {
            return;
        };
        self.selected_account = account.id.clone();
        self.account_menu = false;
        self.selected_conversation.clear();
        self.conversation_title.clear();
        self.conversations.clear();
        self.messages.clear();
        self.layout_dirty = true;
        self.message_scroll = 0.;
        self.conversation_scroll = 0;
        self.poll_enabled = false;
        self.seen.clear();
        self.auth_prompt = None;
        self.auth_value.clear();
        self.account_status = "Checking account…".into();
        self.last_poll = Instant::now();
        self.draft.clear();
        self.draft_key = None;
        self.call(worker, Purpose::Status, "account.status", Value::object());
        self.call(
            worker,
            Purpose::Conversations,
            "conversation.list",
            obj([("limit", 100u64.into())]),
        );
    }

    fn refresh_messages(&mut self, worker: &Worker) {
        if self.selected_conversation.is_empty() {
            return;
        }
        self.call(
            worker,
            Purpose::Messages,
            "message.list",
            obj([
                (
                    "conversation_id",
                    self.selected_conversation.as_str().into(),
                ),
                ("limit", 100u64.into()),
            ]),
        );
    }

    fn set_snapshot(&mut self, value: &Value, worker: &Worker) {
        let previous = self.selected_account.clone();
        self.accounts = values(value, "accounts")
            .iter()
            .filter_map(|value| {
                Some(AccountView {
                    id: value.get_str("id")?.into(),
                    label: value.get_str("label").unwrap_or("Account").into(),
                    provider: value.get_str("provider").unwrap_or("plugin").into(),
                    ai_read: value.get_bool("ai_read").unwrap_or(false),
                    ai_write: value.get_bool("ai_write").unwrap_or(false),
                })
            })
            .collect();
        self.plugins = value.get("plugins").cloned().unwrap_or_else(Value::array);
        self.catalog = value.get("catalog").cloned().unwrap_or(Value::Null);
        if !self.accounts.is_empty() && !self.accounts.iter().any(|account| account.id == previous)
        {
            self.select_account(0, worker);
        }
    }

    fn process_reply(&mut self, reply: Reply, worker: &Worker) {
        if reply.id == 0 {
            match reply.result {
                Ok(value) => self.set_snapshot(&value, worker),
                Err(error) => self.notify(error.message, true),
            }
            return;
        }
        let Some(pending) = self.pending.remove(&reply.id) else {
            return;
        };
        let current_account = pending.account == self.selected_account;
        let current_conversation =
            current_account && pending.conversation == self.selected_conversation;
        match reply.result {
            Err(error) => {
                if matches!(pending.purpose, Purpose::Events) {
                    self.poll_enabled = false;
                }
                if matches!(pending.purpose, Purpose::Status) && current_account {
                    self.account_status = "Needs attention".into();
                }
                if matches!(pending.purpose, Purpose::Native) {
                    self.native_output = error.to_value().stringify();
                    self.native_scroll = 0;
                }
                if matches!(
                    pending.purpose,
                    Purpose::Typing | Purpose::Background | Purpose::Read
                ) {
                    return;
                }
                if current_account || matches!(pending.purpose, Purpose::Add | Purpose::Permissions)
                {
                    self.notify(format!("{} · {}", error.code, error.message), true);
                }
            }
            Ok(value) => match pending.purpose {
                Purpose::Add => {
                    let added = self.form.id.trim().to_string();
                    self.set_snapshot(&value, worker);
                    self.form = ConnectionForm::default();
                    if let Some(index) =
                        self.accounts.iter().position(|account| account.id == added)
                        && self.selected_account != added
                    {
                        self.select_account(index, worker);
                    }
                    self.notify("Account saved. Check its connection status or continue authorization in Tools.", false);
                }
                Purpose::Permissions => {
                    self.set_snapshot(&value, worker);
                    self.notify("AI access controls saved", false);
                }
                Purpose::Status if current_account => {
                    self.account_status = account_state(&value);
                    let state = value
                        .get("authorization_state")
                        .unwrap_or(&Value::Null)
                        .get_str("@type")
                        .unwrap_or("");
                    self.auth_prompt = match state {
                        "authorizationStateWaitCode" => Some(AuthPrompt {
                            method: "checkAuthenticationCode",
                            key: "code",
                            label: "Verification code",
                            secret: false,
                        }),
                        "authorizationStateWaitPassword" => Some(AuthPrompt {
                            method: "checkAuthenticationPassword",
                            key: "password",
                            label: "Two-step password",
                            secret: true,
                        }),
                        "authorizationStateWaitPhoneNumber" => Some(AuthPrompt {
                            method: "setAuthenticationPhoneNumber",
                            key: "phone_number",
                            label: "Phone number",
                            secret: false,
                        }),
                        _ => None,
                    };
                    if let Some(prompt) = &self.auth_prompt {
                        self.native_operation = "native.invoke".into();
                        self.native_method = prompt.method.into();
                        self.auth_value.clear();
                    }
                    if state == "authorizationStateReady" {
                        self.telegram_call(
                            worker,
                            Purpose::Background,
                            "setOption",
                            obj([
                                ("name", "online".into()),
                                (
                                    "value",
                                    obj([
                                        ("@type", "optionValueBoolean".into()),
                                        ("value", true.into()),
                                    ]),
                                ),
                            ]),
                        );
                    }
                }
                Purpose::Conversations if current_account => {
                    self.conversations = values(&value, "conversations")
                        .iter()
                        .filter_map(|value| {
                            let id = identifier(value.get("id")?)?;
                            Some(Conversation {
                                title: value.get_str("title").unwrap_or(&id).into(),
                                id,
                            })
                        })
                        .collect();
                    self.conversation_scroll = self
                        .conversation_scroll
                        .min(self.conversations.len().saturating_sub(1));
                    self.poll_enabled = true;
                }
                Purpose::Messages if current_conversation => {
                    self.messages = values(&value, "messages").iter().map(|value| {
                        let encrypted = value.get_bool("encrypted").unwrap_or(false);
                        Message {
                            id: value.get("id").and_then(identifier).unwrap_or_default(),
                            sender: value.get("sender").and_then(identifier).unwrap_or_else(|| "Message".into()),
                            text: value.get_str("text").map(str::to_string).unwrap_or_else(|| if encrypted { "Encrypted message · open with a crypto-capable plugin".into() } else { "Media or platform content · inspect with the native tools".into() }),
                            outgoing: value.get_bool("outgoing").or_else(|| value.get("raw").and_then(|raw| raw.get_bool("is_outgoing"))).unwrap_or(false),
                            sponsor: None,
                        }
                    }).collect();
                    if value.get_str("order") == Some("newest_first") {
                        self.messages.reverse();
                    }
                    if let Some(message) = sponsored_message(&value) {
                        self.messages.push(message);
                    }
                    self.layout_dirty = true;
                }
                Purpose::Send { text } => {
                    if current_conversation {
                        if self.draft == text {
                            self.draft.clear();
                            self.draft_key = None;
                        }
                        self.set_typing(worker, false);
                        self.message_scroll = 0.;
                        self.refresh_messages(worker);
                    }
                    self.notify("Send request accepted by the platform", false);
                }
                Purpose::Events if current_account => {
                    if !values(&value, "events").is_empty() {
                        self.call(
                            worker,
                            Purpose::Conversations,
                            "conversation.list",
                            obj([("limit", 100u64.into())]),
                        );
                        self.refresh_messages(worker);
                    }
                }
                Purpose::Native => {
                    self.native_output = value.stringify();
                    self.native_scroll = 0;
                    self.auth_value.clear();
                    self.notify("Platform request completed", false);
                    if self.auth_prompt.is_some() {
                        self.call(worker, Purpose::Status, "account.status", Value::object());
                    }
                }
                Purpose::OpenSponsor => {
                    if let Some(chat) = value.get("open_chat")
                        && let Some(id) = chat.get("id").and_then(identifier)
                    {
                        self.selected_conversation = id;
                        self.conversation_title =
                            chat.get_str("title").unwrap_or("Telegram").into();
                        self.messages.clear();
                        self.layout_dirty = true;
                        self.seen.clear();
                        self.message_scroll = 0.;
                        self.refresh_messages(worker);
                    }
                    self.notify("Sponsored destination opened", false);
                }
                _ => (),
            },
        }
    }

    fn send(&mut self, worker: &Worker) {
        if self.draft.trim().is_empty() || self.selected_conversation.is_empty() || self.sending() {
            return;
        }
        let same = self
            .draft_key
            .as_ref()
            .is_some_and(|(account, conversation, text, _)| {
                account == &self.selected_account
                    && conversation == &self.selected_conversation
                    && text == &self.draft
            });
        if !same {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            self.draft_key = Some((
                self.selected_account.clone(),
                self.selected_conversation.clone(),
                self.draft.clone(),
                format!("zchat-{}-{nonce}", std::process::id()),
            ));
        }
        let key = self.draft_key.as_ref().expect("created key").3.clone();
        self.call(
            worker,
            Purpose::Send {
                text: self.draft.clone(),
            },
            "message.send",
            obj([
                (
                    "conversation_id",
                    self.selected_conversation.as_str().into(),
                ),
                ("text", self.draft.as_str().into()),
                ("idempotency_key", key.as_str().into()),
                ("transaction_id", key.into()),
            ]),
        );
    }

    fn execute_native(&mut self, worker: &Worker) {
        let result = (|| {
            if let Some(prompt) = &self.auth_prompt
                && self.native_operation.trim() == "native.invoke"
                && self.native_method.trim() == prompt.method
            {
                if self.auth_value.trim().is_empty() {
                    return Err(ChatError::new(
                        "missing_verification",
                        format!("Enter {}", prompt.label.to_lowercase()),
                    ));
                }
                return Ok(obj([
                    ("method", prompt.method.into()),
                    (
                        "request",
                        obj([
                            ("@type", prompt.method.into()),
                            (prompt.key, self.auth_value.as_str().into()),
                        ]),
                    ),
                ]));
            }
            let mut params = parse(&self.native_params)?;
            let fields = params.as_object_mut().ok_or_else(|| {
                ChatError::new("invalid_params", "Parameters must be a JSON object")
            })?;
            if self.native_operation.trim() == "native.invoke" {
                if self.native_method.trim().is_empty() {
                    return Err(ChatError::new(
                        "missing_method",
                        "Enter the documented platform method",
                    ));
                }
                fields.insert("method".into(), self.native_method.trim().into());
            }
            Ok(params)
        })();
        match result {
            Ok(params) => {
                let operation = self.native_operation.trim().to_string();
                self.call(worker, Purpose::Native, &operation, params);
            }
            Err(error) => self.notify(error.message, true),
        }
    }

    fn telegram_call(
        &mut self,
        worker: &Worker,
        purpose: Purpose,
        method: &str,
        parameters: Value,
    ) {
        let mut fields = parameters.as_object().cloned().unwrap_or_default();
        fields.insert("@type".into(), method.into());
        self.call(
            worker,
            purpose,
            "native.invoke",
            obj([
                ("method", method.into()),
                ("request", Value::Object(fields)),
            ]),
        );
    }

    fn set_typing(&mut self, worker: &Worker, active: bool) {
        if self.selected_conversation.is_empty()
            || !self
                .selected()
                .is_some_and(|account| matches!(account.provider.as_str(), "telegram" | "matrix"))
        {
            return;
        }
        if active && self.last_typing.elapsed() < Duration::from_secs(2) {
            return;
        }
        if !active && !self.typing_active {
            return;
        }
        if self.call(
            worker,
            Purpose::Typing,
            "typing.set",
            obj([
                (
                    "conversation_id",
                    self.selected_conversation.as_str().into(),
                ),
                ("typing", active.into()),
            ]),
        ) {
            self.typing_active = active;
            self.last_typing = Instant::now();
        }
    }

    fn confirm_visible(&mut self, worker: &Worker) {
        let ids = std::mem::take(&mut self.visible_receipts);
        if ids.is_empty() || self.link_prompt.is_some() {
            return;
        }
        let provider = self
            .selected()
            .map(|account| account.provider.as_str())
            .unwrap_or("");
        if provider == "telegram" {
            let Ok(chat) = self.selected_conversation.parse::<i64>() else {
                return;
            };
            let messages: Vec<Value> = ids
                .iter()
                .filter_map(|id| id.parse::<i64>().ok().map(Value::from))
                .collect();
            if messages.is_empty() {
                return;
            }
            let params = obj([
                ("method", "viewMessages".into()),
                (
                    "request",
                    obj([
                        ("@type", "viewMessages".into()),
                        ("chat_id", chat.into()),
                        ("message_ids", Value::Array(messages)),
                        (
                            "source",
                            obj([("@type", "messageSourceChatHistory".into())]),
                        ),
                        ("force_read", true.into()),
                    ]),
                ),
            ]);
            if self.call(worker, Purpose::Read, "native.invoke", params) {
                self.seen.extend(ids);
            }
        } else if matches!(provider, "matrix" | "deltachat")
            && let Some(id) = ids.last()
            && self.call(
                worker,
                Purpose::Read,
                "message.read",
                obj([
                    (
                        "conversation_id",
                        self.selected_conversation.as_str().into(),
                    ),
                    ("message_id", id.as_str().into()),
                ]),
            )
        {
            self.seen.extend(ids);
        }
    }
}

fn values<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn identifier(value: &Value) -> Option<String> {
    match value {
        Value::String(value) | Value::Number(value) => Some(value.clone()),
        _ => None,
    }
}
fn account_state(value: &Value) -> String {
    let state = value
        .get("authorization_state")
        .or_else(|| value.get("authorizationState"))
        .unwrap_or(value);
    let kind = state
        .as_str()
        .or_else(|| state.get_str("@type"))
        .or_else(|| value.get_str("state"))
        .unwrap_or("");
    match kind {
        "authorizationStateReady" => "Connected".into(),
        "authorizationStateWaitCode" => "Verification code required · Tools".into(),
        "authorizationStateWaitPassword" => "Two-step password required · Tools".into(),
        "authorizationStateWaitPhoneNumber" => "Phone authorization required · Tools".into(),
        "authorizationStateClosed" => "Session closed".into(),
        value if !value.is_empty() => value.chars().take(64).collect(),
        _ => "Account checked".into(),
    }
}

fn sponsored_message(value: &Value) -> Option<Message> {
    // TDLib messages_between=0 permits one ad after all ordinary messages.
    // Selecting one also respects every nonzero minimum spacing value.
    let raw = value
        .get("sponsored_messages")?
        .get("messages")?
        .as_array()?
        .first()?;
    let id = identifier(raw.get("message_id")?)?;
    let sponsor = raw.get("sponsor").unwrap_or(&Value::Null);
    let content = raw.get("content").unwrap_or(&Value::Null);
    let formatted = content
        .get("text")
        .or_else(|| content.get("caption"))
        .unwrap_or(&Value::Null);
    let title = raw.get_str("title").unwrap_or("Sponsored message");
    let mut parts = vec![title.to_string()];
    if let Some(text) = formatted.get_str("text")
        && !text.is_empty()
    {
        parts.push(text.into());
    }
    if content
        .get_str("@type")
        .is_some_and(|kind| kind != "messageText")
    {
        parts.push("Media is included in the native platform payload.".into());
    }
    if let Some(info) = sponsor.get_str("info")
        && !info.is_empty()
    {
        parts.push(info.into());
    }
    if let Some(info) = raw.get_str("additional_info")
        && !info.is_empty()
    {
        parts.push(info.into());
    }
    let label = if raw.get_bool("is_recommended").unwrap_or(false) {
        "Recommended"
    } else {
        "Sponsored"
    };
    Some(Message {
        id,
        sender: label.into(),
        text: parts.join("\n\n"),
        outgoing: false,
        sponsor: Some(Sponsor {
            label: label.into(),
            button: raw.get_str("button_text").unwrap_or("Open sponsor").into(),
            url: sponsor.get_str("url").unwrap_or("").into(),
        }),
    })
}

fn td_request(
    engine: &mut Engine,
    account: &str,
    method: &str,
    parameters: Value,
) -> Result<Value, ChatError> {
    let mut fields = parameters.as_object().cloned().unwrap_or_default();
    fields.insert("@type".into(), method.into());
    engine.call(
        request(
            account,
            "native.invoke",
            obj([
                ("method", method.into()),
                ("request", Value::Object(fields)),
            ]),
        ),
        Origin::Human,
    )
}

fn open_sponsor(
    engine: &mut Engine,
    account: &str,
    conversation: &str,
    message: &str,
    url: &str,
) -> Result<Value, ChatError> {
    if url.len() > 4096
        || url.chars().any(char::is_control)
        || !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with("tg://"))
    {
        return Err(ChatError::new(
            "invalid_link",
            "The sponsor URL must use HTTPS, HTTP or a Telegram deep link",
        ));
    }
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or("").to_ascii_lowercase())
        .unwrap_or_default();
    let telegram = url.starts_with("tg://")
        || ["t.me", "telegram.me", "telegram.dog", "telegram.org"]
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")));
    let mut opened = Value::object();
    if telegram {
        let link = td_request(
            engine,
            account,
            "getInternalLinkType",
            obj([("link", url.into())]),
        )?;
        match link.get_str("@type").unwrap_or("") {
            "internalLinkTypePublicChat" => {
                let username = link.get_str("chat_username").ok_or_else(|| {
                    ChatError::new("invalid_link", "Telegram public-chat link has no username")
                })?;
                let chat = td_request(
                    engine,
                    account,
                    "searchPublicChat",
                    obj([("username", username.into())]),
                )?;
                opened = obj([("open_chat", chat)]);
            }
            "internalLinkTypeMessage" => {
                let info = td_request(
                    engine,
                    account,
                    "getMessageLinkInfo",
                    obj([("url", url.into())]),
                )?;
                let chat_id = info.get("chat_id").cloned().ok_or_else(|| {
                    ChatError::new("invalid_link", "Telegram message link has no chat")
                })?;
                let chat = td_request(engine, account, "getChat", obj([("chat_id", chat_id)]))?;
                opened = obj([("open_chat", chat)]);
            }
            "" => open_external_url(url)?,
            _ => {
                return Err(ChatError::new(
                    "link_type_not_supported",
                    "This Telegram deep link needs a platform-specific flow; its URL is available to copy",
                ));
            }
        }
    } else {
        open_external_url(url)?;
    }
    let chat = conversation
        .parse::<i64>()
        .map_err(|_| ChatError::new("invalid_chat", "Invalid Telegram chat identifier"))?;
    let message = message
        .parse::<i64>()
        .map_err(|_| ChatError::new("invalid_message", "Invalid sponsored message identifier"))?;
    td_request(
        engine,
        account,
        "clickChatSponsoredMessage",
        obj([
            ("chat_id", chat.into()),
            ("message_id", message.into()),
            ("is_media_click", false.into()),
            ("from_fullscreen", false.into()),
        ]),
    )?;
    Ok(opened)
}

fn open_external_url(url: &str) -> Result<(), ChatError> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg("--");
        command
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    let mut child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ChatError::new("open_link_failed", error.to_string()))?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

impl App {
    fn render(&mut self, canvas: &mut Canvas<'_>, ui: &mut Ui, worker: &Worker) {
        self.visible_receipts.clear();
        if self.link_prompt.is_some() {
            self.render_link_prompt(canvas, ui, worker);
            return;
        }
        canvas.rect(Rect::new(0., 0., RAIL, canvas.height), 0., 0x151311ff);
        canvas.text("z.", 24., 24., 30, true, ACCENT);
        for (index, (page, label)) in [
            (Page::Inbox, "Chat"),
            (Page::Connections, "Links"),
            (Page::Tools, "Tools"),
        ]
        .into_iter()
        .enumerate()
        {
            let rect = Rect::new(10., 108. + index as f32 * 64., 56., 44.);
            if self.page == page {
                canvas.rect(rect, 12., ACCENT_SOFT);
            }
            if ui.button(
                canvas,
                10 + index as u64,
                rect,
                label,
                ButtonStyle::Quiet,
                true,
            ) {
                self.page = page;
                self.account_menu = false;
            }
        }
        canvas.text("Z", 31., canvas.height - 39., 14, true, colors::MUTED);
        match self.page {
            Page::Inbox => self.render_inbox(canvas, ui, worker),
            Page::Connections => self.render_connections(canvas, ui, worker),
            Page::Tools => self.render_tools(canvas, ui, worker),
        }
        let x = if self.page == Page::Inbox {
            RAIL + SIDEBAR + 30.
        } else {
            RAIL + 36.
        };
        if !self.notice.is_empty() {
            canvas.ellipsis(
                &self.notice,
                x,
                canvas.height - 28.,
                (canvas.width - x - 28.).max(1.),
                12,
                false,
                if self.notice_error {
                    colors::RED
                } else {
                    colors::MUTED
                },
            );
        } else if self.busy() {
            canvas.text("Working…", x, canvas.height - 28., 12, false, colors::MUTED);
        }
    }

    fn render_inbox(&mut self, canvas: &mut Canvas<'_>, ui: &mut Ui, worker: &Worker) {
        let left = RAIL;
        let split = RAIL + SIDEBAR;
        canvas.rect(Rect::new(left, 0., SIDEBAR, canvas.height), 0., 0x1a1715ff);
        canvas.text("Your inbox", left + 26., 32., 24, true, colors::TEXT);
        let account_label = self
            .selected()
            .map(|account| account.label.as_str())
            .unwrap_or("Connect an account")
            .to_string();
        if ui.button(
            canvas,
            101,
            Rect::new(left + 20., 84., SIDEBAR - 40., 38.),
            &account_label,
            ButtonStyle::Quiet,
            true,
        ) {
            if self.accounts.is_empty() {
                self.page = Page::Connections;
            } else {
                self.account_menu = !self.account_menu;
            }
        }
        canvas.ellipsis(
            &self.account_status,
            left + 30.,
            128.,
            SIDEBAR - 60.,
            12,
            false,
            colors::MUTED,
        );
        if ui.input(
            canvas,
            102,
            Rect::new(left + 22., 170., SIDEBAR - 44., 40.),
            &mut self.search,
            "Search conversations",
            false,
            !self.accounts.is_empty(),
        ) {
            self.conversation_scroll = 0;
        }

        let search = self.search.to_lowercase();
        let filtered: Vec<(usize, Conversation)> = self
            .conversations
            .iter()
            .enumerate()
            .filter(|(_, conversation)| {
                search.is_empty()
                    || conversation.title.to_lowercase().contains(&search)
                    || conversation.id.to_lowercase().contains(&search)
            })
            .map(|(index, conversation)| (index, conversation.clone()))
            .collect();
        let visible = ((canvas.height - 296.) / 64.).max(1.) as usize;
        self.conversation_scroll = self
            .conversation_scroll
            .min(filtered.len().saturating_sub(visible));
        for (row, (index, conversation)) in filtered
            .iter()
            .skip(self.conversation_scroll)
            .take(visible)
            .enumerate()
        {
            let rect = Rect::new(left + 14., 234. + row as f32 * 64., SIDEBAR - 28., 56.);
            if self.selected_conversation == conversation.id {
                canvas.rect(rect, 10., ACCENT_SOFT);
            }
            if ui.button(
                canvas,
                200 + *index as u64,
                rect,
                "",
                ButtonStyle::Quiet,
                true,
            ) {
                self.set_typing(worker, false);
                if self
                    .selected()
                    .is_some_and(|account| account.provider == "telegram")
                    && !self.selected_conversation.is_empty()
                    && let Ok(chat) = self.selected_conversation.parse::<i64>()
                {
                    self.telegram_call(
                        worker,
                        Purpose::Background,
                        "closeChat",
                        obj([("chat_id", chat.into())]),
                    );
                }
                self.selected_conversation = conversation.id.clone();
                self.conversation_title = conversation.title.clone();
                self.messages.clear();
                self.layout_dirty = true;
                self.message_scroll = 0.;
                self.draft.clear();
                self.draft_key = None;
                self.seen.clear();
                if self
                    .selected()
                    .is_some_and(|account| account.provider == "telegram")
                    && let Ok(chat) = self.selected_conversation.parse::<i64>()
                {
                    self.telegram_call(
                        worker,
                        Purpose::Background,
                        "openChat",
                        obj([("chat_id", chat.into())]),
                    );
                }
                self.refresh_messages(worker);
            }
            canvas.ellipsis(
                &conversation.title,
                rect.x + 13.,
                rect.y + 9.,
                rect.w - 28.,
                15,
                self.selected_conversation == conversation.id,
                colors::TEXT,
            );
            canvas.ellipsis(
                &conversation.id,
                rect.x + 13.,
                rect.y + 31.,
                rect.w - 28.,
                11,
                false,
                colors::MUTED,
            );
        }
        if filtered.is_empty() {
            let text = if self.accounts.is_empty() {
                "Your accounts will appear here."
            } else if self.busy() {
                "Loading conversations…"
            } else {
                "No conversations to show."
            };
            canvas.ellipsis(
                text,
                left + 28.,
                250.,
                SIDEBAR - 56.,
                13,
                false,
                colors::MUTED,
            );
        }
        if ui.button(
            canvas,
            103,
            Rect::new(left + 22., canvas.height - 66., SIDEBAR - 44., 38.),
            "+  Add connection",
            ButtonStyle::Quiet,
            true,
        ) {
            self.page = Page::Connections;
        }

        let x = split + 34.;
        let width = (canvas.width - x - 34.).max(160.);
        if self.selected_conversation.is_empty() {
            canvas.text("zchat", x, 35., 19, true, colors::TEXT);
            canvas.text(
                "One workspace. Every conversation.",
                x,
                66.,
                13,
                false,
                colors::MUTED,
            );
            if ui.button(
                canvas,
                104,
                Rect::new(canvas.width - 126., 31., 92., 36.),
                "Refresh",
                ButtonStyle::Quiet,
                !self.selected_account.is_empty() && !self.busy(),
            ) {
                self.call(worker, Purpose::Status, "account.status", Value::object());
                self.call(
                    worker,
                    Purpose::Conversations,
                    "conversation.list",
                    obj([("limit", 100u64.into())]),
                );
            }
            let center_x = x + width * 0.5;
            let y = (canvas.height * 0.31).max(170.);
            draw_connections_mark(canvas, center_x - 52., y);
            let title = if self.accounts.is_empty() {
                "Bring your conversations together."
            } else {
                "Choose a conversation."
            };
            let lines = wrapped(canvas, title, width.min(560.), 29, 4);
            let mut line_y = y + 120.;
            for line in lines {
                let measure = canvas.measure(&line, 29, true);
                canvas.text(
                    &line,
                    center_x - measure / 2.,
                    line_y,
                    29,
                    true,
                    colors::TEXT,
                );
                line_y += 39.;
            }
            let subtitle = if self.accounts.is_empty() {
                "Connect an account to start. Your AI access stays off until you enable it."
            } else {
                "Messages load directly from the selected platform."
            };
            for line in wrapped(canvas, subtitle, width.min(510.), 15, 3) {
                let measure = canvas.measure(&line, 15, false);
                canvas.text(
                    &line,
                    center_x - measure / 2.,
                    line_y + 14.,
                    15,
                    false,
                    colors::MUTED,
                );
                line_y += 23.;
            }
            if self.accounts.is_empty()
                && ui.button(
                    canvas,
                    105,
                    Rect::new(center_x - 98., line_y + 43., 196., 44.),
                    "Connect an account",
                    ButtonStyle::Primary,
                    true,
                )
            {
                self.page = Page::Connections;
            }
            canvas.ellipsis(
                "Open protocols  ·  Native plugins  ·  Your AI controls",
                x,
                canvas.height - 100.,
                width,
                12,
                false,
                colors::MUTED,
            );
        } else {
            canvas.ellipsis(
                &self.conversation_title,
                x,
                33.,
                width - 196.,
                23,
                true,
                colors::TEXT,
            );
            canvas.ellipsis(
                &self.selected_conversation,
                x,
                68.,
                width - 196.,
                12,
                false,
                colors::MUTED,
            );
            if ui.button(
                canvas,
                106,
                Rect::new(canvas.width - 126., 35., 92., 36.),
                "Refresh",
                ButtonStyle::Quiet,
                !self.busy(),
            ) {
                self.refresh_messages(worker);
            }
            if ui.button(
                canvas,
                107,
                Rect::new(canvas.width - 224., 35., 92., 36.),
                "Tools",
                ButtonStyle::Quiet,
                true,
            ) {
                self.page = Page::Tools;
            }
            let area = Rect::new(x, 112., width, (canvas.height - 250.).max(80.));
            self.render_messages(canvas, ui, area);
            let can_compose = !self.sending();
            if ui.input(
                canvas,
                108,
                Rect::new(x, canvas.height - 109., (width - 102.).max(50.), 48.),
                &mut self.draft,
                "Write a message…",
                false,
                can_compose,
            ) {
                self.last_edit = Instant::now();
                self.set_typing(worker, !self.draft.is_empty());
            }
            if ui.button(
                canvas,
                109,
                Rect::new(canvas.width - 122., canvas.height - 109., 88., 48.),
                "Send",
                ButtonStyle::Primary,
                !self.draft.trim().is_empty() && !self.sending(),
            ) {
                self.send(worker);
            }
        }

        // Draw the account chooser last so its hit regions and surface are above
        // the conversation list. No account credentials are included here.
        if self.account_menu {
            let height = (self.accounts.len().min(7) as f32 * 46. + 12.).min(canvas.height - 164.);
            canvas.rect(
                Rect::new(left + 16., 128., SIDEBAR - 32., height),
                12.,
                0x3a312bff,
            );
            let mut selected = None;
            self.account_scroll = self
                .account_scroll
                .min(self.accounts.len().saturating_sub(7));
            for (row, (index, account)) in self
                .accounts
                .iter()
                .enumerate()
                .skip(self.account_scroll)
                .take(7)
                .enumerate()
            {
                if ui.button(
                    canvas,
                    600 + index as u64,
                    Rect::new(left + 22., 134. + row as f32 * 46., SIDEBAR - 44., 40.),
                    &account.label,
                    ButtonStyle::Quiet,
                    true,
                ) {
                    selected = Some(index);
                }
            }
            if let Some(index) = selected {
                self.select_account(index, worker);
            }
        }
    }

    fn render_messages(&mut self, canvas: &mut Canvas<'_>, ui: &mut Ui, area: Rect) {
        if self.layout_dirty || (self.layout_width - area.w).abs() > 1. {
            self.message_layout = self
                .messages
                .iter()
                .map(|message| {
                    let lines = wrapped(canvas, &message.text, (area.w - 28.).max(30.), 16, 1024);
                    MessageLayout {
                        id: message.id.clone(),
                        sender: if message.outgoing {
                            "You".into()
                        } else {
                            message.sender.clone()
                        },
                        outgoing: message.outgoing,
                        height: 28.
                            + lines.len() as f32 * 23.
                            + 26.
                            + if message.sponsor.is_some() { 48. } else { 0. },
                        lines,
                        sponsor: message.sponsor.clone(),
                    }
                })
                .collect();
            self.layout_width = area.w;
            self.layout_dirty = false;
        }
        let total = self
            .message_layout
            .iter()
            .map(|layout| layout.height)
            .sum::<f32>();
        self.message_scroll = self.message_scroll.clamp(0., (total - area.h).max(0.));
        let mut y = area.y + (area.h - total).min(0.) + self.message_scroll;
        canvas.clip(area);
        for (index, layout) in self.message_layout.iter().enumerate() {
            if y + layout.height >= area.y && y < area.y + area.h {
                if layout.outgoing || layout.sponsor.is_some() {
                    canvas.rect(
                        Rect::new(area.x - 4., y - 6., area.w, layout.height - 16.),
                        10.,
                        0x30241dff,
                    );
                }
                let sender = layout
                    .sponsor
                    .as_ref()
                    .map(|sponsor| sponsor.label.as_str())
                    .unwrap_or(&layout.sender);
                canvas.ellipsis(
                    sender,
                    area.x + 8.,
                    y,
                    area.w - 24.,
                    12,
                    true,
                    if layout.outgoing || layout.sponsor.is_some() {
                        ACCENT
                    } else {
                        colors::MUTED
                    },
                );
                for (index, line) in layout.lines.iter().enumerate() {
                    let text_y = y + 25. + index as f32 * 23.;
                    if text_y + 22. >= area.y && text_y < area.y + area.h {
                        canvas.text(line, area.x + 8., text_y, 16, false, colors::TEXT);
                    }
                }
                let text_bottom = y + 25. + layout.lines.len() as f32 * 23.;
                let fully_visible = y >= area.y
                    && text_bottom <= area.y + area.h
                    && !layout
                        .lines
                        .last()
                        .is_some_and(|line| line.starts_with("… Continue"));
                let text_visible = text_bottom > area.y && y + 25. < area.y + area.h;
                if !layout.id.is_empty()
                    && !self.seen.contains(&layout.id)
                    && text_visible
                    && (layout.sponsor.is_none() || fully_visible)
                {
                    self.visible_receipts.push(layout.id.clone());
                }
                if let Some(sponsor) = &layout.sponsor {
                    let button =
                        Rect::new(area.x + 8., text_bottom + 5., (area.w - 28.).min(260.), 35.);
                    if button.y >= area.y
                        && button.y + button.h <= area.y + area.h
                        && ui.button(
                            canvas,
                            3000 + index as u64,
                            button,
                            &sponsor.button,
                            ButtonStyle::Secondary,
                            !sponsor.url.is_empty(),
                        )
                    {
                        self.link_prompt = Some(LinkPrompt {
                            message: layout.id.clone(),
                            url: sponsor.url.clone(),
                        });
                    }
                }
            }
            y += layout.height;
        }
        if self.messages.is_empty() {
            canvas.text(
                if self.busy() {
                    "Loading messages…"
                } else {
                    "No messages in this page."
                },
                area.x + 8.,
                area.y + 30.,
                15,
                false,
                colors::MUTED,
            );
        }
        canvas.unclip();
        if total > area.h {
            let thumb = (area.h * area.h / total).max(24.).min(area.h);
            let travel = area.h - thumb;
            let position = 1. - self.message_scroll / (total - area.h);
            canvas.rect(
                Rect::new(area.x + area.w - 3., area.y + travel * position, 3., thumb),
                1.5,
                0x806d60aa,
            );
        }
    }

    fn render_connections(&mut self, canvas: &mut Canvas<'_>, ui: &mut Ui, worker: &Worker) {
        let x = RAIL + 36.;
        let width = (canvas.width - x - 390.).clamp(380., 560.);
        canvas.text("Connections", x, 34., 29, true, colors::TEXT);
        canvas.text(
            "Your accounts, with one place to control access.",
            x,
            80.,
            15,
            false,
            colors::MUTED,
        );
        let chip_width = (width - 24.) / 4.;
        for (index, provider) in PROVIDERS.iter().enumerate() {
            let rect = Rect::new(
                x + (index % 4) as f32 * (chip_width + 8.),
                128. + (index / 4) as f32 * 42.,
                chip_width,
                34.,
            );
            if self.form.provider == index {
                canvas.rect(rect, 10., ACCENT_SOFT);
            }
            if ui.button(
                canvas,
                1000 + index as u64,
                rect,
                provider.label,
                ButtonStyle::Quiet,
                true,
            ) {
                self.form.select_provider(index);
            }
        }
        let provider = PROVIDERS[self.form.provider];
        let cell_width = (width - 20.) / 2.;
        input_field(
            canvas,
            ui,
            1100,
            Rect::new(x, 242., cell_width, 42.),
            "Account name",
            &mut self.form.id,
            "personal-matrix",
            false,
            provider.id != "session",
        );
        input_field(
            canvas,
            ui,
            1101,
            Rect::new(x + cell_width + 20., 242., cell_width, 42.),
            "Display label",
            &mut self.form.label,
            "Personal · optional",
            false,
            provider.id != "session",
        );
        for (index, field) in provider.fields.iter().enumerate() {
            let value = self.form.fields.entry(field.key.into()).or_default();
            input_field(
                canvas,
                ui,
                1200 + index as u64,
                Rect::new(
                    x + (index % 2) as f32 * (cell_width + 20.),
                    326. + (index / 2) as f32 * 80.,
                    cell_width,
                    42.,
                ),
                field.label,
                value,
                field.placeholder,
                field.secret,
                true,
            );
        }
        let after = 326. + provider.fields.len().div_ceil(2) as f32 * 80.;
        for (index, line) in wrapped(canvas, provider.description, width, 13, 3)
            .iter()
            .enumerate()
        {
            canvas.text(
                line,
                x,
                after + index as f32 * 20.,
                13,
                false,
                colors::MUTED,
            );
        }
        let ai_y = after + 58.;
        let ai_enabled = !matches!(provider.id, "telegram" | "session");
        if provider.id == "telegram" {
            canvas.text(
                "Telegram content stays in the human client.",
                x,
                ai_y + 7.,
                13,
                false,
                colors::MUTED,
            );
        } else {
            if checkbox(
                canvas,
                ui,
                1300,
                x,
                ai_y,
                "Allow AI to read this account",
                &mut self.form.ai_read,
                ai_enabled,
            ) && !self.form.ai_read
            {
                self.form.ai_write = false;
            }
            checkbox(
                canvas,
                ui,
                1301,
                x,
                ai_y + 38.,
                "Allow AI actions with explicit tool access",
                &mut self.form.ai_write,
                ai_enabled && self.form.ai_read,
            );
        }
        let adding = self
            .pending
            .values()
            .any(|pending| matches!(pending.purpose, Purpose::Add));
        if ui.button(
            canvas,
            1400,
            Rect::new(x, (ai_y + 111.).min(canvas.height - 82.), 174., 43.),
            if adding {
                "Saving…"
            } else {
                "Save connection"
            },
            ButtonStyle::Primary,
            !adding && provider.id != "session",
        ) {
            match self.form.account(self.path.as_ref()) {
                Ok(account) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    match worker.submit(Job::Add { id, account }) {
                        Ok(()) => {
                            self.pending.insert(
                                id,
                                Pending {
                                    account: String::new(),
                                    conversation: String::new(),
                                    purpose: Purpose::Add,
                                },
                            );
                        }
                        Err(error) => self.notify(error.message, true),
                    }
                }
                Err(error) => self.notify(error.message, true),
            }
        }

        let right = x + width + 54.;
        let right_width = (canvas.width - right - 34.).max(100.);
        canvas.text("Your workspace", right, 136., 17, true, colors::TEXT);
        if self.accounts.is_empty() {
            canvas.text("No accounts yet", right, 184., 15, false, colors::MUTED);
            for (index, line) in wrapped(canvas, "Connection details stay on this device. Each account starts with AI access turned off.", right_width, 14, 6).iter().enumerate() {
                canvas.text(line, right, 223. + index as f32 * 23., 14, false, colors::MUTED);
            }
        } else {
            let mut manage = None;
            self.management_scroll = self
                .management_scroll
                .min(self.accounts.len().saturating_sub(5));
            for (row, (index, account)) in self
                .accounts
                .iter()
                .enumerate()
                .skip(self.management_scroll)
                .take(5)
                .enumerate()
            {
                let rect = Rect::new(right - 8., 174. + row as f32 * 64., right_width + 8., 56.);
                if self.manage_account == account.id {
                    canvas.rect(rect, 10., ACCENT_SOFT);
                }
                if ui.button(
                    canvas,
                    1500 + index as u64,
                    rect,
                    "",
                    ButtonStyle::Quiet,
                    true,
                ) {
                    manage = Some(index);
                }
                canvas.ellipsis(
                    &account.label,
                    right + 5.,
                    rect.y + 7.,
                    right_width - 10.,
                    15,
                    false,
                    colors::TEXT,
                );
                canvas.ellipsis(
                    &account.provider,
                    right + 5.,
                    rect.y + 31.,
                    right_width - 10.,
                    12,
                    false,
                    colors::MUTED,
                );
            }
            if let Some(index) = manage {
                let account = &self.accounts[index];
                self.manage_account = account.id.clone();
                self.manage_read = account.ai_read;
                self.manage_write = account.ai_write;
            }
            let y = 198. + self.accounts.len().min(5) as f32 * 64.;
            if let Some(account) = self
                .accounts
                .iter()
                .find(|account| account.id == self.manage_account)
                .cloned()
            {
                canvas.text("AI access", right, y, 16, true, colors::TEXT);
                let enabled = account.provider != "telegram";
                if checkbox(
                    canvas,
                    ui,
                    1600,
                    right,
                    y + 38.,
                    "Read messages",
                    &mut self.manage_read,
                    enabled,
                ) && !self.manage_read
                {
                    self.manage_write = false;
                }
                checkbox(
                    canvas,
                    ui,
                    1601,
                    right,
                    y + 76.,
                    "Perform actions",
                    &mut self.manage_write,
                    enabled && self.manage_read,
                );
                if ui.button(
                    canvas,
                    1602,
                    Rect::new(right, y + 126., right_width.min(160.), 38.),
                    "Save controls",
                    ButtonStyle::Secondary,
                    enabled && !self.busy(),
                ) {
                    let id = self.next_id;
                    self.next_id += 1;
                    match worker.submit(Job::Permissions {
                        id,
                        account: account.id,
                        read: self.manage_read,
                        write: self.manage_write,
                    }) {
                        Ok(()) => {
                            self.pending.insert(
                                id,
                                Pending {
                                    account: self.manage_account.clone(),
                                    conversation: String::new(),
                                    purpose: Purpose::Permissions,
                                },
                            );
                        }
                        Err(error) => self.notify(error.message, true),
                    }
                }
            } else {
                canvas.ellipsis(
                    "Select an account to manage AI access.",
                    right,
                    y,
                    right_width,
                    13,
                    false,
                    colors::MUTED,
                );
            }
        }
    }

    fn render_tools(&mut self, canvas: &mut Canvas<'_>, ui: &mut Ui, worker: &Worker) {
        let x = RAIL + 36.;
        let width = (canvas.width - x - 364.).max(380.);
        let right = x + width + 54.;
        let right_width = (canvas.width - right - 32.).max(100.);
        canvas.text("Platform tools", x, 34., 29, true, colors::TEXT);
        canvas.text(
            "Authorization steps and platform-specific capabilities.",
            x,
            80.,
            15,
            false,
            colors::MUTED,
        );
        let label = self
            .selected()
            .map(|account| account.label.as_str())
            .unwrap_or("Select an account")
            .to_string();
        if ui.button(
            canvas,
            2000,
            Rect::new(x, 122., width.min(280.), 40.),
            &label,
            ButtonStyle::Secondary,
            !self.accounts.is_empty(),
        ) {
            let current = self
                .accounts
                .iter()
                .position(|account| account.id == self.selected_account)
                .unwrap_or(0);
            self.select_account((current + 1) % self.accounts.len(), worker);
        }
        input_field(
            canvas,
            ui,
            2001,
            Rect::new(x, 212., 220., 42.),
            "Unified operation",
            &mut self.native_operation,
            "native.invoke",
            false,
            true,
        );
        input_field(
            canvas,
            ui,
            2002,
            Rect::new(x + 240., 212., width - 240., 42.),
            "Platform method",
            &mut self.native_method,
            "Method from the platform documentation",
            false,
            self.native_operation.trim() == "native.invoke",
        );
        let auth = self.auth_prompt.as_ref().filter(|prompt| {
            self.native_operation.trim() == "native.invoke"
                && self.native_method.trim() == prompt.method
        });
        if let Some(prompt) = auth {
            input_field(
                canvas,
                ui,
                2003,
                Rect::new(x, 308., width, 44.),
                prompt.label,
                &mut self.auth_value,
                prompt.label,
                prompt.secret,
                true,
            );
        } else {
            input_field(
                canvas,
                ui,
                2003,
                Rect::new(x, 308., width, 44.),
                "Parameters · JSON object",
                &mut self.native_params,
                "{}",
                false,
                true,
            );
        }
        if ui.button(
            canvas,
            2004,
            Rect::new(x, 375., 152., 42.),
            "Run request",
            ButtonStyle::Primary,
            !self.selected_account.is_empty() && !self.busy(),
        ) {
            self.execute_native(worker);
        }
        canvas.ellipsis(
            "Runs only when you choose Run request.",
            x + 174.,
            388.,
            width - 174.,
            12,
            false,
            colors::MUTED,
        );
        canvas.text("Response", x, 449., 16, true, colors::TEXT);
        if ui.button(
            canvas,
            2005,
            Rect::new(x + width - 104., 439., 104., 34.),
            "Copy result",
            ButtonStyle::Quiet,
            !self.native_output.is_empty(),
        ) {
            ui.clipboard = Some(self.native_output.clone());
        }
        let result_rect = Rect::new(x, 488., width, (canvas.height - 544.).max(40.));
        let output = if self.native_output.is_empty() {
            "The platform response will appear here."
        } else {
            &self.native_output
        };
        let lines = wrapped(canvas, output, width - 18., 14, 4096);
        let visible = (result_rect.h / 22.).max(1.) as usize;
        self.native_scroll = self.native_scroll.min(lines.len().saturating_sub(visible));
        canvas.clip(result_rect);
        for (index, line) in lines
            .iter()
            .skip(self.native_scroll)
            .take(visible)
            .enumerate()
        {
            canvas.text(
                line,
                x,
                result_rect.y + index as f32 * 22.,
                14,
                false,
                colors::MUTED,
            );
        }
        canvas.unclip();

        canvas.text("AI, on your terms", right, 136., 18, true, colors::TEXT);
        let help = "Enable account access in Connections. MCP starts in read mode. Sending also requires --allow-write.";
        for (index, line) in wrapped(canvas, help, right_width, 14, 8).iter().enumerate() {
            canvas.text(
                line,
                right,
                180. + index as f32 * 23.,
                14,
                false,
                colors::MUTED,
            );
        }
        canvas.text("zchat mcp", right, 315., 15, true, colors::TEXT);
        if ui.button(
            canvas,
            2010,
            Rect::new(right, 347., right_width.min(170.), 36.),
            "Copy MCP command",
            ButtonStyle::Secondary,
            true,
        ) {
            ui.clipboard = Some(if let Some(path) = &self.path {
                let path = path.to_string_lossy();
                #[cfg(windows)]
                let quoted = format!("'{}'", path.replace('\'', "''"));
                #[cfg(not(windows))]
                let quoted = format!("'{}'", path.replace('\'', "'\\''"));
                format!("zchat --config {quoted} mcp")
            } else {
                "zchat mcp".into()
            });
        }
        canvas.text("Native plugins", right, 436., 18, true, colors::TEXT);
        for (index, line) in wrapped(canvas, "Add providers through the versioned zplugin interface. Only load native libraries you trust.", right_width, 14, 8).iter().enumerate() { canvas.text(line, right, 479. + index as f32 * 23., 14, false, colors::MUTED); }
        let count = self.plugins.as_array().map_or(0, Vec::len);
        canvas.text(
            &format!(
                "{count} plugin registration{}",
                if count == 1 { "" } else { "s" }
            ),
            right,
            594.,
            13,
            false,
            colors::MUTED,
        );
        if ui.button(
            canvas,
            2011,
            Rect::new(right, 627., right_width.min(170.), 36.),
            "Copy plugin help",
            ButtonStyle::Quiet,
            true,
        ) {
            ui.clipboard = Some("zchat plugins --help".into());
        }
    }

    fn render_link_prompt(&mut self, canvas: &mut Canvas<'_>, ui: &mut Ui, worker: &Worker) {
        let Some(prompt) = &self.link_prompt else {
            return;
        };
        let message = prompt.message.clone();
        let url = prompt.url.clone();
        let width = (canvas.width - 100.).min(680.);
        let x = (canvas.width - width) / 2.;
        let y = canvas.height * 0.26;
        canvas.text("Open sponsored destination?", x, y, 27, true, colors::TEXT);
        canvas.text(
            "Review the address before continuing.",
            x,
            y + 47.,
            15,
            false,
            colors::MUTED,
        );
        let lines = wrapped(canvas, &url, width, 15, 8);
        for (index, line) in lines.iter().enumerate() {
            canvas.text(line, x, y + 100. + index as f32 * 23., 15, false, ACCENT);
        }
        let button_y = y + 134. + lines.len() as f32 * 23.;
        if ui.button(
            canvas,
            4000,
            Rect::new(x, button_y, 140., 43.),
            "Open link",
            ButtonStyle::Primary,
            !self.busy(),
        ) {
            let id = self.next_id;
            self.next_id += 1;
            match worker.submit(Job::OpenSponsor {
                id,
                account: self.selected_account.clone(),
                conversation: self.selected_conversation.clone(),
                message,
                url: url.clone(),
            }) {
                Ok(()) => {
                    self.pending.insert(
                        id,
                        Pending {
                            account: self.selected_account.clone(),
                            conversation: self.selected_conversation.clone(),
                            purpose: Purpose::OpenSponsor,
                        },
                    );
                    self.link_prompt = None;
                }
                Err(error) => self.notify(error.message, true),
            }
        }
        if ui.button(
            canvas,
            4001,
            Rect::new(x + 158., button_y, 120., 43.),
            "Cancel",
            ButtonStyle::Quiet,
            true,
        ) {
            self.link_prompt = None;
        }
        if ui.button(
            canvas,
            4002,
            Rect::new(x + width - 120., button_y, 120., 43.),
            "Copy link",
            ButtonStyle::Quiet,
            true,
        ) {
            ui.clipboard = Some(url);
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "native widgets carry explicit geometry and input state"
)]
fn input_field(
    canvas: &mut Canvas<'_>,
    ui: &mut Ui,
    id: u64,
    rect: Rect,
    label: &str,
    value: &mut String,
    placeholder: &str,
    secret: bool,
    enabled: bool,
) {
    canvas.text(label, rect.x, rect.y - 25., 12, false, colors::MUTED);
    ui.input(canvas, id, rect, value, placeholder, secret, enabled);
}

#[allow(
    clippy::too_many_arguments,
    reason = "native widgets carry explicit geometry and input state"
)]
fn checkbox(
    canvas: &mut Canvas<'_>,
    ui: &mut Ui,
    id: u64,
    x: f32,
    y: f32,
    label: &str,
    value: &mut bool,
    enabled: bool,
) -> bool {
    let clicked = ui.button(
        canvas,
        id,
        Rect::new(x, y, 26., 26.),
        if *value { "✓" } else { "" },
        ButtonStyle::Secondary,
        enabled,
    );
    if clicked {
        *value = !*value;
    }
    canvas.text(
        label,
        x + 39.,
        y + 4.,
        13,
        false,
        if enabled { colors::TEXT } else { colors::MUTED },
    );
    clicked
}

fn draw_connections_mark(canvas: &mut Canvas<'_>, x: f32, y: f32) {
    canvas.line((x + 20., y + 40.), (x + 83., y + 23.), 1.5, 0x675044ff);
    canvas.line((x + 20., y + 40.), (x + 68., y + 79.), 1.5, 0x675044ff);
    canvas.line((x + 83., y + 23.), (x + 68., y + 79.), 1.5, 0x675044ff);
    for (left, top, size, color) in [
        (x, y + 21., 40., 0x3a2a22ff),
        (x + 64., y + 4., 38., 0x342c23ff),
        (x + 51., y + 62., 34., 0x30251fff),
    ] {
        canvas.rect(Rect::new(left, top, size, size), size / 2., color);
        canvas.rect(
            Rect::new(left + size / 2. - 3., top + size / 2. - 3., 6., 6.),
            3.,
            ACCENT,
        );
    }
}

fn wrapped(canvas: &Canvas<'_>, text: &str, width: f32, size: i32, maximum: usize) -> Vec<String> {
    wrap_with(text, width, maximum, |text| {
        canvas.measure(text, size, false)
    })
}

/// Find UTF-8-safe measured line prefixes. At most 512 candidate characters are
/// examined per line, so a long unbroken word does not cause quadratic scanning.
fn wrap_with(
    text: &str,
    width: f32,
    maximum: usize,
    mut measure: impl FnMut(&str) -> f32,
) -> Vec<String> {
    let mut output = Vec::new();
    let width = width.max(1.);
    let mut paragraphs = text.split('\n').peekable();
    while let Some(paragraph) = paragraphs.next() {
        if paragraph.is_empty() {
            output.push(String::new());
        }
        let mut rest = paragraph;
        while !rest.is_empty() {
            if output.len() >= maximum {
                output.push("… Continue through the platform tools for the full content.".into());
                return output;
            }
            let mut boundaries: Vec<usize> = rest
                .char_indices()
                .take(513)
                .map(|(index, _)| index)
                .collect();
            if boundaries.len() <= 512 {
                boundaries.push(rest.len());
            }
            let (mut low, mut high) = (1, boundaries.len() - 1);
            let mut fitting = 0;
            while low <= high {
                let middle = low + (high - low) / 2;
                if measure(&rest[..boundaries[middle]]) <= width {
                    fitting = middle;
                    low = middle + 1;
                } else {
                    high = middle - 1;
                }
            }
            let mut end = boundaries[fitting.max(1)];
            if end < rest.len()
                && let Some((space, _)) = rest[..end]
                    .char_indices()
                    .rev()
                    .find(|(index, character)| *index > 0 && character.is_whitespace())
            {
                end = space;
            }
            output.push(rest[..end].trim_end().into());
            rest = rest[end..].trim_start();
        }
        if output.len() >= maximum && paragraphs.peek().is_some() {
            output.push("… Continue through the platform tools for the full content.".into());
            return output;
        }
    }
    if output.is_empty() {
        output.push(String::new());
    }
    output
}

/// Launch the real native window. `smoke_seconds` bounds screenshot/installation
/// checks; it does not create sample conversations or send any messages.
pub fn run(config_path: Option<PathBuf>, smoke_seconds: Option<u64>) -> Result<(), ChatError> {
    let worker = Worker::start(config_path.clone())?;
    let mut app = App::new(config_path);
    let mut window =
        Window::new("zchat", 1180, 780).map_err(|error| ChatError::new("window_open", error))?;
    window.set_theme(Theme::Dark);
    let mut palette = Palette::for_theme(Theme::Dark);
    palette.background = 0x121110ff;
    palette.surface = 0x201c19ff;
    palette.text = 0xeee8dfff;
    palette.muted = 0xafa197ff;
    palette.line = 0x3d332dff;
    palette.accent = ACCENT;
    palette.accent_hover = 0xe7b89bff;
    palette.accent_soft = ACCENT_SOFT;
    palette.hover = 0x2a2420ff;
    palette.success = 0xd9b18bff;
    palette.success_soft = 0x34291fff;
    palette.danger = 0xe4a08fff;
    palette.danger_soft = 0x3c2924ff;
    palette.on_accent = 0x211712ff;
    palette.disabled_surface = 0x1a1715ff;
    palette.disabled_text = 0x766a61ff;
    window.set_palette(palette);
    let mut ui = Ui::default();
    let started = Instant::now();
    let mut dirty = true;
    let mut next_event = None;
    loop {
        if smoke_seconds.is_some_and(|seconds| started.elapsed() >= Duration::from_secs(seconds)) {
            break;
        }
        while let Ok(reply) = worker.output.try_recv() {
            app.process_reply(reply, &worker);
            dirty = true;
        }
        if let Some(event) = next_event.take() {
            if matches!(event, Event::Close) {
                break;
            }
            match event {
                Event::Move(x, y) => {
                    app.pointer = (x, y);
                    dirty |= ui.pointer_changed(x, y);
                }
                Event::Scroll(amount) => {
                    let step = if amount >= 0 { 1 } else { -1 };
                    match app.page {
                        Page::Inbox if app.account_menu && app.pointer.0 < RAIL + SIDEBAR => {
                            app.account_scroll = if step > 0 {
                                app.account_scroll.saturating_sub(1)
                            } else {
                                app.account_scroll.saturating_add(1)
                            }
                        }
                        Page::Inbox if app.pointer.0 < RAIL + SIDEBAR => {
                            app.conversation_scroll = if step > 0 {
                                app.conversation_scroll.saturating_sub(2)
                            } else {
                                app.conversation_scroll.saturating_add(2)
                            }
                        }
                        Page::Inbox if !app.selected_conversation.is_empty() => {
                            app.message_scroll = (app.message_scroll + step as f32 * 84.).max(0.)
                        }
                        Page::Inbox => {
                            app.conversation_scroll = if step > 0 {
                                app.conversation_scroll.saturating_sub(2)
                            } else {
                                app.conversation_scroll.saturating_add(2)
                            }
                        }
                        Page::Tools => {
                            app.native_scroll = if step > 0 {
                                app.native_scroll.saturating_sub(3)
                            } else {
                                app.native_scroll.saturating_add(3)
                            }
                        }
                        Page::Connections => {
                            app.management_scroll = if step > 0 {
                                app.management_scroll.saturating_sub(1)
                            } else {
                                app.management_scroll.saturating_add(1)
                            }
                        }
                    }
                    dirty = true;
                }
                Event::Down(x, y) => {
                    app.pointer = (x, y);
                    next_event = Some(Event::Down(x, y));
                    dirty = true;
                }
                Event::Up(x, y) => {
                    app.pointer = (x, y);
                    next_event = Some(Event::Up(x, y));
                    dirty = true;
                }
                other => {
                    next_event = Some(other);
                    dirty = true;
                }
            }
        }
        if app.page == Page::Inbox
            && app.poll_enabled
            && !app.busy()
            && app.last_poll.elapsed() >= POLL_INTERVAL
        {
            app.last_poll = Instant::now();
            app.call(
                &worker,
                Purpose::Events,
                "events.poll",
                obj([("limit", 50u64.into()), ("timeout_ms", 0u64.into())]),
            );
            dirty = true;
        }
        if app.typing_active
            && app.last_edit.elapsed() >= Duration::from_secs(5)
            && !app
                .pending
                .values()
                .any(|pending| matches!(pending.purpose, Purpose::Typing))
        {
            app.set_typing(&worker, false);
        }
        if dirty {
            let link_prompt_before = app.link_prompt.is_some();
            ui.begin(next_event.take());
            let mut canvas = window.begin(colors::BG);
            app.render(&mut canvas, &mut ui, &worker);
            ui.end();
            drop(canvas);
            window.present();
            if app.page == Page::Inbox {
                app.confirm_visible(&worker);
            }
            if let Some(text) = ui.clipboard.take() {
                window.copy(&text);
            }
            dirty = link_prompt_before != app.link_prompt.is_some();
        }
        if dirty {
            continue;
        }
        let wait = if app.busy() { 100 } else { 500 };
        next_event = window.event(wait);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_form_keeps_ai_off_and_marks_secrets() {
        let mut form = ConnectionForm {
            id: "personal".into(),
            ..ConnectionForm::default()
        };
        form.fields
            .insert("access_token".into(), "private-token".into());
        let account = form.account(None).unwrap();
        assert!(!account.ai_read && !account.ai_write);
        assert!(!account.public().stringify().contains("private-token"));
        for provider in PROVIDERS {
            for field in provider.fields {
                if matches!(
                    field.key,
                    "password" | "access_token" | "secret" | "api_hash"
                ) {
                    assert!(field.secret);
                }
            }
        }
    }

    #[test]
    fn session_does_not_create_a_fake_connection() {
        let mut form = ConnectionForm::default();
        form.select_provider(6);
        form.id = "session".into();
        assert_eq!(form.account(None).unwrap_err().code, "backend_unavailable");
    }

    #[test]
    fn telegram_form_sets_private_directory_and_disables_ai() {
        let mut form = ConnectionForm::default();
        form.select_provider(1);
        form.id = "tg".into();
        form.ai_read = true;
        form.ai_write = true;
        form.fields.insert("api_id".into(), "12345".into());
        form.fields.insert("api_hash".into(), "private".into());
        let directory = std::env::temp_dir().join("zchat-gui");
        let account = form.account(Some(&directory.join("config.json"))).unwrap();
        assert!(!account.ai_read && !account.ai_write);
        let expected = directory.join("telegram-tg");
        assert_eq!(
            account.config.get_str("database_directory"),
            Some(expected.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn wrapping_preserves_unicode_and_long_words() {
        let text = "界🚀雪a".repeat(500);
        let lines = wrap_with(&text, 8., 1000, |value| value.chars().count() as f32);
        assert_eq!(lines.concat(), text);
        assert!(lines.iter().all(|line| line.chars().count() <= 8));
        let paragraphs = wrap_with("one two\n\nthree", 20., 20, |value| value.len() as f32);
        assert_eq!(paragraphs, vec!["one two", "", "three"]);
    }

    #[test]
    fn provider_names_and_input_ids_are_unique() {
        let names: BTreeSet<_> = PROVIDERS.iter().map(|provider| provider.id).collect();
        assert_eq!(names.len(), PROVIDERS.len());
        for provider in PROVIDERS {
            assert_eq!(
                provider
                    .fields
                    .iter()
                    .map(|field| field.key)
                    .collect::<BTreeSet<_>>()
                    .len(),
                provider.fields.len()
            );
        }
    }

    #[test]
    fn secret_fields_preserve_significant_whitespace() {
        let mut form = ConnectionForm {
            id: "personal".into(),
            ..ConnectionForm::default()
        };
        form.fields
            .insert("access_token".into(), " token with spaces ".into());
        assert_eq!(
            form.account(None).unwrap().config.get_str("access_token"),
            Some(" token with spaces ")
        );
    }

    #[test]
    fn sponsored_payload_retains_label_title_text_and_disclosures() {
        let value = parse(r#"{"sponsored_messages":{"messages_between":0,"messages":[{"message_id":42,"is_recommended":true,"title":"Sponsor title","button_text":"Visit","content":{"@type":"messageText","text":{"text":"Full advertisement text"}},"sponsor":{"url":"https://example.org/","info":"Sponsor disclosure"},"additional_info":"Additional disclosure"}]}}"#).unwrap();
        let message = sponsored_message(&value).unwrap();
        assert_eq!(message.id, "42");
        assert_eq!(message.sponsor.as_ref().unwrap().label, "Recommended");
        for expected in [
            "Sponsor title",
            "Full advertisement text",
            "Sponsor disclosure",
            "Additional disclosure",
        ] {
            assert!(message.text.contains(expected));
        }
    }

    #[test]
    fn configuration_success_is_not_reported_as_connected() {
        assert_eq!(account_state(&Value::object()), "Account checked");
        assert_eq!(
            account_state(&obj([(
                "authorization_state",
                obj([("@type", "authorizationStateWaitCode".into())])
            )])),
            "Verification code required · Tools"
        );
    }
}
