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
- Diff / `git show` documents, binary, non-UTF-8 and over-size documents: no LSP.
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
  editors). The resolved server command rides in the control request as a trailing
  field; the viewer never reads rule files and the document key stays the path.
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

### Policy exception: lspmux is named in code

The plugin otherwise names no tool (see AGENTS.md). lspmux is the one exception: the
shim invocation, the running-check and the install/start advice are lspmux-specific
by nature, and the user chose it as the architecture. Language servers themselves stay
in the rules file only.

## Configuration

A third table in `preview-programs.toml` (and `.herdr-sidebar.toml` under the existing
`project_preview_rules` gate), same shape as `[[preview]]` / `[[editor]]`:

```toml
[[lsp]]
patterns = ["*.rs"]
command = "rust-analyzer"

[[lsp]]
patterns = ["*.go"]
command = "gopls"

[[lsp]]
patterns = ["*.c", "*.h", "*.cpp", "*.hpp", "*.cc"]
command = "clangd --background-index"

[[lsp]]
patterns = ["*.ts", "*.tsx", "*.js", "*.jsx"]
command = ["tsgo --lsp --stdio", "typescript-language-server --stdio"]

[[lsp]]
patterns = ["*.py"]
command = ["ty server", "basedpyright-langserver --stdio", "pyright-langserver --stdio"]
```

- `command` is a string or a preference-ordered list; the first program on PATH wins
  (`programs::resolve_program`, unchanged). The resolved entry is the language server,
  never the shim: the viewer wraps it as `lspmux client --server-path <argv0> <args…>`.
  (Verify during implementation that `lspmux client` forwards trailing arguments to
  the server; its README wrapper example passes `$@`, so it should.)
- Placeholders: none. Servers take no file argument; the workspace is sent in
  `initialize`.
- Workspace root: the file's enclosing git worktree root (`Git::owner_of`, nearest
  repository), else the Explorer root. The same root must be computed identically in
  every tab or lspmux will spawn one server per spelling.
- The seeded template gains the five rules above as ACTIVE suggestions (the user can
  edit or delete them, as with the previewer rules). Helix's `languages.toml` is the
  reference for further entries.
- Settings gets a row `Code navigation: <status>` where status is one of
  `lspmux not installed`, `lspmux not running`, `ready` — the same text the first
  Ctrl+click notice uses.

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

Checked lazily on the first document that has an `[[lsp]]` match, cached for the
process, re-checked on the 5 s heartbeat only while the last answer was a failure.

| Finding | How detected | Advice shown (viewer notice + Settings row) |
|---|---|---|
| lspmux not installed | `resolve_program("lspmux")` is None | `lspmux is not installed — brew install lspmux` on macOS; `cargo install lspmux` elsewhere (Linux distro packages exist, see repology). |
| lspmux installed, server not running | `lspmux status` exits non-zero (connection refused to its configured socket). Cheap, run once; no polling while idle. | per init system, below |
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
  "installed via brew" by the resolved path starting with the brew prefix.
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
  `textDocument/didOpen` (full text), `textDocument/didChange` (full sync, only while
  the in-pane editor has the document open), `textDocument/definition`,
  `textDocument/references` (`includeDeclaration: true`), `textDocument/hover`,
  `textDocument/documentSymbol`, `$/cancelRequest`, `shutdown` + `exit`. Optional when
  the server advertises them: `declaration`, `typeDefinition`, `implementation`.
- **Position encoding.** Columns in the viewer are Unicode scalar indexes (the editor's
  convention). Convert to the negotiated encoding at the boundary: utf-8 byte offsets
  when the server accepted `utf-8` (rust-analyzer, clangd, gopls do), UTF-16 code units
  otherwise. One function each way, unit-tested on a line with an emoji and CJK.
- **Lifecycle.** Spawn and `initialize` when a document with an `[[lsp]]` match
  finishes loading (prewarm — the user reads before clicking). `didOpen` right after.
  On a control request for a different file in the same workspace: `didClose` old,
  `didOpen` new, same session. Different workspace: `shutdown`/`exit`, new shim. On
  viewer exit: `shutdown`/`exit` with a 1 s grace, then kill the shim (never the
  server; that is behind lspmux).
- **Threading.** All requests run on the existing load-worker pattern: the UI thread
  sends, polls the channel at `LOAD_POLL` while a request is in flight, and applies a
  result only if the document (and its version) still matches. Request timeout 5 s,
  then `$/cancelRequest` and a notice. A new navigation request cancels the previous.
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
| `Alt+Left` / `[` | Back: pop the stack (request + scroll). `]` forward. |
| Hover | Floating box at the pointer with the server's Markdown rendered through the existing ANSI/plain path, dismissed by any key or click. |

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
- Documents the in-pane editor has dirtied: `didChange` keeps the server current;
  navigation results use the in-memory text's line numbers.

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

1. `programs.rs`: `[[lsp]]` table, seeded rules, `lsp_for`, Settings count text.
2. Control request field + `Request::File { lsp }` round trip.
3. `lsp.rs`: preflight + advice (pure, tested), transport, client with the six
   methods, env scrubbing.
4. Viewer: prewarm, progress footer, Ctrl+click definition with back stack.
5. Ctrl+right-click / `m` menu, references and symbols pickers, hover box.
6. Docs: README sections, AGENTS.md invariants (env scrubbing, lspmux-only exception,
   never own a server), this document marked implemented.

## Open questions

- Does `lspmux client` forward extra server arguments (`--background-index`,
  `--stdio`)? If not, rules must be bare server names and args live in a wrapper
  script, which the doc should say.
- lspmux drops server→client requests (`workspace/configuration`,
  `client/registerCapability`, `window/workDoneProgress/create`). rust-analyzer works
  with it by design; confirm gopls and tsgo tolerate the missing replies, and that
  `$/progress` notifications (not requests) still pass through.
- Whether to honour an existing `~/.config/helix/languages.toml` as a second rule
  source instead of seeding our own list. Deferred: the seeded list is five lines.
