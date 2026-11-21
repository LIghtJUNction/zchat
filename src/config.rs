use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use zchat_core::{ChatError, Value, arr, obj, parse};

const MAX_CONFIG: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub plugin: String,
    pub config: Value,
    pub ai_read: bool,
    pub ai_write: bool,
}

impl Account {
    pub fn from_value(value: &Value) -> Result<Self, ChatError> {
        let id = required(value, "id")?;
        validate_id(&id)?;
        let provider = required(value, "provider")?;
        validate_id(&provider)?;
        let config = value
            .get("config")
            .cloned()
            .unwrap_or_else(|| obj([] as [(&str, Value); 0]));
        if config.as_object().is_none() {
            return Err(ChatError::new(
                "invalid_config",
                "Account config must be an object",
            ));
        }
        Ok(Self {
            label: value
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string(),
            id,
            provider,
            plugin: value
                .get("plugin")
                .and_then(Value::as_str)
                .unwrap_or("builtin")
                .into(),
            config,
            ai_read: value
                .get("ai_read")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ai_write: value
                .get("ai_write")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    pub fn public(&self) -> Value {
        obj([
            ("id", self.id.as_str().into()),
            ("label", self.label.as_str().into()),
            ("provider", self.provider.as_str().into()),
            ("plugin", self.plugin.as_str().into()),
            ("ai_read", self.ai_read.into()),
            ("ai_write", self.ai_write.into()),
            ("configured", true.into()),
        ])
    }

    pub fn private(&self) -> Value {
        let mut value = self.public();
        if let Value::Object(fields) = &mut value {
            fields.insert("config".into(), self.config.clone());
        }
        value
    }
}

#[derive(Clone, Debug)]
pub struct PluginConfig {
    pub id: String,
    pub path: PathBuf,
    pub trusted: bool,
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    pub path: PathBuf,
    pub accounts: BTreeMap<String, Account>,
    pub plugins: BTreeMap<String, PluginConfig>,
}

impl ConfigStore {
    pub fn open(path: Option<PathBuf>) -> Result<Self, ChatError> {
        let path = path.map(Ok).unwrap_or_else(default_path)?;
        let mut store = Self {
            path,
            accounts: BTreeMap::new(),
            plugins: BTreeMap::new(),
        };
        if !store.path.exists() {
            return Ok(store);
        }
        let value = read_json(&store.path)?;
        if value.get("schema").and_then(Value::as_u64) != Some(1) {
            return Err(ChatError::new(
                "config_schema",
                "Expected zchat configuration schema 1",
            ));
        }
        for field in ["accounts", "plugins"] {
            if value
                .get(field)
                .is_some_and(|entries| entries.as_array().is_none())
            {
                return Err(ChatError::new(
                    "config_schema",
                    format!("Configuration field {field} must be an array"),
                ));
            }
        }
        if let Some(accounts) = value.get("accounts").and_then(Value::as_array) {
            for entry in accounts {
                let account = Account::from_value(entry)?;
                if store.accounts.insert(account.id.clone(), account).is_some() {
                    return Err(ChatError::new(
                        "duplicate_account",
                        "Account identifiers must be unique",
                    ));
                }
            }
        }
        if let Some(plugins) = value.get("plugins").and_then(Value::as_array) {
            for entry in plugins {
                let id = required(entry, "id")?;
                validate_id(&id)?;
                if id == "builtin" {
                    return Err(ChatError::new(
                        "invalid_plugin",
                        "The builtin plugin identifier is reserved",
                    ));
                }
                let plugin = PluginConfig {
                    id: id.clone(),
                    path: required(entry, "path")?.into(),
                    trusted: entry
                        .get("trusted")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                if store.plugins.insert(id, plugin).is_some() {
                    return Err(ChatError::new(
                        "duplicate_plugin",
                        "Plugin identifiers must be unique",
                    ));
                }
            }
        }
        Ok(store)
    }

    pub fn save(&self) -> Result<(), ChatError> {
        let value = obj([
            ("schema", 1u64.into()),
            (
                "accounts",
                arr(self.accounts.values().map(Account::private)),
            ),
            (
                "plugins",
                arr(self.plugins.values().map(|p| {
                    obj([
                        ("id", p.id.as_str().into()),
                        ("path", p.path.to_string_lossy().as_ref().into()),
                        ("trusted", p.trusted.into()),
                    ])
                })),
            ),
        ]);
        private_write(&self.path, value.try_stringify()?.as_bytes())
    }

    pub fn add_account(&mut self, account: Account) -> Result<(), ChatError> {
        if self.accounts.contains_key(&account.id) {
            return Err(ChatError::new(
                "account_exists",
                "Choose a new account name or remove the existing account first",
            ));
        }
        if account.plugin != "builtin" && !self.plugins.contains_key(&account.plugin) {
            return Err(ChatError::new(
                "plugin_not_found",
                "Register the selected plugin before adding its account",
            ));
        }
        let id = account.id.clone();
        self.accounts.insert(id.clone(), account);
        if let Err(error) = self.save() {
            self.accounts.remove(&id);
            return Err(error);
        }
        Ok(())
    }

    pub fn public_accounts(&self) -> Value {
        arr(self.accounts.values().map(Account::public))
    }
}

pub fn required(value: &Value, key: &str) -> Result<String, ChatError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ChatError::new(
                "invalid_argument",
                format!("Missing nonempty string: {key}"),
            )
        })
}

pub fn validate_id(value: &str) -> Result<(), ChatError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ChatError::new(
            "invalid_identifier",
            "Identifiers use 1..64 ASCII letters, digits, underscores or dashes",
        ));
    }
    Ok(())
}

