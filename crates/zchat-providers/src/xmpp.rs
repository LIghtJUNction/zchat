//! XMPP through libstrophe's public 0.12 C ABI. TLS, certificate validation,
//! SASL, XML parsing and stream management stay in the maintained native client.
use crate::util::*;
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{CStr, CString, c_char, c_int, c_long, c_ulong, c_void},
    ptr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use zchat_core::{ChatError, Value};
use zsys::dylib::Library;

type Handle = *mut c_void;
type ConnCallback = unsafe extern "C" fn(Handle, c_int, c_int, Handle, Handle);
type StanzaCallback = unsafe extern "C" fn(Handle, Handle, Handle) -> c_int;
const MAX_STANZA_BYTES: usize = 256 * 1024;
const MAX_HISTORY_BYTES: usize = 512 * 1024;
const NS_ROSTER: &CStr = c"jabber:iq:roster";
const NS_DISCO: &CStr = c"http://jabber.org/protocol/disco#info";
const NS_MARKERS: &CStr = c"urn:xmpp:chat-markers:0";

#[derive(Clone, Copy)]
struct Api {
    ctx_new: unsafe extern "C" fn(*const c_void, *const c_void) -> Handle,
    ctx_free: unsafe extern "C" fn(Handle),
    free: unsafe extern "C" fn(Handle, Handle),
    conn_new: unsafe extern "C" fn(Handle) -> Handle,
    conn_release: unsafe extern "C" fn(Handle) -> c_int,
    set_jid: unsafe extern "C" fn(Handle, *const c_char),
    set_pass: unsafe extern "C" fn(Handle, *const c_char),
    set_cafile: unsafe extern "C" fn(Handle, *const c_char),
    set_flags: unsafe extern "C" fn(Handle, c_long) -> c_int,
    is_secured: unsafe extern "C" fn(Handle) -> c_int,
    bound_jid: unsafe extern "C" fn(Handle) -> *const c_char,
    connect: unsafe extern "C" fn(Handle, *const c_char, u16, ConnCallback, Handle) -> c_int,
    disconnect: unsafe extern "C" fn(Handle),
    run_once: unsafe extern "C" fn(Handle, c_ulong),
    handler_add: unsafe extern "C" fn(
        Handle,
        StanzaCallback,
        *const c_char,
        *const c_char,
        *const c_char,
        Handle,
    ),
    stanza_parse: unsafe extern "C" fn(Handle, *const c_char) -> Handle,
    stanza_release: unsafe extern "C" fn(Handle) -> c_int,
    stanza_xml: unsafe extern "C" fn(Handle, *mut *mut c_char, *mut usize) -> c_int,
    send: unsafe extern "C" fn(Handle, Handle),
    name: unsafe extern "C" fn(Handle) -> *const c_char,
    attr: unsafe extern "C" fn(Handle, *const c_char) -> *const c_char,
    set_id: unsafe extern "C" fn(Handle, *const c_char) -> c_int,
    child_ns: unsafe extern "C" fn(Handle, *const c_char) -> Handle,
    children: unsafe extern "C" fn(Handle) -> Handle,
    next: unsafe extern "C" fn(Handle) -> Handle,
    body: unsafe extern "C" fn(Handle) -> *mut c_char,
    uuid: unsafe extern "C" fn(Handle) -> *mut c_char,
}

static API: OnceLock<Mutex<Option<(String, Api)>>> = OnceLock::new();

