//! HTTPS transport using the system TLS implementation. Secrets are passed on
//! stdin, never in process arguments, shell commands or diagnostic text.
use crate::util::{MAX_WIRE_BYTES, io_error, text};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use zchat_core::{ChatError, Value, parse};

pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn json(self) -> Result<Value, ChatError> {
        if !(200..300).contains(&self.status) {
            let parsed = parse(&self.body).unwrap_or(Value::Null);
            let code = text(&parsed, "errcode");
            return Err(ChatError::new(
                "upstream_error",
                format!(
                    "HTTP {}{}",
                    self.status,
                    if code.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", code.chars().take(80).collect::<String>())
                    }
                ),
            ));
        }
        if self.body.trim().is_empty() {
            Ok(Value::Null)
        } else {
            parse(&self.body)
        }
    }
}

pub(crate) fn validate_url(url: &str) -> Result<(), ChatError> {
    if url.bytes().any(|v| v <= b' ' || v == 0x7f) {
        return Err(ChatError::new(
            "invalid_config",
            "URL contains whitespace or control characters",
        ));
    }
    let uri: tungstenite::http::Uri = url
        .parse()
        .map_err(|_| ChatError::new("invalid_config", "invalid HTTP URL"))?;
    let host = uri
        .host()
        .ok_or_else(|| ChatError::new("invalid_config", "URL needs a host"))?;
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err(ChatError::new(
            "invalid_config",
            "URL credentials are not supported; use configuration fields",
        ));
    }
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") if matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1") => Ok(()),
        _ => Err(ChatError::new(
            "invalid_config",
            "HTTPS is required except for loopback test servers",
        )),
    }
}

fn quote(value: &str) -> Result<String, ChatError> {
    if value.contains('\0') {
        return Err(ChatError::new(
            "invalid_params",
            "NUL is not allowed in HTTP input",
        ));
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

pub(crate) fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponse, ChatError> {
    validate_url(url)?;
    if !matches!(
        method,
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
    ) {
        return Err(ChatError::new("invalid_params", "unsupported HTTP method"));
    }
    if body.is_some_and(|v| v.len() > MAX_WIRE_BYTES) {
        return Err(ChatError::new(
            "payload_too_large",
            "HTTP request exceeds 1 MiB",
        ));
    }
    let timeout = timeout.clamp(Duration::from_secs(1), Duration::from_secs(65));
    let mut config = format!(
        "silent\nshow-error\nrequest = {}\nurl = {}\nconnect-timeout = 10\nmax-time = {}\nwrite-out = \"\\n%{{http_code}}\"\n",
        quote(method)?,
        quote(url)?,
        timeout.as_secs_f64()
    );
    for (name, value) in headers {
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.contains(['\r', '\n', '\0'])
        {
            return Err(ChatError::new("invalid_config", "invalid HTTP header"));
        }
        config.push_str(&format!(
            "header = {}\n",
            quote(&format!("{name}: {value}"))?
        ));
    }
    if let Some(body) = body {
        // JSON/form input cannot begin with '@'; curl would interpret that as a
        // filename. Reject it instead of accidentally reading arbitrary files.
        if body.starts_with('@') {
            return Err(ChatError::new(
                "invalid_params",
                "HTTP body cannot start with @",
            ));
        }
        config.push_str(&format!("data-binary = {}\n", quote(body)?));
    }
    let mut child = Command::new("curl")
        .args(["--disable", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| io_error("curl is required for this provider", e))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ChatError::new("transport_error", "curl stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ChatError::new("transport_error", "curl stdout unavailable"))?;
    let writer = thread::spawn(move || {
        let mut stdin = stdin;
        let result = stdin.write_all(config.as_bytes());
        drop(stdin);
        result
    });
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take((MAX_WIRE_BYTES + 128) as u64)
            .read_to_end(&mut output);
        let _ = tx.send((result, output));
    });
    let deadline = Instant::now() + timeout + Duration::from_secs(2);
    let mut output = None;
    let exit;
    loop {
        if let Ok(value) = rx.try_recv() {
            output = Some(value);
        }
        if output
            .as_ref()
            .is_some_and(|(_, bytes)| bytes.len() > MAX_WIRE_BYTES)
        {
            let _ = child.kill();
            let _ = child.wait();
            let _ = writer.join();
            let _ = reader.join();
            return Err(ChatError::new(
                "payload_too_large",
                "HTTP response exceeds 1 MiB",
            ));
        }
        if let Some(status) = child.try_wait().map_err(|e| io_error("curl wait", e))? {
            exit = status;
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = writer.join();
            let _ = reader.join();
            return Err(ChatError::new("timeout", "HTTP request timed out"));
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = writer.join();
    let _ = reader.join();
    let (read_result, bytes) = output
        .or_else(|| rx.try_recv().ok())
        .ok_or_else(|| ChatError::new("transport_error", "curl produced no response"))?;
    read_result.map_err(|e| io_error("read HTTP response", e))?;
    if bytes.len() > MAX_WIRE_BYTES {
        return Err(ChatError::new(
            "payload_too_large",
            "HTTP response exceeds 1 MiB",
        ));
    }
    if !exit.success() {
        return Err(ChatError::new(
            "transport_error",
            format!(
                "HTTPS transport failed (curl exit {})",
                exit.code().unwrap_or(-1)
            ),
        ));
    }
    let output = String::from_utf8(bytes)
        .map_err(|_| ChatError::new("invalid_response", "HTTP response is not UTF-8"))?;
    let (body, status) = output
        .rsplit_once('\n')
        .ok_or_else(|| ChatError::new("invalid_response", "HTTP status missing"))?;
    let status = status
        .parse()
        .map_err(|_| ChatError::new("invalid_response", "invalid HTTP status"))?;
    Ok(HttpResponse {
        status,
        body: body.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn rejects_non_tls_remote_and_header_injection() {
        assert!(validate_url("http://example.org").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("https://user:pass@example.org").is_err());
        assert!(validate_url("https://example.org").is_ok());
        assert!(
            request(
                "GET",
                "http://127.0.0.1:1",
                &[("Authorization", "Bearer a\r\nInjected: yes")],
                None,
                Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn loopback_http_preserves_json_and_bearer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let read = stream.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..read]);
                if let Some(header_end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..header_end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
            String::from_utf8(bytes).unwrap()
        });
        let body = "{\"text\":\"line\\nquoted \\\" text\"}";
        let response = request(
            "POST",
            &format!("http://{address}/test"),
            &[
                ("Authorization", "Bearer private-test-token"),
                ("Content-Type", "application/json"),
            ],
            Some(body),
            Duration::from_secs(4),
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            response.json().unwrap().get("ok").and_then(Value::as_bool),
            Some(true)
        );
        let received = server.join().unwrap();
        assert!(
            received
                .to_ascii_lowercase()
                .contains("authorization: bearer private-test-token")
        );
        assert!(received.ends_with(body));
    }
}
