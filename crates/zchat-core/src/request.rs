use alloc::{format, string::String};

use crate::{ChatError, JsonLimits, Result, Value, arr, obj, parse};

pub const DEFAULT_PAGE_SIZE: usize = 50;
pub const MAX_PAGE_SIZE: usize = 200;
const MAX_TEXT_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Actor {
    #[default]
    Human,
    Ai,
}

impl Actor {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Ai => "ai",
        }
    }
}

/// Common host operations. A plugin's own manifest selects a supported subset.
/// `native.invoke` carries a documented platform method with all its original
/// arguments; its effects are unknown to the common model, so it is privileged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub destructive: bool,
}

macro_rules! operations {
    ($($name:literal => ($description:literal, $read_only:literal, $destructive:literal)),* $(,)?) => {
        pub static OPERATIONS: &[Operation] = &[$(Operation {
            name: $name, description: $description, read_only: $read_only, destructive: $destructive,
        }),*];
    };
}

operations! {
    "account.login" => ("Start or continue the platform's account authorization", false, false),
    "account.logout" => ("Close and revoke this account session when supported", false, true),
    "account.status" => ("Read connection and authorization state", true, false),
    "conversation.list" => ("List conversations with bounded pagination", true, false),
    "conversation.create" => ("Create a direct conversation or group", false, false),
    "conversation.join" => ("Join a conversation using its native identifier", false, false),
    "conversation.leave" => ("Leave a conversation", false, true),
    "conversation.members" => ("Read the conversation's current members", true, false),
    "conversation.update" => ("Change supported conversation properties", false, false),
    "message.list" => ("Read a page of messages in server order", true, false),
    "message.send" => ("Send a message with a stable idempotency key", false, false),
    "message.edit" => ("Edit an existing message", false, false),
    "message.delete" => ("Delete or redact an existing message", false, true),
    "message.react" => ("Add or remove a reaction", false, false),
    "message.read" => ("Send the platform's normal read acknowledgement", false, false),
    "message.search" => ("Search messages using supported server or local indexes", true, false),
    "message.forward" => ("Forward a message to an explicit destination", false, false),
    "attachment.upload" => ("Upload an attachment using the platform's media transport", false, false),
    "attachment.download" => ("Read an attachment from the platform", true, false),
    "presence.set" => ("Update supported presence state", false, false),
    "typing.set" => ("Send normal typing state", false, false),
    "call.start" => ("Start a supported audio or video call", false, false),
    "call.answer" => ("Answer an incoming supported call", false, false),
    "call.end" => ("End an existing call", false, false),
    "native.invoke" => ("Invoke an explicitly named platform-specific method", false, true),
    "events.poll" => ("Read ordered platform updates after a resumable cursor", true, false),
}

pub fn operation(name: &str) -> Option<&'static Operation> {
    OPERATIONS.iter().find(|operation| operation.name == name)
}

pub fn operation_info(name: &str) -> Option<&'static Operation> {
    operation(name)
}
pub fn catalog() -> &'static [Operation] {
    OPERATIONS
}