impl Api {
    fn load(path: &str) -> Result<Self, ChatError> {
        let mut cache = API.get_or_init(|| Mutex::new(None)).lock().map_err(|_| {
            ChatError::new(
                "runtime_poisoned",
                "libstrophe initialization lock was poisoned",
            )
        })?;
        if let Some((loaded, api)) = cache.as_ref() {
            return if loaded == path {
                Ok(*api)
            } else {
                Err(ChatError::new(
                    "invalid_config",
                    "All XMPP accounts in one process must use the same libstrophe library_path",
                ))
            };
        }
        let path_c = c_string(path)?;
        // SAFETY: configured libraries are trusted native backends. zsys pins
        // the image on Drop; no native function pointer outlives its image.
        let library = unsafe { Library::open(&path_c) }.map_err(|_| {
            ChatError::new(
                "backend_unavailable",
                "Install libstrophe 0.12 or newer, or configure library_path",
            )
        })?;
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                let address = library
                    .symbol(
                        CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes())
                            .expect("literal C symbol"),
                    )
                    .map_err(|_| {
                        ChatError::new(
                            "backend_incompatible",
                            concat!("libstrophe is missing ", $name),
                        )
                    })?;
                // SAFETY: these exact declarations are from strophe.h 0.12.
                unsafe { std::mem::transmute::<*mut c_void, $ty>(address) }
            }};
        }
        let initialize = symbol!("xmpp_initialize", unsafe extern "C" fn());
        let version = symbol!(
            "xmpp_version_check",
            unsafe extern "C" fn(c_int, c_int) -> c_int
        );
        // SAFETY: no contexts exist before first initialization under this lock.
        if unsafe { version(0, 12) } == 0 {
            return Err(ChatError::new(
                "backend_incompatible",
                "libstrophe 0.12 or newer is required",
            ));
        }
        let api = Self {
            ctx_new: symbol!(
                "xmpp_ctx_new",
                unsafe extern "C" fn(*const c_void, *const c_void) -> Handle
            ),
            ctx_free: symbol!("xmpp_ctx_free", unsafe extern "C" fn(Handle)),
            free: symbol!("xmpp_free", unsafe extern "C" fn(Handle, Handle)),
            conn_new: symbol!("xmpp_conn_new", unsafe extern "C" fn(Handle) -> Handle),
            conn_release: symbol!("xmpp_conn_release", unsafe extern "C" fn(Handle) -> c_int),
            set_jid: symbol!(
                "xmpp_conn_set_jid",
                unsafe extern "C" fn(Handle, *const c_char)
            ),
            set_pass: symbol!(
                "xmpp_conn_set_pass",
                unsafe extern "C" fn(Handle, *const c_char)
            ),
            set_cafile: symbol!(
                "xmpp_conn_set_cafile",
                unsafe extern "C" fn(Handle, *const c_char)
            ),
            set_flags: symbol!(
                "xmpp_conn_set_flags",
                unsafe extern "C" fn(Handle, c_long) -> c_int
            ),
            is_secured: symbol!(
                "xmpp_conn_is_secured",
                unsafe extern "C" fn(Handle) -> c_int
            ),
            bound_jid: symbol!(
                "xmpp_conn_get_bound_jid",
                unsafe extern "C" fn(Handle) -> *const c_char
            ),
            connect: symbol!(
                "xmpp_connect_client",
                unsafe extern "C" fn(Handle, *const c_char, u16, ConnCallback, Handle) -> c_int
            ),
            disconnect: symbol!("xmpp_disconnect", unsafe extern "C" fn(Handle)),
            run_once: symbol!("xmpp_run_once", unsafe extern "C" fn(Handle, c_ulong)),
            handler_add: symbol!(
                "xmpp_handler_add",
                unsafe extern "C" fn(
                    Handle,
                    StanzaCallback,
                    *const c_char,
                    *const c_char,
                    *const c_char,
                    Handle,
                )
            ),
            stanza_parse: symbol!(
                "xmpp_stanza_new_from_string",
                unsafe extern "C" fn(Handle, *const c_char) -> Handle
            ),
            stanza_release: symbol!("xmpp_stanza_release", unsafe extern "C" fn(Handle) -> c_int),
            stanza_xml: symbol!(
                "xmpp_stanza_to_text",
                unsafe extern "C" fn(Handle, *mut *mut c_char, *mut usize) -> c_int
            ),
            send: symbol!("xmpp_send", unsafe extern "C" fn(Handle, Handle)),
            name: symbol!(
                "xmpp_stanza_get_name",
                unsafe extern "C" fn(Handle) -> *const c_char
            ),
            attr: symbol!(
                "xmpp_stanza_get_attribute",
                unsafe extern "C" fn(Handle, *const c_char) -> *const c_char
            ),
            set_id: symbol!(
                "xmpp_stanza_set_id",
                unsafe extern "C" fn(Handle, *const c_char) -> c_int
            ),
            child_ns: symbol!(
                "xmpp_stanza_get_child_by_ns",
                unsafe extern "C" fn(Handle, *const c_char) -> Handle
            ),
            children: symbol!(
                "xmpp_stanza_get_children",
                unsafe extern "C" fn(Handle) -> Handle
            ),
            next: symbol!(
                "xmpp_stanza_get_next",
                unsafe extern "C" fn(Handle) -> Handle
            ),
            body: symbol!(
                "xmpp_message_get_body",
                unsafe extern "C" fn(Handle) -> *mut c_char
            ),
            uuid: symbol!("xmpp_uuid_gen", unsafe extern "C" fn(Handle) -> *mut c_char),
        };
        unsafe {
            initialize();
        }
        *cache = Some((path.to_owned(), api));
        Ok(api)
    }

    fn attr(&self, stanza: Handle, name: &CStr) -> String {
        // SAFETY: stanza is owned by the active callback or local Stanza guard.
        unsafe { copy_c_string((self.attr)(stanza, name.as_ptr())) }
    }

    fn xml(&self, ctx: Handle, stanza: Handle) -> Result<String, ChatError> {
        let mut bytes = ptr::null_mut();
        let mut length = 0;
        // SAFETY: output pointers are writable; the matching context frees the
        // buffer allocated by libstrophe, including on a size-limit failure.
        let result = unsafe { (self.stanza_xml)(stanza, &mut bytes, &mut length) };
        if result != 0 || bytes.is_null() {
            return Err(ChatError::new(
                "backend_error",
                "Cannot serialize XMPP stanza",
            ));
        }
        let output = if length > MAX_STANZA_BYTES {
            Err(ChatError::new(
                "payload_too_large",
                "XMPP stanza exceeds 256 KiB",
            ))
        } else {
            unsafe { std::str::from_utf8(std::slice::from_raw_parts(bytes.cast::<u8>(), length)) }
                .map(str::to_owned)
                .map_err(|_| ChatError::new("invalid_response", "XMPP stanza is not UTF-8"))
        };
        unsafe {
            (self.free)(ctx, bytes.cast());
        }
        output
    }

    fn unique_id(&self, ctx: Handle) -> Result<String, ChatError> {
        let pointer = unsafe { (self.uuid)(ctx) };
        if pointer.is_null() {
            return Err(ChatError::new(
                "backend_error",
                "Could not allocate an XMPP message ID",
            ));
        }
        let id = unsafe { copy_c_string(pointer) };
        unsafe {
            (self.free)(ctx, pointer.cast());
        }
        Ok(id)
    }
}

