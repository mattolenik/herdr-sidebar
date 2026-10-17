# Stacked Pane Trees Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Explorer shows one file tree per directory that a pane on its tab is working in, stacked and grouped by repository, each repository with its own git button and Source Control view, with back/forward between stack and git view.

**Architecture:** Pure decision code (which roots exist, how they group, how rows are shared, view history) goes in two new lib modules, `stack.rs` and `viewnav.rs`, tested hermetically. The Explorer `App` in `explorer_app.rs` is refactored from one tree to a `Vec<Region>` (one per group) first as a behaviour-preserving step, then grows the stack sampling, stacked rendering and git-entry wiring. `main.rs` decides the starting view from persisted `active`/`active_repo`; `Exit::Switch` re-reads state instead of toggling. Single-tree features (cwd-follow setting, Change Folder, `roots.json`, the Source Control activity chip) are deleted in one commit before the refactor.

**Tech Stack:** Rust 2024 edition (rust-version 1.89), ratatui, crossterm (patched fork already pinned), serde_json, the `ignore` walker. No new dependencies; `rfd` is removed.

**Spec:** `docs/superpowers/specs/2026-10-16-stacked-pane-trees-design.md`

Run every command from `plugins/herdr-sidebar/`. Commit from the repo root with the attribution line `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` at the end of each message.

## Global Constraints

- A pane's live directory is `foreground_cwd` only; never fall back to `cwd` (spec, AGENTS.md).
- Eligible panes are exactly what `launch::sibling_cwds` accepts today: same tab, not a sidebar/Source Control/preview pane, non-empty `foreground_cwd`.
- Heartbeat stamps every event-loop iteration; no stack work may block the TUI thread. Git subprocesses stay on the existing single decoration worker.
- `pane.list` order is never meaningful: sort before deciding.
- `cargo test` and `cargo clippy -- -D warnings` pass after every task.
- User-facing wording stays as the spec gives it: header `SOURCE CONTROL · <repo>`, notice `no repository in this group`.
- Minimum unfolded region body is 3 rows; divider is 1 row between groups; header is 1 row and always visible.
- View history cap is 50 entries.

## Review Focus

1. A pane whose `foreground_cwd` no longer exists on disk: its group must render as `(empty)`, not panic, and must group as a non-repository. Pinned in Task 2 (`group_roots_treats_missing_owner_as_plain_folder`) and Task 6 (`region_for_missing_directory_renders_empty`).
2. The `pane.list` call fails or omits our pane: the current stack must be kept, not replaced by the spawn-directory fallback. Pinned in Task 3 (`stack_roots_is_none_when_our_pane_is_missing`).
3. Windows paths with mixed separators or trailing separators for the same directory must collapse to one root. Pinned in Task 2 (`fold_roots_normalizes_separators`).
4. A pane that briefly `cd`s into another root's subtree and back: the group disappears then reappears with its expansion restored and in its old position if still remembered. Pinned in Task 2 (`group_order_keeps_previous_groups_first`) and Task 6 (`stack_diff_reuses_surviving_regions`).
5. A pane too short for even the headers: `allocate` must not underflow and must leave trailing groups with zero body rather than panicking. Pinned in Task 2 (`allocate_survives_fewer_rows_than_headers`).

---

### Task 1: View history

