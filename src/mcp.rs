use crate::{
    engine::{Engine, Origin},
    schema,
};
use std::io::{self, BufRead, Write};
use zchat_core::{ChatError, Value, obj, parse};
use zmcpio::{
    json::{Document, Limits, Token},
    mcp::{Action, Context, Server},
    tools::{Content, Tool, ToolResult},
    transport::{FrameStatus, LineDecoder},
};

pub struct Session {
    engine: Engine,
    server: Server<'static>,
}

impl Session {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            server: Server::new("zchat", crate::VERSION),
        }
    }

    pub fn handle(&mut self, input: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let mut tokens = vec![Token::EMPTY; 16384];
        let mut output = vec![0u8; 2 * 1024 * 1024];
        let action = match self.server.handle(input,&mut tokens,Limits::default(),&mut output) {
            Ok(value)=>value,
            Err(error)=>return Ok(Some(format!("{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":-32700,\"message\":{}}}}}",Value::from(format!("Invalid MCP request: {error}")).stringify()).into_bytes())),
        };
        let written = match action {
            Action::Reply(size) => Some(size),
            Action::ListTools { context, cursor } => {
                if cursor.is_some() {
                    Some(
                        self.server
                            .error(context, -32602, "Invalid tool cursor", &mut output)
                            .map_err(|e| e.to_string())?,
                    )
                } else {
                    let specs = schema::tools(
                        self.engine.access.allow_write,
                        self.engine.access.allow_native,
                    );
                    let inputs = specs
                        .iter()
                        .map(|s| s.input.stringify())
                        .collect::<Vec<_>>();
                    let mut stores = vec![vec![Token::EMPTY; 1024]; inputs.len()];
                    let docs = inputs
                        .iter()
                        .zip(stores.iter_mut())
                        .map(|(s, t)| Document::parse(s.as_bytes(), t, Limits::default()))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| e.to_string())?;
                    let tools = specs
                        .iter()
                        .zip(&docs)
                        .map(|(s, d)| Tool::new(&s.name, s.description, d.root()))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| e.to_string())?;
                    let size = self
                        .server
                        .list_tools(context, &tools, None, &mut output)
                        .map_err(|e| e.to_string())?;
                    let mut body =
                        parse(std::str::from_utf8(&output[..size]).map_err(|e| e.to_string())?)
                            .map_err(|e| e.to_string())?;
                    if let Value::Object(root) = &mut body
                        && let Some(Value::Object(result)) = root.get_mut("result")
                        && let Some(Value::Array(tools)) = result.get_mut("tools")
                    {
                        for (tool, spec) in tools.iter_mut().zip(&specs) {
                            if let Value::Object(fields) = tool {
                                fields.insert(
                                    "annotations".into(),
                                    obj([
                                        ("readOnlyHint", spec.read_only.into()),
                                        ("destructiveHint", spec.destructive.into()),
                                        ("openWorldHint", true.into()),
                                    ]),
                                );
                            }
                        }
                    }
                    return Ok(Some(body.stringify().into_bytes()));
                }
            }
            Action::CallTool(call) => {
                let specs = schema::tools(
                    self.engine.access.allow_write,
                    self.engine.access.allow_native,
                );
                if let Some(spec) = specs.iter().find(|s| call.name.matches(&s.name)) {
                    let arguments = parse(
                        std::str::from_utf8(call.arguments_bytes()).map_err(|e| e.to_string())?,
                    )
                    .map_err(|e| e.to_string())?;
                    let account = arguments.get("account").cloned().unwrap_or(Value::Null);
                    let params = arguments
                        .get("params")
                        .cloned()
                        .unwrap_or_else(|| obj([] as [(&str, Value); 0]));
                    let request = obj([
                        ("operation", spec.operation.into()),
                        ("account", account),
                        ("params", params),
                    ]);
                    let (value, failed) = match self.engine.call(request, Origin::Ai) {
                        Ok(value) => (value, false),
                        Err(error) => (obj([("error", error.to_value())]), true),
                    };
                    Some(self.encode(call.context, value, failed, &mut output)?)
                } else {
                    Some(
                        self.server
                            .error(
                                call.context,
                                -32602,
                                "Unknown or disabled zchat tool",
                                &mut output,
                            )
                            .map_err(|e| e.to_string())?,
                    )
                }
            }
            Action::Extension { context, .. } => Some(
                self.server
                    .error(context, -32601, "Method not found", &mut output)
                    .map_err(|e| e.to_string())?,
            ),
            Action::Notification(_) | Action::IncomingResponse(_) => None,
        };
        Ok(written.map(|n| output[..n].to_vec()))
    }

    fn encode(
        &self,
        context: Context<'_>,
        mut value: Value,
        failed: bool,
        output: &mut [u8],
    ) -> Result<usize, String> {
        if value.as_object().is_none() {
            value = obj([("result", value)]);
        }
        let text = match value.try_stringify() {
            Ok(text) => text,
            Err(_) => {
                return self
                    .server
                    .tool_result(
                        context,
                        ToolResult {
                            content: &[Content::Text(
                                "Response exceeds MCP limits. Request a smaller page.",
                            )],
                            structured_content: None,
                            is_error: true,
                        },
                        output,
                    )
                    .map_err(|e| e.to_string());
            }
        };
        let mut tokens = vec![Token::EMPTY; 65536];
        let encoded = Document::parse(
            text.as_bytes(),
            &mut tokens,
            Limits {
                max_bytes: 1024 * 1024,
                max_depth: 48,
                max_members: 16384,
                max_string_bytes: 1024 * 1024,
            },
        )
        .and_then(|document| {
            self.server.tool_result(
                context,
                ToolResult {
                    content: &[Content::Text(&text)],
                    structured_content: Some(document.root()),
                    is_error: failed,
                },
                output,
            )
        });
        match encoded {
            Ok(n)=>Ok(n),
            Err(_)=>self.server.tool_result(context,ToolResult {content:&[Content::Text("Response exceeds MCP limits. Request a smaller page or use the local CLI.")],structured_content:None,is_error:true},output).map_err(|e|e.to_string()),
        }
    }
}

