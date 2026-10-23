//! herdr-sidebar — the VS Code sidebar for herdr: file explorer and source
//! control in ONE binary. In unified mode both views share a pane and the
//! activity bar switches between them IN PROCESS (instant, no flash); in
//! separated mode the same binary runs one pane per view, pinned with
//! `--view explorer|git`. `--preview <ctl>` runs the file-preview pane.
//!
//! The native `--ensure` / `--toggle*` modes drive pane lifecycle; the other
//! `--*` stdin→stdout helpers expose the unit-tested launch calculations.

mod explorer_app;
mod scm_app;

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::time::Duration;

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use herdr_sidebar::{ensure, launch, state, viewer, viewnav};
use state::{Exit, State, View};

/// How often the source-control view re-reads `git status` while idle.
const REFRESH_EVERY: Duration = Duration::from_millis(1500);
const ANIMATION_FRAME: Duration = Duration::from_millis(120);

fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--ensure") => return ensure::run(ensure::Mode::Ensure),
        Some("--toggle") => {
            return ensure::run(ensure::Mode::Toggle(View::Explorer));
        }
        Some("--toggle-git") => {
            return ensure::run(ensure::Mode::Toggle(View::SourceControl));
        }
        Some("--show-explorer") => {
            return ensure::run(ensure::Mode::Activate(ensure::Target::Explorer));
        }
        Some("--show-search") => {
            return ensure::run(ensure::Mode::Activate(ensure::Target::Search));
        }
        Some("--show-git") => {
            return ensure::run(ensure::Mode::Activate(ensure::Target::SourceControl));
        }
        Some("--quick-open") => {
            return ensure::run(ensure::Mode::Activate(ensure::Target::QuickOpen));
        }
        Some("--update-latest") => {
            return herdr_sidebar::updates::run().map_err(std::io::Error::other);
        }
        Some("--refresh-sidebars") => return herdr_sidebar::updates::refresh(),
        Some("--open-link") => return herdr_sidebar::actions::open_clicked_link(),
        Some("--tab-back") => return herdr_sidebar::tabnav::action(false),
        Some("--tab-forward") => return herdr_sidebar::tabnav::action(true),
        Some("--run-custom-editor") => return herdr_sidebar::actions::run_configured_editor(),
        Some("--launch-decision") => {
            // Optional second arg picks the source-control decision; default
            // is the explorer/sidebar decision.
            // Optional THIRD arg scopes the decision to a tab or workspace —
            // it must match the scope the hook docks into, or the decision
            // answers for one tab while the dock lands in another.
            let now = state::unix_now();
            let scope = std::env::args().nth(3).unwrap_or_default();
            let mut out = if std::env::args().nth(2).as_deref() == Some("git") {
                launch::launch_decision_git(&read_stdin()?, now)
            } else {
                launch::launch_decision_in(&read_stdin()?, now, &scope)
            };
            // Strict toggle (⚙ Settings): report an open-but-unfocused pane
            // as CLOSE so the toggle launchers close it instead of focusing
            // it first. Safe for the ensure hook, which ignores FOCUS and
            // CLOSE alike (it only acts on OPEN and REPLACE).
            if state::load_state().strict_toggle {
                out = launch::focus_as_close(&out);
            }
            println!("{out}");
            return Ok(());
        }
        Some("--focused-pane") => {
            // Optional scope (tab or workspace id) confines the lookup to the
            // tab being docked; without it the globally focused pane wins and
            // a new tab gets rooted in whatever project was last focused.
            let scope = std::env::args().nth(2).unwrap_or_default();
            println!("{}", launch::focused_pane_in(&read_stdin()?, &scope));
            return Ok(());
        }
        Some("--pane-has-token") => {
            let pane_id = std::env::args().nth(2).unwrap_or_default();
            let present = launch::pane_has_token(&read_stdin()?, &pane_id);
            println!("{}", if present { "yes" } else { "no" });
            return Ok(());
        }
        Some("--event-scope") => {
            let payload = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
            println!("{}", launch::event_scope_in(&payload, &read_stdin()?));
            return Ok(());
        }
        Some("--open-plan") => {
            let state = state::load_state();
            println!(
                "{}",
                launch::open_plan(&read_stdin()?, state.dock_right, state.sidebar_width)
            );
            return Ok(());
        }
        Some("--event-kind") => {
            // Which event ran the ensure hook, so it can treat a brand-new
            // space differently from an ordinary focus. Empty when herdr
            // supplies no payload (e.g. a manual invocation).
            let payload = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
            println!("{}", launch::event_kind(&payload));
            return Ok(());
        }
        Some("--focused-tab") => {
            println!("{}", launch::focused_tab(&read_stdin()?));
            return Ok(());
        }
        Some("--auto-open") => {
            // For the unix ensure hook: skip auto-docking when the user
            // turned "Auto-open sidebar" off in ⚙ Settings (issue #8).
            println!(
                "{}",
                if state::load_state().auto_open {
                    "on"
                } else {
                    "off"
                }
            );
            return Ok(());
        }
        Some("--focus-on-open") => {
            // For the unix toggle launchers: skip the open-then-focus zoom
            // cycle when the user turned "Focus on open" off in ⚙ Settings,
            // so the sidebar docks in the background.
            println!(
                "{}",
                if state::load_state().focus_on_open {
                    "on"
                } else {
                    "off"
                }
            );
            return Ok(());
        }
        Some("--dock-right") => {
            println!(
                "{}",
                if state::load_state().dock_right {
                    "right"
                } else {
                    "left"
                }
            );
            return Ok(());
        }
        Some("--preview") => {
            let Some(control) = std::env::args()
                .nth(2)
                .or_else(|| std::env::var(state::PREVIEW_CONTROL_ENV).ok())
            else {
                eprintln!(
                    "herdr-sidebar: --preview needs {}",
                    state::PREVIEW_CONTROL_ENV
                );
                std::process::exit(2);
            };
            // The viewer is its own process: it must apply the persisted
            // color theme itself, or a preview pane keeps the default palette
            // (and a dark syntax theme) whatever the user chose.
            herdr_sidebar::ui::set_color_theme(state::load_state().color_theme);
            return viewer::run(std::path::Path::new(&control));
        }
        Some("--view") => {}
        Some(other) => {
            eprintln!("herdr-sidebar: unknown argument `{other}`");
            eprintln!(
                "usage: herdr-sidebar [--view explorer|git|--preview [ctl]|--run-custom-editor|--ensure|--toggle|--toggle-git|--show-explorer|--show-search|--show-git|--quick-open|--launch-decision [git]|--focused-pane|--pane-has-token <id>|--open-plan|--focused-tab|--auto-open|--focus-on-open|--dock-right]"
            );
            std::process::exit(2);
        }
        None => {}
    }

    // Starting view: an explicit `--view` pin (separated panes), else the
    // last-active view when the unified sidebar is on.
    let pinned = if mode.as_deref() == Some("--view") {
        std::env::args()
            .nth(2)
            .as_deref()
            .and_then(View::from_view_flag)
    } else {
        None
    };
    let persisted = state::load_state();
    let initial_activity = std::env::var(state::INITIAL_ACTIVITY_ENV)
        .ok()
        .and_then(|value| ensure::Target::from_env_value(&value));
    herdr_sidebar::ui::set_color_theme(persisted.color_theme);

    // Unix launchers use plugin.pane.open so Herdr starts this argv directly,
    // with no shell prompt between the split and the TUI. Keep the host's cwd
    // at the plugin root for relative-command resolution, then adopt the
    // requested project cwd inside the process.
    if let Some(cwd) = std::env::var_os(state::SPAWN_CWD_ENV).filter(|cwd| !cwd.is_empty()) {
        std::env::set_current_dir(cwd)?;
    }
    let spawn_cwd = std::env::current_dir()?;
    let owner = |path: &std::path::Path| {
        herdr_sidebar::git::Git::owner_of(path)
            .ok()
            .map(|git| git.root().to_path_buf())
    };
    // The repositories in this tab's stack: only a unified, unpinned
    // sidebar can re-enter a remembered Source Control repo, so the
    // pane.list round trip is skipped for every other start.
    let group_keys: Vec<std::path::PathBuf> = if pinned.is_none() && persisted.merged {
        let pane_id = std::env::var("HERDR_PANE_ID").unwrap_or_default();
        let roots = (!pane_id.is_empty())
            .then(|| herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})).ok())
            .flatten()
            .and_then(|json| herdr_sidebar::stack::stack_roots(&json, &pane_id))
            .map(|roots| {
                if roots.is_empty() {
                    vec![spawn_cwd.clone()]
                } else {
                    roots
                }
            })
            .unwrap_or_default();
        herdr_sidebar::stack::group_roots(&roots, owner, &[])
            .into_iter()
            .map(|group| group.key)
            .collect()
    } else {
        Vec::new()
    };
    let spawn_owner = if initial_activity == Some(ensure::Target::SourceControl) {
        owner(&spawn_cwd)
    } else {
        None
    };
    let (mut view, mut start_repo) = startup_view(
        &persisted,
        pinned,
        initial_activity,
        spawn_owner,
        &group_keys,
    );
    // Mark the short interval before App::new applies its live identity. The
    // launcher also writes a live stamp after plugin.pane.open returns, so any
    // ordering between the two ends with App::new clearing this marker before
    // the TUI can accept edits.
    if let Some(pane_id) = std::env::var_os("HERDR_PANE_ID").filter(|id| !id.is_empty()) {
        let _ = herdr_sidebar::ipc::report_starting_identity(
            &pane_id.to_string_lossy(),
            view,
            view == View::Explorer && persisted.merged,
        );
    }

    // ONE terminal session for every view: switching drops the old view's
    // state and draws the other in the same alternate screen — instant, and
    // the shell prompt underneath never flashes through.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0),
    );
    // A TUI's colors are interface, not pipeable output: ignore NO_COLOR,
    // which otherwise leaks in whenever the herdr server was (re)started
    // from an agent shell (Claude Code's tool env sets it) and silently
    // turns every pane we draw monochrome.
    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    // First run on a machine without a Nerd Font: offer to install one
    // before any icons render. The prompt stamps the pane's identity token
    // itself (the app loops haven't started yet, and a token-less pane gets
    // REPLACE-killed by the corpse rule while the user reads the prompt).
    herdr_sidebar::fontsetup::maybe_prompt(&mut terminal, view, persisted.merged)?;
    let cwd_follower = Rc::new(RefCell::new(launch::CwdFollower::default()));
    let history = Rc::new(RefCell::new(viewnav::ViewHistory::new()));
    if let Some(repo) = &start_repo {
        history
            .borrow_mut()
            .visit(viewnav::ViewEntry::Git(repo.clone()));
    }
    // Some(focus_query) opens the Search view on the next Explorer render;
    // None doesn't. A resumed search restores unfocused (a switch, not a find).
    let mut search_on_open: Option<bool> = if initial_activity == Some(ensure::Target::Search) {
        Some(false)
    } else {
        (pinned.is_none()
            && persisted.merged
            && persisted.active == View::Explorer
            && persisted.search_active)
            .then_some(false)
    };
    let mut quick_open_on_open = initial_activity == Some(ensure::Target::QuickOpen);
    let result = loop {
        let exit = match view {
            View::Explorer => run_explorer(
                &mut terminal,
                Rc::clone(&history),
                &spawn_cwd,
                std::mem::take(&mut search_on_open),
                std::mem::take(&mut quick_open_on_open),
            ),
            View::SourceControl => {
                // The startup repo first; later entries follow the repo the
                // stack's ⎇ or view history wrote to `active_repo`.
                let repo = start_repo.take().or_else(|| {
                    let state = state::load_state();
                    (pinned.is_none() && state.merged)
                        .then_some(state.active_repo)
                        .flatten()
                        .filter(|repo| repo.is_dir())
                });
                run_scm(
                    &mut terminal,
                    Rc::clone(&cwd_follower),
                    Rc::clone(&history),
                    repo.as_deref().unwrap_or(&spawn_cwd),
                )
            }
        };
        match exit {
            Ok(Exit::Quit) => break Ok(()),
            Ok(Exit::Switch) => {
                view = state::load_state().active;
            }
            Ok(Exit::Search { focus_query }) => {
                view = View::Explorer;
                search_on_open = Some(focus_query);
            }
            Ok(Exit::QuickOpen) => {
                view = View::Explorer;
                quick_open_on_open = true;
            }
            Err(e) => break Err(e),
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

/// (view to start in, repo for a git view). Source Control is entered only when the
/// unified sidebar is on and `repo` is one of `group_keys`; `Target::SourceControl` with
/// no persisted repo falls back to `spawn_owner`.
fn startup_view(
    persisted: &State,
    pinned: Option<View>,
    initial: Option<ensure::Target>,
    spawn_owner: Option<std::path::PathBuf>,
    group_keys: &[std::path::PathBuf],
) -> (View, Option<std::path::PathBuf>) {
    let view = initial.map_or_else(
        || {
            pinned.unwrap_or(if persisted.merged {
                persisted.active
            } else {
                View::Explorer
            })
        },
        ensure::Target::initial_view,
    );
    if view == View::Explorer {
        return (View::Explorer, None);
    }
    // A separated (pinned) Source Control pane starts at its spawn cwd and
    // follows focus; it never re-enters a remembered repo.
    if pinned.is_some() || !persisted.merged {
        return (View::SourceControl, None);
    }
    let in_stack = |repo: &std::path::PathBuf| group_keys.contains(repo);
    let repo = persisted.active_repo.clone().filter(in_stack).or_else(|| {
        (initial == Some(ensure::Target::SourceControl))
            .then_some(spawn_owner)
            .flatten()
            .filter(in_stack)
    });
    match repo {
        Some(repo) => (View::SourceControl, Some(repo)),
        None => (View::Explorer, None),
    }
}

/// The explorer's event loop: short poll so the liveness heartbeat keeps
/// stamping even while idle.
fn run_explorer(
    terminal: &mut ratatui::DefaultTerminal,
    history: Rc<RefCell<viewnav::ViewHistory>>,
    spawn_cwd: &std::path::Path,
    search_on_open: Option<bool>,
    quick_open_on_open: bool,
) -> std::io::Result<Exit> {
    let mut app = explorer_app::App::new(spawn_cwd.to_path_buf(), history);
    if let Some(focus_query) = search_on_open {
        app.open_content_search(focus_query);
    }
    if quick_open_on_open {
        app.open_quick_open();
    }
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        // 500ms: cheap for the heartbeat, prompt for background results.
        let timeout = if app.is_syncing() {
            ANIMATION_FRAME
        } else {
            Duration::from_millis(500)
        };
        if event::poll(timeout)? {
            let exit = match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                Event::Resize(width, _) => {
                    app.on_resize(width);
                    None
                }
                _ => None, // resize, focus, … simply fall through to a redraw
            };
            if let Some(exit) = exit {
                if exit == Exit::Quit {
                    app.clear_identity();
                }
                app.save_layout();
                return Ok(exit);
            }
        }
        app.heartbeat();
        app.tick();
    }
}

