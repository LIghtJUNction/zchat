use crate::{rpc_process::RpcProcess, util::*};
use std::time::Duration;
use zchat_core::{ChatError, Value};

pub(crate) struct DeltaChat {
    command: String,
    args: Vec<String>,
    accounts_path: Option<String>,
    account: u64,
    rpc: Option<RpcProcess>,
    io_started: bool,
}

impl DeltaChat {
    pub fn new(config: &Value) -> Result<Self, ChatError> {
        let command = config
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("deltachat-rpc-server")
            .to_owned();
        if command.is_empty() {
            return Err(ChatError::new(
                "invalid_config",
                "Delta Chat command is empty",
            ));
        }
        let args = config
            .get("args")
            .map(|v| {
                v.as_array()
                    .ok_or_else(|| ChatError::new("invalid_config", "args must be an array"))
            })
            .transpose()?
            .map(|v| {
                v.iter()
                    .map(|v| {
                        v.as_str().map(str::to_owned).ok_or_else(|| {
                            ChatError::new("invalid_config", "args entries must be strings")
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let account = u64_param(config, "account_id", 1);
        if account == 0 || account > u32::MAX as u64 {
            return Err(ChatError::new(
                "invalid_config",
                "account_id must be a nonzero uint32",
            ));
        }
        Ok(Self {
            command,
            args,
            accounts_path: config
                .get("accounts_path")
                .and_then(Value::as_str)
                .map(str::to_owned),
            account,
            rpc: None,
            io_started: false,
        })
    }

    fn connect(&mut self) -> Result<&mut RpcProcess, ChatError> {
        if self.rpc.is_none() {
            let mut rpc =
                RpcProcess::spawn(&self.command, &self.args, self.accounts_path.as_deref())?;
            rpc.request("get_system_info", array([]), Duration::from_secs(10))?;
            self.rpc = Some(rpc);
        }
        self.rpc
            .as_mut()
            .ok_or_else(|| ChatError::new("transport_closed", "Delta Chat RPC is unavailable"))
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, ChatError> {
        self.connect()?
            .request(method, params, Duration::from_secs(30))
    }

    pub fn invoke(&mut self, operation: &str, params: &Value) -> Result<Value, ChatError> {
        if operation == "capabilities" {
            return Ok(Self::capabilities());
        }
        if operation == "native.invoke" {
            return self.call(
                required(params, "method")?,
                params.get("params").cloned().unwrap_or(array([])),
            );
        }
        let account = number(self.account);
        if !self.io_started && operation != "account.status" {
            self.call("start_io", array([account.clone()]))?;
            self.io_started = true;
        }
        match operation {
            "account.status" => self.call("get_account_info", array([account])),
            "conversation.list" => {
                let ids = self.call(
                    "get_chatlist_entries",
                    array([
                        account.clone(),
                        Value::Null,
                        params.get("query").cloned().unwrap_or(Value::Null),
                        Value::Null,
                    ]),
                )?;
                let skip = page_offset(params, u64_param(params, "offset", 0) as usize)?
                    .min(list(&ids).len());
                let limit = u64_param(params, "limit", 100).clamp(1, 200) as usize;
                let selected = list(&ids)
                    .iter()
                    .skip(skip)
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let raw = self.call(
                    "get_chatlist_items_by_entries",
                    array([account, array(selected.clone())]),
                )?;
                let conversations = selected
                    .iter()
                    .map(|id| {
                        let key = scalar_id(id).as_str().unwrap_or_default().to_owned();
                        let item = value_at(&raw, &key);
                        let name = item
                            .get("name")
                            .or_else(|| item.get("chatListItem").and_then(|v| v.get("name")))
                            .cloned()
                            .unwrap_or(string(&key));
                        object([("id", string(key)), ("title", name), ("raw", item.clone())])
                    })
                    .collect::<Vec<_>>();
                let next = skip + selected.len();
                Ok(object([
                    ("conversations", array(conversations)),
                    (
                        "cursor",
                        if next < list(&ids).len() {
                            string(next.to_string())
                        } else {
                            Value::Null
                        },
                    ),
                ]))
            }
            "message.list" => {
                let room = integer_id(params, "conversation_id")?;
                let ids = self.call(
                    "get_message_ids",
                    array([
                        account.clone(),
                        room.clone(),
                        Value::Bool(false),
                        Value::Bool(false),
                    ]),
                )?;
                let ids = list(&ids);
                let end = page_offset(
                    params,
                    u64_param(params, "before_index", ids.len() as u64) as usize,
                )?
                .min(ids.len());
                let start =
                    end.saturating_sub(u64_param(params, "limit", 50).clamp(1, 200) as usize);
                let mut messages = Vec::new();
                for id in &ids[start..end] {
                    let raw = self.call("get_message", array([account.clone(), id.clone()]))?;
                    messages.push(object([
                        ("id", scalar_id(id)),
                        ("conversation_id", scalar_id(&room)),
                        (
                            "sender",
                            scalar_id(
                                raw.get("fromId")
                                    .or_else(|| raw.get("from_id"))
                                    .unwrap_or(&Value::Null),
                            ),
                        ),
                        ("text", value_at(&raw, "text").clone()),
                        ("timestamp", value_at(&raw, "timestamp").clone()),
                        ("raw", raw),
                    ]));
                }
                Ok(object([
                    ("messages", array(messages)),
                    (
                        "cursor",
                        if start > 0 {
                            string(start.to_string())
                        } else {
                            Value::Null
                        },
                    ),
                    ("order", string("oldest_first")),
                ]))
            }
            "message.send" => {
                let room = integer_id(params, "conversation_id")?;
                let id = self.call(
                    "misc_send_text_message",
                    array([account, room.clone(), string(required(params, "text")?)]),
                )?;
                Ok(object([
                    ("id", scalar_id(&id)),
                    ("conversation_id", scalar_id(&room)),
                    ("status", string("queued")),
                    ("raw", id),
                ]))
            }
            "message.edit" => self.call(
                "send_edit_request",
                array([
                    account,
                    integer_id(params, "message_id")?,
                    string(required(params, "text")?),
                ]),
            ),
            "message.delete" => self.call(
                if bool_param(params, "for_everyone", false) {
                    "delete_messages_for_all"
                } else {
                    "delete_messages"
                },
                array([account, array([integer_id(params, "message_id")?])]),
            ),
            "message.read" => self.call(
                "markseen_msgs",
                array([account, array([integer_id(params, "message_id")?])]),
            ),
            "message.react" => self.call(
                "send_reaction",
                array([
                    account,
                    integer_id(params, "message_id")?,
                    array([string(required(params, "reaction")?)]),
                ]),
            ),
            "conversation.join" => self.call(
                "secure_join",
                array([account, string(required(params, "link")?)]),
            ),
            "conversation.leave" => self.call(
                "leave_group",
                array([account, integer_id(params, "conversation_id")?]),
            ),
            "events.poll" => self.connect()?.poll_events(Duration::from_millis(
                u64_param(params, "timeout_ms", 0).min(30_000),
            )),
            _ => Err(unsupported("deltachat", operation)),
        }
    }

    pub fn capabilities() -> Value {
        capability(
            "deltachat",
            &[
                "account.status",
                "conversation.list",
                "conversation.join",
                "conversation.leave",
                "message.list",
                "message.send",
                "message.edit",
                "message.delete",
                "message.read",
                "message.react",
                "events.poll",
                "native.invoke",
            ],
            "managed_by_chatmail_core",
            &[
                "Requires the official deltachat-rpc-server executable and a configured local account.",
                "Message queue acceptance is distinct from delivery; observe backend events.",
                "Native JSON-RPC methods take positional params arrays.",
            ],
        )
    }
}
