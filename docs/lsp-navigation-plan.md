# LSP code navigation — implementation plan

Status: all six phases implemented (commits `bbb6a7f`, `5ffd41d`, `aea0517`, `97b073d`,
`18a0356` and the Phase 6 commit on `feat/per-filetype-previewers`).

Companion to [lsp-navigation.md](lsp-navigation.md) (the design; this file does not
restate its decisions). Six phases, each a separately reviewable commit on
`feat/per-filetype-previewers` (or a branch off it), each leaving `cargo test`,
`cargo clippy -- -D warnings` and `cargo fmt --check` green from `plugins/herdr-sidebar`.
Live checks run ONLY in the `hs-test` headless session (AGENTS.md recipe).

Current code the plan builds on (all under `plugins/herdr-sidebar/src`):

- `programs.rs`: `Kind {Preview, Editor}`, `RawRule {patterns, pattern, command, mode}`,
  `Rule {patterns, candidates, external}`, `Resolved {command, name, external}`,
  `Layer {previews, editors}`, `RuleFile {preview, editor, rule}`, `compile()` with
  per-kind placeholder validation, `Rules::summary()`, `TEMPLATE`, `ensure_global_file`.
- `viewer.rs`: `Request::File {path, line, renderer, editor, external}`,
  `file_request()` / `parse_request()` / `request_payload()` (tab-separated, fields 1–6),
  `Doc {lines, numbered, scroll, rows: Vec<Row{src, line}>, selection, …}`,
  `run_captured` / `render_with`, the `run()` loop with `preview_load` worker channel,
  `POLL`/`LOAD_POLL`, `notice`, `Prompt`, `pending_edit`, `run_external`.
- `editor.rs`: `text_position_at(body, column, row)` (cell → line/col through gutter,
  tabs, wide cells, wrapped rows) and `col_at_display_x`.
- `explorer_app.rs`: `open_preview_at()` resolves `Kind::Preview`/`Kind::Editor` via
  `resolve_rule()` and calls `file_request`; `settings_rows()` returns
  `(Setting, label, value, enabled)`; `Setting::PreviewPrograms` activation.
- `ipc.rs`: `call_text(method, params)` socket helper.

## Phase 0 — verify the assumptions (no code)

In `hs-test`, from the real terminal (not `pane send-keys`, which cannot prove
terminal encoding):

1. Open a preview, hold Ctrl and left-click inside the body. Log what crossterm
   delivers (temporary `eprintln!` to a scratch file is fine): expect
   `MouseEventKind::Down(Left)` with `KeyModifiers::CONTROL`, and no drag selection.
2. Press `Ctrl+-` and `Ctrl+Alt+-` in the viewer (Option-as-Alt on in Ghostty):
   expect `Char('7')`+CONTROL and `Char('7')`+CONTROL|ALT, distinct.
3. `brew install lspmux`; `lspmux status` with the service stopped → non-zero exit;
   `brew services start lspmux`; `lspmux status` → zero.
4. `rust-analyzer --version`, and confirm `lspmux client --server-path rust-analyzer`
   spawned from a shell with cwd `plugins/herdr-sidebar` answers `initialize` (a
   10-line Python or shell probe is enough).
5. Record findings in the design doc's "Verify live" list. If (1) or (2) fail, stop
   and redesign the gesture before Phase 4.

Exit: all four facts recorded.

## Phase 1 — `[[lsp]]` rules (`programs.rs`, sidebar)

- `Kind::Lsp` with `table() == "lsp"`, placeholders `&[]` (any `{x}` is an error).
- `RawRule.roots: Vec<String>`; `Rule.roots: Vec<String>`; `Layer.lsps`;
  `RuleFile.lsp`. `compile()` rejects `roots` on non-lsp kinds and `mode` on lsp.
- `Rules::lsp_for(root, file)`, `lsp_count()`, and `summary()` grows a third segment
  (`… · 5 lsp`; `len()` counts it).
- `pub fn workspace_root(file: &Path, roots: &[String], repo_root: Option<&Path>,
  explorer_root: &Path) -> PathBuf`: walk up from `file.parent()` to `/`, first
  directory containing any marker wins; else `repo_root`; else `explorer_root`. Pure,
  no I/O beyond `exists()`.
- `TEMPLATE`: the five seeded rules from the design, active, after the preview rules.
  Update the template test's expected counts.
