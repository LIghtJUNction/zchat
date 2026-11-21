use crate::config::{ConfigStore, required, resolve_secrets};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
};
use zchat_core::{ChatError, Value, arr, obj, operation, parse};
use zchat_plugin_api::ImPluginApi;
use zchat_providers::ProviderHub;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Human,
    Ai,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Access {
    pub allow_write: bool,
    pub allow_native: bool,
}

type PluginCall = Box<dyn FnMut(&[u8]) -> Result<Vec<u8>, ChatError>>;
struct LoadedPlugin {
    manifest: Value,
    call: PluginCall,
}

pub struct Engine {
    pub store: ConfigStore,
    pub access: Access,
    providers: ProviderHub,
    configured: BTreeSet<String>,
    plugins: BTreeMap<String, LoadedPlugin>,
    cache: VecDeque<(String, String, String, Value)>,
}

impl Engine {
    pub fn new(path: Option<PathBuf>, access: Access) -> Result<Self, ChatError> {
        Ok(Self {
            store: ConfigStore::open(path)?,
            access,
            providers: ProviderHub::new(),
            configured: BTreeSet::new(),
            plugins: BTreeMap::new(),
            cache: VecDeque::new(),
        })
    }

    pub fn status(&self) -> Value {
        obj([
            ("name", "zchat".into()),
            ("version", crate::VERSION.into()),
            ("accounts", (self.store.accounts.len() as u64).into()),
            ("loaded_accounts", (self.configured.len() as u64).into()),
            ("ai_write_enabled", self.access.allow_write.into()),
            ("ai_native_enabled", self.access.allow_native.into()),
            (
                "storage",
                "Local configuration; message content is not persisted by the host".into(),
            ),
        ])
    }

    pub fn catalog(&self) -> Value {
        ProviderHub::catalog()
    }

    pub fn plugins(&self) -> Value {
        let mut plugins = vec![obj([
            ("id", "builtin".into()),
            ("loaded", true.into()),
            ("manifest", self.catalog()),
        ])];
        plugins.extend(self.store.plugins.values().map(|p| {
            obj([
                ("id", p.id.as_str().into()),
                ("trusted", p.trusted.into()),
                ("loaded", self.plugins.contains_key(&p.id).into()),
            ])
        }));
        arr(plugins)
    }

