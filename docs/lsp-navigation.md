# LSP code navigation in the preview pane — design

Status: design, not implemented. Branch `feat/per-filetype-previewers` is the base.

## Goal

IDE-style navigation on a code file shown in the built-in preview viewer: Ctrl+click
follows the symbol under the pointer to its definition, Ctrl+right-click opens a menu
with Go to Definition / Find References / Hover / Symbols, results jump within the same
preview pane, and a back stack returns. Answers must be semantic (a real language
server), and they must be fast enough that the user does not wait on the first click.

## Non-goals (this round)

- Windows. The design is Unix-only until lspmux's Windows story is checked.
- Editing features: completion, diagnostics, rename, code actions, formatting.
- Diff / `git show` documents, binary, non-UTF-8 and over-size documents (the viewer's
  existing text caps: `MAX_BYTES` 1 MiB, `MAX_LINES`): no LSP. `didOpen` must send
  exactly the text on screen, so a truncated document is never opened.
- Owning processes. We never spawn, supervise or kill lspmux or a language server.

## Architecture

```
tree click ─► sidebar resolves [[lsp]] rule ─► control request (… \t<server cmd>)
                                                     │
                       preview viewer (one process per preview tab)
                       │  JSON-RPC over stdio, hand-rolled framing
                       ▼
                 lspmux client --server-path <server>        (shim, ms to start)
                       │  unix socket / localhost TCP
                       ▼
                 lspmux server   ◄── system service (launchd / systemd --user)
                       │  one language server per (workspace, environment)
                       ▼
                 rust-analyzer | gopls | clangd | tsgo | ty | …
```

- **The sidebar resolves, the viewer only runs** (same rule as previewers and
  editors). The resolved server command and the workspace root ride in the control
  request as fields 7 and 8 (after the positional external flag in field 6); the
  viewer never reads rule files and the document key stays the path. Navigation
  requests the viewer writes for itself copy both fields from the origin.
- **lspmux is the only multiplexing we use.** Every preview tab is its own
  `herdr-sidebar --preview` process; without sharing, every tab would pay a full
  index (rust-analyzer: seconds to a minute and up to gigabytes). The lspmux shim
  connects the viewer to the one server instance for that workspace, so the second
  tab onward answers in milliseconds.
- **lspmux is a hard external requirement.** It runs as the user's system service;
  the plugin never starts, stops, supervises or falls back to starting it, and without
  it code navigation is simply off, with advice on how to get it. Reaping is lspmux's `instance_timeout` (default 300 s after the last
  client disconnects): when the herdr server stops, every viewer dies, every shim
  disconnects, and the language servers are gone five minutes later. The idle lspmux
  daemon costs a few MB and is the user's to manage.

### Policy exceptions: lspmux and leaf are named in code

The plugin otherwise names no tool (see AGENTS.md). Two exceptions, both user
decisions: lspmux (the shim invocation, the running-check and the install/start
advice are lspmux-specific by nature) and leaf for rendering hover Markdown (a fixed
inline renderer, independent of the user's Markdown previewer rule). Language servers
themselves stay in the rules file only.

## Configuration

A third table in `preview-programs.toml` (and `.herdr-sidebar.toml` under the existing
`project_preview_rules` gate), same shape as `[[preview]]` / `[[editor]]`:

```toml
[[lsp]]
patterns = ["*.rs"]
roots = ["Cargo.toml"]
command = "rust-analyzer"

[[lsp]]
patterns = ["*.go"]
roots = ["go.mod"]
command = "gopls"

[[lsp]]
patterns = ["*.c", "*.h", "*.cpp", "*.hpp", "*.cc"]
roots = ["compile_commands.json", ".clangd"]
command = "clangd --background-index"

[[lsp]]
patterns = ["*.ts", "*.tsx", "*.js", "*.jsx"]
roots = ["tsconfig.json", "package.json"]
command = ["tsgo --lsp --stdio", "typescript-language-server --stdio"]

[[lsp]]
patterns = ["*.py"]
roots = ["pyproject.toml", "setup.py"]
command = ["ty server", "basedpyright-langserver --stdio", "pyright-langserver --stdio"]
```

- `command` is a string or a preference-ordered list; the first program on PATH wins
  (`programs::resolve_program`, unchanged). The resolved entry is the language server,
  never the shim: the viewer wraps it as `lspmux client --server-path <argv0> <args…>`.
  (Verify during implementation that `lspmux client` forwards trailing arguments to
  the server; its README wrapper example passes `$@`, so it should.)
- Placeholders: none. Servers take no file argument; the workspace is sent in
  `initialize`. `roots` is optional; unknown keys are load errors like elsewhere.
- Workspace root: walk UP from the viewed file's directory; the first ancestor that
  contains any name in the rule's `roots` is the root. No `roots`, or no ancestor
  matches: the file's enclosing git worktree root (`Git::owner_of`, nearest
  repository). Not in a repository: the Explorer root. This is Helix's convention, so
  a Helix pane on the same crate shares the server if Helix also uses the lspmux shim.
  Why not the git root: servers only discover their project file in the root they are
  given (rust-analyzer: `Cargo.toml`, gopls: `go.mod`, …), and this very repository
  keeps its crate at `plugins/herdr-sidebar` with no root `Cargo.toml`. Two crates in
  one repository therefore get two server instances, which is correct. The root is part
  of lspmux's instance key, so the sidebar computes it (it resolves the rule) and passes
  it in the control request with the command; the viewer sends it as
  `workspaceFolders[0]` and `rootUri` and sets the shim's cwd to it.
