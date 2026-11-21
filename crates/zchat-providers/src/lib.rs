//! Built-in IM provider plugin. Each account owns its protocol state while
//! shared interfaces remain independent of any particular messaging network.
mod deltachat;
mod http;
mod matrix;
mod rpc_process;
mod simplex;
mod telegram;
mod threema;
mod util;
mod xmpp;

use std::collections::BTreeMap;
use util::*;
use zchat_core::{ChatError, Value, parse};
use zchat_plugin_api::{ImPlugin, ImPluginExport};
use zplugin::OwnedBytes;

enum Provider {
    Matrix(matrix::Matrix),
    Telegram(telegram::Telegram),
    DeltaChat(deltachat::DeltaChat),
    Simplex(simplex::Simplex),
    Threema(threema::ThreemaGateway),
    Xmpp(xmpp::Xmpp),
}

impl Provider {
    fn name(&self) -> &'static str {
        match self {
            Self::Matrix(_) => "matrix",
            Self::Telegram(_) => "telegram",
            Self::DeltaChat(_) => "deltachat",
            Self::Simplex(_) => "simplex",
            Self::Threema(_) => "threema-gateway",
            Self::Xmpp(_) => "xmpp",
        }
    }
    fn invoke(&mut self, operation: &str, params: &Value) -> Result<Value, ChatError> {
        match self {
            Self::Matrix(p) => p.invoke(operation, params),
            Self::Telegram(p) => p.invoke(operation, params),
            Self::DeltaChat(p) => p.invoke(operation, params),
            Self::Simplex(p) => p.invoke(operation, params),
            Self::Threema(p) => p.invoke(operation, params),
            Self::Xmpp(p) => p.invoke(operation, params),
        }
    }
}

/// In-memory protocol account registry. Hosts persist account configurations
/// separately and restore them through `account.configure`.
#[derive(Default)]
pub struct ProviderHub {
    accounts: BTreeMap<String, Provider>,
}

impl ProviderHub {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn catalog() -> Value {
        catalog()
    }

    pub fn invoke(&mut self, request: &Value) -> Result<Value, ChatError> {
        let operation = required(request, "operation")?;
        let params = value_at(request, "params");
        if matches!(
            operation,
            "catalog.list" | "provider.catalog" | "plugin.catalog"
        ) {
            return Ok(catalog());
        }
        if operation == "account.list" {
            let accounts = self.accounts.iter().map(|(id, provider)| {
                object([
                    ("id", string(id)),
                    ("provider", string(provider.name())),
                    ("state", string("configured")),
                ])
            });
            return Ok(object([("accounts", array(accounts))]));
        }
        let account = required(request, "account")?;
        if account.len() > 128
            || !account
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(ChatError::new(
                "invalid_params",
                "account must be a short ASCII identifier",
            ));
        }
        if operation == "account.configure" {
            if self.accounts.len() >= 64 && !self.accounts.contains_key(account) {
                return Err(ChatError::new(
                    "capacity",
                    "A provider hub supports at most 64 accounts",
                ));
            }
            let name = required(params, "provider")?;
            let config = value_at(params, "config");
            if config.as_object().is_none() {
                return Err(ChatError::new(
                    "invalid_config",
                    "account config must be an object",
                ));
            }
            let provider = match name {
                "matrix" => Provider::Matrix(matrix::Matrix::new(config)?),
                "telegram" => Provider::Telegram(telegram::Telegram::new(config)?),
                "deltachat" => Provider::DeltaChat(deltachat::DeltaChat::new(config)?),
                "simplex" => Provider::Simplex(simplex::Simplex::new(config)?),
                "threema-gateway" => Provider::Threema(threema::ThreemaGateway::new(config)?),
                "xmpp" => Provider::Xmpp(xmpp::Xmpp::new(config)?),
                "session" => {
                    return Err(ChatError::new(
                        "backend_unavailable",
                        "Session requires an independent libsession integration plugin; no Session transport is bundled",
                    ));
                }
                _ => {
                    return Err(ChatError::new(
                        "unknown_provider",
                        "Provider is not installed; inspect provider.catalog",
                    ));
                }
            };
            self.accounts.insert(account.to_owned(), provider);
            return Ok(object([
                ("account", string(account)),
                ("provider", string(name)),
                ("state", string("configured")),
                ("connected", Value::Bool(false)),
            ]));
        }
        if operation == "account.remove" {
            return Ok(object([(
                "removed",
                Value::Bool(self.accounts.remove(account).is_some()),
            )]));
        }
        let provider = self.accounts.get_mut(account).ok_or_else(|| {
            ChatError::new(
                "account_not_found",
                "Configure this account before using it",
            )
        })?;
        let actor = request.get("actor").and_then(Value::as_str).unwrap_or("ai");
        if provider.name() == "telegram"
            && actor != "human"
            && !matches!(
                operation,
                "account.status" | "account.login" | "capabilities"
            )
        {
            return Err(ChatError::new(
                "platform_policy",
                "Telegram API terms restrict AI use of platform content; this adapter's content operations are available through the human UI/CLI only",
            ));
        }
        let mut output = provider.invoke(operation, params)?;
        if operation != "native.invoke" && !bool_param(params, "include_raw", false) {
            remove_raw(&mut output);
        }
        // Validate at the provider boundary so in-process and dynamic callers
        // receive the same bounded-response failure, without an allocation panic.
        output.try_stringify()?;
        Ok(output)
    }
}

