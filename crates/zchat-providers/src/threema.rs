use crate::{http, util::*};
use std::time::Duration;
use zchat_core::{ChatError, Value};

pub(crate) struct ThreemaGateway {
    base: String,
    identity: String,
    secret: String,
    mode: String,
}

impl ThreemaGateway {
    pub fn new(config: &Value) -> Result<Self, ChatError> {
        let identity = required(config, "identity")?.to_owned();
        if identity.len() != 8
            || !identity.starts_with('*')
            || !identity
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'*')
        {
            return Err(ChatError::new(
                "invalid_config",
                "Threema Gateway requires an 8-character API identity beginning with *",
            ));
        }
        let mode = config
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("e2e")
            .to_owned();
        if !matches!(mode.as_str(), "basic" | "e2e") {
            return Err(ChatError::new(
                "invalid_config",
                "Threema mode must be basic or e2e",
            ));
        }
        let base = config
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or("https://msgapi.threema.ch")
            .trim_end_matches('/')
            .to_owned();
        http::validate_url(&base)?;
        Ok(Self {
            base,
            identity,
            secret: required(config, "secret")?.to_owned(),
            mode,
        })
    }

    fn form(&self, fields: &[(&str, &str)]) -> String {
        let mut values = vec![
            ("from", self.identity.as_str()),
            ("secret", self.secret.as_str()),
        ];
        values.extend_from_slice(fields);
        values
            .into_iter()
            .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    fn call(
        &self,
        method: &str,
        endpoint: &str,
        fields: &[(&str, &str)],
    ) -> Result<String, ChatError> {
        let form = self.form(fields);
        // Gateway mandates query authentication for its GET endpoints. The
        // complete URL is supplied over curl's stdin and never logged or placed
        // in argv; redirects are disabled by the shared HTTP transport.
        let url = if method == "GET" {
            format!("{}{endpoint}?{form}", self.base)
        } else {
            format!("{}{endpoint}", self.base)
        };
        let response = http::request(
            method,
            &url,
            &[("Content-Type", "application/x-www-form-urlencoded")],
            (method != "GET").then_some(form.as_str()),
            Duration::from_secs(30),
        )?;
        if !(200..300).contains(&response.status) {
            return Err(ChatError::new(
                "upstream_error",
                format!("Threema Gateway HTTP {}", response.status),
            ));
        }
        Ok(response.body.trim().to_owned())
    }

    pub fn invoke(&mut self, operation: &str, params: &Value) -> Result<Value, ChatError> {
        match operation {
            "capabilities" => Ok(Self::capabilities()),
            "account.status" => {
                let credits = self.call("GET", "/credits", &[])?;
                Ok(object([
                    ("identity", string(&self.identity)),
                    ("mode", string(&self.mode)),
                    ("credits", string(credits)),
                    ("account_type", string("gateway")),
                ]))
            }
            "message.send" => {
                if self.mode != "basic" {
                    return Err(ChatError::new(
                        "encryption_required",
                        "The e2e Gateway mode requires an encrypted nonce/box from a crypto-capable plugin; use native.invoke send_e2e. Plaintext message.send is available only with explicitly selected basic mode.",
                    ));
                }
                let to = recipient(params, "conversation_id")?;
                let id = self.call(
                    "POST",
                    "/send_simple",
                    &[("to", &to), ("text", required(params, "text")?)],
                )?;
                Ok(object([
                    ("id", string(id)),
                    ("conversation_id", string(to)),
                    ("status", string("accepted")),
                    ("encryption", string("server_side")),
                ]))
            }
            "native.invoke" => {
                let method = required(params, "method")?;
                let params = params.get("params").unwrap_or(params);
                match method {
                    "credits" => Ok(object([(
                        "credits",
                        string(self.call("GET", "/credits", &[])?),
                    )])),
                    "capabilities" | "pubkeys" => {
                        let to = recipient(params, "to")?;
                        Ok(object([(
                            "result",
                            string(self.call("GET", &format!("/{method}/{to}"), &[])?),
                        )]))
                    }
                    "send_simple" => {
                        if self.mode != "basic" {
                            return Err(ChatError::new(
                                "encryption_required",
                                "send_simple requires explicitly configured basic mode",
                            ));
                        }
                        let to = recipient(params, "to")?;
                        Ok(object([(
                            "id",
                            string(self.call(
                                "POST",
                                "/send_simple",
                                &[("to", &to), ("text", required(params, "text")?)],
                            )?),
                        )]))
                    }
                    "send_e2e" => {
                        if self.mode != "e2e" {
                            return Err(ChatError::new(
                                "invalid_config",
                                "send_e2e requires e2e Gateway mode",
                            ));
                        }
                        let to = recipient(params, "to")?;
                        let nonce = required(params, "nonce")?;
                        let encrypted = required(params, "box")?;
                        if nonce.len() != 48
                            || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
                            || encrypted.len() > 15624
                            || encrypted.len() < 32
                            || encrypted.len() % 2 != 0
                            || !encrypted.bytes().all(|b| b.is_ascii_hexdigit())
                        {
                            return Err(ChatError::new(
                                "invalid_params",
                                "send_e2e requires a 24-byte hex nonce and a bounded authenticated ciphertext box",
                            ));
                        }
                        Ok(object([(
                            "id",
                            string(self.call(
                                "POST",
                                "/send_e2e",
                                &[("to", &to), ("nonce", nonce), ("box", encrypted)],
                            )?),
                        )]))
                    }
                    _ => Err(unsupported("threema-gateway", method)),
                }
            }
            _ => Err(unsupported("threema-gateway", operation)),
        }
    }

    pub fn capabilities() -> Value {
        capability(
            "threema-gateway",
            &["account.status", "message.send", "native.invoke"],
            "explicit_basic_or_external_e2e",
            &[
                "This is the separate paid Threema Gateway API identity, not a personal Threema account client.",
                "Basic mode explicitly uses server-side encryption and supports outbound text.",
                "E2E transport accepts already encrypted nonce/box values; this adapter does not implement key management or callbacks.",
                "Personal chats/history, inbound webhook hosting, calls and ordinary account login are not supported by this Gateway adapter.",
            ],
        )
    }
}

fn recipient(params: &Value, key: &str) -> Result<String, ChatError> {
    let value = required(params, key)?;
    if value.len() != 8
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'*')
    {
        return Err(ChatError::new(
            "invalid_params",
            "Threema recipients must be 8-character identities",
        ));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gateway_identity_and_encryption_mode_are_explicit() {
        assert!(
            ThreemaGateway::new(&object([
                ("identity", string("PERSONAL")),
                ("secret", string("test"))
            ]))
            .is_err()
        );
        let mut gateway = ThreemaGateway::new(&object([
            ("identity", string("*ZCHAT01")),
            ("secret", string("test")),
        ]))
        .unwrap();
        let result = gateway.invoke(
            "message.send",
            &object([
                ("conversation_id", string("ABCDEFGH")),
                ("text", string("must not leave test")),
            ]),
        );
        assert!(result.is_err());
    }
}