**Files:**
- Create: `src/viewnav.rs`
- Modify: `src/lib.rs` (add `pub mod viewnav;`)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum ViewEntry { Stack, Git(PathBuf) }
  #[derive(Debug, Default)]
  pub struct ViewHistory { /* entries: Vec<ViewEntry>, cursor: usize */ }
  impl ViewHistory {
      pub fn new() -> Self;                       // starts at [Stack], cursor 0
      pub fn current(&self) -> &ViewEntry;
      pub fn visit(&mut self, entry: ViewEntry);  // no-op if == current; else truncate forward, push, cap 50 (drop oldest)
      pub fn back(&mut self) -> Option<&ViewEntry>;
      pub fn forward(&mut self) -> Option<&ViewEntry>;
  }
  pub const HISTORY_CAP: usize = 50;
  ```

- [ ] **Step 1: Write the failing tests** in `src/viewnav.rs` `mod tests`:

```rust
#[test] fn starts_on_the_stack() { assert_eq!(ViewHistory::new().current(), &ViewEntry::Stack); }
#[test] fn visit_pushes_and_back_forward_step() {
    let mut h = ViewHistory::new();
    h.visit(ViewEntry::Git("/r".into()));
    assert_eq!(h.back(), Some(&ViewEntry::Stack));
    assert_eq!(h.back(), None);
    assert_eq!(h.forward(), Some(&ViewEntry::Git("/r".into())));
    assert_eq!(h.forward(), None);
}
#[test] fn visit_after_back_truncates_forward() {
    let mut h = ViewHistory::new();
    h.visit(ViewEntry::Git("/a".into()));
    h.back();
    h.visit(ViewEntry::Git("/b".into()));
    assert_eq!(h.forward(), None);
    assert_eq!(h.back(), Some(&ViewEntry::Stack));
}
#[test] fn visiting_the_current_entry_is_a_noop() {
    let mut h = ViewHistory::new();
    h.visit(ViewEntry::Stack);
    assert_eq!(h.back(), None);
}
#[test] fn history_is_capped_at_fifty() {
    let mut h = ViewHistory::new();
    for i in 0..120 { h.visit(ViewEntry::Git(format!("/r{i}").into())); h.visit(ViewEntry::Stack); }
    let mut steps = 0; while h.back().is_some() { steps += 1; }
    assert_eq!(steps, HISTORY_CAP - 1);
}
```

- [ ] **Step 2: Run** `cargo test viewnav` — expected: compile error, module missing.
- [ ] **Step 3: Implement** `src/viewnav.rs` per the interface; register in `lib.rs`.
- [ ] **Step 4: Run** `cargo test viewnav` — expected: 5 passed.
- [ ] **Step 5: Commit** `feat(viewnav): in-process view history for stack/git views`

---

### Task 2: Stack decisions (fold, group, allocate, drag)

**Files:**
- Create: `src/stack.rs`
- Modify: `src/lib.rs` (add `pub mod stack;`)

**Interfaces:**
- Produces:
  ```rust
  /// Dedupe and drop any path that has another entry as a proper ancestor. Output sorted.
  /// Normalizes: trailing separators trimmed, `\` → `/` compared on Windows (case-insensitive there).
  pub fn fold_roots(cwds: &[PathBuf]) -> Vec<PathBuf>;

  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct Group { pub key: PathBuf, pub repo: bool, pub roots: Vec<PathBuf> }
  impl Group { pub fn name(&self) -> String; /* last path component of key, "/" for a bare root */ }

  /// `owner(root)` = repository root containing `root`, or None. Groups that appear in
  /// `previous` keep that relative order first; new groups follow sorted by key. Roots within
  /// a group are sorted.
  pub fn group_roots(roots: &[PathBuf], owner: impl Fn(&Path) -> Option<PathBuf>, previous: &[Group]) -> Vec<Group>;

  pub const MIN_BODY_ROWS: u16 = 3;
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct Allocation {
      /// Rows per group INCLUDING its divider (groups after the first) and header.
      pub heights: Vec<u16>,
      /// Groups shown folded, manual folds included.
      pub folded: Vec<bool>,
  }
  /// `folded[i]` = user-folded. `dragged` = previous heights to honor when they still fit
  /// (same length, same total ≤ body_rows, every unfolded body ≥ MIN_BODY_ROWS); else equal split.
  /// Auto-folds from the bottom until every unfolded group has ≥ MIN_BODY_ROWS of body.
  pub fn allocate(folded: &[bool], body_rows: u16, dragged: Option<&[u16]>) -> Allocation;

  /// Move `delta` rows across the boundary ABOVE group `boundary` (1-based: boundary 1 sits
  /// between groups 0 and 1). Never shrinks either unfolded body below MIN_BODY_ROWS.
  pub fn drag(alloc: &mut Allocation, boundary: usize, delta: i32);

  /// Overhead rows of group i: header (1) + divider (1 if i > 0).
  pub fn chrome_rows(index: usize) -> u16;
  ```

- [ ] **Step 1: Write the failing tests** in `src/stack.rs` `mod tests`:

```rust
fn p(s: &str) -> PathBuf { PathBuf::from(s) }
#[test] fn fold_roots_collapses_duplicates_and_nested_dirs() {
    let out = fold_roots(&[p("/w/proj/src"), p("/w/proj"), p("/w/proj"), p("/notes")]);
    assert_eq!(out, vec![p("/notes"), p("/w/proj")]);
}
#[test] fn fold_roots_normalizes_separators() {
    let out = fold_roots(&[p("/w/proj/"), p("/w/proj")]);
    assert_eq!(out, vec![p("/w/proj")]);
    #[cfg(windows)] {
        let out = fold_roots(&[p(r"C:\W\Proj"), p("C:/w/proj/")]);
        assert_eq!(out.len(), 1);
    }
}
#[test] fn group_roots_groups_by_owner_and_sorts_roots() {
    let owner = |r: &Path| r.starts_with("/w/mono").then(|| p("/w/mono"));
    let g = group_roots(&[p("/w/mono/apps/web"), p("/notes"), p("/w/mono/apps/api")], owner, &[]);
    assert_eq!(g, vec![
        Group { key: p("/notes"), repo: false, roots: vec![p("/notes")] },
        Group { key: p("/w/mono"), repo: true, roots: vec![p("/w/mono/apps/api"), p("/w/mono/apps/web")] },
    ]);
    assert_eq!(g[1].name(), "mono");
}
#[test] fn group_roots_treats_missing_owner_as_plain_folder() {
    let g = group_roots(&[p("/gone")], |_| None, &[]);
    assert_eq!(g, vec![Group { key: p("/gone"), repo: false, roots: vec![p("/gone")] }]);
}
#[test] fn group_order_keeps_previous_groups_first() {
    let prev = vec![Group { key: p("/z"), repo: false, roots: vec![p("/z")] }];
    let g = group_roots(&[p("/a"), p("/z")], |_| None, &prev);
    assert_eq!(g.iter().map(|g| g.key.clone()).collect::<Vec<_>>(), vec![p("/z"), p("/a")]);
}
#[test] fn allocate_splits_equally_with_chrome() {
    // 2 groups, 20 rows: chrome 1 + 2 = 3, bodies 17 → 9 and 8.
    let a = allocate(&[false, false], 20, None);
    assert_eq!(a.heights, vec![10, 10]);
    assert_eq!(a.folded, vec![false, false]);
}
#[test] fn allocate_folds_from_the_bottom_when_short() {
    // 3 groups in 12 rows: chrome 1+2+2 = 5, bodies 7 < 3*3 → fold the last; 2 unfolded get 7 → 4 and 3.
    let a = allocate(&[false, false, false], 12, None);
    assert_eq!(a.folded, vec![false, false, true]);
    assert_eq!(a.heights, vec![5, 5, 2]);
}
#[test] fn allocate_gives_manual_folds_only_their_header() {
    let a = allocate(&[true, false], 10, None);
    assert_eq!(a.heights, vec![1, 9]);
}
#[test] fn allocate_honors_dragged_heights_that_still_fit() {
    let a = allocate(&[false, false], 20, Some(&[6, 14]));
    assert_eq!(a.heights, vec![6, 14]);
    let a = allocate(&[false, false], 20, Some(&[3, 17])); // body of group 0 would be 2
    assert_eq!(a.heights, vec![10, 10]);
}
#[test] fn allocate_survives_fewer_rows_than_headers() {
    let a = allocate(&[false; 5], 3, None);
    assert_eq!(a.heights.iter().sum::<u16>(), 3);
    assert!(a.folded.iter().all(|f| *f));
}
#[test] fn drag_moves_rows_and_respects_minimums() {
    let mut a = allocate(&[false, false], 20, None);
    drag(&mut a, 1, 3);
    assert_eq!(a.heights, vec![13, 7]);
    drag(&mut a, 1, 50);
    assert_eq!(a.heights, vec![15, 5]); // group 1 keeps divider+header+3
    drag(&mut a, 1, -50);
    assert_eq!(a.heights, vec![4, 16]); // group 0 keeps header+3
}
```

- [ ] **Step 2: Run** `cargo test stack::` — expected: compile error.
- [ ] **Step 3: Implement** `src/stack.rs`. `fold_roots`: normalize each path to a comparison string (trim trailing `/`/`\`, replace `\` with `/`, lowercase on Windows), dedupe, drop entries where another normalized entry is a proper prefix ending at a `/`. `allocate`: compute chrome per group; unfolded count n; if `body_rows < Σchrome` give each group `min(remaining, chrome)` in order and fold all; else distribute `body_rows − Σchrome` over unfolded groups (first groups get the remainder); while the share < MIN_BODY_ROWS and n > 0, fold the last unfolded group and recompute. With `dragged` matching length, same folded pattern, Σ == body_rows and every unfolded body ≥ MIN, return it unchanged.
- [ ] **Step 4: Run** `cargo test stack::` — expected: 11 passed.
- [ ] **Step 5: Commit** `feat(stack): root folding, repo grouping and region allocation`

---

### Task 3: Sampling the tab's panes

**Files:**
- Modify: `src/launch.rs` (`SiblingCwd` and a tab-scoped sampler become `pub(crate)`)
- Modify: `src/stack.rs`

**Interfaces:**
- Consumes: `launch::sibling_cwds` filter rules, `viewer::TOKEN_ORIGIN_TAB` (`"hs-preview-origin-tab"`), `launch::tab_of`, `launch::live_tabs`.
- Produces:
  ```rust
  // launch.rs
  pub(crate) struct SiblingCwd { pub pane_id: String, pub cwd: String, pub focused: bool }
  /// Eligible panes in `tab_id` (the same filter as `sibling_cwds`, minus "not me").
  pub(crate) fn sibling_cwds_in_tab(pane_list_json: &str, tab_id: &str) -> Vec<SiblingCwd>;

  // stack.rs
  /// The tab whose panes feed the stack: the origin tab recorded on a preview viewer in
  /// our tab when that tab still exists, else our own tab. None if our pane is not listed.
  pub fn stack_tab(pane_list_json: &str, my_pane_id: &str) -> Option<String>;
  /// Folded roots of the eligible panes in `stack_tab`. None when the snapshot cannot be
  /// read or does not list our pane; Some(empty) when the tab has no eligible pane.
  pub fn stack_roots(pane_list_json: &str, my_pane_id: &str) -> Option<Vec<PathBuf>>;
  ```

- [ ] **Step 1: Write the failing tests** in `src/stack.rs` `mod tests` (build `pane.list` JSON the way `launch.rs` tests do: `{"result":{"panes":[{"pane_id":..,"tab_id":..,"foreground_cwd":..,"focused":..,"label":..,"tokens":{..}}]}}`):

```rust
#[test] fn stack_roots_folds_the_tabs_live_cwds() {
    let json = panes(&[("s","t1",None,"Sidebar",&[("herdr-sidebar-explorer","1")]),
                       ("a","t1",Some("/w/proj"),"",&[]), ("b","t1",Some("/w/proj/src"),"",&[]),
                       ("c","t2",Some("/elsewhere"),"",&[])]);
    assert_eq!(stack_roots(&json, "s"), Some(vec![p("/w/proj")]));
}
#[test] fn stack_roots_is_none_when_our_pane_is_missing() {
    assert_eq!(stack_roots("{\"result\":{\"panes\":[]}}", "s"), None);
    assert_eq!(stack_roots("not json", "s"), None);
}
#[test] fn stack_roots_is_empty_for_a_tab_with_only_plugin_panes() {
    let json = panes(&[("s","t1",None,"Sidebar",&[("herdr-sidebar-explorer","1")])]);
    assert_eq!(stack_roots(&json, "s"), Some(vec![]));
}
#[test] fn preview_tab_samples_its_origin_tab() {
    let json = panes(&[("s","t9",None,"Sidebar",&[("herdr-sidebar-explorer","1")]),
                       ("v","t9",None,"a.rs · preview",&[("herdr-sidebar-preview","1"),("hs-preview-origin-tab","t1")]),
                       ("a","t1",Some("/w/proj"),"",&[])]);
    assert_eq!(stack_tab(&json, "s"), Some("t1".into()));
    assert_eq!(stack_roots(&json, "s"), Some(vec![p("/w/proj")]));
}
#[test] fn preview_tab_with_dead_origin_samples_itself() {
    let json = panes(&[("s","t9",None,"Sidebar",&[("herdr-sidebar-explorer","1")]),
                       ("v","t9",None,"a.rs · preview",&[("herdr-sidebar-preview","1"),("hs-preview-origin-tab","gone")])]);
    assert_eq!(stack_tab(&json, "s"), Some("t9".into()));
}
```

- [ ] **Step 2: Run** `cargo test stack::` — expected: compile errors for the new functions.
- [ ] **Step 3: Implement.** In `launch.rs`, split `sibling_cwds` into `sibling_cwds_in_tab` (the filter closure) plus the "not me" exclusion; keep `sibling_cwds` behaviour and its tests intact. In `stack.rs`, implement `stack_tab` and `stack_roots` (`fold_roots` over the cwds).
- [ ] **Step 4: Run** `cargo test` — expected: all pass, including existing `launch` follower tests.
- [ ] **Step 5: Commit** `feat(stack): sample a tab's eligible pane cwds, honoring preview origin tabs`