struct State {
    api: Api,
    ctx: Handle,
    jid: String,
    connected: bool,
    failure: Option<c_int>,
    poisoned: bool,
    events: EventQueue,
    conversations: BTreeMap<String, Value>,
    history: VecDeque<(u64, Value, usize)>,
    history_bytes: usize,
    sequence: u64,
    pending: Option<(String, Option<String>)>,
    reply: Option<Value>,
}

impl State {
    fn remember_message(&mut self, mut message: Value) {
        self.sequence = self.sequence.saturating_add(1);
        if text(&message, "id").is_empty() {
            set(
                &mut message,
                "id",
                string(format!("local-{}", self.sequence)),
            );
            set(&mut message, "local_id", Value::Bool(true));
        }
        let room = text(&message, "conversation_id").to_owned();
        if self.conversations.len() < 4096 && !room.is_empty() {
            self.conversations
                .entry(room.clone())
                .or_insert_with(|| object([("id", string(&room)), ("title", string(&room))]));
        }
        if let Ok(encoded) = message.try_stringify() {
            let size = encoded.len();
            if size <= MAX_HISTORY_BYTES {
                while self.history.len() >= 256 || self.history_bytes + size > MAX_HISTORY_BYTES {
                    if let Some((_, _, removed)) = self.history.pop_front() {
                        self.history_bytes -= removed;
                    } else {
                        break;
                    }
                }
                self.history_bytes += size;
                self.history
                    .push_back((self.sequence, message.clone(), size));
            }
        }
        self.events
            .push(object([("kind", string("message")), ("message", message)]));
    }

    fn handle_stanza(&mut self, conn: Handle, stanza: Handle) -> Result<(), ChatError> {
        let api = self.api;
        let name = unsafe { copy_c_string((api.name)(stanza)) };
        let from = api.attr(stanza, c"from");
        let id = api.attr(stanza, c"id");
        let kind = api.attr(stanza, c"type");
        let xml = api.xml(self.ctx, stanza)?;
        if name == "message" {
            let body = unsafe { (api.body)(stanza) };
            if !body.is_null() {
                let text_body = unsafe { copy_c_string(body) };
                unsafe {
                    (api.free)(self.ctx, body.cast());
                }
                let marker = unsafe { (api.child_ns)(stanza, NS_MARKERS.as_ptr()) };
                let markable =
                    !marker.is_null() && unsafe { copy_c_string((api.name)(marker)) } == "markable";
                self.remember_message(object([
                    ("id", string(&id)),
                    ("conversation_id", string(bare_jid(&from))),
                    ("sender", string(&from)),
                    ("text", string(text_body)),
                    ("type", string(&kind)),
                    ("markable", Value::Bool(markable)),
                    ("outgoing", Value::Bool(false)),
                    ("source", string("live")),
                ]));
            }
        }
        let mut response = object([
            ("kind", string(&name)),
            ("type", string(&kind)),
            ("id", string(&id)),
            ("from", string(&from)),
            ("xml", string(xml)),
        ]);
        if name == "iq" {
            let roster = unsafe { (api.child_ns)(stanza, NS_ROSTER.as_ptr()) };
            let server_origin = iq_sender_allowed(&from, None, &self.jid);
            if !roster.is_null() && server_origin && matches!(kind.as_str(), "result" | "set") {
                let mut child = unsafe { (api.children)(roster) };
                for _ in 0..4096 {
                    if child.is_null() {
                        break;
                    }
                    let jid = api.attr(child, c"jid");
                    if !jid.is_empty() && jid.len() <= 3071 {
                        if api.attr(child, c"subscription") == "remove" {
                            self.conversations.remove(&jid);
                        } else if self.conversations.len() < 4096
                            || self.conversations.contains_key(&jid)
                        {
                            let title = api.attr(child, c"name");
                            self.conversations.insert(
                                jid.clone(),
                                object([
                                    ("id", string(&jid)),
                                    (
                                        "title",
                                        string(if title.is_empty() {
                                            jid
                                        } else {
                                            title.chars().take(256).collect()
                                        }),
                                    ),
                                ]),
                            );
                        }
                    }
                    child = unsafe { (api.next)(child) };
                }
                if kind == "set" {
                    send_callback_result(api, self.ctx, conn, &id, &from, "")?;
                }
            }
            let disco = unsafe { (api.child_ns)(stanza, NS_DISCO.as_ptr()) };
            if !disco.is_null() && kind == "result" {
                let mut child = unsafe { (api.children)(disco) };
                let mut features = Vec::new();
                for _ in 0..1024 {
                    if child.is_null() {
                        break;
                    }
                    let feature = api.attr(child, c"var");
                    if !feature.is_empty() {
                        features.push(string(feature));
                    }
                    child = unsafe { (api.next)(child) };
                }
                set(&mut response, "features", array(features));
            } else if !disco.is_null() && kind == "get" {
                send_callback_result(
                    api,
                    self.ctx,
                    conn,
                    &id,
                    &from,
                    "<query xmlns='http://jabber.org/protocol/disco#info'><identity category='client' type='pc' name='zchat'/><feature var='http://jabber.org/protocol/disco#info'/><feature var='http://jabber.org/protocol/chatstates'/><feature var='urn:xmpp:chat-markers:0'/><feature var='urn:xmpp:ping'/></query>",
                )?;
            }
            if kind == "get"
                && !unsafe { (api.child_ns)(stanza, c"urn:xmpp:ping".as_ptr()) }.is_null()
            {
                send_callback_result(api, self.ctx, conn, &id, &from, "")?;
            }
            if matches!(kind.as_str(), "result" | "error")
                && self.pending.as_ref().is_some_and(|(expected, to)| {
                    expected == &id && iq_sender_allowed(&from, to.as_deref(), &self.jid)
                })
            {
                self.reply = Some(response);
                return Ok(());
            }
        }
        self.events.push(response);
        Ok(())
    }
}

