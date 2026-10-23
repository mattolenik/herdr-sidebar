//! The file context menu's model and effects: which entries a target offers,
//! and the filesystem/clipboard/shell operations behind them. UI-free so it is
//! unit-testable; `app.rs` owns the popup rendering and input routing.

use std::borrow::Cow;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuAction {
    NewFile,
    NewFolder,
    CopyPath,
    CopyRelativePath,
    Rename,
    Delete,
    /// `git add` the target — the Explorer's staging entry (issue #20).
    Stage,
    /// Open the file in the editor its `[[editor]]` rule resolved to
    /// (`programs.rs`); offered only when a rule matches AND a candidate
    /// exists on this machine.
    EditWith,
    OpenExternal,
    Reveal,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MenuEntry {
    Action(MenuAction, Cow<'static, str>),
    Separator,
}

fn item(action: MenuAction, label: &'static str) -> MenuEntry {
    MenuEntry::Action(action, Cow::Borrowed(label))
}

/// VS Code-style context menu for a tree row (`target` = `Some(is_dir)` for a
/// row; `None` for a right-click on empty space, which targets the workspace
/// root: creation only). "Open with Default App" is offered for files only —
/// a directory's shell association is the file manager, which is what
/// "Reveal in File Explorer" already does. `in_repo` adds "Stage Changes"
/// (issue #20) — it stays out of the menu entirely when the target is not
/// inside a git repository, so the entry never offers an action that can only
/// fail. `editor` is the resolved per-filetype editor name for a file row
/// ("Edit with nvim"), which precedes the default-app entry.
pub fn menu_entries(target: Option<bool>, in_repo: bool, editor: Option<&str>) -> Vec<MenuEntry> {
    let mut entries = vec![
        item(MenuAction::NewFile, "New File…"),
        item(MenuAction::NewFolder, "New Folder…"),
    ];
    if target == Some(false) {
        entries.push(MenuEntry::Separator);
        if let Some(name) = editor {
            entries.push(MenuEntry::Action(
                MenuAction::EditWith,
                Cow::Owned(format!("Edit with {name}")),
            ));
        }
        entries.push(item(MenuAction::OpenExternal, "Open with Default App"));
    }
    if target.is_some() && in_repo {
        entries.extend([
            MenuEntry::Separator,
            item(MenuAction::Stage, "Stage Changes"),
        ]);
    }
    if target.is_some() {
        entries.extend([
            MenuEntry::Separator,
            item(MenuAction::CopyPath, "Copy Path"),
            item(MenuAction::CopyRelativePath, "Copy Relative Path"),
            MenuEntry::Separator,
            item(MenuAction::Rename, "Rename…"),
            item(MenuAction::Delete, "Delete"),
        ]);
    }
    entries.extend([
        MenuEntry::Separator,
        item(MenuAction::Reveal, "Reveal in File Explorer"),
    ]);
    entries
}

/// A usable file name from prompt input: trimmed, non-empty, no path
/// separators or drive colons (a name, not a path).
pub fn validate_name(input: &str) -> Option<&str> {
    let name = input.trim();
    (!name.is_empty() && !name.contains(['/', '\\', ':']) && name != "." && name != "..")
        .then_some(name)
}

fn fresh_path(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = dir.join(name);
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{name} already exists"),
        ));
    }
    Ok(path)
}

pub fn create_file(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = fresh_path(dir, name)?;
    std::fs::write(&path, b"")?;
    Ok(path)
}

pub fn create_folder(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = fresh_path(dir, name)?;
    std::fs::create_dir(&path)?;
    Ok(path)
}

pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    let target = fresh_path(parent, new_name)?;
    std::fs::rename(path, &target)?;
    Ok(target)
}

pub fn delete(path: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Which path [`copy_to_clipboard`] took, so a caller can word its notice
/// accordingly: a native tool's exit status is a real confirmation, while an
/// OSC 52 write is not (the terminal never acknowledges it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardWrite {
    Native,
    Osc52Unacknowledged,
}

/// herdr's libghostty-vt OSC parser caps a captured OSC 52 payload at 8 MiB
/// (`Parser.MAX_ALLOCATING_BUF` in `terminal/osc.zig`). The captured span
/// starts right after the "52;" prefix (which is consumed by the parser's
/// state machine and never itself counted) and holds the kind + separator
/// ("c;", 2 bytes), the base64 payload, and a trailing NUL byte the parser
/// appends once the sequence ends (`terminal/osc/parsers/clipboard_operation.zig`,
/// `cap.writeByte(0)`). A larger sequence never reaches the clipboard, so
/// reject it up front rather than writing a truncated payload the client
/// can't decode.
const OSC52_MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

/// Copy text to the system clipboard.
///
/// Over SSH there is often no local clipboard tool to shell out to, so this
/// emits an OSC 52 sequence on stdout instead: herdr's own terminal emulator
/// parses it out of the pane's PTY stream and forwards it to the attached
/// client, which writes it to the real (possibly remote) clipboard — the
/// same path core herdr's own selection-copy uses. Otherwise this first pipes
/// to the platform's clipboard tool (a console child of the TUI's own pty —
/// no window is created), then falls back to OSC 52 when stdout is a terminal.
pub fn copy_to_clipboard(text: &str) -> io::Result<ClipboardWrite> {
    if osc52_preferred() {
        return write_osc52_stdout(text);
    }
    match copy_native(text) {
        Ok(()) => Ok(ClipboardWrite::Native),
        Err(_) if osc52_fallback_available() => write_osc52_stdout(text),
        Err(err) => Err(err),
    }
}