- The seeded template gains the five rules above as ACTIVE suggestions (the user can
  edit or delete them, as with the previewer rules). Helix's default `languages.toml`
  is a reference for writing further entries by hand; the plugin never reads it.
- Settings gets a row `Code navigation: <status>` where status is one of
  `lspmux not installed`, `lspmux not running`, `lspmux running`. The SIDEBAR process
  computes it itself when the modal opens (resolve `lspmux` on PATH, then `lspmux
  status` on a worker thread; the row reads `checking…` until it lands) through the
  same shared `lsp::preflight` function the viewer uses, so the two can never disagree
  on wording. "lspmux running" promises nothing per file: whether a given document
  navigates also needs a matching `[[lsp]]` rule whose server is installed, and those
  failures are viewer notices at open time.

### Environment is part of lspmux's instance key — scrub it

lspmux matches a client to a running server by workspace AND the environment the shim
reports, and its default `pass_environment = ["*"]` passes everything. The viewer's
environment differs per pane (`HERDR_PANE_ID`, `HERDR_TAB_ID`, the control path, …), so a
naively spawned shim would get a private server per preview tab and defeat the whole
point. The viewer therefore spawns the shim with a minimal, stable environment:
`PATH`, `HOME`, `USER`, `LANG`, `TMPDIR`, `XDG_*`, `CARGO_HOME`, `RUSTUP_HOME`,
`GOPATH`, `GOROOT` — and nothing `HERDR_*`. A unit test asserts the spawn env contains
no `HERDR_` key.

## Preflight and user advice

In the viewer: checked lazily on the first document that has an `[[lsp]]` match,
cached for the process, re-checked on the next navigation gesture (not on a timer)
while the last answer was a failure. In the sidebar: on Settings open, see above.

| Finding | How detected | Advice shown (viewer notice + Settings row) |
|---|---|---|
| lspmux not installed | `resolve_program("lspmux")` is None | `lspmux is not installed — brew install lspmux` on macOS; `cargo install lspmux` elsewhere (Linux distro packages exist, see repology). |
| lspmux installed, server not running | `lspmux status` exits non-zero (connection refused to its configured socket). Run once per check; never polled. | per init system, below |
| Language server not installed | rule has no resolvable candidate | `no language server for *.rs — tried rust-analyzer` (same wording as the previewer fall-through) |
| Shim connects but server crashes / `initialize` fails | shim exits or `initialize` returns an error | `rust-analyzer failed: <first stderr line>`; retry once on the next click |

Per-init-system start advice:

- **macOS (launchd via Homebrew).** The brew formula ships a service definition
  (`lspmux server`, keep-alive, logs under `$(brew --prefix)/var/log/lspmux.log`):

  ```
  brew services start lspmux
  ```

  If lspmux was installed with cargo instead, point the user at lspmux's example
  `lspmux.plist` (install to `~/Library/LaunchAgents/`, fill in the binary path, then
  `launchctl load -w ~/Library/LaunchAgents/org.codeberg.p2502.lspmux.plist`). Detect
  "installed via brew" by the resolved path starting with `$HOMEBREW_PREFIX`, else
  `/opt/homebrew` or `/usr/local`.