- `explorer_app.rs`: `resolve_rule(path, Kind::Lsp)` in `open_preview_at`; root via
  `workspace_root` with `Git::owner_of(path)` as repo root and `self.root` as the
  Explorer root. Nothing is sent yet (Phase 2 adds the fields); for now the result is
  dropped. "tried …" notice wording reused.
- Tests: parse all three tables; `roots` on a preview rule → error; placeholder in an
  lsp command → error; `lsp_for` precedence project-before-global; `workspace_root`
  on a temp tree with `Cargo.toml` two levels up, with no marker (repo root), with no
  repo (explorer root); summary text with lsp counts.

Exit: rules load, Settings shows `3 previews · 5 lsp` for the seeded file.

## Phase 2 — carry the resolution to the viewer

- `viewer.rs`: `Request::File` gains `lsp: Option<LspSpec>` where
  `pub struct LspSpec { pub command: String, pub root: PathBuf }`.
  `file_request(..)` takes `lsp: Option<&LspSpec>` and appends fields 7 and 8 (empty
  external flag still written as field 6 when fields 7+ are present, so positions are
  stable). `parse_request` reads them; `request_payload` writes them. Legacy 2-, 3-,
  5- and 6-field payloads still parse (test each).
- `explorer_app.rs`: pass the Phase-1 resolution. The Quick Open and search-hit
  paths go through `open_preview_at` already; confirm with a grep that no other
  caller builds a file request by hand (the SCM app only builds diff/show).
- `doc_key()` unchanged: the tab label, `hs-preview-path` token and pinning ignore
  the new fields.
- Tests: round trip with and without `lsp`; equality includes it (changing a rule and
  re-clicking re-sends).

Exit: a click on `viewer.rs` writes a control file whose field 7 is `rust-analyzer`
and field 8 is `…/plugins/herdr-sidebar`; the viewer parses it and ignores it.

## Phase 3 — `lsp.rs`: preflight, transport, client

New lib module, no viewer wiring yet. Everything here is testable without a terminal.