/// Over SSH there is no local clipboard tool to trust, so OSC 52 goes first.
/// Mirrors core herdr's SSH presence check; deliberately narrower than its
/// `should_prefer_osc52` (no WSL/VS Code detection — those aren't "we have
/// no local clipboard tool" cases the way SSH is).
pub fn osc52_preferred() -> bool {
    #[cfg(not(windows))]
    {
        (std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some())
            && std::io::IsTerminal::is_terminal(&std::io::stdout())
    }
    #[cfg(windows)]
    {
        false
    }
}

/// Whether a failed native copy may fall back to OSC 52 (Unix, on a TTY).
pub fn osc52_fallback_available() -> bool {
    cfg!(not(windows)) && std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// Copy through the platform's clipboard tool (a console child of the TUI's
/// own pty — no window is created). Blocks until the tool exits, so callers
/// on a UI thread should run it on a worker.
pub fn copy_native(text: &str) -> io::Result<()> {
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&["clip"]];
    #[cfg(not(windows))]
    let candidates: &[&[&str]] = &[
        &["pbcopy"],
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
    ];

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for argv in candidates {
        match copy_with(argv, text) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

/// Emit OSC 52 on stdout. Must run on the thread that draws the TUI, or the
/// escape sequence could interleave with a frame being written.
pub fn write_osc52_stdout(text: &str) -> io::Result<ClipboardWrite> {
    let stdout = io::stdout();
    write_osc52(stdout.lock(), text)
}

fn write_osc52(mut out: impl std::io::Write, text: &str) -> io::Result<ClipboardWrite> {
    if text.is_empty() {
        // An empty OSC 52 payload clears the terminal's clipboard instead of
        // leaving it untouched.
        return Err(io::Error::other("nothing to copy"));
    }
    if !osc52_sequence_fits(text.len()) {
        return Err(io::Error::other(
            "selection too large for terminal clipboard (OSC 52 limit)",
        ));
    }
    let encoded = base64_encode(text.as_bytes());
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()?;
    Ok(ClipboardWrite::Osc52Unacknowledged)
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard (RFC 4648, padded) base64 encoder. Hand-rolled so the OSC 52
/// path doesn't need a new dependency for something this small.
fn base64_encode(bytes: &[u8]) -> String {
    // A precise capacity is just a preallocation hint here (the caller has
    // already validated the length via `osc52_sequence_fits`); 0 is always a
    // safe fallback if it were ever to overflow, not a claim about the real
    // encoded length.
    let capacity = checked_base64_encoded_len(bytes.len()).unwrap_or(0);
    let mut out = String::with_capacity(capacity);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(BASE64_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(BASE64_ALPHABET[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(match b1 {
            Some(b1) => {
                BASE64_ALPHABET[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char
            }
            None => '=',
        });
        out.push(match b2 {
            Some(b2) => BASE64_ALPHABET[(b2 & 0x3f) as usize] as char,
            None => '=',
        });
    }
    out
}

/// The base64-encoded length of `input_len` raw bytes (RFC 4648 padded:
/// `ceil(input_len / 3) * 4`), or `None` if computing it would overflow
/// `usize`.
fn checked_base64_encoded_len(input_len: usize) -> Option<usize> {
    input_len.checked_add(2)?.checked_div(3)?.checked_mul(4)
}

/// Whether `input_len` raw bytes, once base64-encoded, fit inside herdr's
/// libghostty-vt OSC 52 capture limit (see [`OSC52_MAX_CAPTURE_BYTES`])
/// alongside the "c;" kind prefix and the trailing NUL the parser appends.
/// Factored out (and worked on the byte count, before encoding) so the
/// boundary can be tested without allocating or emitting real escape
/// sequences, and so a pathological length can't be encoded just to be
/// rejected. Uses checked arithmetic throughout rather than a sentinel
/// fallback, so an overflow is reported as "doesn't fit" instead of being
/// silently misrepresented as a real length.
fn osc52_sequence_fits(input_len: usize) -> bool {
    // The capture holds "c;" (2 bytes) then the base64 payload, then the
    // parser appends one trailing NUL once the OSC sequence ends.
    const KIND_PREFIX_BYTES: usize = 2;
    const TRAILING_NUL_BYTES: usize = 1;
    let Some(encoded_len) = checked_base64_encoded_len(input_len) else {
        return false;
    };
    let Some(total) = encoded_len
        .checked_add(KIND_PREFIX_BYTES)
        .and_then(|t| t.checked_add(TRAILING_NUL_BYTES))
    else {
        return false;
    };
    total <= OSC52_MAX_CAPTURE_BYTES
}

/// How long a clipboard helper may take before it is killed. `clip` and
/// friends exit in milliseconds; one that hangs must not wedge its caller.
const CLIPBOARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn copy_with(argv: &[&str], text: &str) -> io::Result<()> {
    use std::io::Write;

    let mut child = clipboard_command(argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("{} opened without stdin", argv[0]),
        ));
    };
    // Write on a helper thread: a helper that never reads would block the
    // write forever once the pipe buffer fills. One deadline covers BOTH the
    // delivery and the exit, and the copy only counts when every byte was
    // delivered AND the helper succeeded — callers such as the editor's cut
    // delete text on success, and there is no undo.
    let bytes = text.as_bytes().to_vec();
    let (sent, delivery) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = stdin.write_all(&bytes);
        drop(stdin);
        let _ = sent.send(result);
    });
    let deadline = std::time::Instant::now() + CLIPBOARD_TIMEOUT;
    let mut delivered: Option<io::Result<()>> = None;
    let mut exited = None;
    loop {
        if delivered.is_none() {
            match delivery.try_recv() {
                Ok(result) => delivered = Some(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    delivered = Some(Err(io::Error::other("clipboard writer failed")));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if exited.is_none() {
            exited = child.try_wait()?;
        }
        if let (Some(_), Some(_)) = (&delivered, &exited) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // A descendant that inherited the pipe can outlive the helper;
            // the writer thread is then abandoned rather than awaited.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} did not finish in time", argv[0]),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let (Some(delivered), Some(status)) = (delivered, exited) else {
        unreachable!("the loop only exits with both outcomes");
    };
    delivered.map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", argv[0])))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            argv[0]
        )))
    }
}

/// A clipboard helper process. Windows PowerShell children must not inherit
/// a PowerShell 7 `PSModulePath` (issue #96): 5.1 would autoload PS7's
/// CoreCLR modules and lose `Get-Clipboard`. Without the variable, 5.1 uses
/// its own defaults.
fn clipboard_command(argv: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(argv[0]);
    command.args(&argv[1..]);
    if argv[0].eq_ignore_ascii_case("powershell") {
        command.env_remove("PSModulePath");
    }
    command
}

/// Read text from the system clipboard when a platform clipboard command is
/// available. Keeping this best-effort matches [`copy_to_clipboard`]: the
/// editor remains useful over SSH/headless sessions where no clipboard exists.
pub fn paste_from_clipboard() -> io::Result<String> {
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&[
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); Get-Clipboard -Raw",
    ]];
    #[cfg(target_os = "macos")]
    let candidates: &[&[&str]] = &[&["pbpaste"]];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[&[&str]] = &[
        &["wl-paste", "--no-newline"],
        &["xclip", "-selection", "clipboard", "-o"],
    ];

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for argv in candidates {
        match clipboard_command(argv).output() {
            Ok(output) if output.status.success() => {
                return String::from_utf8(output.stdout)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            Ok(output) => {
                last_err = io::Error::other(format!("{} exited with {}", argv[0], output.status));
            }
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

/// Open a directory itself, or open its parent with a file selected
/// (best-effort). Revealing a folder in its parent made the Explorer action
/// look like it had ignored the clicked tree row.
pub fn reveal(path: &Path, directory: bool) {
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("explorer");
        if directory {
            command.arg(path);
        } else {
            command.arg(format!("/select,{}", path.display()));
        }
        let _ = command.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        if !directory {
            command.arg("-R");
        }
        let _ = command.arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if directory { Some(path) } else { path.parent() };
        if let Some(target) = target {
            let _ = std::process::Command::new("xdg-open").arg(target).spawn();
        }
    }
}

/// Open a path with the OS-associated application (VS Code's "Open with
/// Default App" / a double click in the file manager).
///
/// Windows goes through `explorer.exe <path>` rather than `cmd /c start`:
/// explorer is a GUI-subsystem process, so no console is created for it and
/// Windows 11 doesn't flash a Windows Terminal window (the same reason the
/// [[events]] hooks use the windowless sidecar). It resolves the shell
/// association exactly like a double click. Its exit code is unreliable
/// (explorer routinely returns 1 on success), so only the spawn is checked.
pub fn open_external(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    detached(program, &[path.as_os_str()])
}

/// Spawn a GUI program with no stdio; only the spawn is checked.
fn detached(program: impl AsRef<std::ffi::OsStr>, args: &[&std::ffi::OsStr]) -> io::Result<()> {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Open a file in the DEFAULT WEB BROWSER, not the file type's association:
/// a Markdown file's association is an editor, but Shift+Ctrl+click wants it
/// shown by whatever the browser (or its Markdown extension) makes of it.
/// The browser is resolved the way the OS resolves an `https` link; when
/// that lookup fails the plain association (`open_external`) is the fallback.
pub fn open_in_browser(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // LaunchServices remembers the https handler's bundle id; `open -b`
        // hands the file URL to that app whatever it associates `.md` with.
        let Some(bundle) = macos_default_browser() else {
            return open_external(path);
        };
        let url = crate::lsp::uri_for(path);
        detached("open", &["-b".as_ref(), bundle.as_ref(), url.as_ref()])
    }
    #[cfg(windows)]
    {
        // Browsers take a plain path; a `file://` URL would need the drive
        // letter and backslashes rewritten.
        match windows_default_browser() {
            Some(exe) => detached(exe, &[path.as_os_str()]),
            None => open_external(path),
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let url = crate::lsp::uri_for(path);
        if let Some(browser) = std::env::var_os("BROWSER").filter(|b| !b.is_empty()) {
            return detached(browser, &[url.as_ref()]);
        }
        if let Some(desktop) = linux_default_browser()
            && detached("gtk-launch", &[desktop.as_ref(), url.as_ref()]).is_ok()
        {
            return Ok(());
        }
        open_external(path)
    }
}

/// The `--open-link` action behind the manifest's Markdown link handler:
/// herdr routes a Ctrl-click on a matching terminal hyperlink (the `file://`
/// links AI harnesses print for file paths) here with the URL in
/// `HERDR_PLUGIN_CLICKED_URL` (and `clicked_url` in the context JSON), and
/// the file opens in the default web browser.
pub fn open_clicked_link() -> io::Result<()> {
    let url = clicked_url().ok_or_else(|| io::Error::other("no clicked URL in the plugin context"))?;
    let path = link_path(&url).ok_or_else(|| io::Error::other(format!("not a file link: {url}")))?;
    open_in_browser(&path)
}

fn clicked_url() -> Option<String> {
    std::env::var("HERDR_PLUGIN_CLICKED_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .or_else(|| {
            let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
            let context: serde_json::Value = serde_json::from_str(&context).ok()?;
            context.get("clicked_url")?.as_str().map(str::to_string)
        })
}

/// The file behind a `file://` link: fragment and query dropped, percent
/// escapes decoded, a harness's trailing `:line[:col]` removed, and on Windows
/// the `/C:/…` form turned back into a drive path.
pub fn link_path(url: &str) -> Option<std::path::PathBuf> {
    let url = url.split(['#', '?']).next()?;
    let path = crate::lsp::path_for(url)?;
    let text = path.to_string_lossy();
    let mut text = strip_line_suffix(&text);
    if cfg!(windows)
        && let Some(rest) = text.strip_prefix('/')
        && rest.as_bytes().get(1) == Some(&b':')
    {
        text = rest;
    }
    Some(std::path::PathBuf::from(text))
}

/// `a.md:12` / `a.md:12:3` → `a.md`.
fn strip_line_suffix(text: &str) -> &str {
    let mut end = text.len();
    for _ in 0..2 {
        let trimmed = text[..end].trim_end_matches(|c: char| c.is_ascii_digit());
        match trimmed.strip_suffix(':') {
            Some(head) if trimmed.len() < end => end = head.len(),
            _ => break,
        }
    }
    &text[..end]
}

/// Bundle id of the `https` handler in LaunchServices' handler list, read
/// through `plutil` (the plist is binary).
#[cfg(target_os = "macos")]
fn macos_default_browser() -> Option<String> {
    let plist = std::env::var_os("HOME").map(std::path::PathBuf::from)?.join(
        "Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure.plist",
    );
    let out = std::process::Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(plist)
        .output()
        .ok()?;
    let handlers: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    browser_from_handlers(&handlers)
}

#[cfg(any(test, target_os = "macos"))]
fn browser_from_handlers(handlers: &serde_json::Value) -> Option<String> {
    let list = handlers.get("LSHandlers")?.as_array()?;
    ["https", "http"].iter().find_map(|scheme| {
        list.iter()
            .filter(|h| h.get("LSHandlerURLScheme").and_then(|v| v.as_str()) == Some(scheme))
            .find_map(|h| h.get("LSHandlerRoleAll")?.as_str().map(str::to_string))
            .filter(|bundle| bundle != "-")
    })
}

/// The executable behind the user's `https` ProgId, from the registry.
#[cfg(windows)]
fn windows_default_browser() -> Option<std::path::PathBuf> {
    use std::os::windows::process::CommandExt;
    let query = |key: &str, value: &[&str]| -> Option<String> {
        let out = std::process::Command::new("reg")
            .arg("query")
            .arg(key)
            .args(value)
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .output()
            .ok()?;
        reg_sz_value(&String::from_utf8_lossy(&out.stdout))
    };
    let prog_id = query(
        r"HKCU\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice",
        &["/v", "ProgId"],
    )?;
    let command = query(&format!(r"HKCR\{prog_id}\shell\open\command"), &["/ve"])?;
    exe_from_command(&command).map(std::path::PathBuf::from)
}

/// The data of the first `REG_SZ` line in `reg query` output.
#[cfg(any(test, windows))]
fn reg_sz_value(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (_, value) = line.split_once("REG_SZ")?;
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// The program of a shell `open` command: the quoted first token, else the
/// first whitespace-delimited one (`"C:\x\chrome.exe" --single-argument %1`).
#[cfg(any(test, windows))]
fn exe_from_command(command: &str) -> Option<String> {
    let command = command.trim();
    let exe = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => command.split_whitespace().next()?,
    };
    (!exe.is_empty()).then(|| exe.to_string())
}

/// `firefox.desktop`-style id of the default browser, for `gtk-launch`.
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_default_browser() -> Option<String> {
    let out = std::process::Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .output()
        .ok()?;
    let desktop = String::from_utf8_lossy(&out.stdout).trim().to_string();
    desktop.ends_with(".desktop").then_some(desktop)
}

const EDITOR_COMMAND_ENV: &str = "HERDR_SIDEBAR_EDITOR";
const EDITOR_FILE_ENV: &str = "HERDR_SIDEBAR_EDITOR_FILE";
const EDITOR_FILE_TOKEN_ENV: &str = "HERDR_SIDEBAR_EDITOR_FILE_TOKEN";
/// Optional 1-based line for `{line}` (a search hit); absent → `1`.
const EDITOR_LINE_ENV: &str = "HERDR_SIDEBAR_EDITOR_LINE";
const EDITOR_METADATA_SOURCE: &str = "herdr-sidebar-editor";
const EDITOR_PATH_TOKEN: &str = "hs-editor-path";
const EDITOR_HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

pub fn configured_editor() -> Option<String> {
    crate::state::load_editor_command().or_else(|| {
        [EDITOR_COMMAND_ENV, "VISUAL", "EDITOR"]
            .into_iter()
            .find_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
    })
}

/// Split `command` without a shell and substitute placeholders: every
/// `{key}` in `subs` (`line`, `width`, `height`, `theme`, …) and then
/// `{file}`, which is appended as the last argument when no argument
/// mentions it. Shared by the editor wrapper and the viewer's previewers.
pub(crate) fn command_argv(
    command: &str,
    file: &Path,
    subs: &[(&str, String)],
) -> io::Result<Vec<String>> {
    let mut argv = split_editor_command(command)?;
    if argv.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command is empty",
        ));
    }
    let file = file.display().to_string();
    let mut replaced = false;
    for arg in &mut argv {
        for (key, value) in subs {
            let token = format!("{{{key}}}");
            if arg.contains(&token) {
                *arg = arg.replace(&token, value);
            }
        }
        if arg.contains("{file}") {
            *arg = arg.replace("{file}", &file);
            replaced = true;
        }
    }
    if !replaced {
        argv.push(file);
    }
    Ok(argv)
}