- **Linux with systemd (user session).** The advice names the unit and the command,
  nothing else: `install lspmux's user service (see its README / lspmux.service), then
  systemctl --user enable --now lspmux`. The plugin never writes a unit file.
  Detect systemd by `systemctl --user` being on PATH and `/run/systemd/system`
  existing.
- **Linux without systemd.** `lspmux server must be running; start it with your init
  system`. Nothing more specific is offered.

The plugin never starts `lspmux server` itself, under any init system or as a fallback:
a running lspmux is a hard external requirement, exactly like the language servers.

The advice is a notice, never a blocking modal, and Ctrl+click simply does nothing
beyond the notice until the preflight passes. The notice text is produced by one pure
function `lsp::advice(os, init_system, lspmux_path) -> String` with unit tests per case.

## Viewer: LSP client

- **Transport.** `lspmux client --server-path …` as a child with piped stdio, scrubbed
  env, `CREATE_NO_WINDOW`-equivalent not needed (Unix only). Content-Length framing,
  a reader thread pushing decoded messages to a channel, a request-id → oneshot map.
  serde_json only; `lsp-types` is optional and not worth the compile time for the six
  methods used. No async runtime (the viewer is a synchronous crossterm loop).
- **Messages used.** `initialize` (capabilities: `positionEncodings: ["utf-8",
  "utf-16"]`, `window.workDoneProgress: true`), `initialized`,
  `textDocument/didOpen` (full text), `textDocument/didChange` (full sync, only in the
  fallback in-pane editor; the normal `e` path runs an external editor and is covered
  by `didClose` + `didOpen` of the reloaded text when it exits), `textDocument/definition`,
  `textDocument/references` (`includeDeclaration: true`), `textDocument/hover`,
  `textDocument/documentSymbol`, `shutdown` + `exit`. Optional when the server
  advertises them: `declaration`, `typeDefinition`, `implementation`. The client must
  also ANSWER two server→client requests, because lspmux forwards them to a connected
  client and rust-analyzer blocks on the reply: `workspace/configuration` (an array of
  `null`, one per item) and `window/workDoneProgress/create` (`null`). Every other
  server request gets a `null` success reply too, as a default. There is no
  `$/cancelRequest`: lspmux retags request ids per client and forwards the cancel
  untouched, so the server never matches it; staleness is handled by "apply only if
  the document still matches".
- **Position encoding.** Columns in the viewer are Unicode scalar indexes (the editor's
  convention). Convert at the boundary to the encoding named by the `initialize` RESULT
  the viewer receives, not the one it asked for: lspmux initializes a server once with
  the first client's parameters and replays that result to every later client, so a
  shared rust-analyzer may be speaking whatever Helix negotiated. A missing
  `positionEncoding` means UTF-16 (the spec default); `utf-8` means byte offsets. For
  the same reason `$/progress` is optional: the first client may not have asked for
  it, and the footer simply shows nothing. One conversion function each way,
  unit-tested on a line with an emoji and CJK.
- **Lifecycle.** Spawn and `initialize` when a document with an `[[lsp]]` match
  finishes loading (prewarm — the user reads before clicking). `didOpen` right after.
  On a control request for a different file in the same workspace: `didClose` old,
  `didOpen` new, same session. Navigation targets keep the origin's session and
  workspace whatever directory they live in (a definition in `~/.cargo/registry` is
  just another `didOpen`); only a fresh sidebar click re-resolves rules and may start
  a session for another workspace (`shutdown`/`exit`, new shim). On viewer exit:
  `shutdown`/`exit` with a 1 s grace, then kill the shim (never the server; that is
  behind lspmux). The shim stays connected for the life of the tab, by decision: an
  open preview tab is a client like an open editor, and keeping its language server
  warm is lspmux's job as a background service. There is no idle disconnect.
- **Threading.** All requests run on the existing load-worker pattern: the UI thread
  sends, polls the channel at `LOAD_POLL` while a request is in flight, and applies a
  result only if the document (and its version) still matches. Request timeout 5 s,
  then a notice. A new navigation request supersedes the previous; a late result for
  an older request is discarded.