---

### Task 4: Remove single-tree machinery; persist the active repo; start-view resolution

This is the deletion commit. After it the sidebar still shows one tree (the spawn directory) and everything compiles; the stack arrives in Tasks 5–8.

**Files:**
- Modify: `src/state.rs` — `State`: remove `follow_cwd`; add `pub active_repo: Option<PathBuf>` (serde default, `None`); drop `Copy` from `State` (keep `Clone`) and fix call sites with `.clone()`. Remove `follow_cwd_setting_value`, `load_root`, `save_root`, the `roots.json` path helper and their tests (`remembered_roots_are_keyed_by_workspace_and_project`). Add a parse test.
- Modify: `src/main.rs` — remove `remembered_root_key`, `resolve_root`, `remembered_root` tracking, `root_key`/`legacy_workspace_label` parameters and `remembered_root_keys_are_project_stable_not_tab_scoped`. Add `startup_view`. Create `Rc<RefCell<viewnav::ViewHistory>>` and pass it to both apps. `Exit::Switch` → `view = state::load_state().active`.
- Modify: `src/explorer_app.rs` — remove `Setting::FollowCwd`, `Setting::Folder`, `PromptKind::ChangeFolder`, `picking`, `poll_picker`, `change_folder_dialog`, `change_folder_prompt`, `change_folder`, `change_folder_impl`, `follow_sibling_cwd`, the `c` key, the `("c","folder")` hint, the `cwd_follower` field; `App::new(root: PathBuf, history: Rc<RefCell<ViewHistory>>)`.
- Modify: `src/scm_app.rs` — remove `Setting::FollowCwd`, `Setting::Folder`, `picking`, `poll_picker`, `change_folder_dialog`; `follow_sibling_cwd` returns early when `self.merged()` (no setting gate anymore); `App::new(cwd: PathBuf, cwd_follower: Rc<RefCell<CwdFollower>>, history: Rc<RefCell<ViewHistory>>)`.
- Modify: `src/actions.rs` — remove `MenuAction::ChangeFolder`, `MenuAction::ChangeFolderTyped`, their menu entries, `pick_folder` and its parser tests; `Cargo.toml` — remove the `rfd` dependency.
- Modify: `src/launch.rs` — remove `CwdFollower::mark_manual_folder`, the `manual_override` branch and `manual_folder_wins_until_an_observed_pane_moves`.
- Modify: `AGENTS.md` (repo root) — replace the "Following the neighbour pane's folder" section with a pointer to the spec; remove the `roots.json` bullet and the `c`/Change Folder bullet under "Unified sidebar".

