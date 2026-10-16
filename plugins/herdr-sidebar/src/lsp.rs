//! LSP client for code navigation in the preview pane: a hand-rolled
//! JSON-RPC-over-stdio transport, the handful of methods the viewer uses
//! (definition, references, hover, symbols), and the lspmux preflight that
//! tells the user how to get a running multiplexer.
//!
//! Two tools are named here by decision, against the plugin's usual rule:
//! lspmux (the shim the viewer spawns and the service it checks for) and,
//! in the viewer, leaf for hover Markdown. Language servers themselves come
//! from `[[lsp]]` rules only. The plugin never starts, supervises or kills
//! lspmux or a language server.

use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::viewer::LspSpec;

/// How long `lspmux status` may take before it counts as not running.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
/// Back/forward history depth.
pub const HISTORY_CAP: usize = 50;

// ---------------------------------------------------------------- preflight

/// Whether the lspmux service is usable from this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preflight {
    NotInstalled,
    NotRunning,
    Running,
}

impl Preflight {
    /// Settings-row wording.
    pub fn label(self) -> &'static str {
        match self {
            Preflight::NotInstalled => "lspmux not installed",
            Preflight::NotRunning => "lspmux not running",
            Preflight::Running => "lspmux running",
        }
    }
}

/// Resolve `lspmux` on PATH, then ask the service for its status. Shared by
/// the viewer (lazily, on the first navigable document) and the sidebar's
/// Settings row, so the two can never disagree.
pub fn preflight() -> Preflight {
    let Some(lspmux) = crate::programs::resolve_program_here("lspmux") else {
        return Preflight::NotInstalled;
    };
    let mut child = match std::process::Command::new(lspmux)
        .arg("status")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Preflight::NotRunning,
    };
    let deadline = Instant::now() + STATUS_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Preflight::Running,
            Ok(Some(_)) => return Preflight::NotRunning,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Preflight::NotRunning;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Other,
}

pub fn host_os() -> Os {
    if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else {
        Os::Other
    }
}

/// systemd is in charge of the user session when its runtime directory
/// exists and `systemctl` is on PATH.
pub fn detect_init() -> Init {
    if Path::new("/run/systemd/system").is_dir()
        && crate::programs::resolve_program_here("systemctl").is_some()
    {
        Init::Systemd
    } else {
        Init::Other
    }
}

/// The one-line advice for a failed preflight; `None` when lspmux runs.
/// Pure, so every wording has a test. `lspmux_path` decides whether a macOS
/// install came from Homebrew (its formula ships the launchd service).
pub fn advice(state: Preflight, os: Os, init: Init, lspmux_path: Option<&Path>) -> Option<String> {
    let text = match (state, os) {
        (Preflight::Running, _) => return None,
        (Preflight::NotInstalled, Os::MacOs) => "lspmux is not installed — brew install lspmux".into(),
        (Preflight::NotInstalled, _) => "lspmux is not installed — cargo install lspmux".into(),
        (Preflight::NotRunning, Os::MacOs) => {
            if lspmux_path.is_some_and(is_homebrew_path) {
                "lspmux is not running — brew services start lspmux".into()
            } else {
                "lspmux is not running — install its launchd agent (lspmux.plist from its repository \
                 into ~/Library/LaunchAgents, then launchctl load -w it)"
                    .into()
            }
        }
        (Preflight::NotRunning, Os::Linux) if init == Init::Systemd => {
            "lspmux is not running — install its user service (lspmux.service from its repository), \
             then systemctl --user enable --now lspmux"
                .into()
        }
        (Preflight::NotRunning, _) => {
            "lspmux server must be running; start it with your init system".into()
        }
    };
    Some(text)
}

fn is_homebrew_path(path: &Path) -> bool {
    let prefixes = std::env::var("HOMEBREW_PREFIX")
        .ok()
        .into_iter()
        .chain(["/opt/homebrew".to_string(), "/usr/local".to_string()]);
    prefixes
        .into_iter()
        .any(|prefix| path.starts_with(Path::new(&prefix)))
}

/// Advice for the current host.
pub fn advice_here(state: Preflight) -> Option<String> {
    let lspmux = crate::programs::resolve_program_here("lspmux");
    advice(state, host_os(), detect_init(), lspmux.as_deref())
}

// --------------------------------------------------------------------- env

/// The shim's environment. lspmux keys a server instance by the client's
/// environment as well as its workspace (`pass_environment = ["*"]` by
/// default), and every preview pane carries its own `HERDR_*` values, so a
/// naively inherited environment would give every tab a private server.
/// Only stable, server-relevant variables pass.
pub fn shim_env() -> Vec<(String, String)> {
    shim_env_from(std::env::vars())
}