/// The source-control view's event loop: poll + tick so external changes and
/// finished background work (✧ suggestions, syncs) show up on their own.
fn run_scm(
    terminal: &mut ratatui::DefaultTerminal,
    cwd_follower: Rc<RefCell<launch::CwdFollower>>,
    history: Rc<RefCell<viewnav::ViewHistory>>,
    cwd: &std::path::Path,
) -> std::io::Result<Exit> {
    let mut app = scm_app::App::new(cwd.to_path_buf(), cwd_follower, history);
    let mut last_tick = std::time::Instant::now();
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        let mut timeout = REFRESH_EVERY.saturating_sub(last_tick.elapsed());
        if app.is_syncing() {
            timeout = timeout.min(ANIMATION_FRAME);
        }
        if event::poll(timeout)? {
            let exit = match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                Event::Resize(width, _) => {
                    app.on_resize(width);
                    None
                }
                _ => None,
            };
            if let Some(exit) = exit {
                // Switching drops this App and rebuilds it later, so it needs
                // the same persistence gate as quitting or a draft would be
                // lost despite the process staying alive.
                if !app.persist_scm() {
                    continue;
                }
                if exit == Exit::Quit {
                    app.clear_identity();
                }
                return Ok(exit);
            }
        }
        app.heartbeat();
        if last_tick.elapsed() >= REFRESH_EVERY {
            app.tick();
            last_tick = std::time::Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_enters_git_view_only_for_a_repo_in_the_stack() {
        let mut s = State::default();
        s.merged = true;
        s.active = View::SourceControl;
        s.active_repo = Some("/w/mono".into());
        assert_eq!(
            startup_view(&s, None, None, None, &["/w/mono".into()]),
            (View::SourceControl, Some("/w/mono".into()))
        );
        assert_eq!(
            startup_view(&s, None, None, None, &["/other".into()]),
            (View::Explorer, None)
        );
        s.active_repo = None;
        assert_eq!(
            startup_view(&s, None, None, None, &["/w/mono".into()]),
            (View::Explorer, None)
        );
    }

    #[test]
    fn show_git_action_uses_the_spawn_owner_when_nothing_is_remembered() {
        let mut s = State::default();
        s.merged = true;
        assert_eq!(
            startup_view(
                &s,
                None,
                Some(ensure::Target::SourceControl),
                Some("/w/x".into()),
                &["/w/x".into()]
            ),
            (View::SourceControl, Some("/w/x".into()))
        );
    }

    #[test]
    fn pinned_panes_ignore_persisted_view() {
        let mut s = State::default();
        s.active = View::SourceControl;
        s.active_repo = Some("/r".into());
        assert_eq!(
            startup_view(&s, Some(View::Explorer), None, None, &["/r".into()]),
            (View::Explorer, None)
        );
        assert_eq!(
            startup_view(&s, Some(View::SourceControl), None, None, &[]),
            (View::SourceControl, None)
        );
    }
}