- **Progress.** `$/progress` begin/report/end from the server (rust-analyzer "Indexing",
  gopls "Loading packages") is shown in the viewer footer as `indexing… 43%`. A click
  during indexing is queued (one slot) and fires on `end`, instead of returning an
  empty definition list.

## UI

Mouse gestures only reach the app with a modifier (herdr intercepts plain right-click;
Cmd is not encoded in SGR mouse reports), so:

| Gesture | Action |
|---|---|
| Ctrl+left click on a symbol | Go to definition. One result: jump. Several: picker overlay. None: notice `no definition`. |
| Ctrl+right click | Menu at the pointer: Go to Definition, Go to Type Definition / Implementation (capability-gated), Find References, Hover, Document Symbols, Back. Acts on the position under the pointer. |
| `m` (existing keyboard menu key) | Same menu, acting on the last clicked position (the selection anchor). |
| `Ctrl+-` | Back: pop the history (request + scroll). `Ctrl+Alt+-` forward. |
| Pointer dwell (~500 ms on one cell) or the menu's Hover | `textDocument/hover` for that position; the result is shown in a floating box at the pointer (see **Hover** below), dismissed by motion off the cell, any key, click or scroll, and by a timer since terminals send no "pointer left the pane" event. |

- **Cell → text position.** Rows are tagged with their source line (`Row` in
  `viewer.rs`); the editor already maps a cell to a Unicode column through the gutter,
  tabs, wide cells and wrapped-row offsets (`text_position_at`). Lift that into a shared
  helper the read-only preview uses too. A click on a continuation row maps to its
  source line with the wrap offset added.
- **Jumps.** A target in the same file: scroll to the line and flash it (reuse the
  line-anchored request path: `file\t<path>\t<line>`). Another file: load it in the
  SAME pane through the normal file request (renderer/editor/lsp fields re-resolved by
  the viewer asking the sidebar? No: the viewer cannot resolve rules. It writes the
  request itself with the same `lsp` field it already has, and an empty renderer —
  code navigation targets are source files, and the line-anchored rule already forces
  the source view.) Pinned/ephemeral status of the tab is untouched: navigation is the
  same document family, so the tab keeps its label suffix; the label updates to the new
  file name as it does for any request.
- **History.** Every document change pushes the document being left (request + scroll)
  onto the back history: a Ctrl+click jump, a picker choice, AND a sidebar click that
  replaces the ephemeral preview, so Back after a tree click returns to what was being
  read before it. Forward is cleared by any new push, browser-style. Capped at 50.
  Keys: `Ctrl+-` back, `Ctrl+Alt+-` forward. In legacy terminal keyboard encoding
  (herdr panes do not enable the kitty keyboard protocol) `Ctrl+-` arrives as `0x1F`
  and `Ctrl+Alt+-` as `ESC 0x1F`; crossterm reports them as Ctrl+`7` and Ctrl+Alt+`7`,
  so the viewer matches on `Char('7')` with CONTROL, with and without ALT. `Ctrl+Shift+-`
  was rejected because it also encodes as `0x1F`. macOS users need Option to act as Alt
  (Ghostty: `macos-option-as-alt = true`), which the README states. Verify the chords
  from a real terminal in `hs-test` first; `pane send-keys` injects synthetic keys and
  proves nothing about the terminal's encoding.
- **Hover.** Servers return hover content as Markdown (a fenced signature, then the
  doc comment). It is rendered with `leaf --inline ansi:{width}`, the Markdown piped on
  stdin (verified: leaf renders stdin with no file argument), through the existing
  `render_with` spawn path so it gets the same timeout, output cap and colour env; the
  ANSI result goes through `ansi::to_lines` into the box. This is a FIXED command, not
  the user's `[[preview]]` Markdown rule (that rule may be an external viewer or a
  slow renderer; the tooltip must be instant and inline). With leaf missing, the box
  shows the Markdown as plain text with fence markers stripped. Box width is min(60,
  pane width − 4); height is capped at half the pane with the rest cut.
- **References picker.** Overlay list `path:line  <trimmed line text>`, grouped by
  file, j/k and mouse, Enter jumps, Esc closes. Capped at 500 entries with a `+N more`
  footer. Reuses the Quick Open list rendering.