**Interfaces:**
- Produces:
  ```rust
  // main.rs
  /// (view to start in, repo for a git view). Source Control is entered only when the
  /// unified sidebar is on and `repo` is one of `group_keys`; `Target::SourceControl` with
  /// no persisted repo falls back to `spawn_owner`.
  fn startup_view(persisted: &State, pinned: Option<View>, initial: Option<ensure::Target>,
                  spawn_owner: Option<PathBuf>, group_keys: &[PathBuf]) -> (View, Option<PathBuf>);
  ```
  `main` computes `group_keys` by `stack::stack_roots(pane.list, HERDR_PANE_ID)` → `stack::group_roots(.., |r| Git::owner_of(r).ok().map(|g| g.root().to_path_buf()), &[])`, with `[spawn_cwd]` when the sample is `Some(empty)` and no keys when it is `None` or there is no pane id. When the result is `(SourceControl, Some(repo))`, `history.visit(ViewEntry::Git(repo.clone()))` before the loop, and `run_scm` is given `repo` as its cwd.

- [ ] **Step 1: Write the failing tests**

In `src/state.rs` tests:
```rust
#[test] fn active_repo_round_trips_and_defaults_to_none() {
    assert_eq!(parse_state("{}").active_repo, None);
    let s = parse_state(r#"{"active":"SourceControl","active_repo":"/w/mono"}"#);
    assert_eq!(s.active_repo, Some(PathBuf::from("/w/mono")));
}
```
In `src/main.rs` tests (unit, no IPC):
```rust
#[test] fn startup_enters_git_view_only_for_a_repo_in_the_stack() {
    let mut s = State::default(); s.merged = true; s.active = View::SourceControl; s.active_repo = Some("/w/mono".into());
    assert_eq!(startup_view(&s, None, None, None, &["/w/mono".into()]), (View::SourceControl, Some("/w/mono".into())));
    assert_eq!(startup_view(&s, None, None, None, &["/other".into()]), (View::Explorer, None));
    s.active_repo = None;
    assert_eq!(startup_view(&s, None, None, None, &["/w/mono".into()]), (View::Explorer, None));
}
#[test] fn show_git_action_uses_the_spawn_owner_when_nothing_is_remembered() {
    let mut s = State::default(); s.merged = true;
    assert_eq!(startup_view(&s, None, Some(ensure::Target::SourceControl), Some("/w/x".into()), &["/w/x".into()]),
               (View::SourceControl, Some("/w/x".into())));
}
#[test] fn pinned_panes_ignore_persisted_view() {
    let mut s = State::default(); s.active = View::SourceControl; s.active_repo = Some("/r".into());
    assert_eq!(startup_view(&s, Some(View::Explorer), None, None, &["/r".into()]), (View::Explorer, None));
    assert_eq!(startup_view(&s, Some(View::SourceControl), None, None, &[]), (View::SourceControl, None));
}
```

- [ ] **Step 2: Run** `cargo test startup_view active_repo` — expected: compile errors.
- [ ] **Step 3: Make the deletions and additions listed under Files.** Note: `View::SourceControl` pinned (separated SCM pane) returns `(SourceControl, None)` so `run_scm` uses the spawn cwd and follows focus. Explorer still constructs one `Tree` at `spawn_cwd` in this task.
- [ ] **Step 4: Run** `cargo test` and `cargo clippy -- -D warnings` — expected: pass; grep confirms no `follow_cwd`, `ChangeFolder`, `load_root`, `rfd` remain: `rg -n 'follow_cwd|ChangeFolder|load_root|save_root|pick_folder|rfd' src Cargo.toml` prints nothing.
- [ ] **Step 5: Commit** `refactor: drop single-tree roots, cwd-follow setting and Change Folder; persist active repo`

---

### Task 5: Extract `Region` (behaviour-preserving)

**Files:**
- Modify: `src/explorer_app.rs`