struct Connection {
    api: Api,
    ctx: Handle,
    conn: Handle,
    state: Box<State>,
}

impl Connection {
    fn open(config: &Value) -> Result<Self, ChatError> {
        let api = Api::load(
            config
                .get("library_path")
                .and_then(Value::as_str)
                .unwrap_or(default_library()),
        )?;
        let jid = c_string(required(config, "jid")?)?;
        let password = c_string(required(config, "password")?)?;
        let host = config
            .get("host")
            .and_then(Value::as_str)
            .map(c_string)
            .transpose()?;
        let cafile = config
            .get("cafile")
            .and_then(Value::as_str)
            .map(c_string)
            .transpose()?;
        let port: u16 = u64_param(config, "port", 0)
            .try_into()
            .map_err(|_| ChatError::new("invalid_config", "XMPP port must fit uint16"))?;
        let ctx = unsafe { (api.ctx_new)(ptr::null(), ptr::null()) };
        if ctx.is_null() {
            return Err(ChatError::new(
                "backend_error",
                "Could not allocate libstrophe context",
            ));
        }
        let conn = unsafe { (api.conn_new)(ctx) };
        if conn.is_null() {
            unsafe {
                (api.ctx_free)(ctx);
            }
            return Err(ChatError::new(
                "backend_error",
                "Could not allocate XMPP connection",
            ));
        }
        let mut value = Self {
            api,
            ctx,
            conn,
            state: Box::new(State {
                api,
                ctx,
                jid: required(config, "jid")?.to_owned(),
                connected: false,
                failure: None,
                poisoned: false,
                events: EventQueue::default(),
                conversations: BTreeMap::new(),
                history: VecDeque::new(),
                history_bytes: 0,
                sequence: 0,
                pending: None,
                reply: None,
            }),
        };
        let userdata = (&mut *value.state as *mut State).cast();
        // Mandatory TLS is bit 1. Never set TRUST_TLS, DISABLE_TLS or legacy
        // password authentication flags; native certificate verification stays on.
        unsafe {
            if (api.set_flags)(conn, 1 << 1) != 0 {
                return Err(ChatError::new(
                    "backend_error",
                    "Could not require XMPP TLS",
                ));
            }
            (api.set_jid)(conn, jid.as_ptr());
            (api.set_pass)(conn, password.as_ptr());
            if let Some(cafile) = &cafile {
                (api.set_cafile)(conn, cafile.as_ptr());
            }
            if (api.connect)(
                conn,
                host.as_ref().map_or(ptr::null(), |v| v.as_ptr()),
                port,
                connection_callback,
                userdata,
            ) != 0
            {
                return Err(ChatError::new(
                    "transport_error",
                    "XMPP connection could not be started",
                ));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while !value.state.connected {
            value.pump(Duration::from_millis(20))?;
            if Instant::now() >= deadline {
                return Err(ChatError::new("timeout", "XMPP authentication timed out"));
            }
        }
        if unsafe { (api.is_secured)(conn) } == 0 {
            return Err(ChatError::new(
                "encryption_required",
                "XMPP TLS was not negotiated",
            ));
        }
        value.send_xml("<presence/>", false)?;
        Ok(value)
    }

    fn pump(&mut self, timeout: Duration) -> Result<(), ChatError> {
        unsafe {
            (self.api.run_once)(self.ctx, timeout.as_millis().min(100) as c_ulong);
        }
        if self.state.poisoned {
            return Err(ChatError::new("backend_error", "XMPP callback failed"));
        }
        if let Some(code) = self.state.failure {
            return Err(ChatError::new(
                "transport_closed",
                format!("XMPP disconnected (native error {code})"),
            ));
        }
        Ok(())
    }

    fn send_xml(&mut self, xml: &str, wait_iq: bool) -> Result<Value, ChatError> {
        if !self.state.connected || unsafe { (self.api.is_secured)(self.conn) } == 0 {
            return Err(ChatError::new(
                "transport_closed",
                "XMPP requires an authenticated TLS session",
            ));
        }
        let stanza = Stanza::parse(self.api, self.ctx, xml)?;
        let name = unsafe { copy_c_string((self.api.name)(stanza.pointer)) };
        if !matches!(name.as_str(), "message" | "presence" | "iq") {
            return Err(ChatError::new(
                "invalid_params",
                "native XML must be one message, presence, or iq stanza",
            ));
        }
        let mut id = self.api.attr(stanza.pointer, c"id");
        if id.is_empty() {
            id = self.api.unique_id(self.ctx)?;
            let id_c = c_string(&id)?;
            if unsafe { (self.api.set_id)(stanza.pointer, id_c.as_ptr()) } != 0 {
                return Err(ChatError::new(
                    "backend_error",
                    "Could not set XMPP stanza ID",
                ));
            }
        }
        let wait_iq = wait_iq
            && name == "iq"
            && matches!(
                self.api.attr(stanza.pointer, c"type").as_str(),
                "get" | "set"
            );
        if wait_iq {
            let to = self.api.attr(stanza.pointer, c"to");
            self.state.pending = Some((id.clone(), (!to.is_empty()).then_some(to)));
            self.state.reply = None;
        }
        unsafe {
            (self.api.send)(self.conn, stanza.pointer);
        }
        drop(stanza);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let pumped = self.pump(if wait_iq {
                Duration::from_millis(20)
            } else {
                Duration::ZERO
            });
            if let Err(error) = pumped {
                self.state.pending = None;
                return Err(error);
            }
            if !wait_iq {
                return Ok(object([("id", string(id)), ("status", string("queued"))]));
            }
            if let Some(reply) = self.state.reply.take() {
                self.state.pending = None;
                if text(&reply, "type") == "error" {
                    return Err(ChatError::new(
                        "upstream_error",
                        "XMPP server returned an IQ error; inspect events for protocol details",
                    ));
                }
                return Ok(reply);
            }
            if Instant::now() >= deadline {
                self.state.pending = None;
                return Err(ChatError::new(
                    "timeout",
                    "XMPP IQ request timed out; inspect state before retrying",
                ));
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // No callbacks can outlive conn_release; keep the boxed userdata and
        // native context alive until the connection and its handlers are gone.
        unsafe {
            (self.api.disconnect)(self.conn);
            (self.api.run_once)(self.ctx, 0);
            (self.api.conn_release)(self.conn);
            (self.api.ctx_free)(self.ctx);
        }
    }
}

struct Stanza {
    api: Api,
    pointer: Handle,
}
impl Stanza {
    fn parse(api: Api, ctx: Handle, xml: &str) -> Result<Self, ChatError> {
        if xml.len() > MAX_STANZA_BYTES || xml.contains("<!") || xml.contains("<?") {
            return Err(ChatError::new(
                "invalid_params",
                "XMPP stanza must be at most 256 KiB and cannot contain XML declarations or entities",
            ));
        }
        let xml = c_string(xml)?;
        let pointer = unsafe { (api.stanza_parse)(ctx, xml.as_ptr()) };
        if pointer.is_null() {
            return Err(ChatError::new(
                "invalid_params",
                "Native XMPP stanza is not well-formed XML",
            ));
        }
        Ok(Self { api, pointer })
    }
}
impl Drop for Stanza {
    fn drop(&mut self) {
        unsafe {
            (self.api.stanza_release)(self.pointer);
        }
    }
}

unsafe extern "C" fn connection_callback(
    conn: Handle,
    event: c_int,
    error: c_int,
    _stream_error: Handle,
    userdata: Handle,
) {
    // SAFETY: the boxed State is stable for the connection's entire lifetime.
    let state = unsafe { &mut *userdata.cast::<State>() };
    if event == 0 {
        state.connected = true;
        state.failure = None;
        unsafe {
            (state.api.handler_add)(
                conn,
                stanza_callback,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                userdata,
            );
        }
    } else if event != 1 {
        state.connected = false;
        state.failure = Some(error);
    }
}

unsafe extern "C" fn stanza_callback(conn: Handle, stanza: Handle, userdata: Handle) -> c_int {
    let state = unsafe { &mut *userdata.cast::<State>() };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.handle_stanza(conn, stanza)
    }));
    match outcome {
        Ok(Ok(())) => 1,
        Ok(Err(error)) => {
            state.events.push(object([
                ("kind", string("provider.error")),
                ("error", error.to_value()),
            ]));
            1
        }
        Err(_) => {
            state.poisoned = true;
            0
        }
    }
}

