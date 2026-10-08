//! Localhost JSON-lines control server (one request per line, one reply per line). Loopback only.
//!
//! The server only ever reads requests: a line that is not a JSON object with a string `method`
//! (an HTTP request line from a browser's cross-origin `fetch`, a stray `nc`, binary junk) gets
//! one error reply and the connection is closed, so nothing after it is executed. Lines are
//! bounded ([`MAX_LINE`]) and so is the number of open connections ([`MAX_CONNECTIONS`]); junk
//! never reaches the UI thread.
//!
//! Every connection must authenticate first: its first request is
//! `{"method":"auth","params":{"token":"…"}}` with the token the app wrote to
//! `<data dir>/control-<port>.token` (owner-only on Unix) or the one given in
//! `FILMCRAFT_CONTROL_TOKEN`. Anything else, or a wrong token, gets an error reply and the
//! connection is closed. See `filmcraft_engine::control_auth` and `docs/control-protocol.md`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use filmcraft_engine::control_auth;
use filmcraft_ui_egui::ControlRequest;
use serde_json::{Value, json};

/// Longest accepted request line (bytes, without the newline). Requests are small JSON objects;
/// anything longer is refused and the connection closed.
pub const MAX_LINE: usize = 4 * 1024 * 1024;
/// Connections served at once; further ones get an error line and are closed.
pub const MAX_CONNECTIONS: usize = 16;

/// A bound control port and the token its connections must present.
pub struct Prepared {
    listener: TcpListener,
    token: Arc<str>,
}

/// The token file written for clients; removed again when dropped (clean shutdown).
pub struct TokenFile {
    path: PathBuf,
    token: Arc<str>,
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        // only our own token: never a file another instance wrote since
        if control_auth::read_token_file(&self.path).is_some_and(|t| control_auth::tokens_match(&t, &self.token)) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A fresh 256-bit token from the operating system's random number generator.
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no secure random numbers for the control token: {e}"))?;
    Ok(control_auth::hex(&bytes))
}

/// Bind the control port on loopback and set up its token: `FILMCRAFT_CONTROL_TOKEN` when set,
/// else a random one written to `<data_dir>/control-<port>.token` for local clients. Fails closed:
/// without a token clients can read, the server does not start.
pub fn prepare(port: u16, data_dir: Option<&Path>) -> Result<(Prepared, Option<TokenFile>), String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("control server failed to bind 127.0.0.1:{port}: {e}"))?;
    if std::env::var_os(control_auth::TOKEN_ENV).is_some_and(|v| !v.is_empty()) && control_auth::env_token().is_none() {
        eprintln!("filmcraft: ignoring {}: a token must be 1–1024 printable ASCII characters without spaces", control_auth::TOKEN_ENV);
    }
    if let Some(t) = control_auth::env_token() {
        return Ok((Prepared { listener, token: t.into() }, None));
    }
    let token: Arc<str> = random_token()?.into();
    let dir = data_dir.ok_or("control server: no data folder to write the access token to (set --data-dir or FILMCRAFT_CONTROL_TOKEN)")?;
    let path = control_auth::token_path(dir, port);
    control_auth::write_token_file(&path, &token).map_err(|e| format!("control server: can't write the access token {}: {e}", path.display()))?;
    Ok((Prepared { listener, token: Arc::clone(&token) }, Some(TokenFile { path, token })))
}

/// Serve the prepared port on a background thread; requests arrive on the returned receiver.
pub fn start(p: Prepared, ctx: egui::Context) -> Receiver<ControlRequest> {
    let (tx, rx) = channel::<ControlRequest>();
    if let Ok(addr) = p.listener.local_addr() {
        eprintln!("filmcraft: control server listening on {addr}");
    }
    std::thread::spawn(move || accept_loop(p.listener, tx, ctx, p.token));
    rx
}

fn accept_loop(listener: TcpListener, tx: Sender<ControlRequest>, ctx: egui::Context, token: Arc<str>) {
    let open = Arc::new(AtomicUsize::new(0));
    for mut stream in listener.incoming().flatten() {
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            let _ = writeln!(stream, "{}", json!({"ok": false, "error": "too many control connections"}));
            continue;
        }
        let tx = tx.clone();
        let ctx = ctx.clone();
        let token = Arc::clone(&token);
        // The slot is given back when the guard drops: after `serve` returns, if it panics, or if
        // the thread can't be started (the closure, and with it the guard, is dropped).
        let slot = Slot(Arc::clone(&open));
        let spawned = std::thread::Builder::new().name("control-conn".into()).spawn(move || {
            let _slot = slot;
            serve(stream, &tx, &ctx, &token);
        });
        if let Err(e) = spawned {
            eprintln!("filmcraft: control server could not start a connection thread: {e}");
        }
    }
}