**Interfaces:**
- Produces (private to `explorer_app.rs`):
  ```rust
  struct Region {
      group: stack::Group,
      trees: Vec<Tree>,            // one per group.roots, same order
      rows: Vec<Row>,
      selected: Option<usize>,
      scroll: usize,
      snap: bool,
      hovered: Option<usize>,
      body: BodyGeom,
      /// Roots of a multi-root group whose root row is collapsed.
      collapsed_roots: BTreeSet<PathBuf>,
  }
  impl Region {
      fn new(group: stack::Group, show_hidden: bool) -> Self;  // loads tree.json expansion + selection per root
      fn rebuild(&mut self);                                   // see Task 6 for multi-root rows
      fn tree_for(&self, path: &Path) -> Option<usize>;         // longest root prefix
      fn root_of(&self, path: &Path) -> Option<PathBuf>;
      fn persist(&self);                                       // save_tree_state per root; selected only for its root
      fn sync_shared(&mut self) -> bool;                        // set_expanded per root from tree.json, re-select by path
      fn drain_events(&mut self) -> bool;                       // any tree
      fn refresh(&mut self);                                    // every tree
      fn collapse_all(&mut self);
      fn show_hidden(&self) -> bool; fn set_show_hidden(&mut self, on: bool);
  }
  impl App {
      fn region(&self) -> &Region; fn region_mut(&mut self) -> &mut Region;   // self.regions[self.focused]
      fn all_roots(&self) -> Vec<PathBuf>;
      fn root_of(&self, path: &Path) -> PathBuf;                 // owning root across regions, else spawn_cwd
  }
  ```
  `App` fields: `regions: Vec<Region>`, `focused: usize`, `spawn_cwd: PathBuf`, `folded: BTreeSet<PathBuf>`, `heights: Option<Vec<u16>>`, `dragging: Option<usize>`, `group_zones: Vec<GroupZones>`, `repo_status: HashMap<PathBuf, Status>`, `last_stack: Instant`. Fields `tree`, `rows`, `selected`, `scroll`, `snap`, `hovered`, `body`, `git_footer_status` are removed (footer status becomes `repo_status.get(&self.region().group.key)`).

- [ ] **Step 1: Write the failing test** (temp dirs via `std::env::temp_dir()` + unique name, as other tests in this file do):

```rust
#[test] fn single_root_region_rows_match_the_tree() {
    let dir = scratch_dir("single-root"); std::fs::write(dir.join("a.txt"), "").unwrap();
    let group = stack::Group { key: dir.clone(), repo: false, roots: vec![dir.clone()] };
    let region = Region::new(group, false);
    assert_eq!(region.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["a.txt"]);
    assert_eq!(region.tree_for(&dir.join("a.txt")), Some(0));
}
```

- [ ] **Step 2: Run** `cargo test single_root_region` — expected: compile error.
- [ ] **Step 3: Refactor.** `App::new(spawn_cwd, history)` builds `regions = vec![Region::new(Group{key: spawn_cwd, repo: Git::owner_of(..).is_ok(), roots: vec![spawn_cwd]}, false)]`. Every former `self.tree` / `self.rows` / `self.selected` / `self.scroll` / `self.snap` / `self.hovered` / `self.body` use goes through `region()`/`region_mut()`; `persist_tree` → `region().persist()`; `sync_shared_tree` → loop over regions; `tick`'s `drain_events` → any region; `refresh_tree` → focused region; `.` → every region's `set_show_hidden`; `rediscover_repos` → `Git::discover_all` over `all_roots()` deduped by root; `collect_decorations` fills `repo_status` from each refreshed repo's status; `create_target_dir`, `CopyRelativePath`, `resolve_rule`/`reload_rules`, `open_preview_at` use `self.root_of(path)`; `draw_header` prints `EXPLORER` instead of the root name. Drawing still renders only `regions[0]` into the body.
- [ ] **Step 4: Run** `cargo test` and `cargo clippy -- -D warnings` — expected: pass; all existing explorer tests unchanged.
- [ ] **Step 5: Commit** `refactor(explorer): hold the tree in a Region in preparation for stacking`

---

### Task 6: Multi-root regions and live stack sampling

**Files:**
- Modify: `src/explorer_app.rs`

**Interfaces:**
- Consumes: `stack::stack_roots`, `stack::group_roots`, Task 5's `Region`.
- Produces:
  ```rust
  impl Region {
      /// Multi-root rows: for each root in order, a root row
      /// `Row { path: root, name: <root relative to group.key, '/'-joined>, is_dir: true,
      ///        is_symlink: false, depth: 0, expanded: !collapsed_roots.contains(root) }`
      /// followed (unless collapsed) by that tree's rows with depth + 1. Single-root: tree rows as-is.
      fn rebuild(&mut self);
      fn is_root_row(&self, index: usize) -> bool;
  }
  impl App {
      /// Called from `heartbeat` every 5 s: sample, group, diff into `regions`.
      fn sample_stack(&mut self);
      /// Pure diff: keep regions whose group key survives (updating their roots), drop the
      /// rest, create new ones; returns true when the group set or any root list changed.
      fn apply_groups(&mut self, groups: Vec<stack::Group>) -> bool;
  }
  ```
  Rules: `stack_roots` → `None` keeps the current stack; `Some(empty)` → `[spawn_cwd]`. `group_roots(roots, owner, &current_groups)` with `owner = |r| self.owner_cache.entry(r).or_insert_with(|| Git::owner_of(r).ok().map(root))` (`owner_cache: HashMap<PathBuf, Option<PathBuf>>`, entries dropped for roots that leave the stack). On a change: `heights = None`, `focused = focused.min(regions.len()-1)`, `invalidate_quick_index()`, `rediscover_repos()`, `request_decorations(true)`. Root rows: `toggle`/`expand_or_enter`/`collapse_or_parent` on a root row flip `collapsed_roots`; `tree_for` on a root row is that root.

- [ ] **Step 1: Write the failing tests**:

```rust
#[test] fn multi_root_region_shows_root_rows_with_relative_names() {
    let base = scratch_dir("multi-root");
    for sub in ["apps/web", "apps/api"] { std::fs::create_dir_all(base.join(sub).join("src")).unwrap(); }
    let group = stack::Group { key: base.clone(), repo: true, roots: vec![base.join("apps/api"), base.join("apps/web")] };
    let region = Region::new(group, false);
    let names: Vec<_> = region.rows.iter().map(|r| (r.name.as_str(), r.depth)).collect();
    assert_eq!(names, vec![("apps/api", 0), ("src", 1), ("apps/web", 0), ("src", 1)]);
    assert!(region.is_root_row(0) && !region.is_root_row(1));
    assert_eq!(region.tree_for(&base.join("apps/web/src")), Some(1));
}
#[test] fn collapsing_a_root_row_hides_its_tree() {
    let base = scratch_dir("multi-root-fold");
    for sub in ["a", "b"] { std::fs::create_dir_all(base.join(sub).join("x")).unwrap(); }
    let mut region = Region::new(stack::Group { key: base.clone(), repo: false, roots: vec![base.join("a"), base.join("b")] }, false);
    region.collapsed_roots.insert(base.join("a")); region.rebuild();
    assert_eq!(region.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["a", "b", "x"]);
    assert!(!region.rows[0].expanded);
}
#[test] fn region_for_missing_directory_renders_empty() {
    let gone = scratch_dir("gone").join("nope");
    let region = Region::new(stack::Group { key: gone.clone(), repo: false, roots: vec![gone] }, false);
    assert!(region.rows.is_empty());
}
#[test] fn stack_diff_reuses_surviving_regions() {
    let a = scratch_dir("diff-a"); let b = scratch_dir("diff-b");
    let mut app = App::new(a.clone(), Rc::new(RefCell::new(ViewHistory::new())));
    app.region_mut().selected = Some(0);
    let g = |k: &PathBuf| stack::Group { key: k.clone(), repo: false, roots: vec![k.clone()] };
    assert!(app.apply_groups(vec![g(&a), g(&b)]));
    assert_eq!(app.regions.len(), 2);
    assert_eq!(app.regions[0].selected, Some(0));          // survivor untouched
    assert!(!app.apply_groups(vec![g(&a), g(&b)]));        // no change → false
    assert!(app.apply_groups(vec![g(&b)]));
    assert_eq!(app.regions[0].group.key, b);
    assert_eq!(app.focused, 0);
}
```
`App::new` in tests runs without a pane (`HERDR_PANE_ID` unset) — `PaneCtl::from_env()` is `None` then, as in `focused_pane_detection_is_scoped_to_our_pane_id`'s environment; point `HERDR_PLUGIN_STATE_DIR` at a scratch dir in these tests so `tree.json`/`state.json` writes stay out of the real state (see AGENTS.md "Testing hooks headless"), and set `HERDR_SIDEBAR_ICONS=emoji` so `IconTheme::resolve` skips the font probe. Guard env edits with one test-local `Mutex` shared by every test in this file that constructs an `App`.

- [ ] **Step 2: Run** `cargo test multi_root stack_diff region_for_missing` — expected: fail/compile error.
- [ ] **Step 3: Implement** per the interface; wire `sample_stack` into `heartbeat` after `report_tokens` (replacing the removed `follow_sibling_cwd` call site), skipped while an overlay other than `ContentSearch` is open.
- [ ] **Step 4: Run** `cargo test` and clippy — expected: pass.
- [ ] **Step 5: Commit** `feat(explorer): regions follow the tab's panes; multi-root groups`

