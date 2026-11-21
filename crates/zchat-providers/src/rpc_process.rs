//! A persistent, correlated stdio JSON-RPC connection. A pending event request
//! never blocks subsequent requests. Input, responses and event storage are bounded.
use crate::util::*;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zchat_core::{ChatError, Value, parse};

pub(crate) struct RpcProcess {
    child: Child,
    input: Option<SyncSender<Vec<u8>>>,
    output: Option<Receiver<Result<Value, ChatError>>>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    next_id: u64,
    event_id: Option<u64>,
    events: EventQueue,
}

impl RpcProcess {
    pub fn spawn(
        command: &str,
        args: &[String],
        accounts_path: Option<&str>,
    ) -> Result<Self, ChatError> {
        let mut process = Command::new(command);
        process
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.process_group(0);
        }
        if let Some(path) = accounts_path {
            process.env("DC_ACCOUNTS_PATH", path);
        }
        let mut child = process
            .spawn()
            .map_err(|e| io_error("start Delta Chat RPC server", e))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ChatError::new("transport_error", "RPC stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ChatError::new("transport_error", "RPC stdout unavailable"))?;
        let (input, input_rx) = mpsc::sync_channel::<Vec<u8>>(4);
        let (out_tx, output) = mpsc::sync_channel(4);
        let writer_errors = out_tx.clone();
        let writer = thread::spawn(move || {
            while let Ok(bytes) = input_rx.recv() {
                if let Err(error) = stdin.write_all(&bytes).and_then(|_| stdin.flush()) {
                    let _ = writer_errors.send(Err(io_error("write RPC request", error)));
                    break;
                }
            }
        });
        let reader = thread::spawn(move || {
            let mut reader = BufReader::with_capacity(8192, stdout);
            loop {
                let result = read_line(&mut reader);
                match result {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let parsed = parse(&line);
                        let failed = parsed.is_err();
                        if out_tx.send(parsed).is_err() || failed {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = out_tx.send(Err(ChatError::new(
                            "transport_closed",
                            "RPC server closed stdout",
                        )));
                        break;
                    }
                    Err(error) => {
                        let _ = out_tx.send(Err(error));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            input: Some(input),
            output: Some(output),
            reader: Some(reader),
            writer: Some(writer),
            next_id: 1,
            event_id: None,
            events: EventQueue::default(),
        })
    }

    fn send(&mut self, method: &str, params: Value) -> Result<u64, ChatError> {
        if method.is_empty() || method.len() > 128 {
            return Err(ChatError::new("invalid_params", "invalid RPC method"));
        }
        if !matches!(params, Value::Array(_)) {
            return Err(ChatError::new(
                "invalid_params",
                "Delta Chat RPC params must be a positional array",
            ));
        }
        if self
            .child
            .try_wait()
            .map_err(|e| io_error("RPC process status", e))?
            .is_some()
        {
            return Err(ChatError::new(
                "transport_closed",
                "Delta Chat RPC server has exited",
            ));
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| ChatError::new("capacity", "RPC request counter exhausted"))?;
        let mut request = object([
            ("jsonrpc", string("2.0")),
            ("id", number(id)),
            ("method", string(method)),
            ("params", params),
        ])
        .try_stringify()?;
        request.push('\n');
        self.input
            .as_ref()
            .ok_or_else(|| ChatError::new("transport_closed", "RPC writer closed"))?
            .try_send(request.into_bytes())
            .map_err(|_| ChatError::new("backpressure", "RPC request queue is full or closed"))?;
        Ok(id)
    }

    fn receive(&self, timeout: Duration) -> Result<Option<Value>, ChatError> {
        let output = self
            .output
            .as_ref()
            .ok_or_else(|| ChatError::new("transport_closed", "RPC reader closed"))?;
        match output.recv_timeout(timeout) {
            Ok(result) => result.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(ChatError::new("transport_closed", "RPC reader stopped"))
            }
        }
    }

    fn route_event(&mut self, value: Value) -> Result<(), ChatError> {
        if self.event_id.is_some() && value.get("id").and_then(Value::as_u64) == self.event_id {
            self.event_id = None;
            let event = rpc_result(value)?;
            self.events.push(event);
        } else if value.get("id").is_none() {
            self.events.push(value);
        }
        // Late replies to requests that timed out are intentionally discarded;
        // they must never be confused with a later request's result.
        Ok(())
    }

    pub fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ChatError> {
        let id = self.send(method, params)?;
        let deadline = Instant::now() + timeout;
        loop {
            let value = self.receive(deadline.saturating_duration_since(Instant::now()))?.ok_or_else(|| ChatError::new("timeout", format!("Delta Chat {method} timed out; delivery may have occurred, inspect state before retrying a mutation")))?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return rpc_result(value);
            }
            self.route_event(value)?;
            if Instant::now() >= deadline {
                return Err(ChatError::new("timeout", "RPC request timed out"));
            }
        }
    }

    pub fn poll_events(&mut self, timeout: Duration) -> Result<Value, ChatError> {
        if self.event_id.is_none() {
            self.event_id = Some(self.send("get_next_event", array([]))?);
        }
        let deadline = Instant::now() + timeout;
        let mut count = 0;
        loop {
            let wait = if self.events.is_empty() {
                deadline.saturating_duration_since(Instant::now())
            } else {
                Duration::ZERO
            };
            let Some(value) = self.receive(wait)? else {
                break;
            };
            self.route_event(value)?;
            count += 1;
            if self.event_id.is_none() {
                self.event_id = Some(self.send("get_next_event", array([]))?);
            }
            if count >= MAX_EVENTS || Instant::now() >= deadline {
                break;
            }
        }
        Ok(self.events.drain())
    }
}

impl Drop for RpcProcess {
    fn drop(&mut self) {
        // Closing channels first releases producers blocked by backpressure.
        self.output.take();
        self.input.take();
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // This is our private process group (created in spawn), never the
            // caller's group. Close inherited pipes held by backend children.
            // Once wait has reaped a child its PID may be reused; only signal
            // the group while our unreaped leader still owns that PID.
            if matches!(self.child.try_wait(), Ok(None))
                && let Ok(pid) = i32::try_from(self.child.id())
            {
                unsafe {
                    kill(-pid, 9);
                }
            }
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let exited = self.child.try_wait().ok().flatten().is_some();
            let finished = self.writer.as_ref().is_none_or(JoinHandle::is_finished)
                && self.reader.as_ref().is_none_or(JoinHandle::is_finished);
            if exited && finished || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        // A nonstandard backend can pass its pipes to an unrelated process.
        // Never let that make plugin shutdown block forever. Normal backends
        // are reaped and joined; lingering IO threads own no plugin account.
        if let Some(thread) = self.writer.take().filter(JoinHandle::is_finished) {
            let _ = thread.join();
        }
        if let Some(thread) = self.reader.take().filter(JoinHandle::is_finished) {
            let _ = thread.join();
        }
    }
}

fn rpc_result(value: Value) -> Result<Value, ChatError> {
    if let Some(error) = value.get("error") {
        let code = match value_at(error, "code") {
            Value::Number(code) => code.chars().take(32).collect::<String>(),
            _ => "unknown".to_owned(),
        };
        return Err(ChatError::new(
            "upstream_error",
            format!("Delta Chat RPC error {code}"),
        ));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| ChatError::new("invalid_response", "RPC response has no result"))
}

fn read_line(reader: &mut impl BufRead) -> Result<Option<String>, ChatError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|e| io_error("read RPC response", e))?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(ChatError::new(
                "invalid_response",
                "RPC response ended without newline",
            ));
        }
        let count = available
            .iter()
            .position(|v| *v == b'\n')
            .map_or(available.len(), |p| p + 1);
        if bytes.len() + count > MAX_WIRE_BYTES {
            return Err(ChatError::new(
                "payload_too_large",
                "RPC response exceeds 1 MiB",
            ));
        }
        let complete = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| ChatError::new("invalid_response", "RPC response is not UTF-8"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn python() -> &'static str {
        if cfg!(target_os = "windows") {
            "python"
        } else {
            "python3"
        }
    }

    #[test]
    fn pending_event_does_not_block_correlated_requests() {
        let script = "import sys,json\npending=None\nfor line in sys.stdin:\n r=json.loads(line)\n if r['method']=='get_next_event': pending=r['id']; continue\n if pending is not None:\n  print(json.dumps({'jsonrpc':'2.0','id':pending,'result':{'kind':'message'}}),flush=True); pending=None\n print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':r['params']}),flush=True)\n";
        let mut rpc =
            RpcProcess::spawn(python(), &["-u".into(), "-c".into(), script.into()], None).unwrap();
        let initial = rpc.poll_events(Duration::ZERO).unwrap();
        assert!(list(value_at(&initial, "events")).is_empty());
        let response = rpc
            .request(
                "echo",
                array([string("exact"), number(7)]),
                Duration::from_secs(3),
            )
            .unwrap();
        assert_eq!(
            list(&response).first().and_then(Value::as_str),
            Some("exact")
        );
        let events = rpc.poll_events(Duration::ZERO).unwrap();
        assert_eq!(list(value_at(&events, "events")).len(), 1);
    }

    #[test]
    fn silent_process_times_out_and_drops() {
        let mut rpc = RpcProcess::spawn(
            python(),
            &[
                "-u".into(),
                "-c".into(),
                "import time; time.sleep(10)".into(),
            ],
            None,
        )
        .unwrap();
        let start = Instant::now();
        assert!(
            rpc.request("echo", array([]), Duration::from_millis(25))
                .is_err()
        );
        drop(rpc);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn bounded_line_reader_rejects_unterminated_and_large_records() {
        assert!(read_line(&mut std::io::Cursor::new(b"{}".to_vec())).is_err());
        assert!(read_line(&mut std::io::Cursor::new(vec![b'a'; MAX_WIRE_BYTES + 1])).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn child_inherited_pipe_does_not_hang_shutdown() {
        let script = "import os,time,sys\nos.fork()\nprint('ready',flush=True)\ntime.sleep(30)";
        let rpc =
            RpcProcess::spawn(python(), &["-u".into(), "-c".into(), script.into()], None).unwrap();
        let _ = rpc.receive(Duration::from_secs(3));
        let start = Instant::now();
        drop(rpc);
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
