# Stacked pane trees

Design for turning the sidebar from a focus view into a tab view: one file tree per
directory that a pane on the sidebar's tab is working in, stacked vertically, grouped by
git repository.

Status: approved design, 2026-10-16. Builder questions were folded in; calls marked
"(my call)" were made by the designer, not the user, and may be revisited.

## Intent

Today the sidebar shows one tree, and cwd-follow re-roots it to whichever pane was last
focused. With several panes in different directories the user's mental model is the union
of all of them, but the sidebar only ever shows one. The stacked view shows them all at
once, appearing and disappearing with the panes themselves, so the user never touches the
sidebar to make it match the tab.

## Goals

- Every eligible pane on the sidebar's own tab contributes its live directory as a tree.
- Trees add and remove themselves as panes open, close, and change directory.
- Trees are grouped by repository; each repository gets exactly one git button and its own
  Source Control view, run from that repository.
- Explorer, Search, and the gear stay single, in their current positions.
- Search covers every tree at once.
- Back/forward steps between the stack and a repository's git view.

## Non-goals

- Keeping the single pinned tree as an option. The stacked view replaces it outright.
- Manual roots. Change Folder, the typed-path prompt, and the remembered root per tab are
  removed; roots come from panes only.
- Resizing panes, or anything outside the sidebar pane.
- Changing the Source Control view's internals beyond how it is entered and rooted.

## Anti-goals

- A sidebar that disagrees with the tab: a tree for a pane that is gone, or a missing tree
  for a pane that is there, for longer than one sampling period.
- Git processes multiplying with the number of trees and preview tabs.
- Trees that reset their expansion every time the stack changes.

## Constraints (from the codebase, see AGENTS.md)

- A pane's live directory is `foreground_cwd` from `pane.list`; `cwd` is the spawn
  directory and must not be used as a fallback for following. The existing
  `launch::sibling_cwds` already filters out sidebar, Source Control, and preview panes and
  panes with no live cwd; that filter is the eligibility rule.
- The heartbeat must stamp every event-loop iteration; nothing in the stack refresh may
  block the TUI thread. Git work stays on the existing one-at-a-time background worker.
- `pane.list` order is not meaningful; sort before deciding anything.
- Settings are shared across every sidebar process through `state.json`; tree expansion
  is shared through `tree.json` keyed by root.
- Preview tabs get their own sidebar process from the `tab.created` hook, with the origin
  tab's cwd in `HERDR_SIDEBAR_SPAWN_CWD`.

## Behaviour

### Which trees exist

Sample the tab's eligible panes on the existing ~5s heartbeat (the same sample cwd-follow
takes today). From their live directories, build the set of roots:

1. Collapse: panes with the same directory yield one root.
2. Fold: a directory inside another pane's directory yields no root of its own; it is
   already visible inside the outer tree.

The resulting roots are the trees. A root whose last pane closes removes its tree; a new
pane in a new directory adds one; a pane that `cd`s moves between roots like a close plus
an open. The stack appears at most one sample period after the tab changes.

Zero eligible panes (a tab holding only the sidebar and a viewer) shows the sidebar's
spawn directory as a single tree, so the sidebar is never empty (my call).

### Grouping by repository

Each root's group key is its nearest enclosing git repository, `Git::owner_of(root)`, or
the root itself when it is not in a repository. Roots sharing a repository sit under one
group header, ordered by path. Groups are ordered by first appearance, then by path for
ties (my call: appearance order keeps the stack from reshuffling when a pane opens).

A group header shows:

- a ▾/▸ fold chevron and the repository name (last path component of the repo root), or the
  folder name for a non-repository group;
- for a repository, the branch with a `*` when dirty, from the same background status the
  git footer uses today;
- for a repository, one ⎇ button at the right. Non-repository groups have none.

A group with one root shows the tree directly under the header. A group with several
roots shows each root's repo-relative path as a dim sub-header, then that root's tree,
so `apps/web` and `apps/api` read as two trees under `mono`.

Nested repositories: a pane inside a child repository (submodule, vendored checkout) forms
its own group, because `owner_of` returns the nearest repo. The parent's tree still shows
that directory as a folder; nothing is deduplicated across groups (my call).

### Layout

Trees stack vertically. Each group is its own scroll region with its own selection,
scroll, hover, and scrollbar. A one-row divider sits between groups; dragging it with the
mouse moves the boundary between the two adjacent regions. Folding a group with the
chevron collapses it to its header row and gives its space to the others.

Initial heights split the available rows equally among unfolded groups, with a minimum
of four rows per region (header plus three). Dragged heights are kept while the stack
has the same groups; when a group is added or removed, heights are recomputed as an
equal split (my call: simplest rule that never leaves a region unreachable). Heights are
not persisted.

