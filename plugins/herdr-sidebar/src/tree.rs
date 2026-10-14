//! Filesystem tree model: which directories are expanded, and the flat list of
//! visible rows the UI renders. Directory listings are cached so redraws never
//! touch the disk; a non-recursive `notify` watch on the root and every expanded
//! directory evicts a listing when something inside it is created, removed or
//! renamed (`drain_events`).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
}

/// One visible line of the tree, in render order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub depth: usize,
    pub expanded: bool,
}

pub struct Tree {
    root: PathBuf,
    expanded: BTreeSet<PathBuf>,
    cache: HashMap<PathBuf, Vec<Entry>>,
    pub show_hidden: bool,
    /// `None` when the platform watcher could not be created; the tree then
    /// only re-reads on explicit refresh and when a folder is opened.
    watcher: Option<RecommendedWatcher>,
    events: Receiver<notify::Result<notify::Event>>,
    /// Directories currently watched: the root plus the expanded set.
    watched: BTreeSet<PathBuf>,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let (tx, events) = mpsc::channel();
        let watcher = notify::recommended_watcher(tx).ok();
        let mut tree = Self {
            root,
            expanded: BTreeSet::new(),
            cache: HashMap::new(),
            show_hidden: true,
            watcher,
            events,
            watched: BTreeSet::new(),
        };
        tree.sync_watches();
        tree
    }

    /// Keep the OS watch set equal to root + expanded. Non-recursive on
    /// purpose: a recursive watch would walk `target/`/`node_modules/` and
    /// exhaust inotify's per-user watch budget across preview-tab sidebars.
    fn sync_watches(&mut self) {
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let mut wanted = self.expanded.clone();
        wanted.insert(self.root.clone());
        for gone in self.watched.difference(&wanted) {
            let _ = watcher.unwatch(gone);
        }
        self.watched.retain(|dir| wanted.contains(dir));
        for dir in wanted
            .difference(&self.watched)
            .cloned()
            .collect::<Vec<_>>()
        {
            // Failures (deleted dir, watch limit) are retried on the next
            // expansion change; the folder still re-reads when reopened.
            if watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
                self.watched.insert(dir);
            }
        }
    }

    /// Apply queued filesystem events: evict the listing of every directory
    /// whose entries changed. Returns whether any listing was dropped, i.e.
    /// whether the rows need rebuilding. Cheap when idle, so call it every tick.
    pub fn drain_events(&mut self) -> bool {
        let mut changed = false;
        for event in self.events.try_iter().flatten() {
            if !changes_listing(&event.kind) {
                continue;
            }
            for path in &event.paths {
                // The entry's parent lists it; the path itself may be a
                // directory whose own listing is now gone.
                let hit = path
                    .parent()
                    .is_some_and(|dir| self.cache.remove(dir).is_some())
                    | self.cache.remove(path).is_some();
                if hit {
                    changed = true;
                } else if !path.starts_with(&self.root) && !self.cache.is_empty() {
                    // FSEvents reports canonical paths (`/private/tmp` for
                    // `/tmp`); when they do not map onto our keys, re-read
                    // everything displayed rather than miss the change.
                    self.cache.clear();
                    changed = true;
                }
            }
        }
        changed
    }

    /// The workspace root directory the tree is rooted at.
    pub fn root_path(&self) -> PathBuf {
        self.root.clone()
    }

    /// Display name for the header: the folder's own name, or the full path for
    /// roots like `C:\` that have no final component.
    pub fn root_name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// Drop all cached listings; the next `rows()` re-reads the disk.
    pub fn refresh(&mut self) {
        self.cache.clear();
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    /// The expanded set, for persisting so a sidebar opened in a new tab
    /// comes up showing what the tree already showed.
    pub fn expanded_paths(&self) -> Vec<PathBuf> {
        self.expanded.iter().cloned().collect()
    }

    /// Restore a persisted expanded set. Paths outside this tree's root are
    /// dropped: one state file is shared by every workspace's sidebars.
    pub fn set_expanded(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.expanded = paths
            .into_iter()
            .filter(|p| p.starts_with(&self.root))
            .collect();
        self.cache.clear();
        self.sync_watches();
    }

    /// Opening a folder always re-reads it: collapsed folders are not watched,
    /// so a listing cached from an earlier expansion may be stale.
    pub fn expand(&mut self, path: &Path) {
        if self.expanded.insert(path.to_path_buf()) {
            self.cache.remove(path);
            self.sync_watches();
        }
    }

    pub fn collapse(&mut self, path: &Path) {
        if self.expanded.remove(path) {
            self.cache.remove(path);
            self.sync_watches();
        }
    }

    /// Collapse every expanded directory (the title bar's Collapse All).
    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.cache.clear();
        self.sync_watches();
    }

    pub fn toggle(&mut self, path: &Path) {
        if self.expanded.contains(path) {
            self.collapse(path);
        } else {
            self.expand(path);
        }
    }

    fn children(&mut self, dir: &Path) -> Vec<Entry> {
        if let Some(cached) = self.cache.get(dir) {
            return cached.clone();
        }
        let mut entries: Vec<Entry> = fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| {
                        let kind = e.file_type().ok();
                        let is_symlink = kind.as_ref().is_some_and(|kind| kind.is_symlink());
                        let is_dir = if is_symlink {
                            e.path().is_dir()
                        } else {
                            kind.is_some_and(|kind| kind.is_dir())
                        };
                        Entry {
                            is_dir,
                            is_symlink,
                            name: e.file_name().to_string_lossy().into_owned(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        sort_entries(&mut entries);
        self.cache.insert(dir.to_path_buf(), entries.clone());
        entries
    }

    /// The visible rows, depth-first through expanded directories.
    pub fn rows(&mut self) -> Vec<Row> {
        let mut out = Vec::new();
        let root = self.root.clone();
        self.walk(&root, 0, &mut out);
        out
    }

    fn walk(&mut self, dir: &Path, depth: usize, out: &mut Vec<Row>) {
        let show_hidden = self.show_hidden;
        for entry in self.children(dir) {
            if !visible(&entry.name, show_hidden) {
                continue;
            }
            let path = dir.join(&entry.name);
            let expanded = entry.is_dir && self.is_expanded(&path);
            out.push(Row {
                name: entry.name,
                is_dir: entry.is_dir,
                is_symlink: entry.is_symlink,
                depth,
                expanded,
                path: path.clone(),
            });
            if expanded {
                self.walk(&path, depth + 1, out);
            }
        }
    }
}

/// VS Code Explorer order: directories first, then files, each case-insensitive.
/// Only events that add, remove or rename an entry change a listing; content
/// saves and metadata touches are far more frequent and change nothing shown.
fn changes_listing(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Name(_) | ModifyKind::Any)
            | EventKind::Any
            | EventKind::Other
    )
}

pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// `.git` is always hidden; other dotfiles only when `show_hidden` is off.
fn visible(name: &str, show_hidden: bool) -> bool {
    name != ".git" && (show_hidden || !name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("aa-filetree-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn mkdir(&self, rel: &str) {
            fs::create_dir_all(self.0.join(rel)).unwrap();
        }
        fn touch(&self, rel: &str) {
            fs::write(self.0.join(rel), b"").unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(rows: &[Row]) -> Vec<(String, usize)> {
        rows.iter().map(|r| (r.name.clone(), r.depth)).collect()
    }

    #[test]
    fn restored_expansion_ignores_other_workspaces() {
        let tmp = TempDir::new("restore");
        tmp.mkdir("src");
        let mut tree = Tree::new(tmp.0.clone());
        assert!(tree.rows().iter().all(|r| !r.expanded));

        // One state file serves every workspace's sidebars, so a foreign
        // root must be dropped instead of resurrecting as a phantom row.
        tree.set_expanded(vec![
            tmp.0.join("src"),
            PathBuf::from("/somewhere/else/src"),
        ]);
        assert_eq!(tree.expanded_paths(), vec![tmp.0.join("src")]);
        assert!(tree.rows().iter().any(|r| r.name == "src" && r.expanded));
    }

    #[test]
    fn dirs_first_case_insensitive_and_git_hidden() {
        let tmp = TempDir::new("order");
        tmp.mkdir("b_dir");
        tmp.mkdir("A_dir");
        tmp.mkdir(".git");
        tmp.touch("Zebra.txt");
        tmp.touch("apple.rs");
        let mut tree = Tree::new(tmp.0.clone());
        assert_eq!(
            names(&tree.rows()),
            vec![
                ("A_dir".into(), 0),
                ("b_dir".into(), 0),
                ("apple.rs".into(), 0),
                ("Zebra.txt".into(), 0),
            ]
        );
    }

    #[test]
    fn expand_and_collapse_nest_children() {
        let tmp = TempDir::new("expand");
        tmp.mkdir("src");
        tmp.touch("src/main.rs");
        tmp.touch("Cargo.toml");
        let mut tree = Tree::new(tmp.0.clone());
        tree.toggle(&tmp.0.join("src"));
        assert_eq!(
            names(&tree.rows()),
            vec![
                ("src".into(), 0),
                ("main.rs".into(), 1),
                ("Cargo.toml".into(), 0),
            ]
        );
        assert!(tree.rows()[0].expanded);
        tree.toggle(&tmp.0.join("src"));
        assert_eq!(
            names(&tree.rows()),
            vec![("src".into(), 0), ("Cargo.toml".into(), 0)]
        );
    }

    #[test]
    fn collapse_all_closes_every_expanded_dir() {
        let tmp = TempDir::new("collapseall");
        tmp.mkdir("a/inner");
        tmp.mkdir("b");
        let mut tree = Tree::new(tmp.0.clone());
        tree.expand(&tmp.0.join("a"));
        tree.expand(&tmp.0.join("a/inner"));
        tree.expand(&tmp.0.join("b"));
        assert!(tree.rows().iter().any(|r| r.expanded));
        tree.collapse_all();
        assert!(tree.rows().iter().all(|r| !r.expanded));
        assert_eq!(tree.rows().len(), 2, "only the top level remains");
    }

    #[test]
    fn hidden_toggle_filters_dotfiles() {
        let tmp = TempDir::new("hidden");
        tmp.touch(".env");
        tmp.touch("visible.txt");
        let mut tree = Tree::new(tmp.0.clone());
        assert_eq!(tree.rows().len(), 2);
        tree.show_hidden = false;
        assert_eq!(names(&tree.rows()), vec![("visible.txt".into(), 0)]);
    }

    #[test]
    fn refresh_picks_up_new_files() {
        let tmp = TempDir::new("refresh");
        tmp.touch("one.txt");
        let mut tree = Tree::new(tmp.0.clone());
        assert_eq!(tree.rows().len(), 1);
        tmp.touch("two.txt");
        assert_eq!(tree.rows().len(), 1, "cached listing must not re-read disk");
        tree.refresh();
        assert_eq!(tree.rows().len(), 2);
    }

    #[test]
    fn reopening_a_folder_rereads_it() {
        let tmp = TempDir::new("reopen");
        tmp.mkdir("src");
        let src = tmp.0.join("src");
        let mut tree = Tree::new(tmp.0.clone());
        tree.toggle(&src);
        assert_eq!(tree.rows().len(), 1);
        tree.toggle(&src);
        fs::File::create(src.join("new.rs")).unwrap();
        tree.toggle(&src);
        assert_eq!(tree.rows().len(), 2, "reopened folder shows the new file");
    }

    /// Poll `drain_events` until it reports a change or the deadline passes.
    fn wait_for_change(tree: &mut Tree) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if tree.drain_events() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn watcher_picks_up_creates_and_deletes_in_expanded_dirs() {
        let tmp = TempDir::new("watch");
        tmp.mkdir("src");
        let src = tmp.0.join("src");
        let mut tree = Tree::new(tmp.0.clone());
        tree.expand(&src);
        assert_eq!(tree.rows().len(), 1);
        assert!(!tree.drain_events(), "idle tree reports no change");

        fs::File::create(src.join("new.rs")).unwrap();
        assert!(wait_for_change(&mut tree), "create in expanded dir");
        assert_eq!(tree.rows().len(), 2);

        fs::remove_file(src.join("new.rs")).unwrap();
        assert!(wait_for_change(&mut tree), "delete in expanded dir");
        assert_eq!(tree.rows().len(), 1);

        tmp.touch("root.txt");
        assert!(wait_for_change(&mut tree), "create in root");
        assert_eq!(tree.rows().len(), 2);
    }

    #[test]
    fn collapsed_dirs_are_not_watched() {
        let tmp = TempDir::new("unwatched");
        tmp.mkdir("src");
        let src = tmp.0.join("src");
        let mut tree = Tree::new(tmp.0.clone());
        tree.expand(&src);
        tree.collapse(&src);
        let _ = tree.rows();
        let _ = tree.drain_events();
        assert_eq!(tree.watched.len(), 1, "only the root stays watched");
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_sorted_and_expand_like_directories() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new("dir-symlink");
        tmp.mkdir("real");
        fs::write(tmp.0.join("real/child.txt"), "child").unwrap();
        fs::write(tmp.0.join("plain.txt"), "file").unwrap();
        symlink(tmp.0.join("real"), tmp.0.join("linked")).unwrap();

        let mut tree = Tree::new(tmp.0.clone());
        let rows = tree.rows();
        let linked = rows.iter().find(|row| row.name == "linked").unwrap();
        assert!(linked.is_dir);
        assert!(linked.is_symlink);
        let plain = rows.iter().position(|row| row.name == "plain.txt").unwrap();
        let linked_index = rows.iter().position(|row| row.name == "linked").unwrap();
        assert!(linked_index < plain, "directory links sort with folders");

        tree.expand(&tmp.0.join("linked"));
        assert!(
            tree.rows()
                .iter()
                .any(|row| row.path == tmp.0.join("linked/child.txt") && row.depth == 1)
        );
    }

    #[test]
    fn unreadable_or_missing_dir_is_empty() {
        let mut tree = Tree::new(std::env::temp_dir().join("aa-filetree-does-not-exist"));
        assert!(tree.rows().is_empty());
    }

    #[test]
    fn root_name_uses_final_component() {
        let tmp = TempDir::new("rootname");
        let tree = Tree::new(tmp.0.clone());
        assert!(tree.root_name().starts_with("aa-filetree-"));
    }
}