fn send_callback_result(
    api: Api,
    ctx: Handle,
    conn: Handle,
    id: &str,
    to: &str,
    content: &str,
) -> Result<(), ChatError> {
    let target = if to.is_empty() {
        String::new()
    } else {
        format!(" to='{}'", xml_escape(to)?)
    };
    let reply = Stanza::parse(
        api,
        ctx,
        &format!(
            "<iq type='result' id='{}'{target}>{content}</iq>",
            xml_escape(id)?
        ),
    )?;
    unsafe {
        (api.send)(conn, reply.pointer);
    }
    Ok(())
}

pub(crate) struct Xmpp {
    config: Value,
    connection: Option<Connection>,
}

impl Xmpp {
    pub fn new(config: &Value) -> Result<Self, ChatError> {
        let jid = required(config, "jid")?;
        if !bare_jid(jid).contains('@') || jid.len() > 3071 || jid.chars().any(char::is_whitespace)
        {
            return Err(ChatError::new(
                "invalid_config",
                "XMPP jid must be a complete user@domain address",
            ));
        }
        c_string(jid)?;
        c_string(required(config, "password")?)?;
        if u64_param(config, "port", 0) > u16::MAX as u64 {
            return Err(ChatError::new(
                "invalid_config",
                "XMPP port must fit uint16",
            ));
        }
        Ok(Self {
            config: config.clone(),
            connection: None,
        })
    }