pub fn stdio(engine: Engine) -> Result<(), ChatError> {
    let mut session = Session::new(engine);
    let mut storage = vec![0u8; 1024 * 1024];
    let mut decoder = LineDecoder::new(&mut storage, 1024 * 1024);
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let bytes = input.fill_buf().map_err(crate::config::io_error)?;
        if bytes.is_empty() {
            decoder.finish().map_err(|e| {
                ChatError::new("invalid_frame", format!("Incomplete MCP input: {e:?}"))
            })?;
            break;
        }
        let step = decoder.feed(bytes);
        input.consume(step.consumed);
        match step.status {
            FrameStatus::NeedInput => continue,
            FrameStatus::Ready => (),
            other => {
                return Err(ChatError::new(
                    "invalid_frame",
                    format!("MCP frame rejected: {other:?}"),
                ));
            }
        }
        if let Some(reply) = session
            .handle(
                decoder
                    .message()
                    .ok_or_else(|| ChatError::new("invalid_frame", "Missing MCP frame"))?,
            )
            .map_err(|e| ChatError::new("mcp_error", e))?
        {
            output.write_all(&reply).map_err(crate::config::io_error)?;
            output.write_all(b"\n").map_err(crate::config::io_error)?;
            output.flush().map_err(crate::config::io_error)?;
        }
        decoder.consume();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Access;
    fn session() -> Session {
        Session::new(
            Engine::new(
                Some(std::env::temp_dir().join(format!("zchat-mcp-{}.json", std::process::id()))),
                Access::default(),
            )
            .unwrap(),
        )
    }
    #[test]
    fn actual_mcp_handshake_lists_readonly_tools_and_executes_status() {
        let mut session = session();
        let init=session.handle(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}"#).unwrap().unwrap();
        assert!(String::from_utf8(init).unwrap().contains("zchat"));
        assert!(
            session
                .handle(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap()
                .is_none()
        );
        let tools = String::from_utf8(
            session
                .handle(br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(tools.contains("zchat_message_list"));
        assert!(!tools.contains("zchat_message_send"));
        assert!(!tools.contains("zchat_native_invoke"));
        let status=String::from_utf8(session.handle(br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"zchat_status","arguments":{}}}"#).unwrap().unwrap()).unwrap();
        assert!(status.contains("structuredContent"));
        assert!(status.contains("ai_write_enabled"));
    }
    #[test]
    fn disabled_write_tool_returns_protocol_error() {
        let mut session = session();
        session.handle(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}"#).unwrap();
        session
            .handle(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .unwrap();
        let result=String::from_utf8(session.handle(br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"zchat_message_send","arguments":{}}}"#).unwrap().unwrap()).unwrap();
        assert!(result.contains("Unknown or disabled"));
    }
}