---

### Task 7: Stacked rendering, region focus, fold and drag

**Files:**
- Modify: `src/explorer_app.rs`
- Modify: `src/ui.rs` (one new palette-free helper; no new colors outside `Palette`)
- Modify: `src/icons.rs` only if a chevron/divider glyph per theme is needed (text fallback `▾`/`▸`, `─`)

**Interfaces:**
- Consumes: `stack::allocate`, `stack::drag`, `stack::chrome_rows`, `ui::hits`, `ui::activity_button_style`, `branch_ui::draw_git_footer`.
- Produces:
  ```rust
  #[derive(Clone, Copy, Default)]
  struct GroupZones { divider: Rect, header: Rect, chevron: Rect, git: Rect }  // Rect::default() = absent
  impl App {
      fn region_at(&self, y: u16) -> Option<usize>;             // by each region's body geometry
      fn group_at(&self, x: u16, y: u16) -> Option<(usize, GroupPart)>;
      fn focus_region(&mut self, index: usize);
      fn toggle_fold(&mut self, index: usize);                  // flips `folded` for that group key
      fn draw_stack(&mut self, frame: &mut Frame, area: Rect);   // allocate → per group: divider, header, body (+scrollbar)
      fn draw_group_header(&mut self, frame: &mut Frame, area: Rect, index: usize, folded: bool);
  }
  enum GroupPart { Divider, Chevron, Header, Git }
  /// Pure: which group's divider/header/git button a click lands on, from the zones list.
  fn group_part_at(zones: &[GroupZones], x: u16, y: u16) -> Option<(usize, GroupPart)>;
  ```
  Header content: ` {▾|▸} {group.name()}` bold `header_accent`; for a repo group, ` ⎇ {branch}{*}` dim from `repo_status` (`*` when `staged`+`unstaged` non-empty); for a repo group in `merged()` mode, a right-aligned chip ` {git_icon} ` (the third glyph of `ui::activity_icons`, styled with `activity_button_style(false, hovered)`) whose rect is `zones.git`. Separated mode draws no chip. The focused region's header name uses `palette().selection_fg`-free bold; unfocused regions dim their selection with the existing `selection_unfocused` colours (same rule as the title: only the focused region shows a *focused* selection). Divider: a row of `─` in `Color::DarkGray` replaced by the palette's existing dim colour. Fold state: `folded` (manual) ∪ `Allocation::folded` (auto); a manual fold on an auto-folded group is a no-op.

  Input: `Tab`/`BackTab` cycle `focused` (tree view only, no overlay). Mouse: left press on `Chevron`/`Header` → `toggle_fold`; on `Divider` → `dragging = Some(index)` and `heights = Some(allocation.heights)`; `Drag(Left)` with `dragging` → `stack::drag(&mut alloc, index, dy)` where dy = current row − divider row, then `heights = Some(alloc.heights)`; `Up(Left)` → `dragging = None`. Wheel scrolls the region under the cursor (`region_at`), without focusing it. A left press on a row in another region focuses it first, then runs the existing row logic. `title_action`, `m`, `r`, `.` keep acting on the focused region. `page` = focused region's body height − 1.

- [ ] **Step 1: Write the failing tests** (pure helpers):