pub fn operations_manifest() -> Value {
    arr(OPERATIONS.iter().map(|operation| {
        obj([
            ("operation", Value::from(operation.name)),
            ("description", Value::from(operation.description)),
            ("read_only", Value::from(operation.read_only)),
            ("destructive", Value::from(operation.destructive)),
        ])
    }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: Option<Value>,
    pub account: String,
    pub operation: String,
    pub params: Value,
    pub actor: Actor,
}

impl Request {
    pub fn new(account: impl Into<String>, operation: impl Into<String>, params: Value) -> Self {
        Self {
            id: None,
            account: account.into(),
            operation: operation.into(),
            params,
            actor: Actor::Human,
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        Self::from_value(&parse(input)?)
    }

    pub fn from_value(value: &Value) -> Result<Self> {
        if !value.is_object() {
            return Err(ChatError::new(
                "invalid_request",
                "request must be a JSON object",
            ));
        }
        let account = value
            .get_str("account")
            .ok_or_else(|| ChatError::new("invalid_request", "account must be a string"))?;
        let operation = value
            .get_str("operation")
            .ok_or_else(|| ChatError::new("invalid_request", "operation must be a string"))?;
        let actor = match value.get("actor") {
            None => Actor::Human,
            Some(Value::String(actor)) if actor == "human" => Actor::Human,
            Some(Value::String(actor)) if actor == "ai" => Actor::Ai,
            _ => {
                return Err(ChatError::new(
                    "invalid_request",
                    "actor must be human or ai",
                ));
            }
        };
        let request = Self {
            id: value.get("id").cloned(),
            account: account.into(),
            operation: operation.into(),
            params: value.get("params").cloned().unwrap_or_else(Value::object),
            actor,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn to_value(&self) -> Value {
        let mut value = obj([
            ("account", Value::from(self.account.as_str())),
            ("operation", Value::from(self.operation.as_str())),
            ("params", self.params.clone()),
            ("actor", Value::from(self.actor.as_str())),
        ]);
        if let Some(id) = &self.id {
            value
                .as_object_mut()
                .expect("constructed object")
                .insert("id".into(), id.clone());
        }
        value
    }

    /// Check common shape and resource limits without changing platform fields.
    /// Authorization is a separate `RequestPolicy::authorize` call.
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.account, "account", 256)?;
        let operation = operation(&self.operation).ok_or_else(|| {
            ChatError::new(
                "unknown_operation",
                "unknown unified operation; use native.invoke for a documented platform extension",
            )
        })?;
        if !self.params.is_object() {
            return Err(ChatError::new("invalid_params", "params must be an object"));
        }
        if let Some(id) = &self.id {
            match id {
                Value::String(id) => validate_identifier(id, "id", 256)?,
                Value::Number(_) => {
                    id.try_stringify()?;
                }
                _ => {
                    return Err(ChatError::new(
                        "invalid_request",
                        "id must be a string or number when present",
                    ));
                }
            }
        }
        if let Some(limit) = self.params.get("limit") {
            let limit = limit.as_u64().ok_or_else(|| {
                ChatError::new("invalid_params", "limit must be a positive integer")
            })?;
            if limit == 0 || limit > MAX_PAGE_SIZE as u64 {
                return Err(ChatError::new(
                    "invalid_params",
                    format!("limit must be between 1 and {MAX_PAGE_SIZE}"),
                ));
            }
        }
        if let Some(cursor) = self.params.get("cursor") {
            match cursor {
                Value::String(cursor)
                    if cursor.len() <= 4096 && !cursor.chars().any(char::is_control) => {}
                Value::Number(_) if cursor.as_u64().is_some() => {}
                _ => {
                    return Err(ChatError::new(
                        "invalid_params",
                        "cursor must be a bounded string or unsigned integer",
                    ));
                }
            }
        }
        if let Some(text) = self.params.get("text") {
            let text = text
                .as_str()
                .ok_or_else(|| ChatError::new("invalid_params", "text must be a string"))?;
            if text.len() > MAX_TEXT_BYTES {
                return Err(ChatError::new("invalid_params", "text exceeds 256 KiB"));
            }
        }
        if let Some(key) = self.params.get("idempotency_key") {
            let key = key.as_str().ok_or_else(|| {
                ChatError::new("invalid_params", "idempotency_key must be a string")
            })?;
            validate_identifier(key, "idempotency_key", 128)?;
        }
        if operation.name == "native.invoke" {
            if self
                .params
                .as_object()
                .is_some_and(|fields| fields.is_empty())
            {
                return Err(ChatError::new(
                    "invalid_params",
                    "native.invoke requires the provider's documented request parameters",
                ));
            }
            // HTTP/RPC method names are one native convention. Other engines
            // use command strings, XML stanzas or complete request objects.
            // Their schemas belong to the plugin, not the portable host.
            if let Some(method) = self.params.get("method") {
                let method = method.as_str().ok_or_else(|| {
                    ChatError::new("invalid_params", "native method must be a string")
                })?;
                validate_identifier(method, "method", 256)?;
            }
        }
        // Also validate directly constructed Values; serializable size bounds the
        // combined request, not just each field considered in isolation.
        self.to_value()
            .stringify_with_limits(JsonLimits::default())?;
        Ok(())
    }

    pub fn page_size(&self) -> usize {
        self.params
            .get_u64("limit")
            .map_or(DEFAULT_PAGE_SIZE, |limit| {
                limit.clamp(1, MAX_PAGE_SIZE as u64) as usize
            })
    }
}

fn validate_identifier(value: &str, field: &str, maximum: usize) -> Result<()> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(ChatError::new(
            "invalid_request",
            format!("{field} must contain 1..{maximum} bytes without control characters"),
        ));
    }
    Ok(())
}

/// The host chooses a policy from authenticated context, never from untrusted
/// request fields. In particular, an MCP entrypoint must force `Actor::Ai` after
/// parsing so a client cannot grant itself human permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestPolicy {
    pub allow_ai: bool,
    pub allow_writes: bool,
    pub allow_destructive: bool,
    pub allow_native: bool,
    pub require_idempotency_key: bool,
}

impl Default for RequestPolicy {
    fn default() -> Self {
        Self::read_only()
    }
}

impl RequestPolicy {
    pub const fn read_only() -> Self {
        Self {
            allow_ai: false,
            allow_writes: false,
            allow_destructive: false,
            allow_native: false,
            require_idempotency_key: false,
        }
    }

    pub const fn interactive() -> Self {
        Self {
            allow_ai: false,
            allow_writes: true,
            allow_destructive: true,
            allow_native: true,
            require_idempotency_key: false,
        }
    }

