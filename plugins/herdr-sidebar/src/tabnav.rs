//! Browser-style history over herdr TABS: back returns to the tab you were on
//! before, forward undoes that.
//!
//! The focus hook (`ensure::run` on `tab.focused` / `workspace.focused` /
//! `pane.focused`) appends the tab each event landed on to a per-session
//! journal in the private runtime directory; the `tab-back` / `tab-forward`
//! actions step through it and move the client with
//! [`crate::viewer::focus_tab_for_client`]. A step records a `navigating`
//! marker so the focus event it causes is not pushed as a NEW entry — without
//! that, back and forward collapse into a two-tab loop. The journal is keyed
//! by the session socket because tab ids are only unique per server and
//! change across restarts; dead tabs are pruned against `pane.list` on every
//! use. Herdr emits no event for restore or attach, so history starts empty
//! after a server restart.
//!
//! Code navigation layers on top: a focused preview viewer receives the
//! transport key ([`BACK_KEY`] / [`FORWARD_KEY`]) instead of a direct step, so
//! its in-document history goes first and only an empty one falls through to
//! the tab history (`viewer::history_step`).

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{ipc, launch, rundir};

/// Oldest entries drop off beyond this many.
pub const CAP: usize = 50;
/// How long a step's marker waits for the focus event it causes (seconds).
const MARKER_TTL: u64 = 5;
/// Transport keys the actions send to a focused preview viewer; the PTY
/// decoder emits function keys reliably where a synthetic Ctrl chord would
/// not survive legacy encoding (F9–F12 are the activity keys).
pub const BACK_KEY: &str = "f7";
pub const FORWARD_KEY: &str = "f8";

/// The persisted history: `entries[index]` is the tab we believe is current,
/// `panes[index]` the pane that was focused in it (so a return lands where
/// you were, not on the tab's first pane; "" when unknown).
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    #[serde(default)]
    entries: Vec<String>,
    #[serde(default)]
    panes: Vec<String>,
    #[serde(default)]
    index: usize,
    /// A step this plugin initiated: `(target tab, deadline)`. The focus
    /// event it produces must land on the existing entry, not push a new one.
    #[serde(default)]
    navigating: Option<(String, u64)>,
}

impl Journal {
    /// `panes` always pairs with `entries` (a journal written by an older
    /// build may lack it).
    fn normalize(&mut self) {
        self.panes.resize(self.entries.len(), String::new());
    }