    fn connection(&mut self) -> Result<&mut Connection, ChatError> {
        if self
            .connection
            .as_ref()
            .is_some_and(|c| c.state.failure.is_some() || c.state.poisoned)
        {
            self.connection = None;
        }
        if self.connection.is_none() {
            self.connection = Some(Connection::open(&self.config)?);
        }
        self.connection
            .as_mut()
            .ok_or_else(|| ChatError::new("transport_closed", "XMPP connection unavailable"))
    }

    pub fn invoke(&mut self, operation: &str, params: &Value) -> Result<Value, ChatError> {
        if operation == "capabilities" {
            return Ok(Self::capabilities());
        }
        let connection = self.connection()?;
        match operation {
            "account.status" => {
                connection.pump(Duration::ZERO)?;
                Ok(object([
                    ("connected", Value::Bool(connection.state.connected)),
                    (
                        "jid",
                        string(unsafe {
                            copy_c_string((connection.api.bound_jid)(connection.conn))
                        }),
                    ),
                    ("tls", Value::Bool(true)),
                ]))
            }
            "native.invoke" => connection.send_xml(
                required(params, "xml")?,
                bool_param(params, "wait_iq", true),
            ),
            "server.info" => {
                let server = bare_jid(&connection.state.jid)
                    .split_once('@')
                    .map(|(_, domain)| domain)
                    .unwrap_or_default();
                connection.send_xml(&format!("<iq type='get' to='{}'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>", xml_escape(server)?), true)
            }
            "conversation.list" => {
                connection.send_xml(
                    "<iq type='get'><query xmlns='jabber:iq:roster'/></iq>",
                    true,
                )?;
                let limit = u64_param(params, "limit", 50).clamp(1, 200) as usize;
                let skip = page_offset(params, 0)?.min(connection.state.conversations.len());
                let values = connection
                    .state
                    .conversations
                    .values()
                    .skip(skip)
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let next = skip + values.len();
                Ok(object([
                    ("conversations", array(values)),
                    (
                        "cursor",
                        if next < connection.state.conversations.len() {
                            string(next.to_string())
                        } else {
                            Value::Null
                        },
                    ),
                    ("source", string("roster_and_observed_chats")),
                ]))
            }
            "message.list" => {
                connection.pump(Duration::ZERO)?;
                let room = required(params, "conversation_id")?;
                let before = page_offset(params, usize::MAX)? as u64;
                let limit = u64_param(params, "limit", 50).clamp(1, 200) as usize;
                let mut messages = connection
                    .state
                    .history
                    .iter()
                    .rev()
                    .filter(|(seq, v, _)| {
                        *seq < before && text(v, "conversation_id") == bare_jid(room)
                    })
                    .take(limit)
                    .collect::<Vec<_>>();
                messages.reverse();
                let cursor = messages
                    .first()
                    .map(|(seq, _, _)| string(seq.to_string()))
                    .unwrap_or(Value::Null);
                Ok(object([
                    (
                        "messages",
                        array(messages.iter().map(|(_, value, _)| value.clone())),
                    ),
                    ("cursor", cursor),
                    ("order", string("oldest_first")),
                    ("source", string("session_cache")),
                    ("partial_history", Value::Bool(true)),
                ]))
            }
            "message.send" => {
                let room = required(params, "conversation_id")?;
                let body = required(params, "text")?;
                let kind = message_kind(params)?;
                let id = connection.api.unique_id(connection.ctx)?;
                let xml = format!(
                    "<message to='{}' type='{kind}' id='{}'><body>{}</body><active xmlns='http://jabber.org/protocol/chatstates'/><markable xmlns='urn:xmpp:chat-markers:0'/></message>",
                    xml_escape(room)?,
                    xml_escape(&id)?,
                    xml_escape(body)?
                );
                connection.send_xml(&xml, false)?;
                let message = object([
                    ("id", string(id)),
                    ("conversation_id", string(bare_jid(room))),
                    ("sender", string(&connection.state.jid)),
                    ("text", string(body)),
                    ("type", string(kind)),
                    ("status", string("queued")),
                    ("outgoing", Value::Bool(true)),
                ]);
                connection.state.remember_message(message.clone());
                Ok(message)
            }
            "typing.set" => {
                let room = xml_escape(required(params, "conversation_id")?)?;
                let state = if bool_param(params, "typing", true) {
                    "composing"
                } else {
                    "active"
                };
                connection.send_xml(&format!("<message to='{room}' type='{}'><{state} xmlns='http://jabber.org/protocol/chatstates'/></message>", message_kind(params)?), false)
            }
            "message.read" => {
                let room = required(params, "conversation_id")?;
                let id = identifier(params, "message_id")?;
                let kind = message_kind(params)?;
                if kind != "groupchat"
                    && !connection.state.history.iter().any(|(_, message, _)| {
                        text(message, "conversation_id") == bare_jid(room)
                            && text(message, "id") == id
                            && bool_param(message, "markable", false)
                    })
                {
                    return Err(ChatError::new(
                        "unsupported_operation",
                        "The direct message did not request an XMPP displayed marker, or is outside this session's cache",
                    ));
                }
                connection.send_xml(&format!("<message to='{}' type='{kind}'><displayed xmlns='urn:xmpp:chat-markers:0' id='{}'/></message>", xml_escape(room)?, xml_escape(&id)?), false)
            }
            "conversation.join" | "conversation.leave" => {
                let room = required(params, "conversation_id")?;
                let nickname = params
                    .get("nickname")
                    .and_then(Value::as_str)
                    .unwrap_or("zchat");
                if nickname.contains('/') {
                    return Err(ChatError::new(
                        "invalid_params",
                        "XMPP nickname cannot contain /",
                    ));
                }
                let presence_type = if operation == "conversation.leave" {
                    " type='unavailable'"
                } else {
                    ""
                };
                connection.send_xml(&format!("<presence to='{}'{presence_type}><x xmlns='http://jabber.org/protocol/muc'/></presence>", xml_escape(&format!("{room}/{nickname}"))?), false)
            }
            "events.poll" => {
                let deadline = Instant::now()
                    + Duration::from_millis(u64_param(params, "timeout_ms", 0).min(30_000));
                for _ in 0..MAX_EVENTS {
                    connection.pump(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(100)),
                    )?;
                    if !connection.state.events.is_empty() || Instant::now() >= deadline {
                        break;
                    }
                }
                Ok(connection.state.events.drain())
            }
            _ => Err(unsupported("xmpp", operation)),
        }
    }

    pub fn capabilities() -> Value {
        capability(
            "xmpp",
            &[
                "account.status",
                "server.info",
                "conversation.list",
                "conversation.join",
                "conversation.leave",
                "message.list",
                "message.send",
                "message.read",
                "typing.set",
                "events.poll",
                "native.invoke",
            ],
            "transport_tls_only",
            &[
                "Requires libstrophe 0.12+; TLS and certificate verification are mandatory.",
                "message.list covers the bounded current-session cache, not a complete archive. MAM queries can be sent through native.invoke.",
                "OMEMO, encrypted file transfer and call media are not implemented; native XML does not constitute a crypto or media engine.",
                "Server and peer XEP support varies. server.info returns actual disco#info features; queued does not mean delivered.",
            ],
        )
    }
}