pub(crate) fn split_editor_command(command: &str) -> io::Result<Vec<String>> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Some(active) if ch == active => quote = None,
            Some('\'') => current.push(ch),
            Some('"') if ch == '\\' => {
                if matches!(chars.peek(), Some('"' | '\\')) {
                    current.push(chars.next().unwrap());
                } else {
                    current.push(ch);
                }
            }
            Some(_) => current.push(ch),
            None if matches!(ch, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    argv.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None if ch == '\\' => {
                if chars
                    .peek()
                    .is_some_and(|next| next.is_whitespace() || matches!(next, '\'' | '"' | '\\'))
                {
                    current.push(chars.next().unwrap());
                } else {
                    current.push(ch);
                }
                started = true;
            }
            None => {
                current.push(ch);
                started = true;
            }
        }
    }

    if quote.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid editor command",
        ));
    }
    if started {
        argv.push(current);
    }
    Ok(argv)
}

pub fn run_configured_editor() -> io::Result<()> {
    let command = std::env::var(EDITOR_COMMAND_ENV)
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "custom editor is not configured"))?;
    let file = std::env::var_os(EDITOR_FILE_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "editor file is missing"))?;
    let line = std::env::var(EDITOR_LINE_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok());
    let argv = command_argv(
        &command,
        &file,
        &[("line", line.unwrap_or(1).max(1).to_string())],
    )?;
    let pane_id = std::env::var("HERDR_PANE_ID").unwrap_or_default();
    let file_token = std::env::var(EDITOR_FILE_TOKEN_ENV).unwrap_or_default();
    let heartbeat = if pane_id.is_empty() || file_token.is_empty() {
        None
    } else {
        let _ = report_editor_identity(&pane_id, Some(&file_token));
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let heartbeat_pane = pane_id.clone();
        let heartbeat_token = file_token.clone();
        let handle = std::thread::spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                stop_rx.recv_timeout(EDITOR_HEARTBEAT_EVERY)
            {
                let _ = report_editor_identity(&heartbeat_pane, Some(&heartbeat_token));
            }
        });
        Some((stop_tx, handle))
    };
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status();
    if let Some((stop_tx, handle)) = heartbeat {
        let _ = stop_tx.send(());
        let _ = handle.join();
        let _ = report_editor_identity(&pane_id, None);
    }
    let status = status?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            argv[0]
        )))
    }
}