pub fn default_path() -> Result<PathBuf, ChatError> {
    if let Some(path) = std::env::var_os("ZCHAT_CONFIG") {
        return Ok(path.into());
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".config")));
    base.map(|v| v.join("zchat/config.json"))
        .ok_or_else(|| ChatError::new("config_path", "Set ZCHAT_CONFIG or pass --config PATH"))
}

pub fn read_json(path: &Path) -> Result<Value, ChatError> {
    let file = fs::File::open(path).map_err(io_error)?;
    let mut input = String::new();
    file.take(MAX_CONFIG + 1)
        .read_to_string(&mut input)
        .map_err(io_error)?;
    if input.len() as u64 > MAX_CONFIG {
        return Err(ChatError::new(
            "config_too_large",
            "Configuration exceeds 1 MiB",
        ));
    }
    parse(&input)
}

pub fn resolve_secrets(value: &Value) -> Result<Value, ChatError> {
    match value {
        Value::Object(fields) if fields.len() == 1 && fields.contains_key("env") => {
            let name = fields["env"].as_str().ok_or_else(|| {
                ChatError::new("invalid_secret", "env references require a variable name")
            })?;
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(ChatError::new(
                    "invalid_secret",
                    "Invalid environment variable name",
                ));
            }
            std::env::var(name).map(Value::from).map_err(|_| {
                ChatError::new("missing_secret", format!("Set environment variable {name}"))
            })
        }
        Value::Object(fields) => fields
            .iter()
            .map(|(k, v)| Ok((k.clone(), resolve_secrets(v)?)))
            .collect::<Result<BTreeMap<_, _>, ChatError>>()
            .map(Value::Object),
        Value::Array(items) => items
            .iter()
            .map(resolve_secrets)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        value => Ok(value.clone()),
    }
}

pub fn private_write(path: &Path, bytes: &[u8]) -> Result<(), ChatError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let exists = parent.exists();
    fs::create_dir_all(parent).map_err(io_error)?;
    #[cfg(unix)]
    if !exists {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    #[cfg(not(unix))]
    let _ = exists;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".zchat-{}-{nonce}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary).map_err(io_error)?;
        file.write_all(bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        fs::rename(&temporary, path).map_err(io_error)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn io_error(error: std::io::Error) -> ChatError {
    ChatError::new("io_error", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_existing_collections_are_rejected_without_overwriting() {
        let path = std::env::temp_dir().join(format!(
            "zchat-invalid-config-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for text in [
            r#"{"schema":1,"accounts":{},"plugins":[]}"#,
            r#"{"schema":1,"accounts":[],"plugins":null}"#,
        ] {
            private_write(&path, text.as_bytes()).unwrap();
            assert_eq!(
                ConfigStore::open(Some(path.clone())).unwrap_err().code,
                "config_schema"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn account_public_view_never_contains_backend_credentials() {
        let account = Account::from_value(
            &parse(r#"{"id":"test","provider":"matrix","config":{"access_token":"private"}}"#)
                .unwrap(),
        )
        .unwrap();
        assert!(!account.public().stringify().contains("private"));
        assert!(!account.ai_read);
        assert!(validate_id("../escape").is_err());
    }
    #[test]
    fn persistence_preserves_account_and_secret_reference() {
        let path = std::env::temp_dir().join(format!(
            "zchat-config-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut config = ConfigStore::open(Some(path.clone())).unwrap();
        config.add_account(Account::from_value(&parse(r#"{"id":"work","provider":"matrix","config":{"access_token":{"env":"ZCHAT_TEST_TOKEN"}}}"#).unwrap()).unwrap()).unwrap();
        let loaded = ConfigStore::open(Some(path.clone())).unwrap();
        assert_eq!(
            loaded.accounts["work"].config,
            config.accounts["work"].config
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_file(path).unwrap();
    }
}