fn shim_env_from(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "TMPDIR",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "GOPATH",
        "GOROOT",
    ];
    let mut kept: Vec<(String, String)> = vars
        .filter(|(key, _)| KEEP.contains(&key.as_str()) || key.starts_with("XDG_"))
        .collect();
    kept.sort();
    kept
}

// ------------------------------------------------------------------ framing

/// Read one `Content-Length`-framed message body; `None` at a clean EOF.
fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut length: Option<usize> = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Ok(None);
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    let length = length.ok_or_else(|| io::Error::other("frame without Content-Length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_frame(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    writer.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

/// The child shim plus a reader thread that turns frames into values.
pub struct Transport {
    writer: Box<dyn Write + Send>,
    incoming: Receiver<Value>,
    child: Option<std::process::Child>,
}

impl Transport {
    /// Spawn `lspmux client --server-path <server> <args…>` for `spec`, cwd
    /// at the workspace root, with the scrubbed environment.
    pub fn spawn(spec: &LspSpec) -> io::Result<Transport> {
        let argv = crate::actions::split_editor_command(&spec.command)?;
        let Some(server) = argv.first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty server command",
            ));
        };
        // Absolute paths on both sides: the daemon's PATH is not ours, and a
        // stable path keeps lspmux's instance key stable across tabs.
        let server = crate::programs::resolve_program_here(server).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("{server} is not on PATH"))
        })?;
        let lspmux = crate::programs::resolve_program_here("lspmux")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "lspmux is not on PATH"))?;
        let mut child = std::process::Command::new(lspmux)
            .arg("client")
            .arg("--server-path")
            .arg(&server)
            .args(&argv[1..])
            .current_dir(&spec.root)
            .env_clear()
            .envs(shim_env())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("stdout unavailable"))?;
        Ok(Self::from_io(stdout, stdin, Some(child)))
    }

    /// Build a transport over any reader/writer pair (tests use
    /// `std::io::pipe`).
    pub fn from_io(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        child: Option<std::process::Child>,
    ) -> Transport {
        let (sender, incoming) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = io::BufReader::new(reader);
            while let Ok(Some(body)) = read_frame(&mut reader) {
                let Ok(value) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                if sender.send(value).is_err() {
                    break;
                }
            }
            // Dropping `sender` is the EOF signal: the pump sees Disconnected.
        });
        Transport {
            writer: Box::new(writer),
            incoming,
            child,
        }
    }

    fn send(&mut self, message: &Value) -> io::Result<()> {
        write_frame(&mut self.writer, message.to_string().as_bytes())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------- positions

/// How the server counts columns; from the `initialize` RESULT (lspmux
/// replays the first client's negotiation to every later one).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Encoding {
    Utf8,
    #[default]
    Utf16,
}

/// A Unicode-scalar column (the viewer's unit) → the server's column.
pub fn to_lsp_col(line: &str, scalar_col: usize, encoding: Encoding) -> u32 {
    line.chars()
        .take(scalar_col)
        .map(|c| match encoding {
            Encoding::Utf8 => c.len_utf8(),
            Encoding::Utf16 => c.len_utf16(),
        } as u32)
        .sum()
}

/// The server's column → a Unicode-scalar column, clamped to the line.
pub fn from_lsp_col(line: &str, lsp_col: u32, encoding: Encoding) -> usize {
    let mut units: u32 = 0;
    for (index, c) in line.chars().enumerate() {
        if units >= lsp_col {
            return index;
        }
        units += match encoding {
            Encoding::Utf8 => c.len_utf8(),
            Encoding::Utf16 => c.len_utf16(),
        } as u32;
        // A column inside a multi-unit char lands on that char.
        if units > lsp_col {
            return index;
        }
    }
    line.chars().count()
}

/// `file://` URI for an absolute path (percent-encoding what RFC 3986 needs).
pub fn uri_for(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The path of a `file://` URI; `None` for other schemes.
pub fn path_for(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(byte) = raw
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            bytes.push(byte);
            i += 3;
            continue;
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&bytes).into_owned();
    (!decoded.is_empty()).then(|| PathBuf::from(decoded))
}

/// A navigation target: a file plus a zero-based line and the server's
/// column (in its own encoding).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub line: usize,
    pub character: u32,
}