Overflow: every group header is always visible. When the unfolded groups cannot each get
three body rows, the lowest unfolded groups auto-fold to their header, bottom first, until
the rest fit; unfolding one may therefore fold another. There is no outer scroll axis and
nothing is clipped off the bottom.

Within a region, keyboard navigation, the wheel, snapping, hover, and the right-edge
scrollbar behave exactly as the single tree does today. Keyboard focus moves between
regions with Tab/Shift+Tab, and clicking a row in another region moves focus there (my
call). The focused region is the one the title actions (New File, New Folder, Refresh,
Collapse All) and `m` act on.

Top bar: Explorer and Search chips as today, the gear at the right. The activity bar's
third chip (Source Control) is removed; the per-group ⎇ buttons replace it. `3` and
`Ctrl+3` open the git view of the focused region's repository, or do nothing with a
notice when that group has no repository.

### Expansion and selection persistence

`tree.json` keeps working per root: a tree that disappears and comes back restores its
expanded folders. The selected row is restored only for the focused region. A preview
tab's docked sidebar restores every tree from the same file, so it mirrors the origin tab.

### Git view

Clicking a group's ⎇ (or `3` in its region) swaps the whole sidebar to the Source
Control view for that repository, rooted at the repository root. The header reads
`SOURCE CONTROL · <repo>` so the user knows which one they are in. All existing Source
Control behaviour applies unchanged. The view's cwd-follow is off while entered this way:
the repository was chosen explicitly and must not drift (my call).

In separated mode (unified sidebar off), the pinned Explorer pane shows the stack with no
⎇ buttons, and `3` does nothing there. The separate Source Control pane shows the
repository of the focused eligible pane and follows focus, which is today's behaviour.

A fresh sidebar process (redeploy, ensure-hook re-dock, the `show-git`/`open-git`
actions, a preview tab opened from a git view) needs a repository to enter. `state.json`
keeps `active_repo` (a repository root) next to `active`; a process whose persisted or
requested view is Source Control enters that repository's git view when the repository is
one of its stack's groups, and the `show-git` action with no remembered repo uses the
repository owning the spawn directory. Otherwise it starts at the stack. The history
starts with `Stack` beneath that entry so back still works (my call).

### Search

Search runs the existing content search once per root in the stack, on the same worker,
and merges results into one list grouped by file path. Paths are shown relative to their
root, prefixed with the group name when there is more than one group. Quick Open indexes
every root the same way, with the same 20,000-file cap applied across the whole stack.

### Back/forward

The sidebar keeps a small in-process view history: entries are `Stack` and
`Git(repo root)`. Entering a git view pushes; `Ctrl+-` and the mouse back button step back
(to the stack), `Ctrl+Alt+-` and the mouse forward button step forward. When the sidebar's
own history has nothing in that direction, the gesture falls through to tab history, the
way the preview viewer already layers its in-pane history over `tabnav`. The Explorer
and Search chips do not push history; neither does folding a group.

### Preview tabs

A preview tab's sidebar shows the stack of the tab it was opened from, not of the preview
tab (which holds only a viewer). The origin tab id already travels in
`hs-preview-origin-tab`; the sidebar in a preview tab samples that tab's panes instead of
its own (my call). If the origin tab is gone, it falls back to the spawn directory rule.

## What is removed

- The `follow_cwd` setting and its Settings row. The follower's precedence rules go with
  it; the sampling helper stays.
- Change Folder, Change Folder (Type Path), the native picker, and the typed prompt.
- `roots.json` and `resolve_root`/`save_root`. Existing files are ignored.
- The Source Control activity chip.

## Left to the builder

- Internal structure: whether the stack is one `App` holding several tree regions or a
  thin container over several of today's tree states. Keep rendering and hit-testing
  honest about variable region heights.
- How the sampled stack diff is applied (which trees to keep, drop, rebuild).
- Divider drag mechanics and the exact chevron, header, and divider glyphs per icon theme.
- Per-root caching of `Git::owner_of`, re-run only when the root set changes.
- The `tab-back`/`tab-forward` actions send F7/F8 to a focused sidebar pane as they do to
  a viewer, so the in-process history goes first.
- Project `.herdr-sidebar.toml` rules, `programs::workspace_root`, and the preview spawn
  cwd use the root of the tree the file was clicked in. The git footer reflects the
  focused region's repository. The first region is focused on startup.
- Test strategy. Hermetic tests for root collapse/fold, grouping, height recomputation,
  history stepping, and the pane filter; a live check in `hs-test` for add/remove.
- AGENTS.md updates for anything learned the hard way.