/// Open `file` in a new tab running the global custom editor.
pub fn open_in_editor_tab(my_pane_id: &str, root: &Path, file: &Path) -> io::Result<()> {
    let command = configured_editor().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "set HERDR_SIDEBAR_EDITOR, VISUAL, or EDITOR",
        )
    })?;
    open_in_editor_tab_with(my_pane_id, root, file, &command, None)
}

/// Open `file` in a new tab running `command` (a global custom editor or a
/// per-filetype program). The tab is keyed by FILE: a live tab for the same
/// file is focused instead, whatever command it runs and whatever `line` was
/// requested this time.
pub fn open_in_editor_tab_with(
    my_pane_id: &str,
    root: &Path,
    file: &Path,
    command: &str,
    line: Option<usize>,
) -> io::Result<()> {
    let panes = crate::ipc::call_text("pane.list", serde_json::json!({}))?;
    let workspace_id = crate::launch::workspace_of(&panes, my_pane_id);
    if workspace_id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not find the current workspace",
        ));
    }
    let editor_file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        root.join(file)
    };
    let file_token = editor_file_token(&editor_file);
    if let Some((tab_id, pane_id)) =
        editor_tab_for_file(&panes, &workspace_id, &file_token, crate::state::unix_now())
    {
        crate::viewer::focus_tab_for_client(&tab_id, Some(&pane_id));
        return Ok(());
    }
    let mut env = crate::state::spawn_env()
        .as_object()
        .cloned()
        .unwrap_or_default();
    env.insert(
        EDITOR_COMMAND_ENV.into(),
        serde_json::Value::String(command.to_string()),
    );
    env.insert(
        EDITOR_FILE_ENV.into(),
        serde_json::Value::String(editor_file.display().to_string()),
    );
    env.insert(
        EDITOR_FILE_TOKEN_ENV.into(),
        serde_json::Value::String(file_token.clone()),
    );
    if let Some(line) = line {
        env.insert(
            EDITOR_LINE_ENV.into(),
            serde_json::Value::String(line.to_string()),
        );
    }
    let label = file
        .file_name()
        .unwrap_or(file.as_os_str())
        .to_string_lossy();
    let response = crate::ipc::call_text(
        "tab.create",
        serde_json::json!({
            "workspace_id": workspace_id,
            "label": format!("{label} · editor"),
            "cwd": root.display().to_string(),
            "focus": false,
            "env": env,
        }),
    )?;
    let (tab_id, pane_id) = tab_create_ids(&response).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "editor tab opened without pane metadata",
        )
    })?;
    if let Err(error) = report_editor_identity(&pane_id, Some(&file_token)) {
        let _ = crate::ipc::call_text("tab.close", serde_json::json!({ "tab_id": tab_id }));
        return Err(error);
    }
    if let Err(error) = crate::ipc::call_text(
        "pane.send_input",
        serde_json::json!({
            "pane_id": pane_id,
            "text": format!("{} --run-custom-editor", crate::state::EXECUTABLE_NAME),
            "keys": ["Enter"],
        }),
    ) {
        let _ = crate::ipc::call_text("tab.close", serde_json::json!({ "tab_id": tab_id }));
        return Err(error);
    }
    crate::viewer::focus_tab_for_client(&tab_id, Some(&pane_id));
    Ok(())
}