    pub fn authorize(self, request: &Request) -> Result<()> {
        request.validate()?;
        if request.actor == Actor::Ai && !self.allow_ai {
            return Err(ChatError::new(
                "ai_access_denied",
                "AI access is disabled for this account",
            ));
        }
        let operation = operation(&request.operation).expect("validated operation");
        if operation.name == "native.invoke" && !self.allow_native {
            return Err(ChatError::new(
                "native_access_denied",
                "platform-native invocation is disabled",
            ));
        }
        if !operation.read_only && !self.allow_writes {
            return Err(ChatError::new(
                "write_access_denied",
                "this entrypoint allows read-only operations",
            ));
        }
        if operation.destructive && !self.allow_destructive {
            return Err(ChatError::new(
                "destructive_access_denied",
                "destructive operations are disabled",
            ));
        }
        if !operation.read_only
            && self.require_idempotency_key
            && request.params.get_str("idempotency_key").is_none()
        {
            return Err(ChatError::new(
                "idempotency_key_required",
                "this entrypoint requires an idempotency_key for writes",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(operation: &str) -> Request {
        Request::new("work-matrix", operation, Value::object())
    }

    #[test]
    fn platform_parameters_and_large_request_id_survive() {
        let input = r#"{"id":9007199254740993123,"account":"personal","operation":"message.send","params":{"text":"hi","vendor":{"expires":30}},"actor":"human"}"#;
        let request = Request::parse(input).unwrap();
        assert_eq!(
            request.params.get("vendor").unwrap().get_u64("expires"),
            Some(30)
        );
        assert_eq!(Request::from_value(&request.to_value()).unwrap(), request);
    }

    #[test]
    fn ai_actor_needs_account_permission_and_write_permission() {
        let mut request = request("message.send");
        request.actor = Actor::Ai;
        let mut policy = RequestPolicy::read_only();
        assert_eq!(
            policy.authorize(&request).unwrap_err().code,
            "ai_access_denied"
        );
        policy.allow_ai = true;
        assert_eq!(
            policy.authorize(&request).unwrap_err().code,
            "write_access_denied"
        );
        policy.allow_writes = true;
        assert!(policy.authorize(&request).is_ok());
    }

    #[test]
    fn read_receipts_are_writes_and_deletion_is_destructive() {
        assert!(!operation("message.read").unwrap().read_only);
        let mut policy = RequestPolicy::interactive();
        policy.allow_destructive = false;
        assert_eq!(
            policy
                .authorize(&request("message.delete"))
                .unwrap_err()
                .code,
            "destructive_access_denied"
        );
    }

    #[test]
    fn native_method_is_preserved_and_privileged() {
        let request = Request::new(
            "telegram",
            "native.invoke",
            obj([
                ("method", Value::from("getSupergroupFullInfo")),
                ("arguments", obj([("supergroup_id", Value::from(123u64))])),
            ]),
        );
        request.validate().unwrap();
        assert_eq!(
            RequestPolicy::read_only()
                .authorize(&request)
                .unwrap_err()
                .code,
            "native_access_denied"
        );
        assert!(RequestPolicy::interactive().authorize(&request).is_ok());
    }

    #[test]
    fn native_protocol_envelopes_reach_their_plugin_without_a_fake_method() {
        for params in [
            obj([("command", Value::from("/_get chats 1"))]),
            obj([("xml", Value::from("<iq type='get' id='1'/>"))]),
            obj([("request", obj([("@type", Value::from("getMe"))]))]),
        ] {
            let request = Request::new("account", "native.invoke", params);
            assert_eq!(Request::from_value(&request.to_value()).unwrap(), request);
            assert_eq!(
                RequestPolicy::read_only()
                    .authorize(&request)
                    .unwrap_err()
                    .code,
                "native_access_denied"
            );
        }
    }

    #[test]
    fn limit_and_shape_errors_are_explicit() {
        for input in [
            r#"{"account":"x","operation":"message.list","params":{"limit":0}}"#,
            r#"{"account":"x","operation":"message.list","params":{"limit":201}}"#,
            r#"{"account":"x","operation":"message.list","params":[]}"#,
            r#"{"account":"x","operation":"message.list","actor":"root"}"#,
            r#"{"account":"x","operation":"anything"}"#,
        ] {
            assert!(Request::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn idempotency_policy_is_explicit() {
        let mut policy = RequestPolicy::interactive();
        policy.require_idempotency_key = true;
        let mut request = request("message.send");
        assert_eq!(
            policy.authorize(&request).unwrap_err().code,
            "idempotency_key_required"
        );
        request.params = obj([("idempotency_key", Value::from("send-0001"))]);
        assert!(policy.authorize(&request).is_ok());
    }
}