- **Document Symbols.** Same picker, hierarchical symbols flattened with indentation.
- The `e` editor path is unaffected: `e` still runs the editor rule in the pane.

## Performance budget

| Phase | Target | How |
|---|---|---|
| Shim spawn + `initialize` round trip | < 50 ms when the server is already warm in lspmux | shim is a thin socket proxy; measured on first implementation and recorded here |
| First definition on a warm server | < 100 ms | request off the UI thread, no re-render until the result lands |
| Cold server (first tab in a workspace) | server-dependent: clangd 0.4–1.2 s, gopls 0.6–1.4 s, tsgo ~1 s, ty ~2 s, rust-analyzer 3 s on a toy crate and up to a minute here | prewarm at document open; progress in the footer; click queued until `end` |
| Memory per extra preview tab | ~0 (one shim process, a few MB) | sharing via lspmux; env scrubbing is what makes it hold |
| Viewer idle cost | 0 extra wakeups | the reader thread blocks on stdout; no polling while nothing is in flight |

## Failure handling

- lspmux goes away mid-session (service stopped): the shim's stdout hits EOF; the viewer
  marks the session dead, shows the "not running" advice on the next gesture, and
  re-runs preflight on the heartbeat until it passes, then re-initializes lazily.
- Server returns an error for a request: notice with the message; no retry.
- Server crashes under lspmux: lspmux restarts it on the next client; the viewer sees
  EOF, treats it as above.
- A result referencing a file outside the workspace (stdlib, registry source): still
  opened; the sidebar tree does not need to contain it (the viewer loads any readable
  path, as it does today).
- After the external editor exits, the viewer reloads and re-opens the document
  (`didClose`/`didOpen`); edits made elsewhere reach the server through its own file
  watching. The fallback in-pane editor sends `didChange`, and navigation results use
  the in-memory text's line numbers.

## Testing

- Unit: framing encoder/decoder (including split reads and two messages in one read),
  id matching, position-encoding conversions, `lsp::advice` per OS/init, env scrubbing,
  cell → position on wrapped and gutter rows, picker grouping and cap.
- Hermetic integration: a 40-line fake language server in `tests/` (a Rust test binary
  speaking JSON-RPC over stdio) that answers `initialize`, `definition` and `references`
  with canned locations; the viewer's client is driven against it directly (no lspmux),
  the same way renderer tests use `git` as a known program.
- Live: in the `hs-test` headless session only (AGENTS.md recipe), with the real lspmux
  service and rust-analyzer on this repo: open `viewer.rs`, Ctrl+click `Doc`, assert
  the pane shows the struct definition; `pane read` to confirm; second tab answers
  without a new rust-analyzer process (`ps` count).

## Implementation order

See [lsp-navigation-plan.md](lsp-navigation-plan.md): six phases, each a reviewable
commit, Phase 0 being the live verification list below.

## Resolved from lspmux's source (0.3.x)

- `lspmux client --server-path X args…` forwards trailing arguments to the server
  (`SERVER_ARGS`), so rules may carry `--background-index` / `--stdio`. Server path,
  arguments, environment and workspace root form the instance key.
- Server→client requests are not dropped: `window/workDoneProgress/create` and
  `client/registerCapability` are broadcast with faked replies (capabilities cached
  and replayed to late clients); `workspace/configuration` goes to one client, hence
  the answering requirement above. Server notifications, `$/progress` included, are
  broadcast to every client after initialization; messages during the server's own
  `initialize` are discarded.
- `initialize` runs once per instance with the first client's parameters; the result
  is replayed to later clients (hence the position-encoding rule above).
- Workspace root is taken from `workspaceFolders[0]`, then `rootUri`, then the shim's
  cwd, keyed by device and inode so symlink spellings collapse.

## Open questions

None. Decided: the plugin never reads Helix's (or any editor's) configuration; it
borrows Helix's conventions (`roots` markers, preference lists) and seeds its own
rules.

## Verify live in `hs-test` before building

- Ctrl+left click reaches the viewer with the ctrl bit and does not start a drag
  selection.
- `Ctrl+-` and `Ctrl+Alt+-` arrive as distinct byte sequences from the real terminal.
- `lspmux status` exit code with the service stopped.
- `tsgo --lsp --stdio` and `ty server` are the right invocations for the installed
  versions.