    /// A focus event landed on `pane` in `tab` at `now`.
    pub fn record(&mut self, tab: &str, pane: &str, now: u64) {
        self.normalize();
        let same_tab = self.entries.get(self.index).map(String::as_str) == Some(tab);
        if same_tab && !pane.is_empty() {
            self.panes[self.index] = pane.to_string();
        }
        if let Some((target, until)) = self.navigating.take()
            && target == tab
            && now <= until
        {
            return;
        }
        if same_tab {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.panes.truncate(self.index + 1);
        self.entries.push(tab.to_string());
        self.panes.push(pane.to_string());
        if self.entries.len() > CAP {
            let excess = self.entries.len() - CAP;
            self.entries.drain(..excess);
            self.panes.drain(..excess);
        }
        self.index = self.entries.len() - 1;
    }

    /// Forget tabs that no longer exist, keeping `index` on the current entry
    /// (or the nearest earlier survivor) and collapsing adjacent duplicates
    /// that the removals expose.
    pub fn prune(&mut self, live: &BTreeSet<String>) {
        self.normalize();
        let mut kept: Vec<(String, String)> = Vec::with_capacity(self.entries.len());
        let mut index = 0;
        for (i, (tab, pane)) in self.entries.iter().zip(&self.panes).enumerate() {
            if !live.contains(tab) || kept.last().is_some_and(|(last, _)| last == tab) {
                continue;
            }
            if i <= self.index {
                index = kept.len();
            }
            kept.push((tab.clone(), pane.clone()));
        }
        (self.entries, self.panes) = kept.into_iter().unzip();
        self.index = index.min(self.entries.len().saturating_sub(1));
    }

    /// The `(tab, pane)` one step back (`forward == false`) or forward,
    /// marking the move so its focus event is not recorded again. `None` at
    /// either end.
    pub fn step(&mut self, forward: bool, now: u64) -> Option<(String, String)> {
        self.normalize();
        let target = if forward {
            self.index + 1
        } else {
            self.index.checked_sub(1)?
        };
        let tab = self.entries.get(target)?.clone();
        self.index = target;
        self.navigating = Some((tab.clone(), now + MARKER_TTL));
        Some((tab, self.panes[target].clone()))
    }
}

fn session_key() -> String {
    ipc::socket_path()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn journal_path() -> PathBuf {
    let key = crate::viewer::document_token(&session_key());
    rundir::dir("tabs").join(format!("{key}.json"))
}

/// Read-modify-write under an OS lock shared by every sidebar process and
/// hook of this user (released by the kernel on death; never polled).
fn with_journal<T>(update: impl FnOnce(&mut Journal) -> T) -> io::Result<T> {
    let path = journal_path();
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("journal has no parent"))?;
    rundir::ensure_private(dir)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    let mut journal = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let out = update(&mut journal);
    std::fs::write(&path, serde_json::to_vec(&journal)?)?;
    Ok(out)
}

/// A tab id as herdr prints it (`w1:t5`): one colon, flag-safe characters.
fn is_tab_id(value: &str) -> bool {
    value.len() <= 64
        && value.matches(':').count() == 1
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_'))
}

/// Hook side: remember the tab a focus event landed on. Costs one `pane.list`
/// per event, so only focus kinds pay it. Never fails loudly: a hook that
/// cannot record must still go on to dock sidebars.
pub fn record_event(event_json: &str) {
    if !matches!(
        launch::event_kind(event_json).as_str(),
        "tab_focused" | "workspace_focused" | "pane_focused"
    ) {
        return;
    }
    let Ok(panes) = ipc::call_text("pane.list", serde_json::json!({})) else {
        return;
    };
    let context_tab = std::env::var("HERDR_TAB_ID").unwrap_or_default();
    let tab = launch::event_scope_with_tab_context(event_json, &panes, &context_tab);
    if !is_tab_id(&tab) {
        return;
    }
    let pane = focused_pane_within(&panes, &tab);
    let live = launch::live_tabs(&panes);
    let now = crate::state::unix_now();
    let _ = with_journal(|journal| {
        journal.prune(&live);
        journal.record(&tab, &pane, now);
    });
}

/// The focused pane inside `tab` ("" when the snapshot has none). Not
/// `launch::focused_pane_in`: that one answers "where should a sidebar be
/// rooted" and yields nothing for a pane without a cwd, which is exactly what
/// a preview viewer is.
fn focused_pane_within(panes: &str, tab: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(launch::strip_bom(panes)) else {
        return String::new();
    };
    value
        .get("result")
        .and_then(|r| r.get("panes"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|pane| {
            pane.get("tab_id").and_then(serde_json::Value::as_str) == Some(tab)
                && pane.get("focused").and_then(serde_json::Value::as_bool) == Some(true)
        })
        .and_then(|pane| pane.get("pane_id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .unwrap_or_default()
}

/// Step the tab history and move the client. `Ok(None)` means nothing lies in
/// that direction.
pub fn step(forward: bool) -> Result<Option<String>, String> {
    let panes = ipc::call_text("pane.list", serde_json::json!({})).map_err(|e| e.to_string())?;
    let live = launch::live_tabs(&panes);
    let focused = launch::focused_tab(&panes);
    let focused_pane = focused_pane_within(&panes, &focused);
    let now = crate::state::unix_now();
    let target = with_journal(|journal| {
        journal.prune(&live);
        // The hook may have missed the tab we are on (restore, a skipped
        // event); anchor the position on it before stepping. This also
        // refreshes the pane to return to.
        if is_tab_id(&focused) {
            journal.record(&focused, &focused_pane, now);
        }
        journal.step(forward, now)
    })
    .map_err(|e| format!("tab history: {e}"))?;
    let Some((tab, pane)) = target else {
        return Ok(None);
    };
    crate::viewer::focus_tab_for_client(&tab, Some(&pane));
    Ok(Some(tab))
}

/// [`step`] reduced to a footer notice: `None` when the client moved.
pub fn step_notice(forward: bool) -> Option<String> {
    match step(forward) {
        Ok(Some(_)) => None,
        Ok(None) => Some(
            if forward {
                "no later tab"
            } else {
                "no earlier tab"
            }
            .into(),
        ),
        Err(why) => Some(why),
    }
}

/// The `tab-back` / `tab-forward` actions. A focused preview viewer gets the
/// transport key so its in-document history goes first; anything else steps
/// the tab history directly.
pub fn action(forward: bool) -> io::Result<()> {
    let panes = ipc::call_text("pane.list", serde_json::json!({}))?;
    let focused = launch::focused_pane_in(&panes, "")
        .split_once('\t')
        .map(|(pane, _)| pane.to_string())
        .unwrap_or_default();
    if !focused.is_empty()
        && crate::viewer::previews_in(&panes)
            .iter()
            .any(|preview| preview.pane_id == focused)
    {
        let key = if forward { FORWARD_KEY } else { BACK_KEY };
        ipc::call_text(
            "pane.send_input",
            serde_json::json!({ "pane_id": focused, "text": "", "keys": [key] }),
        )?;
        return Ok(());
    }
    step(forward).map(|_| ()).map_err(io::Error::other)
}

/// Which way a key steps the history, if it is one of ours: the transport
/// keys, and `Ctrl+-` (byte 0x1F, which crossterm reports as Ctrl+`7`).
/// `Ctrl+Alt+-` is deliberately NOT claimed here: on Windows AltGr layouts
/// CONTROL|ALT chars are text, and the sidebars have text fields. The viewer
/// has none and accepts it separately.
pub fn key_direction(key: &crossterm::event::KeyEvent) -> Option<bool> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match key.code {
        KeyCode::F(7) => Some(false),
        KeyCode::F(8) => Some(true),
        KeyCode::Char('7')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            Some(false)
        }
        _ => None,
    }
}

/// Which way a mouse side button steps the history (press only).
pub fn mouse_direction(kind: crossterm::event::MouseEventKind) -> Option<bool> {
    use crossterm::event::{MouseButton, MouseEventKind};
    match kind {
        MouseEventKind::Down(MouseButton::Back) => Some(false),
        MouseEventKind::Down(MouseButton::Forward) => Some(true),
        _ => None,
    }
}

/// A side button's release, which must not reach the app's ordinary mouse
/// handling as if it were a click.
pub fn is_side_release(kind: crossterm::event::MouseEventKind) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    matches!(
        kind,
        MouseEventKind::Up(MouseButton::Back) | MouseEventKind::Up(MouseButton::Forward)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(tabs: &[&str]) -> BTreeSet<String> {
        tabs.iter().map(|t| t.to_string()).collect()
    }

    fn tab(step: Option<(String, String)>) -> Option<String> {
        step.map(|(tab, _)| tab)
    }

    #[test]
    fn record_dedupes_and_truncates_forward_history() {
        let mut j = Journal::default();
        j.record("w1:t1", "w1:p1", 10);
        j.record("w1:t1", "w1:p1", 11);
        j.record("w1:t2", "w1:p2", 12);
        j.record("w1:t3", "w1:p3", 13);
        assert_eq!(j.entries, ["w1:t1", "w1:t2", "w1:t3"]);
        assert_eq!(tab(j.step(false, 14)), Some("w1:t2".into()));
        assert_eq!(tab(j.step(false, 14)), Some("w1:t1".into()));
        assert_eq!(j.step(false, 14), None, "nothing before the first tab");
        // A new tab from the middle drops the forward branch, browser-style.
        j.record("w1:t1", "w1:p1", 15); // our own step landing
        j.record("w1:t9", "w1:p9", 16);
        assert_eq!(j.entries, ["w1:t1", "w1:t9"]);
        assert_eq!(j.step(true, 17), None);
    }

    #[test]
    fn a_step_is_not_recorded_again_but_a_late_or_different_focus_is() {
        let mut j = Journal::default();
        j.record("w1:t1", "w1:p1", 1);
        j.record("w1:t2", "w1:p2", 2);
        assert_eq!(tab(j.step(false, 3)), Some("w1:t1".into()));
        j.record("w1:t1", "w1:p1", 4);
        assert_eq!(
            j.index, 0,
            "the focus event for our step keeps the position"
        );
        assert_eq!(
            tab(j.step(true, 5)),
            Some("w1:t2".into()),
            "forward still works"
        );
        // The marker is consumed by a DIFFERENT tab arriving first.
        assert_eq!(tab(j.step(false, 6)), Some("w1:t1".into()));
        j.record("w1:t7", "w1:p7", 7);
        assert_eq!(j.entries, ["w1:t1", "w1:t7"]);
        // And it expires.
        let mut j = Journal::default();
        j.record("w1:t1", "w1:p1", 1);
        j.record("w1:t2", "w1:p2", 2);
        j.step(false, 3);
        j.record("w1:t1", "w1:p1", 3 + MARKER_TTL + 1);
        assert_eq!(
            j.entries,
            ["w1:t1", "w1:t2"],
            "an expired marker is just a dedupe"
        );
    }

    #[test]
    fn a_return_lands_on_the_pane_that_was_focused() {
        let mut j = Journal::default();
        j.record("w1:t1", "w1:p1", 1);
        j.record("w1:t2", "w1:p2", 2);
        // Focus moved within t2 (a pane_focused event): remember the new pane.
        j.record("w1:t2", "w1:p9", 3);
        assert_eq!(j.step(false, 4), Some(("w1:t1".into(), "w1:p1".into())));
        assert_eq!(j.step(true, 5), Some(("w1:t2".into(), "w1:p9".into())));
        // An unknown pane never overwrites a known one.
        j.record("w1:t2", "", 6);
        assert_eq!(j.panes, ["w1:p1", "w1:p9"]);
        // Journals written before `panes` existed still load.
        let old: Journal =
            serde_json::from_str(r#"{"entries":["w1:t1","w1:t2"],"index":1,"navigating":null}"#)
                .unwrap();
        let mut old = old;
        assert_eq!(old.step(false, 7), Some(("w1:t1".into(), String::new())));
    }

    #[test]
    fn prune_drops_dead_tabs_and_keeps_the_position() {
        let mut j = Journal::default();
        for t in ["w1:t1", "w1:t2", "w1:t3", "w1:t4"] {
            j.record(t, "", 1);
        }
        j.step(false, 2); // on t3
        j.prune(&live(&["w1:t1", "w1:t3", "w1:t4"]));
        assert_eq!(j.entries, ["w1:t1", "w1:t3", "w1:t4"]);
        assert_eq!(j.index, 1, "still on t3");
        j.prune(&live(&["w1:t1", "w1:t4"]));
        assert_eq!(j.index, 0, "current tab gone: nearest earlier survivor");
        // Removing a tab between two visits to the same tab collapses them.
        let mut j = Journal::default();
        for t in ["w1:t1", "w1:t2", "w1:t1"] {
            j.record(t, "", 1);
        }
        j.prune(&live(&["w1:t1"]));
        assert_eq!(j.entries, ["w1:t1"]);
        assert_eq!(j.index, 0);
        j.prune(&live(&[]));
        assert!(j.entries.is_empty());
        assert_eq!(j.step(false, 3), None);
    }

    #[test]
    fn history_is_capped() {
        let mut j = Journal::default();
        for i in 0..(CAP + 10) {
            j.record(&format!("w1:t{i}"), "", 1);
        }
        assert_eq!(j.entries.len(), CAP);
        assert_eq!(j.panes.len(), CAP);
        assert_eq!(j.entries[0], "w1:t10");
        assert_eq!(j.index, CAP - 1);
    }

    #[test]
    fn tab_ids_are_recognised() {
        assert!(is_tab_id("w1:t5"));
        assert!(is_tab_id("wB:tJ"));
        assert!(!is_tab_id("w1"), "a workspace scope is not a tab");
        assert!(!is_tab_id(""));
        assert!(!is_tab_id("w1:t5 --flag"));
    }

    #[test]
    fn key_and_mouse_directions() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
        assert_eq!(
            key_direction(&KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)),
            Some(false)
        );
        assert_eq!(
            key_direction(&KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
            Some(true)
        );
        assert_eq!(
            key_direction(&KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL)),
            Some(false)
        );
        assert_eq!(
            key_direction(&KeyEvent::new(
                KeyCode::Char('7'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            None,
            "AltGr-safe: CONTROL|ALT chars stay text in the sidebars"
        );
        assert_eq!(
            key_direction(&KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            mouse_direction(MouseEventKind::Down(MouseButton::Back)),
            Some(false)
        );
        assert_eq!(
            mouse_direction(MouseEventKind::Down(MouseButton::Forward)),
            Some(true)
        );
        assert_eq!(
            mouse_direction(MouseEventKind::Down(MouseButton::Left)),
            None
        );
        assert!(is_side_release(MouseEventKind::Up(MouseButton::Back)));
        assert!(!is_side_release(MouseEventKind::Up(MouseButton::Left)));
    }
}