impl ImPlugin for ProviderHub {
    fn manifest(&self) -> zplugin::Result<OwnedBytes> {
        Ok(OwnedBytes::from_vec(encode_reply(Ok(catalog()))))
    }
    fn invoke(&mut self, request: &[u8]) -> zplugin::Result<OwnedBytes> {
        let result = std::str::from_utf8(request)
            .map_err(|_| ChatError::new("invalid_json", "Request must be UTF-8"))
            .and_then(parse)
            .and_then(|request| ProviderHub::invoke(self, &request));
        Ok(OwnedBytes::from_vec(encode_reply(result)))
    }
}

fn encode_reply(result: Result<Value, ChatError>) -> Vec<u8> {
    let response = match result {
        Ok(value) => value,
        Err(error) => object([("error", error.to_value())]),
    };
    match response.try_stringify() {
        Ok(json) => json.into_bytes(),
        Err(error) => object([("error", error.to_value())]).try_stringify().unwrap_or_else(|_| "{\"error\":{\"code\":\"response_too_large\",\"message\":\"Provider response exceeds its limit\"}}".to_owned()).into_bytes(),
    }
}

fn create_plugin() -> zplugin::Result<ProviderHub> {
    Ok(ProviderHub::new())
}
const fn version_component(text: &str) -> u32 {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut value = 0u32;
    while index < bytes.len() {
        assert!(
            bytes[index] >= b'0' && bytes[index] <= b'9',
            "Cargo version component must be numeric"
        );
        value = value * 10 + (bytes[index] - b'0') as u32;
        index += 1;
    }
    value
}
zplugin::export!(
    ImPluginExport<ProviderHub>,
    constructor = create_plugin,
    version = (
        version_component(env!("CARGO_PKG_VERSION_MAJOR")) as _,
        version_component(env!("CARGO_PKG_VERSION_MINOR")) as _,
        version_component(env!("CARGO_PKG_VERSION_PATCH")) as _
    ),
    unload = pinned
);