fn c_string(value: &str) -> Result<CString, ChatError> {
    CString::new(value)
        .map_err(|_| ChatError::new("invalid_params", "XMPP input cannot contain NUL"))
}
unsafe fn copy_c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}
fn bare_jid(jid: &str) -> &str {
    jid.split('/').next().unwrap_or(jid)
}
fn iq_sender_allowed(from: &str, expected: Option<&str>, own: &str) -> bool {
    match expected {
        Some(to) => from == to || (!to.contains('/') && bare_jid(from) == to),
        None => {
            from.is_empty()
                || bare_jid(from) == bare_jid(own)
                || from
                    == bare_jid(own)
                        .split_once('@')
                        .map(|(_, domain)| domain)
                        .unwrap_or_default()
        }
    }
}
fn message_kind(params: &Value) -> Result<&str, ChatError> {
    match params
        .get("message_type")
        .and_then(Value::as_str)
        .unwrap_or("chat")
    {
        "chat" => Ok("chat"),
        "groupchat" => Ok("groupchat"),
        _ => Err(ChatError::new(
            "invalid_params",
            "XMPP message_type must be chat or groupchat",
        )),
    }
}
fn xml_escape(value: &str) -> Result<String, ChatError> {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\'' => output.push_str("&apos;"),
            '"' => output.push_str("&quot;"),
            '\t'
            | '\n'
            | '\r'
            | ' '..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}' => output.push(character),
            _ => {
                return Err(ChatError::new(
                    "invalid_params",
                    "Text contains a character forbidden in XML 1.0",
                ));
            }
        }
        if output.len() > MAX_STANZA_BYTES {
            return Err(ChatError::new(
                "payload_too_large",
                "Escaped XMPP input exceeds 256 KiB",
            ));
        }
    }
    Ok(output)
}
fn default_library() -> &'static str {
    if cfg!(target_os = "windows") {
        "libstrophe-0.dll"
    } else if cfg!(target_os = "macos") {
        "libstrophe.dylib"
    } else {
        "libstrophe.so.0"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_stanza_content_and_rejects_invalid_xml_characters() {
        assert_eq!(
            xml_escape("a<&'\"你好").unwrap(),
            "a&lt;&amp;&apos;&quot;你好"
        );
        assert!(xml_escape("invalid\0text").is_err());
        assert!(xml_escape(&"&".repeat(MAX_STANZA_BYTES)).is_err());
    }
    #[test]
    fn iq_replies_cannot_be_satisfied_by_another_contact() {
        assert!(iq_sender_allowed(
            "example.org",
            None,
            "a@example.org/desktop"
        ));
        assert!(iq_sender_allowed(
            "a@example.org",
            None,
            "a@example.org/desktop"
        ));
        assert!(!iq_sender_allowed(
            "attacker@example.org",
            None,
            "a@example.org/desktop"
        ));
        assert!(iq_sender_allowed(
            "b@example.org/phone",
            Some("b@example.org"),
            "a@example.org"
        ));
        assert!(!iq_sender_allowed(
            "b@example.org/laptop",
            Some("b@example.org/phone"),
            "a@example.org"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn native_c_abi_handles_tls_callbacks_and_iq_without_network() {
        use std::{
            fs,
            process::Command,
            time::{SystemTime, UNIX_EPOCH},
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("zchat-strophe-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("mock_strophe.c");
        let library = directory.join(if cfg!(target_os = "macos") {
            "mock_strophe.dylib"
        } else {
            "mock_strophe.so"
        });
        fs::write(&source, include_str!("test_support/mock_strophe.c")).unwrap();
        let compilation = Command::new("cc")
            .arg(if cfg!(target_os = "macos") {
                "-dynamiclib"
            } else {
                "-shared"
            })
            .arg("-fPIC")
            .arg("-std=c11")
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .output()
            .expect("provider C ABI test requires a C compiler");
        assert!(
            compilation.status.success(),
            "{}",
            String::from_utf8_lossy(&compilation.stderr)
        );
        let mut account = Xmpp::new(&object([
            ("jid", string("a@example.org/fixture")),
            ("password", string("fixture-only")),
            ("library_path", string(library.to_str().unwrap())),
        ]))
        .unwrap();
        let status = account.invoke("account.status", &empty()).unwrap();
        assert_eq!(status.get("tls").and_then(Value::as_bool), Some(true));
        let sent = account
            .invoke(
                "message.send",
                &object([
                    ("conversation_id", string("b@example.org")),
                    ("text", string("local fixture only")),
                ]),
            )
            .unwrap();
        assert_eq!(text(&sent, "status"), "queued");
        let history = account
            .invoke(
                "message.list",
                &object([("conversation_id", string("b@example.org"))]),
            )
            .unwrap();
        assert_eq!(
            history.get("partial_history").and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            list(value_at(&history, "messages"))
                .iter()
                .any(|message| text(message, "text") == "inbound fixture")
        );
        let reply = account.invoke("native.invoke", &object([("xml", string("<iq type='get' to='example.org'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>"))])).unwrap();
        assert_eq!(text(&reply, "type"), "result");
        drop(account);
        fs::remove_dir_all(&directory).unwrap();
    }
}