/// One counted connection; decrements the open-connection count when dropped.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn serve(stream: TcpStream, tx: &Sender<ControlRequest>, ctx: &egui::Context, token: &str) {
    let Ok(read) = stream.try_clone() else { return };
    serve_lines(read, stream, token, |method, params| {
        let (req, rrx) = ControlRequest::new(method, params);
        tx.send(req).ok()?;
        ctx.request_repaint();
        Some(rrx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| json!({"ok": false, "error": "timeout"})))
    });
}

/// One parsed line.
enum Line {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Blank,
    /// Not a request: reply with this error and close the connection.
    Reject(String),
}

fn parse_line(bytes: &[u8]) -> Line {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Line::Reject("not a control request (invalid UTF-8); closing the connection".into());
    };
    let text = text.trim();
    if text.is_empty() {
        return Line::Blank;
    }
    let msg = match serde_json::from_str::<Value>(text) {
        Ok(v) => v,
        Err(e) => return Line::Reject(format!("bad JSON ({e}); expected one JSON request object per line; closing the connection")),
    };
    let Some(obj) = msg.as_object() else {
        return Line::Reject("not a control request (expected a JSON object); closing the connection".into());
    };
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Line::Reject("not a control request (missing string `method`); closing the connection".into());
    };
    Line::Request { id: obj.get("id").cloned().unwrap_or(Value::Null), method: method.to_string(), params: obj.get("params").cloned().unwrap_or(json!({})) }
}

/// Read one `\n`-terminated line of at most `max` bytes into `buf` (newline stripped).
/// `Ok(false)` at end of stream; `Err` when the line is too long or the read fails.
fn read_bounded_line(reader: &mut impl BufRead, buf: &mut Vec<u8>, max: usize) -> Result<bool, String> {
    buf.clear();
    // `take` bounds the bytes buffered for one line: a peer can't make us allocate more than `max`.
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let n = reader.take(limit).read_until(b'\n', buf).map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        return Ok(true);
    }
    if buf.len() > max {
        return Err(format!("request line longer than {max} bytes; closing the connection"));
    }
    // Last line without a newline, then end of stream.
    Ok(true)
}

/// Check an `auth` request's token; the reply on success, else the error that closes the connection.
fn check_auth(params: &Value, token: &str) -> Result<Value, String> {
    let given = params.get("token").and_then(Value::as_str).unwrap_or("");
    if control_auth::tokens_match(given, token) {
        Ok(json!({"ok": true, "result": {"authenticated": true}}))
    } else {
        Err("authentication failed: wrong control token; closing the connection".into())
    }
}