fn editor_file_token(file: &Path) -> String {
    let path = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let key = path.display().to_string();
    #[cfg(windows)]
    let key = key.replace('/', "\\").to_lowercase();
    crate::viewer::document_token(&key)
}

fn editor_tab_for_file(
    panes_json: &str,
    workspace_id: &str,
    file_token: &str,
    now: u64,
) -> Option<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(serde::Deserialize)]
    struct Res {
        #[serde(default)]
        panes: Vec<Pane>,
    }
    #[derive(serde::Deserialize)]
    struct Pane {
        pane_id: Option<String>,
        tab_id: Option<String>,
        workspace_id: Option<String>,
        #[serde(default)]
        tokens: std::collections::BTreeMap<String, serde_json::Value>,
    }

    let msg = serde_json::from_str::<Msg>(crate::launch::strip_bom(panes_json)).ok()?;
    msg.result.panes.into_iter().find_map(|pane| {
        if pane.workspace_id.as_deref() != Some(workspace_id)
            || pane.tokens.get(EDITOR_PATH_TOKEN)?.as_str()? != file_token
        {
            return None;
        }
        let heartbeat = pane
            .tokens
            .get(EDITOR_METADATA_SOURCE)?
            .as_str()?
            .parse::<u64>()
            .ok()?;
        if now.saturating_sub(heartbeat) > crate::launch::HEARTBEAT_STALE_SECS {
            return None;
        }
        Some((pane.tab_id?, pane.pane_id?))
    })
}