- **Preflight.** `pub enum Preflight { NotInstalled, NotRunning, Running }`;
  `pub fn preflight() -> Preflight` = `resolve_program_here("lspmux")` then
  `lspmux status` (stdio null, 2 s timeout via `run_captured`'s pattern) exit code.
  `pub fn advice(p: Preflight, os: Os, init: Init, lspmux_path: Option<&Path>) ->
  Option<String>` is pure and produces the exact strings from the design (macOS brew /
  macOS cargo+plist / Linux systemd / Linux other). `detect_init()` checks
  `/run/systemd/system` and `systemctl` on PATH.
- **Env scrubbing.** `pub fn shim_env() -> Vec<(String, String)>` keeps exactly the
  allow-list in the design; test asserts no `HERDR_*` and that `PATH` survives.
- **Transport.** `Transport::spawn(spec: &LspSpec) -> io::Result<Transport>` runs
  `lspmux client --server-path <argv0> <args…>` (argv via `split_editor_command`),
  cwd = `spec.root`, env = `shim_env()`, stdio piped. Internally generic over
  `Read + Write` so tests drive it with `std::io::pipe()` (stable since 1.87; the
  crate already requires 1.89). Reader thread: Content-Length framing → `serde_json::
  Value` → `mpsc::Sender<Incoming>`. Writer behind a `Mutex<Box<dyn Write>>`.
- **Client.** `Client { next_id, pending: HashMap<u64, Sender<Result<Value, String>>>,
  encoding: Encoding, caps: Value, progress: Option<(String, Option<u8>)> }` with:
  `initialize(root) -> Result<(), String>` (sends `initialize` with
  `positionEncodings: ["utf-8","utf-16"]`, `workDoneProgress: true`,
  `workspaceFolders` + `rootUri`; reads `positionEncoding` from the RESULT, default
  utf-16; sends `initialized`), `did_open(path, text)`, `did_close(path)`,
  `did_change(path, version, text)`, `request(method, params) -> Receiver<…>` with
  the 5 s deadline applied by the caller, `shutdown()` (shutdown/exit, 1 s grace,
  kill). `pump()` drains incoming: responses → pending; `$/progress` → `progress`;
  server requests `workspace/configuration` → reply `[null; items.len()]`, everything
  else → `null` success; other notifications ignored.
- **Positions.** `to_lsp_col(line: &str, scalar_col, enc) -> u32` and
  `from_lsp_col(line, lsp_col, enc) -> usize`, tested on `"aé😀中b"` for both
  encodings.
- **Fake server for tests.** `#[cfg(test)] mod fake`: a thread reading frames from a
  pipe, answering `initialize` (with a chosen `positionEncoding`), `definition`
  (canned `Location` list), `references`, `hover` (Markdown), and issuing one
  `workspace/configuration` request and one `$/progress` sequence after
  `initialized`. Tests: handshake picks the server's encoding; definition round trip;
  the configuration request is answered with nulls; progress lands in `progress`;
  split frames and two frames in one read; deadline on a request the fake never
  answers.

Exit: `cargo test lsp::` green; no production caller yet.

## Phase 4 — viewer: session, Ctrl+click definition, history

- **Row offsets.** `Row` gains `start: usize` (char offset of the row's first cell
  within its source line), set in `build_rows` from `wrap_line`'s output; unwrapped
  rows have `start = 0`. `Doc::text_position_at(body, column, row) -> Option<(line,
  col)>`: port of the editor's, using `numbered` to size the gutter exactly as
  `draw_doc` does, and `col_at_display_x` from the row's start. Test on a numbered
  wrapped doc with a tab and a wide char.
- **Session state** in `run()`: `lsp: Option<lsp::Session>` where `Session {client,
  spec, open: Option<PathBuf>, version: i32}`; `history: Vec<(Request, usize)>`,
  `forward: Vec<(Request, usize)>`; `nav: Option<(NavKind, Receiver<…>, Instant)>`
  for the in-flight request; `lsp_preflight: Option<Preflight>` cached.
- **Prewarm.** When a `preview_load` result lands for `Request::File {lsp: Some(spec),
  line: _, ..}` and the doc is `numbered` (source view) and not truncated: run
  preflight once; on `Running`, if `lsp` is `None` or its `spec.root`/`command`
  differ, shut the old session down and `Transport::spawn` + `initialize` on a worker
  (the handshake may take seconds on a cold server; the viewer must not block). Then
  `did_open` with the loaded text. On any other document: `did_close` the open one,
  keep the session. The session is never dropped for idle.
- **Ctrl+click.** In the `ViewMode::Preview` mouse arm, before `doc.on_mouse`:
  `Down(Left)` with CONTROL → `text_position_at` → `textDocument/definition`; store
  `nav`; the click is NOT forwarded to selection. Result handling in the per-iteration
  collect step (next to the `preview_load` try_recv): zero locations → notice `no
  definition`; one → `navigate(loc)`; several → open the Phase-5 picker (until then,
  take the first and notice `+N more`).
- **`navigate(loc)`.** Push `(current, doc.scroll)` onto `history`, clear `forward`,
  then: same path → `doc.pending_src = line` (and flash via a 1 s `notice` of
  `path:line`); other path → `current = Request::File {path, line: Some(line),
  renderer: None, editor: None, external: false, lsp: spec.clone()}`, write it to
  the control file with `write_scratch_file` so the sidebar's routing stays
  consistent, `mode = loading_doc`, `preview_load = start_preview_load`.
- **External requests push too.** In the control-file branch, before replacing
  `current` with a different request, push the old one onto `history` and clear
  `forward` (design decision: tree clicks are history).
- **Keys.** `Char('7')` + CONTROL (no ALT) → back: pop `history`, push current onto
  `forward`, reload with the saved scroll via `pending_src`. With ALT → forward.
  Both no-ops with a notice when empty. `history` capped at 50 (drop oldest).
- **Progress footer.** `draw_doc` takes an extra `Option<&str>` status; the loop
  passes `indexing… 43%` from `client.progress` while set. A Ctrl+click during
  progress is queued in one slot and fired on `end`.
- **Timeouts.** `nav` older than 5 s → drop it, notice `rust-analyzer did not answer`.
  A new Ctrl+click replaces `nav`; a late reply for a replaced id is ignored by the
  pending map.
- **Preflight failures.** `NotInstalled`/`NotRunning` → notice with `advice(..)` on the
  first Ctrl+click; re-run preflight on the next Ctrl+click only.
- **Exit paths.** `shutdown()` on viewer exit and before `run_external` (the editor
  owns the pane meanwhile; reopen after it returns: `did_close` + `did_open` with the
  reloaded text).
- Tests: history push/pop/cap as a pure helper (`History` struct in `lsp.rs` or a
  small `nav.rs`); `text_position_at`; the request-building functions. The loop
  itself stays covered by the live check.

Exit (live, `hs-test`): open `viewer.rs`, Ctrl+click `Doc` in a `ViewMode::Preview(doc)`
match arm → pane shows `struct Doc` at the top; `Ctrl+-` returns to the arm with the
same scroll; `Ctrl+Alt+-` goes forward; a second preview tab on the same crate shows
no second `rust-analyzer` in `ps`.

## Phase 5 — menu, pickers, hover

- **Viewer overlay.** `enum Overlay { Menu { at: (u16,u16), entries: Vec<NavAction>,
  selected }, Picker { title, items: Vec<PickItem{label, loc}>, selected, scroll },
  Hover { lines: Vec<Line>, at, shown: Instant } }` in `viewer.rs`, drawn with
  `Clear` + a `Block` using `ui::Palette` and `ui::draw_scrollbar`; keys j/k/↑/↓,
  Enter, Esc; mouse hover highlights, click selects. Overlay input is dispatched
  before document input, the way `prompt` is today.
- **Menu.** Ctrl+right-click (modifier bit, same as the sidebar's) at a body cell, and
  `m` at the selection anchor or last clicked cell: entries Go to Definition, Go to
  Type Definition / Implementation (only when `caps` advertise them), Find
  References, Hover, Document Symbols, Back, Forward. Each entry maps to the
  Phase-4 request path with a `NavKind`.
- **References.** `textDocument/references` with `includeDeclaration: true`; result
  → picker items `rel/path:line  <trimmed line>` grouped by file (file header rows
  unselectable), snippet text read on the worker (one `fs::read` per distinct file,
  first 500 locations, `+N more` footer). Enter → `navigate`.
- **Document Symbols.** `textDocument/documentSymbol`; flatten nested `DocumentSymbol`
  with two-space indentation per depth (handle the flat `SymbolInformation` form
  too). Enter → same-file navigate.
- **Hover.** Two triggers: pointer dwell (track `last_motion: (cell, Instant)` from
  `MouseEventKind::Moved`; when the cell is unchanged for 500 ms and no overlay is
  up, request `textDocument/hover`) and the menu entry. Result Markdown (`contents`
  as `MarkupContent`, `MarkedString`, or an array — normalise all three to one
  Markdown string) → `render_with("leaf --inline ansi:{width}", …)` on the worker
  with the Markdown as stdin and `{width}` = min(60, body width − 4); leaf missing →
  plain text with ``` fence lines removed. Box at the pointer, clamped inside the
  body, height ≤ body/2. Dismiss on any key, click, scroll, motion to another cell,
  or 8 s.
- Tests: symbol flattening; hover `contents` normalisation; picker grouping and cap;
  menu entries gated by capabilities; fence stripping.

Exit (live): references for `Doc` list hits across `viewer.rs`; Enter jumps; dwell
over `run_captured` shows its signature box rendered by leaf; `m` opens the menu.

## Phase 6 — Settings row, docs, cleanup

- `Setting::CodeNavigation` in `settings_rows()`: label `Code navigation`, value from
  a worker started when the modal opens (`checking…` → `lspmux running` / `lspmux not
  running` / `lspmux not installed`), polled from `tick()` like the folder picker.
  Enter on the row shows the `advice(..)` text as a notice. No action otherwise.
- README (root + plugin): a "Code navigation" section: what it does, the `[[lsp]]`
  table with `roots`, the five seeded rules, lspmux as a requirement with the
  per-OS start commands, the editor-sharing note, Option-as-Alt, keys.
- AGENTS.md: invariants — env scrubbing (why), lspmux and leaf as the two named
  tools, never own a server, roots walk and why not git root, position encoding from
  the replayed result, no cancel through lspmux, control fields 7/8, tree clicks
  push history, `std::io::pipe` fake server pattern.
- `docs/lsp-navigation.md`: status line → implemented, with Phase-0 findings.
- Final pass: `cargo clippy --all-targets -- -D warnings`, `cargo fmt`, the full
  test suite, a release build, `herdr plugin action invoke herdr-sidebar.refresh-sidebars`,
  and the three live checks above repeated once on the release binary in `hs-test`.

## Size estimate

| Phase | New/changed lines (approx.) | Risk |
|---|---|---|
| 0 | 0 | Gesture delivery is the only unknown that can change the design |
| 1 | 150 + tests | low |
| 2 | 60 + tests | low |
| 3 | 450 + 200 tests | medium: framing and threading, but hermetic |
| 4 | 350 | high: loop integration, worker handshake, history |
| 5 | 400 | medium: overlay drawing, hover plumbing |
| 6 | docs + 60 | low |

Phases 1–3 can proceed in parallel with Phase 0; Phase 4 waits on Phase 0's answer.