/// Public, credential-free catalog. The installed backends remain optional and
/// are probed only when the user connects or invokes an account.
pub fn catalog() -> Value {
    object([
        ("id", string("zchat.providers")),
        ("version", string(env!("CARGO_PKG_VERSION"))),
        ("interface", string(zchat_plugin_api::INTERFACE_ID)),
        (
            "providers",
            array([
                descriptor(
                    "telegram",
                    "Telegram",
                    "tdlib_json",
                    telegram::Telegram::capabilities(),
                    &["api_id", "api_hash", "database_directory"],
                    &["library_path", "phone_number", "database_encryption_key"],
                ),
                descriptor(
                    "matrix",
                    "Matrix",
                    "https_client_server",
                    matrix::Matrix::capabilities(),
                    &["homeserver", "access_token"],
                    &["cursor"],
                ),
                descriptor(
                    "xmpp",
                    "XMPP",
                    "libstrophe",
                    xmpp::Xmpp::capabilities(),
                    &["jid", "password"],
                    &["library_path", "host", "port", "cafile"],
                ),
                descriptor(
                    "simplex",
                    "SimpleX",
                    "loopback_websocket",
                    simplex::Simplex::capabilities(),
                    &[],
                    &["url", "user_id"],
                ),
                descriptor(
                    "deltachat",
                    "Delta Chat",
                    "stdio_jsonrpc",
                    deltachat::DeltaChat::capabilities(),
                    &[],
                    &["command", "args", "accounts_path", "account_id"],
                ),
                descriptor(
                    "threema-gateway",
                    "Threema Gateway",
                    "https_gateway",
                    threema::ThreemaGateway::capabilities(),
                    &["identity", "secret"],
                    &["mode", "base_url"],
                ),
                object([
                    ("id", string("session")),
                    ("name", string("Session")),
                    ("status", string("unavailable")),
                    (
                        "reason",
                        string(
                            "Requires a separately implemented libsession client plugin; a transport is not bundled.",
                        ),
                    ),
                    (
                        "source",
                        string("https://github.com/session-foundation/libsession"),
                    ),
                ]),
            ]),
        ),
    ])
}

fn descriptor(
    id: &str,
    name: &str,
    transport: &str,
    capabilities: Value,
    required: &[&str],
    optional: &[&str],
) -> Value {
    object([
        ("id", string(id)),
        ("name", string(name)),
        ("status", string("implemented")),
        ("transport", string(transport)),
        ("capabilities", capabilities),
        (
            "config",
            object([
                ("required", array(required.iter().map(|v| string(*v)))),
                ("optional", array(optional.iter().map(|v| string(*v)))),
            ]),
        ),
    ])
}

fn remove_raw(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("raw");
            for value in map.values_mut() {
                remove_raw(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                remove_raw(value);
            }
        }
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unconfigured_and_unavailable_accounts_do_not_claim_success() {
        let mut hub = ProviderHub::new();
        assert!(
            hub.invoke(&object([
                ("operation", string("message.list")),
                ("account", string("missing")),
                ("params", empty())
            ]))
            .is_err()
        );
        assert!(
            hub.invoke(&object([
                ("operation", string("account.configure")),
                ("account", string("session")),
                (
                    "params",
                    object([("provider", string("session")), ("config", empty())])
                )
            ]))
            .is_err()
        );
    }
    #[test]
    fn configuring_is_lazy_and_catalog_contains_no_credentials() {
        let mut hub = ProviderHub::new();
        let configured = hub
            .invoke(&object([
                ("operation", string("account.configure")),
                ("account", string("matrix")),
                (
                    "params",
                    object([
                        ("provider", string("matrix")),
                        (
                            "config",
                            object([
                                ("homeserver", string("https://example.invalid")),
                                ("access_token", string("private-secret")),
                            ]),
                        ),
                    ]),
                ),
            ]))
            .unwrap();
        assert_eq!(
            configured.get("connected").and_then(Value::as_bool),
            Some(false)
        );
        assert!(!catalog().stringify().contains("private-secret"));
    }
    #[test]
    fn telegram_ai_policy_applies_before_loading_tdlib() {
        let mut hub = ProviderHub::new();
        hub.invoke(&object([
            ("operation", string("account.configure")),
            ("account", string("tg")),
            (
                "params",
                object([
                    ("provider", string("telegram")),
                    (
                        "config",
                        object([
                            ("api_id", number(123)),
                            ("api_hash", string("test")),
                            ("database_directory", string("unused")),
                        ]),
                    ),
                ]),
            ),
        ]))
        .unwrap();
        let error = hub
            .invoke(&object([
                ("operation", string("native.invoke")),
                ("account", string("tg")),
                ("actor", string("ai")),
                ("params", object([("method", string("getChats"))])),
            ]))
            .unwrap_err();
        assert_eq!(text(&error.to_value(), "code"), "platform_policy");
    }
}