    pub fn call(&mut self, mut request: Value, origin: Origin) -> Result<Value, ChatError> {
        let operation_name = required(&request, "operation")?;
        match operation_name.as_str() {
            "status" => return Ok(self.status()),
            "catalog" => return Ok(self.catalog()),
            "account.list" => return Ok(obj([("accounts", self.store.public_accounts())])),
            "plugin.list" => return Ok(obj([("plugins", self.plugins())])),
            "schema" => return Ok(crate::schema::manifest()),
            _ => (),
        }
        let operation = operation(&operation_name).ok_or_else(|| {
            ChatError::new(
                "unknown_operation",
                "Use schema to discover operations and native.invoke for provider extensions",
            )
        })?;
        let account_id = required(&request, "account")?;
        let account = self
            .store
            .accounts
            .get(&account_id)
            .ok_or_else(|| {
                ChatError::new(
                    "account_not_found",
                    "Add this account through zchat accounts add or the desktop",
                )
            })?
            .clone();
        if operation_name == "account.configure" {
            return Err(ChatError::new(
                "configuration_control",
                "Use the local account setup flow to change credentials",
            ));
        }
        if origin == Origin::Ai {
            if account.provider == "telegram" && operation_name != "account.status" {
                return Err(ChatError::new(
                    "platform_policy",
                    "Telegram API Terms section 1.5 restricts AI data use. Telegram content is available through the human client; AI content access is disabled.",
                ));
            }
            if !account.ai_read {
                return Err(ChatError::new(
                    "ai_access_disabled",
                    "This account has not enabled AI access. Enable ai_read in local account settings.",
                ));
            }
            if !operation.read_only && (!self.access.allow_write || !account.ai_write) {
                return Err(ChatError::new(
                    "write_access_disabled",
                    "This operation requires the account's ai_write setting and zchat mcp --allow-write",
                ));
            }
            if operation_name == "native.invoke" && !self.access.allow_native {
                return Err(ChatError::new(
                    "native_access_disabled",
                    "Native provider calls require zchat mcp --allow-native as well as write access",
                ));
            }
        }
        if let Value::Object(fields) = &mut request {
            fields.insert(
                "actor".into(),
                Value::from(if origin == Origin::Ai { "ai" } else { "human" }),
            );
            fields
                .entry("params".into())
                .or_insert_with(|| obj([] as [(&str, Value); 0]));
        }
        // Validation is shared with plugins and SDK callers. The caller origin
        // above is assigned by this entry point, never trusted from JSON input.
        zchat_core::Request::from_value(&request)?;
        let key = request
            .get("params")
            .and_then(|p| p.get("idempotency_key"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let payload = obj([
            ("operation", operation_name.as_str().into()),
            (
                "params",
                request.get("params").cloned().unwrap_or(Value::Null),
            ),
        ])
        .try_stringify()?;
        if let Some(key) = &key
            && let Some((_, _, previous, result)) = self
                .cache
                .iter()
                .find(|(a, k, _, _)| a == &account_id && k == key)
        {
            if previous != &payload {
                return Err(ChatError::new(
                    "idempotency_conflict",
                    "This key has already been used with different parameters",
                ));
            }
            return Ok(result.clone());
        }
        self.ensure_account(&account_id)?;
        let result = self.dispatch(&account.plugin, &request)?;
        if let Some(key) = key
            && !operation.read_only
            && result
                .try_stringify()
                .is_ok_and(|encoded| payload.len() + encoded.len() <= 32 * 1024)
        {
            if self.cache.len() == 32 {
                self.cache.pop_front();
            }
            self.cache
                .push_back((account_id, key, payload, result.clone()));
        }
        Ok(result)
    }

    fn ensure_account(&mut self, id: &str) -> Result<(), ChatError> {
        if self.configured.contains(id) {
            return Ok(());
        }
        let account = self
            .store
            .accounts
            .get(id)
            .ok_or_else(|| ChatError::new("account_not_found", "Unknown account"))?
            .clone();
        if account.provider == "telegram"
            && self
                .configured
                .iter()
                .filter_map(|other| self.store.accounts.get(other))
                .any(|other| other.provider == "telegram" && other.plugin != account.plugin)
        {
            return Err(ChatError::new(
                "telegram_runtime_conflict",
                "All Telegram accounts in one zchat process must use the same plugin bundle so TDLib has one ordered receiver",
            ));
        }
        let configuration = resolve_secrets(&account.config)?;
        let request = obj([
            ("account", id.into()),
            ("operation", "account.configure".into()),
            ("actor", "human".into()),
            (
                "params",
                obj([
                    ("provider", account.provider.into()),
                    ("config", configuration),
                ]),
            ),
        ]);
        self.dispatch(&account.plugin, &request)?;
        self.configured.insert(id.to_string());
        Ok(())
    }

    fn dispatch(&mut self, plugin: &str, request: &Value) -> Result<Value, ChatError> {
        if plugin == "builtin" {
            return self.providers.invoke(request);
        }
        if !self.plugins.contains_key(plugin) {
            let settings =
                self.store.plugins.get(plugin).ok_or_else(|| {
                    ChatError::new("plugin_not_found", "Plugin is not registered")
                })?;
            if !settings.trusted {
                return Err(ChatError::new(
                    "plugin_not_trusted",
                    "Native plugins run with your user permissions. Register only a reviewed library with --trust.",
                ));
            }
            self.plugins
                .insert(plugin.to_string(), load_plugin(&settings.path)?);
        }
        let plugin = self.plugins.get_mut(plugin).expect("plugin loaded above");
        let bytes = (plugin.call)(request.try_stringify()?.as_bytes())?;
        if bytes.len() > 1024 * 1024 {
            return Err(ChatError::new(
                "plugin_output_limit",
                "Plugin response exceeds 1 MiB; request a smaller page",
            ));
        }
        let result =
            parse(std::str::from_utf8(&bytes).map_err(|_| {
                ChatError::new("plugin_encoding", "Plugin output must be UTF-8 JSON")
            })?)?;
        if let Some(error) = result.get("error") {
            return Err(ChatError {
                code: error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin_error")
                    .into(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Plugin request failed")
                    .into(),
                retryable: error
                    .get("retryable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
        Ok(result)
    }
}

fn load_plugin(path: &Path) -> Result<LoadedPlugin, ChatError> {
    // SAFETY: only the explicit local --trust setup path can register a native
    // library. zplugin verifies the versioned ABI and owns each library lease.
    let library = Rc::new(
        unsafe { zplugin::Plugin::load_path(path, zplugin::UnloadPolicy::Pinned) }
            .map_err(plugin_error)?,
    );
    let instance =
        zplugin::Plugin::create_owned(&library, ImPluginApi::SPEC).map_err(plugin_error)?;
    let mut proxy = ImPluginApi::wrap(instance).map_err(plugin_error)?;
    let manifest = {
        let bytes = proxy.manifest().map_err(plugin_error)?;
        if bytes.len() > 1024 * 1024 {
            return Err(ChatError::new(
                "plugin_manifest_limit",
                "Plugin manifest exceeds 1 MiB",
            ));
        }
        parse(
            std::str::from_utf8(&bytes)
                .map_err(|_| ChatError::new("plugin_encoding", "Plugin manifest must be UTF-8"))?,
        )?
    };
    Ok(LoadedPlugin {
        manifest,
        call: Box::new(move |request| {
            let bytes = proxy.invoke(request).map_err(plugin_error)?;
            if bytes.len() > 1024 * 1024 {
                return Err(ChatError::new(
                    "plugin_output_limit",
                    "Plugin response exceeds 1 MiB; request a smaller page",
                ));
            }
            Ok(bytes.to_vec())
        }),
    })
}

fn plugin_error(error: zplugin::Error) -> ChatError {
    ChatError::new("plugin_error", format!("{error:?}"))
}

pub fn inspect_plugin(path: &Path, trusted: bool) -> Result<Value, ChatError> {
    if !trusted {
        return Err(ChatError::new(
            "plugin_not_trusted",
            "Pass --trust only for a library you reviewed; native plugins execute code",
        ));
    }
    Ok(load_plugin(path)?.manifest)
}

pub fn request(account: &str, operation: &str, params: Value) -> Value {
    obj([
        ("account", account.into()),
        ("operation", operation.into()),
        ("params", params),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Account;
    fn engine() -> Engine {
        Engine::new(
            Some(
                std::env::temp_dir()
                    .join(format!("zchat-absent-{}-config.json", std::process::id())),
            ),
            Access::default(),
        )
        .unwrap()
    }
    #[test]
    fn ai_cannot_spoof_human_origin_or_mutate_without_scope() {
        let mut engine = engine();
        let account = Account::from_value(
            &parse(r#"{"id":"test","provider":"matrix","ai_read":true,"config":{}}"#).unwrap(),
        )
        .unwrap();
        engine.store.accounts.insert(account.id.clone(), account);
        let mut value = request("test", "message.send", obj([("text", "hello".into())]));
        if let Value::Object(fields) = &mut value {
            fields.insert("actor".into(), "human".into());
        }
        assert_eq!(
            engine.call(value, Origin::Ai).unwrap_err().code,
            "write_access_disabled"
        );
    }
    #[test]
    fn telegram_platform_policy_applies_before_network_or_native_dispatch() {
        let mut engine = engine();
        engine.access = Access {
            allow_write: true,
            allow_native: true,
        };
        let account = Account::from_value(
            &parse(
                r#"{"id":"test","provider":"telegram","ai_read":true,"ai_write":true,"config":{}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        engine.store.accounts.insert(account.id.clone(), account);
        assert_eq!(
            engine
                .call(
                    request("test", "native.invoke", obj([] as [(&str, Value); 0])),
                    Origin::Ai
                )
                .unwrap_err()
                .code,
            "platform_policy"
        );
    }

    #[test]
    fn telegram_accounts_cannot_split_the_global_receiver_across_plugin_images() {
        let mut engine = engine();
        let first = Account::from_value(&parse(r#"{"id":"one","provider":"telegram","config":{"api_id":1,"api_hash":"test","database_directory":"unused"}}"#).unwrap()).unwrap();
        let mut second = first.clone();
        second.id = "two".into();
        second.plugin = "external".into();
        engine.store.accounts.insert(first.id.clone(), first);
        engine.store.accounts.insert(second.id.clone(), second);
        engine.ensure_account("one").unwrap();
        assert_eq!(
            engine.ensure_account("two").unwrap_err().code,
            "telegram_runtime_conflict"
        );
    }
}