/// Stamp (or, with `None`, clear) the editor pane's identity.
fn report_editor_identity(pane_id: &str, file_token: Option<&str>) -> io::Result<()> {
    let heartbeat = file_token.map(|_| crate::state::unix_now().to_string());
    let tokens = serde_json::json!({
        EDITOR_METADATA_SOURCE: heartbeat,
        EDITOR_PATH_TOKEN: file_token,
    });
    crate::ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": EDITOR_METADATA_SOURCE,
            "tokens": tokens,
        }),
    )?;
    Ok(())
}

fn tab_create_ids(response: &str) -> Option<(String, String)> {
    let value: serde_json::Value =
        serde_json::from_str(response.trim_start_matches('\u{feff}')).ok()?;
    let result = value.get("result")?;
    let tab_id = result.get("tab")?.get("tab_id")?.as_str()?.to_string();
    let pane_id = result
        .get("root_pane")?
        .get("pane_id")?
        .as_str()?
        .to_string();
    Some((tab_id, pane_id))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_link_becomes_its_path_without_line_fragment_or_escapes() {
        let p = |url: &str| super::link_path(url).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(p("file:///Users/m/docs/a%20b.md").as_deref(), Some("/Users/m/docs/a b.md"));
        assert_eq!(p("file:///Users/m/x.md:12:3#L12").as_deref(), Some("/Users/m/x.md"));
        assert_eq!(p("file:///Users/m/x.md?x=1").as_deref(), Some("/Users/m/x.md"));
        assert_eq!(p("https://example.com/x.md"), None);
        assert_eq!(super::strip_line_suffix("a.md:7"), "a.md");
        assert_eq!(super::strip_line_suffix("a.md"), "a.md");
        assert_eq!(super::strip_line_suffix("v1:2:3:4"), "v1:2");
    }

    #[test]
    fn default_browser_is_the_https_handler_in_launch_services() {
        let handlers = serde_json::json!({"LSHandlers": [
            {"LSHandlerURLScheme": "http", "LSHandlerRoleAll": "-"},
            {"LSHandlerURLScheme": "https", "LSHandlerRoleAll": "com.google.chrome"},
        ]});
        assert_eq!(
            super::browser_from_handlers(&handlers).as_deref(),
            Some("com.google.chrome")
        );
        assert_eq!(
            super::browser_from_handlers(&serde_json::json!({"LSHandlers": []})),
            None
        );
    }

    #[test]
    fn browser_exe_comes_from_the_registry_open_command() {
        let out = "\r\nHKEY_CLASSES_ROOT\\ChromeHTML\\shell\\open\\command\r\n    (Default)    REG_SZ    \"C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe\" --single-argument %1\r\n";
        let command = super::reg_sz_value(out).unwrap();
        assert_eq!(
            super::exe_from_command(&command).as_deref(),
            Some(r"C:\Program Files\Google\Chrome\Application\chrome.exe")
        );
        assert_eq!(
            super::exe_from_command("firefox.exe -osint -url %1").as_deref(),
            Some("firefox.exe")
        );
        assert_eq!(super::exe_from_command("   "), None);
    }

    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-ft-actions-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn has(entries: &[MenuEntry], action: MenuAction) -> bool {
        entries
            .iter()
            .any(|e| matches!(e, MenuEntry::Action(a, _) if *a == action))
    }

    #[test]
    fn menu_shape_for_rows_and_root() {
        let row = menu_entries(Some(false), true, None);
        assert!(matches!(row[0], MenuEntry::Action(MenuAction::NewFile, _)));
        assert!(has(&row, MenuAction::Delete));
        let root = menu_entries(None, true, None);
        assert!(!has(&root, MenuAction::Rename));
        assert!(has(&root, MenuAction::Reveal));
    }

    #[test]
    fn open_external_is_offered_for_files_only() {
        assert!(
            has(
                &menu_entries(Some(false), true, None),
                MenuAction::OpenExternal
            ),
            "file row"
        );
        assert!(
            !has(
                &menu_entries(Some(true), true, None),
                MenuAction::OpenExternal
            ),
            "directory row"
        );
        assert!(
            !has(&menu_entries(None, true, None), MenuAction::OpenExternal),
            "empty space"
        );
        // Directories keep everything else they had.
        assert!(has(
            &menu_entries(Some(true), true, None),
            MenuAction::Rename
        ));
    }

    #[test]
    fn stage_is_offered_for_rows_inside_a_repo_only() {
        assert!(
            has(&menu_entries(Some(false), true, None), MenuAction::Stage),
            "file row"
        );
        assert!(
            has(&menu_entries(Some(true), true, None), MenuAction::Stage),
            "directory row"
        );
        assert!(
            !has(&menu_entries(None, true, None), MenuAction::Stage),
            "empty space"
        );
        assert!(
            !has(&menu_entries(Some(false), false, None), MenuAction::Stage),
            "outside a repo"
        );
        assert!(!has(
            &menu_entries(Some(true), false, None),
            MenuAction::Stage
        ));
    }

    #[test]
    fn name_validation_rejects_paths_and_blanks() {
        assert_eq!(validate_name("  notes.md "), Some("notes.md"));
        assert_eq!(validate_name(""), None);
        assert_eq!(validate_name("   "), None);
        assert_eq!(validate_name("a/b"), None);
        assert_eq!(validate_name("a\\b"), None);
        assert_eq!(validate_name("C:"), None);
        assert_eq!(validate_name(".."), None);
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_commands_must_accept_input_and_exit_successfully() {
        assert!(copy_with(&["sh", "-c", "cat >/dev/null"], "copied").is_ok());
        let error = copy_with(&["sh", "-c", "cat >/dev/null; exit 7"], "not copied")
            .unwrap_err()
            .to_string();
        assert!(error.contains("exited with"), "{error}");
    }

    #[test]
    fn editor_command_is_split_without_a_shell_and_substitutes_the_file() {
        let file = Path::new("C:/work/my project/main.rs");
        assert_eq!(
            command_argv("nvim -f", file, &[]).unwrap(),
            vec!["nvim", "-f", "C:/work/my project/main.rs"]
        );
        assert_eq!(
            command_argv("code --goto \"{file}:12\"", file, &[]).unwrap(),
            vec!["code", "--goto", "C:/work/my project/main.rs:12"]
        );
        assert_eq!(
            command_argv(r#"C:\Tools\Code\code.exe --wait"#, file, &[]).unwrap(),
            vec![
                r#"C:\Tools\Code\code.exe"#,
                "--wait",
                "C:/work/my project/main.rs"
            ]
        );
        assert_eq!(
            command_argv(r#""C:\Program Files\Code\code.exe" --wait"#, file, &[]).unwrap(),
            vec![
                r#"C:\Program Files\Code\code.exe"#,
                "--wait",
                "C:/work/my project/main.rs"
            ]
        );
        assert!(command_argv("\"unterminated", file, &[]).is_err());
    }

    #[test]
    fn command_argv_substitutes_placeholders_and_still_appends_the_file() {
        let file = Path::new("/p/f.rs");
        let subs = |pairs: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
            pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
        };
        assert_eq!(
            command_argv("nvim +{line} {file}", file, &subs(&[("line", "12")])).unwrap(),
            vec!["nvim", "+12", "/p/f.rs"]
        );
        // `{line}` without `{file}`: the file is still appended last.
        assert_eq!(
            command_argv("less +{line}", file, &subs(&[("line", "7")])).unwrap(),
            vec!["less", "+7", "/p/f.rs"]
        );
        assert_eq!(
            command_argv(
                "code --goto \"{file}:{line}\"",
                file,
                &subs(&[("line", "3")])
            )
            .unwrap(),
            vec!["code", "--goto", "/p/f.rs:3"]
        );
        // Previewer placeholders; an unknown one is left literally.
        assert_eq!(
            command_argv(
                "viu -b -w {width} -h {height} -s {file} {nope}",
                file,
                &subs(&[("width", "80"), ("height", "24"), ("theme", "dark")])
            )
            .unwrap(),
            vec![
                "viu", "-b", "-w", "80", "-h", "24", "-s", "/p/f.rs", "{nope}"
            ]
        );
    }

    #[test]
    fn edit_with_is_offered_only_for_files_with_a_resolved_editor() {
        let entries = menu_entries(Some(false), true, Some("leaf"));
        let open_with = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Action(MenuAction::EditWith, _)))
            .expect("Edit with entry");
        assert_eq!(
            entries[open_with],
            MenuEntry::Action(MenuAction::EditWith, Cow::Owned("Edit with leaf".into()))
        );
        let external = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Action(MenuAction::OpenExternal, _)))
            .unwrap();
        assert_eq!(external, open_with + 1, "precedes the default-app entry");
        assert!(!has(
            &menu_entries(Some(false), true, None),
            MenuAction::EditWith
        ));
        assert!(!has(
            &menu_entries(Some(true), true, Some("leaf")),
            MenuAction::EditWith
        ));
        assert!(!has(
            &menu_entries(None, true, Some("leaf")),
            MenuAction::EditWith
        ));
    }

    #[test]
    fn tab_create_response_exposes_editor_root_pane() {
        let response = r#"{"result":{"tab":{"tab_id":"w1:t9"},"root_pane":{"pane_id":"w1:p8"}}}"#;
        assert_eq!(
            tab_create_ids(response),
            Some(("w1:t9".into(), "w1:p8".into()))
        );
        assert_eq!(tab_create_ids("garbage"), None);
    }

    #[test]
    fn editor_tabs_reuse_only_the_live_matching_path_in_the_same_workspace() {
        let first = editor_file_token(Path::new("/one/README.md"));
        let second = editor_file_token(Path::new("/two/README.md"));
        assert_ne!(first, second, "same-named files keep distinct identities");
        let panes = serde_json::json!({
            "result": { "panes": [
                {
                    "pane_id": "w1:p1",
                    "tab_id": "w1:t1",
                    "workspace_id": "w1",
                    "tokens": {
                        EDITOR_METADATA_SOURCE: "100",
                        EDITOR_PATH_TOKEN: first,
                    }
                },
                {
                    "pane_id": "w2:p1",
                    "tab_id": "w2:t1",
                    "workspace_id": "w2",
                    "tokens": {
                        EDITOR_METADATA_SOURCE: "100",
                        EDITOR_PATH_TOKEN: first,
                    }
                },
                {
                    "pane_id": "w1:p2",
                    "tab_id": "w1:t2",
                    "workspace_id": "w1",
                    "tokens": {
                        EDITOR_METADATA_SOURCE: "1",
                        EDITOR_PATH_TOKEN: second,
                    }
                }
            ] }
        })
        .to_string();
        assert_eq!(
            editor_tab_for_file(&panes, "w1", &first, 100),
            Some(("w1:t1".into(), "w1:p1".into()))
        );
        assert_eq!(editor_tab_for_file(&panes, "w1", &second, 100), None);
        assert_eq!(
            editor_tab_for_file(&panes, "w2", &first, 100),
            Some(("w2:t1".into(), "w2:p1".into()))
        );
    }

    #[test]
    fn create_rename_delete_roundtrip() {
        let dir = tmp("roundtrip");
        let file = create_file(&dir, "a.txt").unwrap();
        assert!(file.exists());
        assert!(create_file(&dir, "a.txt").is_err(), "no overwrite");
        let folder = create_folder(&dir, "sub").unwrap();
        assert!(folder.is_dir());
        let renamed = rename(&file, "b.txt").unwrap();
        assert!(renamed.exists() && !file.exists());
        assert!(rename(&renamed, "sub").is_err(), "no clobbering existing");
        delete(&renamed, false).unwrap();
        delete(&folder, true).unwrap();
        assert!(!renamed.exists() && !folder.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Astra round 3 HIGH: a helper that exits 0 without reading its input
    /// did NOT copy. Reporting success would let the editor's cut delete the
    /// selection with nothing on the clipboard.
    #[test]
    fn a_helper_that_ignores_its_input_is_not_a_successful_copy() {
        let big = "x".repeat(4 * 1024 * 1024);
        #[cfg(unix)]
        let argv: &[&str] = &["sh", "-c", "exit 0"];
        #[cfg(windows)]
        let argv: &[&str] = &["cmd", "/c", "exit 0"];
        assert!(
            copy_with(argv, &big).is_err(),
            "undelivered input must fail"
        );
    }

    #[test]
    fn powershell_clipboard_helpers_drop_an_inherited_module_path() {
        let command = clipboard_command(&["powershell", "-NoProfile"]);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "PSModulePath" && value.is_none()),
            "PSModulePath must be removed for Windows PowerShell children"
        );
        assert!(clipboard_command(&["clip"]).get_envs().next().is_none());
    }

    #[test]
    fn base64_matches_rfc4648_test_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_unicode_and_embedded_nul_bytes() {
        // Cross-checked against the shell: `printf '%s' 'héllo→✓' | base64`.
        assert_eq!(base64_encode("héllo→✓".as_bytes()), "aMOpbGxv4oaS4pyT");
        // `printf 'a\x00b' | base64`.
        assert_eq!(base64_encode(b"a\x00b"), "YQBi");
    }

    #[test]
    fn osc52_size_check_accepts_up_to_the_limit_and_rejects_past_it() {
        // libghostty-vt's capture holds "c;" (2 bytes) + the base64 payload
        // + a trailing NUL (1 byte) within OSC52_MAX_CAPTURE_BYTES
        // (8_388_608), so the payload budget is 8_388_608 - 3 == 8_388_605
        // bytes. Base64 output length is always a multiple of 4, so the
        // largest encoded length that fits is
        // floor(8_388_605 / 4) * 4 == 2_097_151 * 4 == 8_388_604, i.e.
        // ceil(n / 3) == 2_097_151; the largest raw n giving that is
        // 3 * 2_097_151 == 6_291_453.
        const MAX_FITTING_LEN: usize = 6_291_453;
        assert!(osc52_sequence_fits(MAX_FITTING_LEN));
        assert!(!osc52_sequence_fits(MAX_FITTING_LEN + 1));
    }

    #[test]
    fn osc52_size_check_rejects_usize_max_without_panicking() {
        assert!(!osc52_sequence_fits(usize::MAX));
    }

    #[test]
    fn osc52_writer_emits_the_complete_terminal_sequence() {
        let mut bytes = Vec::new();
        assert_eq!(
            write_osc52(&mut bytes, "hello").unwrap(),
            ClipboardWrite::Osc52Unacknowledged
        );
        assert_eq!(bytes, b"\x1b]52;c;aGVsbG8=\x07");
        assert!(write_osc52(Vec::new(), "").is_err());
    }
}
