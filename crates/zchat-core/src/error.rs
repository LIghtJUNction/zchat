use alloc::string::String;
use core::fmt;

use crate::{Value, obj};

pub type Result<T> = core::result::Result<T, ChatError>;

/// A machine-readable failure; credentials and message contents must not be put
/// into `message` by callers that send errors to logs or AI clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl ChatError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn to_value(&self) -> Value {
        obj([
            ("code", Value::from(self.code.as_str())),
            ("message", Value::from(self.message.as_str())),
            ("retryable", Value::from(self.retryable)),
        ])
    }
}

impl fmt::Display for ChatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl core::error::Error for ChatError {}