/// Targets from a `Location`, `Location[]`, `LocationLink[]` or `null`
/// result.
pub fn targets(result: &Value) -> Vec<Target> {
    fn one(value: &Value) -> Option<Target> {
        let (uri, range) = if let Some(uri) = value.get("targetUri") {
            (
                uri,
                value
                    .get("targetSelectionRange")
                    .or_else(|| value.get("targetRange"))?,
            )
        } else {
            (value.get("uri")?, value.get("range")?)
        };
        let start = range.get("start")?;
        Some(Target {
            path: path_for(uri.as_str()?)?,
            line: start.get("line")?.as_u64()? as usize,
            character: start.get("character")?.as_u64().unwrap_or(0) as u32,
        })
    }
    match result {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

// ------------------------------------------------------------------- client

/// Progress reported by the server (`$/progress`): title and percentage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub title: String,
    pub percentage: Option<u8>,
}

/// The viewer's connection to one language server (through the shim).
/// Single-owner: whoever holds it calls [`Client::pump`] to route replies.
pub struct Client {
    transport: Transport,
    next_id: u64,
    pending: HashMap<u64, Sender<Result<Value, String>>>,
    pub encoding: Encoding,
    /// The server's `capabilities` from `initialize`.
    pub capabilities: Value,
    /// Work-done progress in flight, keyed by token.
    progress: HashMap<String, Progress>,
    dead: bool,
}

impl Client {
    pub fn new(transport: Transport) -> Client {
        Client {
            transport,
            next_id: 0,
            pending: HashMap::new(),
            encoding: Encoding::default(),
            capabilities: Value::Null,
            progress: HashMap::new(),
            dead: false,
        }
    }

    /// The shim went away (EOF on its stdout).
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// The most recent progress item still running, for the footer.
    pub fn progress(&self) -> Option<&Progress> {
        self.progress.values().next()
    }

    pub fn supports(&self, provider: &str) -> bool {
        match self.capabilities.get(provider) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(_) => true,
        }
    }

    /// `initialize` + `initialized`, blocking up to `timeout` (run on a
    /// worker: a cold server can take seconds to answer).
    pub fn initialize(&mut self, root: &Path, timeout: Duration) -> Result<(), String> {
        let root_uri = uri_for(root);
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "herdr-sidebar"},
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-8", "utf-16"]},
                "window": {"workDoneProgress": true},
                "textDocument": {
                    "definition": {"linkSupport": true},
                    "typeDefinition": {"linkSupport": true},
                    "implementation": {"linkSupport": true},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "synchronization": {"didSave": false}
                }
            }
        });
        let receiver = self.request("initialize", params);
        let result = self.wait(&receiver, timeout)?;
        self.capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        self.encoding = match self
            .capabilities
            .get("positionEncoding")
            .and_then(Value::as_str)
        {
            Some("utf-8") => Encoding::Utf8,
            _ => Encoding::Utf16,
        };
        self.notify("initialized", json!({}));
        Ok(())
    }

    /// Pump until `receiver` answers or `timeout` passes.
    pub fn wait(
        &mut self,
        receiver: &Receiver<Result<Value, String>>,
        timeout: Duration,
    ) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            match receiver.try_recv() {
                Ok(result) => return result,
                Err(mpsc::TryRecvError::Disconnected) => return Err("request dropped".into()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if self.dead {
                return Err("language server connection closed".into());
            }
            if Instant::now() >= deadline {
                return Err(format!("no answer after {} s", timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn did_open(&mut self, path: &Path, text: &str, version: i32) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri_for(path),
                "languageId": language_id(path),
                "version": version,
                "text": text,
            }}),
        );
    }

    pub fn did_close(&mut self, path: &Path) {
        self.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": uri_for(path)}}),
        );
    }

    /// Full-document sync (the fallback in-pane editor).
    pub fn did_change(&mut self, path: &Path, text: &str, version: i32) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri_for(path), "version": version},
                "contentChanges": [{"text": text}],
            }),
        );
    }

    /// Send a request; the reply arrives on the receiver once
    /// [`Client::pump`] routes it. The caller applies its own deadline.
    pub fn request(&mut self, method: &str, params: Value) -> Receiver<Result<Value, String>> {
        self.next_id += 1;
        let id = self.next_id;
        let (sender, receiver) = mpsc::channel();
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.transport.send(&message).is_err() {
            self.dead = true;
            let _ = sender.send(Err("language server connection closed".into()));
        } else {
            self.pending.insert(id, sender);
        }
        receiver
    }

    /// `textDocument/<method>` at a position.
    pub fn at(
        &mut self,
        method: &str,
        path: &Path,
        line: usize,
        character: u32,
    ) -> Receiver<Result<Value, String>> {
        self.request(
            &format!("textDocument/{method}"),
            json!({
                "textDocument": {"uri": uri_for(path)},
                "position": {"line": line, "character": character},
            }),
        )
    }

    pub fn references(
        &mut self,
        path: &Path,
        line: usize,
        character: u32,
    ) -> Receiver<Result<Value, String>> {
        self.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri_for(path)},
                "position": {"line": line, "character": character},
                "context": {"includeDeclaration": true},
            }),
        )
    }

    pub fn document_symbols(&mut self, path: &Path) -> Receiver<Result<Value, String>> {
        self.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri_for(path)}}),
        )
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if self.transport.send(&message).is_err() {
            self.dead = true;
        }
    }

    /// Drain everything the reader thread decoded: replies to their
    /// receivers, progress into [`Client::progress`], server requests
    /// answered (`workspace/configuration` with one `null` per item, all
    /// others with `null` — lspmux forwards them and rust-analyzer blocks on
    /// the reply). Call once per event-loop iteration.
    pub fn pump(&mut self) {
        loop {
            let message = match self.transport.incoming.try_recv() {
                Ok(message) => message,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.dead = true;
                    for (_, sender) in self.pending.drain() {
                        let _ = sender.send(Err("language server connection closed".into()));
                    }
                    return;
                }
            };
            let method = message.get("method").and_then(Value::as_str);
            let id = message.get("id");
            match (method, id) {
                (Some(method), Some(id)) => {
                    let result = match method {
                        "workspace/configuration" => {
                            let items = message
                                .get("params")
                                .and_then(|p| p.get("items"))
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len);
                            Value::Array(vec![Value::Null; items])
                        }
                        _ => Value::Null,
                    };
                    let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                    if self.transport.send(&reply).is_err() {
                        self.dead = true;
                    }
                }
                (Some("$/progress"), None) => self.on_progress(message.get("params")),
                (Some(_), None) => {}
                (None, Some(id)) => {
                    let Some(id) = id.as_u64() else { continue };
                    if let Some(sender) = self.pending.remove(&id) {
                        let outcome = if let Some(error) = message.get("error") {
                            Err(error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("request failed")
                                .to_string())
                        } else {
                            Ok(message.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = sender.send(outcome);
                    }
                }
                (None, None) => {}
            }
        }
    }

    fn on_progress(&mut self, params: Option<&Value>) {
        let Some(params) = params else { return };
        let token = match params.get("token") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => return,
        };
        let Some(value) = params.get("value") else {
            return;
        };
        let percentage = value
            .get("percentage")
            .and_then(Value::as_u64)
            .map(|p| p.min(100) as u8);
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") => {
                let title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("working")
                    .to_string();
                self.progress.insert(token, Progress { title, percentage });
            }
            Some("report") => {
                if let Some(progress) = self.progress.get_mut(&token)
                    && percentage.is_some()
                {
                    progress.percentage = percentage;
                }
            }
            Some("end") => {
                self.progress.remove(&token);
            }
            _ => {}
        }
    }

    /// `shutdown` → `exit`, a short grace for the shim, then the transport's
    /// drop kills it (never the server: that is behind lspmux).
    pub fn shutdown(mut self) {
        if self.dead {
            return;
        }
        let receiver = self.request("shutdown", Value::Null);
        let _ = self.wait(&receiver, Duration::from_secs(1));
        self.notify("exit", Value::Null);
        if let Some(child) = &mut self.transport.child {
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// LSP `languageId` from the extension; servers mostly ignore it but some
/// (tsserver) refuse unknown ids.
pub fn language_id(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("rs") => "rust",
        Some("go") => "go",
        Some("c") => "c",
        Some("h" | "cc" | "cpp" | "hpp" | "cxx" | "hh") => "cpp",
        Some("ts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("jsx") => "javascriptreact",
        Some("py") => "python",
        Some("java") => "java",
        Some("kt") => "kotlin",
        Some("rb") => "ruby",
        Some("lua") => "lua",
        Some("zig") => "zig",
        Some("toml") => "toml",
        Some("json") => "json",
        Some("yaml" | "yml") => "yaml",
        Some("md") => "markdown",
        Some("sh" | "bash" | "zsh") => "shellscript",
        _ => "plaintext",
    }
}

// ------------------------------------------------------------------ history

/// Browser-style back/forward over navigation entries.
#[derive(Debug)]
pub struct History<T> {
    back: Vec<T>,
    forward: Vec<T>,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        History {
            back: Vec::new(),
            forward: Vec::new(),
        }
    }
}

impl<T> History<T> {
    /// Leaving `entry` for somewhere new: it becomes the back target and
    /// any forward entries are gone.
    pub fn push(&mut self, entry: T) {
        self.back.push(entry);
        self.forward.clear();
        if self.back.len() > HISTORY_CAP {
            self.back.remove(0);
        }
    }

    /// Go back from `current`: the previous entry, `current` moves forward.
    pub fn back(&mut self, current: T) -> Option<T> {
        let previous = self.back.pop()?;
        self.forward.push(current);
        Some(previous)
    }

    /// Go forward from `current`.
    pub fn forward(&mut self, current: T) -> Option<T> {
        let next = self.forward.pop()?;
        self.back.push(current);
        Some(next)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
pub(crate) mod fake {
    //! A language server over any reader/writer pair: answers the methods
    //! the viewer uses with canned data and, after `initialized`, issues a
    //! `workspace/configuration` request plus a progress sequence — the
    //! server→client traffic the client must cope with.
    use super::*;

    pub struct Spec {
        pub encoding: &'static str,
        pub definition: Value,
        pub references: Value,
        pub hover: Value,
        pub symbols: Value,
        /// Methods the fake never answers (deadline tests).
        pub ignore: Vec<&'static str>,
    }

    impl Default for Spec {
        fn default() -> Self {
            Spec {
                encoding: "utf-8",
                definition: json!([{"uri": "file:///r/src/lib.rs", "range": {"start": {"line": 9, "character": 4}, "end": {"line": 9, "character": 7}}}]),
                references: json!([]),
                hover: json!({"contents": {"kind": "markdown", "value": "```rust\nstruct Doc\n```\n\nA document."}}),
                symbols: json!([]),
                ignore: Vec::new(),
            }
        }
    }

    /// Every request the fake received, in order (method, params).
    pub type Log = std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>;

    pub fn serve(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        spec: Spec,
    ) -> Log {
        let log: Log = Default::default();
        let seen = log.clone();
        std::thread::spawn(move || {
            let mut reader = io::BufReader::new(reader);
            let mut writer = writer;
            let mut reply = |value: Value| {
                let _ = write_frame(&mut writer, value.to_string().as_bytes());
            };
            while let Ok(Some(body)) = read_frame(&mut reader) {
                let message: Value = serde_json::from_slice(&body).unwrap();
                let method = message
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let id = message.get("id").cloned();
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                if let Some(method) = &method {
                    seen.lock().unwrap().push((method.clone(), params.clone()));
                }
                match (method.as_deref(), id) {
                    (Some("initialize"), Some(id)) => reply(json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"capabilities": {
                            "positionEncoding": spec.encoding,
                            "definitionProvider": true,
                            "referencesProvider": true,
                            "hoverProvider": true,
                            "documentSymbolProvider": true,
                            "typeDefinitionProvider": false,
                        }, "serverInfo": {"name": "fake"}}
                    })),
                    (Some("initialized"), None) => {
                        reply(
                            json!({"jsonrpc": "2.0", "id": 1000, "method": "workspace/configuration",
                            "params": {"items": [{"section": "rust-analyzer"}, {"section": "x"}]}}),
                        );
                        reply(
                            json!({"jsonrpc": "2.0", "id": 1001, "method": "window/workDoneProgress/create",
                            "params": {"token": "idx"}}),
                        );
                        reply(json!({"jsonrpc": "2.0", "method": "$/progress",
                            "params": {"token": "idx", "value": {"kind": "begin", "title": "Indexing", "percentage": 0}}}));
                        reply(json!({"jsonrpc": "2.0", "method": "$/progress",
                            "params": {"token": "idx", "value": {"kind": "report", "percentage": 43}}}));
                    }
                    (Some(method), Some(id)) if spec.ignore.contains(&method) => {
                        let _ = (method, id);
                    }
                    (Some("textDocument/definition"), Some(id)) => {
                        reply(json!({"jsonrpc": "2.0", "id": id, "result": spec.definition}))
                    }
                    (Some("textDocument/references"), Some(id)) => {
                        reply(json!({"jsonrpc": "2.0", "id": id, "result": spec.references}))
                    }
                    (Some("textDocument/hover"), Some(id)) => {
                        reply(json!({"jsonrpc": "2.0", "id": id, "result": spec.hover}))
                    }
                    (Some("textDocument/documentSymbol"), Some(id)) => {
                        reply(json!({"jsonrpc": "2.0", "id": id, "result": spec.symbols}))
                    }
                    (Some("shutdown"), Some(id)) => {
                        reply(json!({"jsonrpc": "2.0", "id": id, "result": null}))
                    }
                    (Some("exit"), None) => break,
                    (Some("textDocument/didOpen"), None) => {
                        // The index finished once a document is open.
                        reply(json!({"jsonrpc": "2.0", "method": "$/progress",
                            "params": {"token": "idx", "value": {"kind": "end"}}}));
                    }
                    (Some(_), Some(id)) => reply(json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}})),
                    (None, Some(_)) => {} // a reply to our own request
                    _ => {}
                }
            }
        });
        log
    }

    /// A client wired to a fake server over two pipes.
    pub fn client(spec: Spec) -> (Client, Log) {
        let (to_client_r, to_client_w) = io::pipe().unwrap();
        let (to_server_r, to_server_w) = io::pipe().unwrap();
        let log = serve(to_server_r, to_client_w, spec);
        (
            Client::new(Transport::from_io(to_client_r, to_server_w, None)),
            log,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn advice_per_state_os_and_init() {
        use Preflight::*;
        let brew = Path::new("/opt/homebrew/bin/lspmux");
        let cargo = Path::new("/Users/x/.cargo/bin/lspmux");
        assert_eq!(advice(Running, Os::MacOs, Init::Other, Some(brew)), None);
        assert_eq!(
            advice(NotInstalled, Os::MacOs, Init::Other, None).unwrap(),
            "lspmux is not installed — brew install lspmux"
        );
        assert_eq!(
            advice(NotInstalled, Os::Linux, Init::Systemd, None).unwrap(),
            "lspmux is not installed — cargo install lspmux"
        );
        assert_eq!(
            advice(NotRunning, Os::MacOs, Init::Other, Some(brew)).unwrap(),
            "lspmux is not running — brew services start lspmux"
        );
        assert!(
            advice(NotRunning, Os::MacOs, Init::Other, Some(cargo))
                .unwrap()
                .contains("launchctl load -w")
        );
        assert!(
            advice(NotRunning, Os::Linux, Init::Systemd, Some(cargo))
                .unwrap()
                .ends_with("systemctl --user enable --now lspmux")
        );
        assert_eq!(
            advice(NotRunning, Os::Linux, Init::Other, Some(cargo)).unwrap(),
            "lspmux server must be running; start it with your init system"
        );
        assert_eq!(
            advice(NotRunning, Os::Other, Init::Other, None).unwrap(),
            "lspmux server must be running; start it with your init system"
        );
        assert_eq!(Running.label(), "lspmux running");
        assert_eq!(NotRunning.label(), "lspmux not running");
        assert_eq!(NotInstalled.label(), "lspmux not installed");
    }

    #[test]
    fn shim_env_keeps_the_allow_list_only() {
        let vars = [
            ("PATH", "/bin"),
            ("HOME", "/h"),
            ("HERDR_PANE_ID", "p1"),
            ("HERDR_SIDEBAR_PREVIEW_CONTROL", "c"),
            ("XDG_CONFIG_HOME", "/x"),
            ("CARGO_HOME", "/c"),
            ("TERM", "xterm"),
            ("NO_COLOR", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let env = shim_env_from(vars);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["CARGO_HOME", "HOME", "PATH", "XDG_CONFIG_HOME"]);
        assert!(shim_env().iter().all(|(k, _)| !k.starts_with("HERDR_")));
    }

    #[test]
    fn position_encodings_round_trip() {
        let line = "aé😀中b";
        // scalar columns: a=0 é=1 😀=2 中=3 b=4
        assert_eq!(to_lsp_col(line, 0, Encoding::Utf8), 0);
        assert_eq!(to_lsp_col(line, 2, Encoding::Utf8), 3); // a(1) é(2)
        assert_eq!(to_lsp_col(line, 3, Encoding::Utf8), 7); // + 😀(4)
        assert_eq!(to_lsp_col(line, 4, Encoding::Utf8), 10); // + 中(3)
        assert_eq!(to_lsp_col(line, 2, Encoding::Utf16), 2);
        assert_eq!(to_lsp_col(line, 3, Encoding::Utf16), 4); // 😀 is a pair
        assert_eq!(to_lsp_col(line, 4, Encoding::Utf16), 5);
        assert_eq!(to_lsp_col(line, 99, Encoding::Utf16), 6);
        for enc in [Encoding::Utf8, Encoding::Utf16] {
            for col in 0..=5 {
                assert_eq!(from_lsp_col(line, to_lsp_col(line, col, enc), enc), col);
            }
        }
        assert_eq!(from_lsp_col(line, 1000, Encoding::Utf16), 5);
        // A column inside a surrogate pair lands on that char.
        assert_eq!(from_lsp_col(line, 3, Encoding::Utf16), 2);
    }

    #[test]
    fn uris_round_trip_with_escapes() {
        let path = Path::new("/Users/m/my project/src/α.rs");
        let uri = uri_for(path);
        assert_eq!(uri, "file:///Users/m/my%20project/src/%CE%B1.rs");
        assert_eq!(path_for(&uri).unwrap(), path);
        assert_eq!(
            path_for("file://localhost/a/b"),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(path_for("untitled:x"), None);
        assert_eq!(path_for("file:///bad%zz"), Some(PathBuf::from("/bad%zz")));
    }

    #[test]
    fn targets_parse_every_location_shape() {
        let location = json!({"uri": "file:///r/a.rs", "range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 5}}});
        assert_eq!(
            targets(&location),
            vec![Target {
                path: PathBuf::from("/r/a.rs"),
                line: 3,
                character: 1
            }]
        );
        let link = json!([{
            "targetUri": "file:///r/b.rs",
            "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}},
            "targetSelectionRange": {"start": {"line": 2, "character": 7}, "end": {"line": 2, "character": 9}}
        }]);
        assert_eq!(targets(&link)[0].line, 2);
        assert_eq!(targets(&link)[0].character, 7);
        assert!(targets(&Value::Null).is_empty());
        assert!(targets(&json!([])).is_empty());
        assert!(
            targets(
                &json!([{"uri": "untitled:1", "range": {"start": {"line": 0, "character": 0}}}])
            )
            .is_empty()
        );
    }

    #[test]
    fn framing_splits_and_joins_messages() {
        let (reader, mut writer) = io::pipe().unwrap();
        let mut reader = io::BufReader::new(reader);
        std::thread::spawn(move || {
            // Two frames in one write, then one frame across two writes.
            let a = b"Content-Length: 2\r\n\r\n{}";
            let b =
                b"content-length: 7\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n{\"a\":1}";
            let mut both = a.to_vec();
            both.extend_from_slice(b);
            writer.write_all(&both).unwrap();
            writer.flush().unwrap();
            writer.write_all(b"Content-Length: 1").unwrap();
            writer.flush().unwrap();
            std::thread::sleep(Duration::from_millis(30));
            writer.write_all(b"0\r\n\r\n{\"b\":true").unwrap();
            writer.flush().unwrap();
            writer.write_all(b"}").unwrap();
        });
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"{}");
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"{\"a\":1}");
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"{\"b\":true}");
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn handshake_takes_the_servers_encoding_and_answers_its_requests() {
        let (mut client, log) = fake::client(fake::Spec {
            encoding: "utf-16",
            ..Default::default()
        });
        client.initialize(Path::new("/r"), WAIT).unwrap();
        assert_eq!(client.encoding, Encoding::Utf16);
        assert!(client.supports("definitionProvider"));
        assert!(!client.supports("typeDefinitionProvider"));
        assert!(!client.supports("implementationProvider"));
        // Progress lands and the server's requests are answered without
        // anyone asking.
        let deadline = Instant::now() + WAIT;
        while client.progress().is_none() && Instant::now() < deadline {
            client.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            client.progress(),
            Some(&Progress {
                title: "Indexing".into(),
                percentage: Some(43)
            })
        );
        client.did_open(Path::new("/r/src/lib.rs"), "fn main() {}\n", 1);
        let deadline = Instant::now() + WAIT;
        while client.progress().is_some() && Instant::now() < deadline {
            client.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            client.progress().is_none(),
            "progress end clears the footer"
        );
        let seen = log.lock().unwrap();
        let init = &seen[0];
        assert_eq!(init.0, "initialize");
        assert_eq!(init.1["rootUri"], "file:///r");
        assert_eq!(init.1["workspaceFolders"][0]["name"], "r");
        assert_eq!(
            init.1["capabilities"]["general"]["positionEncodings"][0],
            "utf-8"
        );
        assert!(seen.iter().any(|(m, _)| m == "initialized"));
        let open = seen
            .iter()
            .find(|(m, _)| m == "textDocument/didOpen")
            .unwrap();
        assert_eq!(open.1["textDocument"]["languageId"], "rust");
        assert_eq!(open.1["textDocument"]["text"], "fn main() {}\n");
    }

    #[test]
    fn server_requests_get_null_replies() {
        // The server asks for two configuration items and a progress token;
        // the client answers `[null, null]` and `null` without being asked.
        let (to_client_r, mut to_client_w) = io::pipe().unwrap();
        let (to_server_r, to_server_w) = io::pipe().unwrap();
        let mut client = Client::new(Transport::from_io(to_client_r, to_server_w, None));
        let (replies_tx, replies_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut server_in = io::BufReader::new(to_server_r);
            while let Ok(Some(frame)) = read_frame(&mut server_in) {
                let _ = replies_tx.send(serde_json::from_slice::<Value>(&frame).unwrap());
            }
        });
        for message in [
            json!({"jsonrpc": "2.0", "id": 7, "method": "workspace/configuration",
                "params": {"items": [{"section": "a"}, {"section": "b"}]}}),
            json!({"jsonrpc": "2.0", "id": "tok", "method": "window/workDoneProgress/create",
                "params": {"token": "t"}}),
        ] {
            write_frame(&mut to_client_w, message.to_string().as_bytes()).unwrap();
        }
        let deadline = Instant::now() + WAIT;
        let mut replies = Vec::new();
        while replies.len() < 2 && Instant::now() < deadline {
            client.pump();
            while let Ok(reply) = replies_rx.try_recv() {
                replies.push(reply);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["id"], 7);
        assert_eq!(replies[0]["result"], json!([null, null]));
        assert_eq!(replies[1]["id"], "tok");
        assert_eq!(replies[1]["result"], Value::Null);
    }

    #[test]
    fn definition_round_trip_and_error_replies() {
        let (mut client, _log) = fake::client(fake::Spec::default());
        client.initialize(Path::new("/r"), WAIT).unwrap();
        let receiver = client.at("definition", Path::new("/r/src/main.rs"), 4, 2);
        let result = client.wait(&receiver, WAIT).unwrap();
        assert_eq!(
            targets(&result),
            vec![Target {
                path: PathBuf::from("/r/src/lib.rs"),
                line: 9,
                character: 4
            }]
        );
        let receiver = client.request("textDocument/rename", json!({}));
        assert_eq!(
            client.wait(&receiver, WAIT).unwrap_err(),
            "method not found"
        );
        let receiver = client.references(Path::new("/r/src/main.rs"), 0, 0);
        assert!(targets(&client.wait(&receiver, WAIT).unwrap()).is_empty());
    }

    #[test]
    fn an_unanswered_request_hits_the_deadline_and_a_closed_shim_reads_dead() {
        let (mut client, _log) = fake::client(fake::Spec {
            ignore: vec!["textDocument/hover"],
            ..Default::default()
        });
        client.initialize(Path::new("/r"), WAIT).unwrap();
        let receiver = client.at("hover", Path::new("/r/a.rs"), 0, 0);
        let error = client
            .wait(&receiver, Duration::from_millis(60))
            .unwrap_err();
        assert!(error.starts_with("no answer after"), "{error}");
        assert!(!client.is_dead());
        // `exit` makes the fake drop its end; the next pump notices.
        client.notify("exit", Value::Null);
        let deadline = Instant::now() + WAIT;
        while !client.is_dead() && Instant::now() < deadline {
            client.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(client.is_dead());
        let late = client.at("definition", Path::new("/r/a.rs"), 0, 0);
        assert!(client.wait(&late, WAIT).is_err());
    }

    #[test]
    fn shutdown_is_orderly() {
        let (client, log) = fake::client(fake::Spec::default());
        let mut client = client;
        client.initialize(Path::new("/r"), WAIT).unwrap();
        client.shutdown();
        let deadline = Instant::now() + WAIT;
        loop {
            let seen = log.lock().unwrap();
            let methods: Vec<&str> = seen.iter().map(|(m, _)| m.as_str()).collect();
            if methods.ends_with(&["shutdown", "exit"]) {
                break;
            }
            drop(seen);
            assert!(Instant::now() < deadline, "shutdown/exit never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn history_is_browser_like_and_capped() {
        let mut history: History<u32> = History::default();
        assert!(history.back(0).is_none());
        history.push(1);
        history.push(2);
        assert!(history.can_go_back());
        assert!(!history.can_go_forward());
        assert_eq!(history.back(3), Some(2));
        assert_eq!(history.back(2), Some(1));
        assert!(history.back(1).is_none());
        assert_eq!(history.forward(1), Some(2));
        assert_eq!(history.forward(2), Some(3));
        assert!(history.forward(3).is_none());
        history.back(3);
        history.push(9); // clears forward
        assert!(!history.can_go_forward());
        for i in 0..200 {
            history.push(i);
        }
        let mut depth = 0;
        let mut current = 1000;
        while let Some(previous) = history.back(current) {
            current = previous;
            depth += 1;
        }
        assert_eq!(depth, HISTORY_CAP);
    }

    #[test]
    fn language_ids() {
        assert_eq!(language_id(Path::new("a/b.rs")), "rust");
        assert_eq!(language_id(Path::new("a/b.TSX")), "typescriptreact");
        assert_eq!(language_id(Path::new("a/b.hpp")), "cpp");
        assert_eq!(language_id(Path::new("Makefile")), "plaintext");
    }
}