/// Serve requests from `read`, answering on `out`, until the peer closes, sends something that is
/// not a request, fails to authenticate, or `handle` returns `None` (app gone). The first request
/// must be `auth` with `token`. `handle` gets `(method, params)` and returns the reply object (the
/// request's `id` is added here).
fn serve_lines<R: Read, W: Write>(read: R, mut out: W, token: &str, mut handle: impl FnMut(String, Value) -> Option<Value>) {
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();
    let mut authed = false;
    loop {
        let reject = match read_bounded_line(&mut reader, &mut buf, MAX_LINE) {
            Ok(false) => return,
            Err(e) => e,
            Ok(true) => match parse_line(&buf) {
                Line::Blank => continue,
                Line::Reject(e) => e,
                Line::Request { method, .. } if !authed && method != control_auth::AUTH_METHOD => format!(
                    "authentication required: send {{\"method\":\"auth\",\"params\":{{\"token\":\"…\"}}}} first (token in the app's control-<port>.token file or {}); closing the connection",
                    control_auth::TOKEN_ENV
                ),
                Line::Request { id, method, params } => {
                    let reply = if method == control_auth::AUTH_METHOD {
                        match check_auth(&params, token) {
                            Ok(r) => {
                                authed = true;
                                Some(r)
                            }
                            Err(e) => {
                                let _ = writeln!(out, "{}", json!({"id": id, "ok": false, "error": e})).and_then(|()| out.flush());
                                return;
                            }
                        }
                    } else {
                        handle(method, params)
                    };
                    let Some(mut r) = reply else { return };
                    if let Some(o) = r.as_object_mut() {
                        o.insert("id".into(), id);
                    }
                    if writeln!(out, "{r}").and_then(|()| out.flush()).is_err() {
                        return;
                    }
                    continue;
                }
            },
        };
        let _ = writeln!(out, "{}", json!({"ok": false, "error": reject})).and_then(|()| out.flush());
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const AUTH: &[u8] = b"{\"method\":\"auth\",\"params\":{\"token\":\"0123456789abcdef0123456789abcdef\"}}\n";

    /// Serve `input` without authenticating first.
    fn run_raw(input: &[u8]) -> (Vec<Value>, Vec<String>) {
        let mut out = Vec::new();
        let mut calls = Vec::new();
        serve_lines(input, &mut out, TOKEN, |m, _p| {
            calls.push(m);
            Some(json!({"ok": true, "result": null}))
        });
        let replies = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).collect();
        (replies, calls)
    }

    /// Serve `input` after a correct `auth` line (whose reply is dropped).
    fn run(input: &[u8]) -> (Vec<Value>, Vec<String>) {
        let (mut r, calls) = run_raw(&[AUTH, input].concat());
        assert_eq!(r.first().map(|v| v["ok"].clone()), Some(json!(true)));
        r.remove(0);
        (r, calls)
    }

    #[test]
    fn unauthenticated_requests_are_refused() {
        let (r, calls) = run_raw(b"{\"id\":1,\"method\":\"engine.execute\",\"params\":{\"command\":\"captions.export\"}}\n{\"method\":\"auth\",\"params\":{\"token\":\"0123456789abcdef0123456789abcdef\"}}\n{\"method\":\"a\"}\n");
        assert!(calls.is_empty(), "{calls:?}");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["ok"], false);
        assert!(r[0]["error"].as_str().unwrap().contains("authentication required"));
    }

    #[test]
    fn wrong_token_is_refused() {
        for bad in [
            &b"{\"method\":\"auth\",\"params\":{\"token\":\"0123456789abcdef0123456789abcdee\"}}\n{\"method\":\"a\"}\n"[..],
            b"{\"method\":\"auth\",\"params\":{\"token\":\"\"}}\n{\"method\":\"a\"}\n",
            b"{\"method\":\"auth\",\"params\":{}}\n{\"method\":\"a\"}\n",
            b"{\"method\":\"auth\",\"params\":{\"token\":5}}\n{\"method\":\"a\"}\n",
        ] {
            let (r, calls) = run_raw(bad);
            assert!(calls.is_empty(), "{calls:?}");
            assert_eq!(r.len(), 1);
            assert!(r[0]["error"].as_str().unwrap().contains("wrong control token"));
        }
    }

    #[test]
    fn right_token_works() {
        let (r, calls) = run_raw(
            &[&b"{\"id\":9,\"method\":\"auth\",\"params\":{\"token\":\"0123456789abcdef0123456789abcdef\"}}\n"[..], b"{\"id\":10,\"method\":\"a\"}\n"].concat(),
        );
        assert_eq!(calls, ["a"]);
        assert_eq!(r[0], json!({"id": 9, "ok": true, "result": {"authenticated": true}}));
        assert_eq!(r[1]["id"], 10);
    }

    #[test]
    fn prepare_writes_an_owner_only_token_file() {
        let dir = std::env::temp_dir().join(format!("fc-control-prepare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if control_auth::env_token().is_some() {
            return; // an explicit token in the environment replaces the file
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (p, file) = prepare(port, Some(&dir)).unwrap();
        let path = control_auth::token_path(&dir, port);
        let t = control_auth::read_token_file(&path).unwrap();
        assert_eq!(t.len(), 64);
        assert_eq!(&*p.token, t.as_str());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // two servers never share a token
        assert_ne!(random_token().unwrap(), t);
        drop(file);
        assert!(!path.exists(), "removed on clean shutdown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_are_answered_in_order_with_ids() {
        let (r, calls) = run(b"{\"id\":1,\"method\":\"a\"}\n\n{\"id\":2,\"method\":\"b\",\"params\":{}}\r\n{\"method\":\"c\"}");
        assert_eq!(calls, ["a", "b", "c"]);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0]["id"], 1);
        assert_eq!(r[1]["id"], 2);
        assert_eq!(r[2]["id"], Value::Null);
    }

    #[test]
    fn http_request_closes_before_the_body_runs() {
        // A browser's cross-origin `fetch` POST (issue #119): the request line is rejected and the
        // connection closed, so the JSON body is never executed.
        let http = b"POST / HTTP/1.1\r\nHost: 127.0.0.1:9876\r\nOrigin: https://attacker.example\r\nContent-Type: text/plain\r\n\r\n\
            {\"id\":7,\"method\":\"engine.execute\",\"params\":{\"command\":\"file.openDemoProject\",\"params\":{}}}\n";
        let (r, calls) = run(http);
        assert!(calls.is_empty(), "nothing may run: {calls:?}");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["ok"], false);
        assert!(r[0]["error"].as_str().unwrap().contains("closing the connection"));
    }

    #[test]
    fn non_request_json_closes_the_connection() {
        for junk in [
            &b"[1,2]\n{\"method\":\"a\"}\n"[..],
            b"{\"id\":1}\n{\"method\":\"a\"}\n",
            b"{\"method\":5}\n{\"method\":\"a\"}\n",
            b"\xff\xfe\n{\"method\":\"a\"}\n",
        ] {
            let (r, calls) = run(junk);
            assert!(calls.is_empty(), "{calls:?}");
            assert_eq!(r.len(), 1);
            assert_eq!(r[0]["ok"], false);
        }
    }

    #[test]
    fn a_request_before_junk_still_runs() {
        let (r, calls) = run(b"{\"method\":\"a\"}\nGET / HTTP/1.1\n{\"method\":\"b\"}\n");
        assert_eq!(calls, ["a"]);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1]["ok"], false);
    }

    #[test]
    fn overlong_line_is_refused() {
        // One byte over the limit, then a valid request: neither runs.
        let mut input = vec![b'x'; MAX_LINE + 1];
        input.extend_from_slice(b"\n{\"method\":\"a\"}\n");
        let (r, calls) = run(&input);
        assert!(calls.is_empty(), "{calls:?}");
        assert_eq!(r.len(), 1);
        assert!(r[0]["error"].as_str().unwrap().contains("longer than"));
    }

    #[test]
    fn line_at_the_limit_is_accepted() {
        let mut buf = Vec::new();
        let line = vec![b'a'; 10];
        let mut input = line.clone();
        input.push(b'\n');
        assert_eq!(read_bounded_line(&mut &input[..], &mut buf, 10), Ok(true));
        assert_eq!(buf, line);
        assert!(read_bounded_line(&mut &[b'a'; 11][..], &mut buf, 10).is_err());
    }

    #[test]
    fn tcp_connection_is_closed_after_junk() {
        // End to end over a real loopback socket (the transport the app uses).
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let read = s.try_clone().unwrap();
            serve_lines(read, s, TOKEN, |_, _| Some(json!({"ok": true})));
        });
        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(AUTH).unwrap();
        c.write_all(b"{\"id\":7,\"method\":\"x\"}\nhello\n{\"method\":\"y\"}\n").unwrap();
        let mut text = String::new();
        c.read_to_string(&mut text).unwrap(); // returns at EOF: the server closed the connection
        server.join().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains("authenticated"));
        assert!(lines[1].contains("\"id\":7"));
        assert!(lines[2].contains("closing the connection"));
    }

    #[test]
    fn extra_connections_are_refused() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, _rx) = channel::<ControlRequest>();
        std::thread::spawn(move || accept_loop(listener, tx, egui::Context::default(), TOKEN.into()));
        // Idle connections are accepted in order and held open.
        let held: Vec<TcpStream> = (0..16).map(|_| TcpStream::connect(addr).unwrap()).collect();
        let mut extra = TcpStream::connect(addr).unwrap();
        extra.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut line = String::new();
        BufReader::new(&mut extra).read_line(&mut line).unwrap();
        assert!(line.contains("too many control connections"), "{line}");
        drop(held);
    }
}