```rust
#[test] fn group_parts_hit_test_from_zones() {
    let z = GroupZones { divider: Rect::new(0, 5, 30, 1), header: Rect::new(0, 6, 30, 1), chevron: Rect::new(0, 6, 3, 1), git: Rect::new(27, 6, 3, 1) };
    assert_eq!(group_part_at(&[z], 10, 5), Some((0, GroupPart::Divider)));
    assert_eq!(group_part_at(&[z], 1, 6), Some((0, GroupPart::Chevron)));
    assert_eq!(group_part_at(&[z], 28, 6), Some((0, GroupPart::Git)));
    assert_eq!(group_part_at(&[z], 10, 6), Some((0, GroupPart::Header)));
    assert_eq!(group_part_at(&[z], 10, 7), None);
}
#[test] fn region_bodies_follow_the_allocation() {
    // Two scratch regions; the assertions are relative so the exact body height does not matter.
    let a = scratch_dir("geom-a"); let b = scratch_dir("geom-b");
    let mut app = App::new(a.clone(), Rc::new(RefCell::new(ViewHistory::new())));
    app.apply_groups(vec![/* g(a), g(b) as in Task 6 */]);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 30)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    assert_eq!(app.regions[0].body.top, app.group_zones[0].header.y + 1);
    assert_eq!(app.group_zones[1].divider.y, app.regions[0].body.top + app.regions[0].body.height);
    assert_eq!(app.region_at(app.regions[1].body.top), Some(1));
}
#[test] fn fold_gives_the_space_to_the_other_group() {
    /* same setup */ app.toggle_fold(0); term.draw(|f| app.draw(f)).unwrap();
    assert_eq!(app.regions[0].body.height, 0);
    assert!(app.regions[1].body.height > 15);
}
```
(`TestBackend` is a ratatui feature available by default; `App::draw` must not need a pane.)

- [ ] **Step 2: Run** `cargo test group_parts region_bodies fold_gives` — expected: compile errors.
- [ ] **Step 3: Implement** `draw_stack` replacing the single-region body rendering in `draw`; move the per-region list rendering (clamp scroll, snap, `row_item`, `draw_scrollbar`) into a `draw_region_body(&mut self, frame, area, index)`; selection style for an unfocused region uses the existing unfocused selection colours from `Palette`. Add the mouse/key handling described above.
- [ ] **Step 4: Run** `cargo test` and clippy — expected: pass. Then a live check in `hs-test` (see AGENTS.md "Testing hooks headless"): open two shells in different directories on one tab, confirm two groups appear within ~5 s, close one, confirm it disappears; drag the divider with `pane.send_input` SGR sequences `ESC[<0;x;yM` (press on divider), `ESC[<32;x;y+3M` (drag), `ESC[<0;x;y+3m` (release).
- [ ] **Step 5: Commit** `feat(explorer): stacked group regions with headers, fold and draggable dividers`

---

### Task 8: Git view entry and back/forward

**Files:**
- Modify: `src/explorer_app.rs`, `src/scm_app.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `viewnav::{ViewHistory, ViewEntry}`, `tabnav::{key_direction, mouse_direction, step_notice}`, Task 7's `GroupPart::Git`.
- Produces:
  ```rust
  // explorer_app.rs
  impl App {
      /// `3` / Ctrl+3 / F11 / the group's ⎇: enter the git view of `regions[index]`'s repo.
      /// Non-repo group → notice "no repository in this group". Separated mode → None.
      fn enter_git(&mut self, index: usize) -> Option<Exit>;
      /// Ctrl+- / F7 / F8 / mouse side buttons: step the view history, else tab history.
      fn history_step(&mut self, forward: bool) -> Option<Exit>;
  }
  // scm_app.rs
  impl App { fn history_step(&mut self, forward: bool) -> Option<Exit>; }
  ```
  `enter_git`: `update_state(|s| { s.active = View::SourceControl; s.active_repo = Some(key.clone()); s.search_active = false })`, `history.visit(ViewEntry::Git(key))`, `Some(Exit::Switch)`. Explorer `history_step`: `Git(root)` → set `active`/`active_repo` as above WITHOUT `visit`, return `Exit::Switch`; `Stack` → `None` (already here); `None` → `self.notice = tabnav::step_notice(forward)`, `None`. SCM `history_step`: `Stack` → `update_state(active = Explorer)`, `Exit::Switch`; `Git(root)` equal to `self.cwd` → `None`; other root → set `active_repo`, `Exit::Switch`; end of history → `tabnav::step_notice` into `self.flash`. SCM `switch_to(View::Explorer)` and `open_search` call `history.visit(ViewEntry::Stack)`. SCM `3`/F11 and `switch_to(View::SourceControl)` are no-ops. Both activity bars drop the third chip (explorer: delete `ActivityZones::source_control` and its click branch; SCM: nothing is active). SCM header when `merged()`: `format!(" ▾ Source Control · {}", first repo name)` (the repo entered); right text unchanged. `main`: `run_scm` cwd = `load_state().active_repo` when merged and `is_dir()`, else spawn cwd; `Exit::Switch` → `view = load_state().active`.

- [ ] **Step 1: Write the failing tests**:

```rust
// explorer_app.rs
#[test] fn entering_a_repo_group_records_it_and_switches() {
    let repo = scratch_repo("enter-git");   // `git init` a scratch dir
    let h = Rc::new(RefCell::new(ViewHistory::new()));
    let mut app = App::new(repo.clone(), Rc::clone(&h));
    app.sidebar_state.merged = true; app.other_exe = Some(std::env::current_exe().unwrap());
    assert_eq!(app.enter_git(0), Some(Exit::Switch));
    assert_eq!(h.borrow().current(), &ViewEntry::Git(app.regions[0].group.key.clone()));
    assert_eq!(sidebar::load_state().active_repo, Some(app.regions[0].group.key.clone()));
}
#[test] fn entering_a_plain_folder_group_notices_instead() {
    let dir = scratch_dir("enter-plain");
    let mut app = App::new(dir, Rc::new(RefCell::new(ViewHistory::new())));
    app.sidebar_state.merged = true; app.other_exe = Some(std::env::current_exe().unwrap());
    assert_eq!(app.enter_git(0), None);
    assert_eq!(app.notice.as_deref(), Some("no repository in this group"));
}
#[test] fn back_from_the_stack_falls_through_to_tab_history() {
    let dir = scratch_dir("back-stack");
    let mut app = App::new(dir, Rc::new(RefCell::new(ViewHistory::new())));
    assert_eq!(app.history_step(false), None);
    assert!(app.notice.is_some());   // tabnav's "no earlier tab"-style notice
}
#[test] fn forward_reenters_the_git_view() {
    let h = Rc::new(RefCell::new(ViewHistory::new()));
    h.borrow_mut().visit(ViewEntry::Git("/r".into())); h.borrow_mut().back();
    let mut app = App::new(scratch_dir("fwd"), Rc::clone(&h));
    assert_eq!(app.history_step(true), Some(Exit::Switch));
    assert_eq!(sidebar::load_state().active, View::SourceControl);
    assert_eq!(sidebar::load_state().active_repo, Some("/r".into()));
}
```
These tests set `HERDR_PLUGIN_STATE_DIR` to a scratch dir (serialize with a test-local mutex, as the existing state tests do) so `update_state` writes there.

- [ ] **Step 2: Run** `cargo test entering_a forward_reenters back_from_the_stack` — expected: compile errors.
- [ ] **Step 3: Implement** as described; wire `GroupPart::Git` clicks and the `3`/Ctrl+3/F11 paths to `enter_git(self.focused)` (click uses the clicked group's index); replace both apps' direct `tabnav::step_notice` calls in `on_key`/`on_mouse` with `history_step`; update `hints()` so `("3","git")` reads `("3","repo git")`.
- [ ] **Step 4: Run** `cargo test` and clippy — expected: pass. Live check in `hs-test`: click a group's ⎇ → Source Control header reads `Source Control · <repo>`; send `ESC[<128;5;5M` (mouse back) → stack returns; `ESC[<129;5;5M` → git view returns; from the stack with nothing further back, Ctrl+- (byte 0x1F via `pane.send_input`) steps tab history.
- [ ] **Step 5: Commit** `feat: per-repo git view entry with view history back/forward`

---

### Task 9: Search, Quick Open, previews and footer across the stack

**Files:**
- Modify: `src/explorer_app.rs`
- Modify: `AGENTS.md` (repo root): add a "Stacked pane trees" subsection under "Unified sidebar" recording the invariants (sampling cadence, `stack_roots` None-vs-empty, group order, allocation and auto-fold, view history layering, `active_repo`).

**Interfaces:**
- Consumes: `collect_quick_files(root, show_hidden, limit)`, `collect_content_hits(root, ..)`, `App::all_roots`, `App::root_of`.
- Produces:
  ```rust
  /// Label prefix for a hit/file under `root`: "" with one group, else "<group name>/".
  fn stack_label_prefix(groups: &[stack::Group], root: &Path) -> String;
  /// Walk every root in order on the worker; the shared `limit` is what is left after
  /// earlier roots; `truncated` if any root truncated or the limit ran out.
  fn collect_quick_files_in(roots: &[(PathBuf, String)], show_hidden: bool, limit: usize) -> (Vec<QuickFile>, bool);
  fn collect_content_hits_in(roots: &[(PathBuf, String)], show_hidden: bool, query: &str, include: &str,
                             exclude: &str, options: SearchOptions, limit: usize) -> Result<(Vec<ContentHit>, bool), String>;
  ```
  `QuickIndex.root: PathBuf` → `roots: Vec<PathBuf>`; `ContentSearchResult.root` likewise; cache validity compares the whole list. `open_preview_at` and `resolve_rule` already use `root_of` (Task 5); `programs::workspace_root`'s final fallback is `root_of(path)`. Git footer shows `repo_status[&focused group key]`; `sync_git_footer` syncs that repo.

- [ ] **Step 1: Write the failing tests**:

```rust
#[test] fn labels_are_prefixed_only_with_several_groups() {
    let one = vec![stack::Group { key: "/a".into(), repo: false, roots: vec!["/a".into()] }];
    assert_eq!(stack_label_prefix(&one, Path::new("/a")), "");
    let two = vec![one[0].clone(), stack::Group { key: "/w/mono".into(), repo: true, roots: vec!["/w/mono/apps".into()] }];
    assert_eq!(stack_label_prefix(&two, Path::new("/w/mono/apps")), "mono/");
}
#[test] fn quick_index_spans_every_root_under_one_cap() {
    let a = scratch_dir("qi-a"); let b = scratch_dir("qi-b");
    for (d, n) in [(&a, "one.rs"), (&b, "two.rs")] { std::fs::write(d.join(n), "").unwrap(); }
    let (files, truncated) = collect_quick_files_in(&[(a.clone(), "a/".into()), (b.clone(), "b/".into())], false, 10);
    assert_eq!(files.iter().map(|f| f.label.as_str()).collect::<Vec<_>>(), vec!["a/one.rs", "b/two.rs"]);
    assert!(!truncated);
    let (files, truncated) = collect_quick_files_in(&[(a, "a/".into()), (b, "b/".into())], false, 1);
    assert_eq!(files.len(), 1); assert!(truncated);
}
#[test] fn content_search_merges_hits_from_every_root() {
    let a = scratch_dir("cs-a"); let b = scratch_dir("cs-b");
    std::fs::write(a.join("x.txt"), "needle\n").unwrap(); std::fs::write(b.join("y.txt"), "needle\n").unwrap();
    let (hits, _) = collect_content_hits_in(&[(a, "a/".into()), (b, "b/".into())], false, "needle", "", "", SearchOptions::default(), 100).unwrap();
    assert_eq!(hits.iter().map(|h| h.label.as_str()).collect::<Vec<_>>(), vec!["a/x.txt", "b/y.txt"]);
}
```

- [ ] **Step 2: Run** `cargo test labels_are_prefixed quick_index_spans content_search_merges` — expected: compile errors.
- [ ] **Step 3: Implement**; `open_quick_open` and `start_content_search` pass `all_roots()` zipped with prefixes. Write the AGENTS.md subsection.
- [ ] **Step 4: Run** `cargo test` and clippy — expected: pass. Live check: two groups open, `Ctrl+P` lists files from both with prefixes; `Ctrl+F` finds a string present in both.
- [ ] **Step 5: Commit** `feat(explorer): search, quick open and previews span the stack`

---

## Self-review notes

- Spec coverage: which trees exist → T3/T6; grouping → T2/T6; layout, fold, drag, overflow → T2/T7; region focus → T7; expansion persistence → T5/T6; git view, header, follow-off when merged → T4/T8; separated mode → T4 (`startup_view` pinned case, SCM follow when not merged) and T7 (no chip); search/quick open → T9; back/forward → T1/T8; preview tabs → T3; fresh-process entry → T4; removals → T4; `tab-back`/`tab-forward` actions already send F7/F8 to the focused pane, which T8 routes through `history_step` (`ensure.rs` unchanged).
- Types: `stack::Group`, `Allocation`, `ViewEntry`, `Region`, `GroupZones`, `GroupPart`, `startup_view` are used with the same names and shapes in every task that references them.
- Proportion: the plan fixes names, signatures, values and tests; bodies are left to the implementer except where a rule is numeric.
