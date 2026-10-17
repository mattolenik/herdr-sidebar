//! Pure decisions behind the stacked Explorer view: which directories become
//! roots, how roots group by repository, how rows split between groups.

use std::path::{Path, PathBuf};

fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) { s.to_lowercase() } else { s.to_string() }
}

/// Dedupe and drop any path that has another entry as a proper ancestor. Output sorted.
pub fn fold_roots(cwds: &[PathBuf]) -> Vec<PathBuf> {
    let mut keyed: Vec<(String, &PathBuf)> = cwds.iter().map(|p| (norm(p), p)).collect();
    // Shortest original first on key ties, so "/w/proj" beats "/w/proj/".
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.as_os_str().len().cmp(&b.1.as_os_str().len())));
    keyed.dedup_by(|b, a| a.0 == b.0);
    let keys: Vec<&String> = keyed.iter().map(|(k, _)| k).collect();
    keyed
        .iter()
        .filter(|(k, _)| !keys.iter().any(|o| k.len() > o.len() && k.starts_with(o.as_str()) && k.as_bytes()[o.len()] == b'/'))
        .map(|(_, p)| (*p).clone())
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub key: PathBuf,
    pub repo: bool,
    pub roots: Vec<PathBuf>,
}

impl Group {
    /// Last path component of the key, "/" for a bare root.
    pub fn name(&self) -> String {
        self.key.file_name().map_or_else(|| "/".into(), |n| n.to_string_lossy().into_owned())
    }
}

/// `owner(root)` = repository root containing `root`, or None. Groups that appear in
/// `previous` keep that relative order first; new groups follow sorted by key.
pub fn group_roots(roots: &[PathBuf], owner: impl Fn(&Path) -> Option<PathBuf>, previous: &[Group]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for r in roots {
        let o = owner(r);
        let (key, repo) = match o {
            Some(k) => (k, true),
            None => (r.clone(), false),
        };
        match groups.iter_mut().find(|g| g.key == key) {
            Some(g) => g.roots.push(r.clone()),
            None => groups.push(Group { key, repo, roots: vec![r.clone()] }),
        }
    }
    for g in &mut groups {
        g.roots.sort();
    }
    groups.sort_by(|a, b| {
        let pos = |g: &Group| previous.iter().position(|p| p.key == g.key);
        match (pos(a), pos(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.key.cmp(&b.key),
        }
    });
    groups
}

pub const MIN_BODY_ROWS: u16 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allocation {
    /// Rows per group INCLUDING its divider (groups after the first) and header.
    pub heights: Vec<u16>,
    /// Groups shown folded, manual folds included.
    pub folded: Vec<bool>,
}

/// Overhead rows of group i: header (1) + divider (1 if i > 0).
pub fn chrome_rows(index: usize) -> u16 {
    1 + u16::from(index > 0)
}

/// `folded[i]` = user-folded. `dragged` = previous heights to honor when they still fit;
/// else equal split. Auto-folds from the bottom until every unfolded group has
/// at least MIN_BODY_ROWS of body.
pub fn allocate(folded: &[bool], body_rows: u16, dragged: Option<&[u16]>) -> Allocation {
    let mut a = equal_split(folded, body_rows);
    // Checked against the COMPUTED fold pattern (manual and auto folds), so a drag still
    // holds while some other group is auto-folded.
    if let Some(d) = dragged
        && d.len() == a.folded.len()
        && d.iter().sum::<u16>() == body_rows
        && (0..d.len()).all(|i| if a.folded[i] { d[i] == chrome_rows(i) } else { d[i] >= chrome_rows(i) + MIN_BODY_ROWS })
    {
        a.heights = d.to_vec();
    }
    a
}

fn equal_split(folded: &[bool], body_rows: u16) -> Allocation {
    let n = folded.len();
    let mut fold = folded.to_vec();
    let chrome_total: u16 = (0..n).map(chrome_rows).sum();
    if body_rows < chrome_total {
        let mut left = body_rows;
        let heights = (0..n)
            .map(|i| {
                let h = chrome_rows(i).min(left);
                left -= h;
                h
            })
            .collect();
        return Allocation { heights, folded: vec![true; n] };
    }
    let extra = body_rows - chrome_total;
    loop {
        let k = fold.iter().filter(|f| !**f).count() as u16;
        if k > 0 && extra / k < MIN_BODY_ROWS {
            let last = fold.iter().rposition(|f| !*f).unwrap();
            fold[last] = true;
            continue;
        }
        let mut idx = 0u16;
        let heights = (0..n)
            .map(|i| {
                if fold[i] {
                    return chrome_rows(i);
                }
                let body = extra / k + u16::from(idx < extra % k);
                idx += 1;
                chrome_rows(i) + body
            })
            .collect();
        return Allocation { heights, folded: fold };
    }
}

/// Move `delta` rows across the boundary ABOVE group `boundary` (1-based).
/// Never shrinks either unfolded body below MIN_BODY_ROWS.
pub fn drag(alloc: &mut Allocation, boundary: usize, delta: i32) {
    if boundary == 0 || boundary >= alloc.heights.len() || alloc.folded[boundary - 1] || alloc.folded[boundary] {
        return;
    }
    let (a, b) = (boundary - 1, boundary);
    let spare = |i: usize| i32::from(alloc.heights[i]) - i32::from(chrome_rows(i) + MIN_BODY_ROWS);
    let moved = if delta >= 0 { delta.min(spare(b)) } else { -((-delta).min(spare(a))) };
    alloc.heights[a] = (i32::from(alloc.heights[a]) + moved) as u16;
    alloc.heights[b] = (i32::from(alloc.heights[b]) - moved) as u16;
}

/// The tab whose panes feed the stack: the origin tab recorded on a preview viewer in
/// our tab when that tab still exists, else our own tab. None if our pane is not listed.
pub fn stack_tab(pane_list_json: &str, my_pane_id: &str) -> Option<String> {
    let mine = crate::launch::tab_of(pane_list_json, my_pane_id);
    if mine.is_empty() {
        return None;
    }
    let live = crate::launch::live_tabs(pane_list_json);
    let v: serde_json::Value = serde_json::from_str(pane_list_json.trim_start_matches('\u{feff}')).ok()?;
    let origin = v["result"]["panes"].as_array()?.iter().find_map(|p| {
        let o = p["tokens"][crate::viewer::TOKEN_ORIGIN_TAB].as_str()?;
        (p["tab_id"].as_str() == Some(&mine) && live.contains(o)).then(|| o.to_string())
    });
    Some(origin.unwrap_or(mine))
}

/// Folded roots of the eligible panes in `stack_tab`. None when the snapshot cannot be
/// read or does not list our pane; Some(empty) when the tab has no eligible pane.
pub fn stack_roots(pane_list_json: &str, my_pane_id: &str) -> Option<Vec<PathBuf>> {
    let tab = stack_tab(pane_list_json, my_pane_id)?;
    let cwds: Vec<PathBuf> = crate::launch::sibling_cwds_in_tab(pane_list_json, &tab)
        .into_iter()
        .map(|s| PathBuf::from(s.cwd))
        .collect();
    Some(fold_roots(&cwds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    fn p(s: &str) -> PathBuf { PathBuf::from(s) }
    fn panes(list: &[(&str, &str, Option<&str>, &str, &[(&str, &str)])]) -> String {
        let panes: Vec<_> = list.iter().map(|(id, tab, cwd, label, toks)| {
            let tokens: serde_json::Map<_, _> = toks.iter().map(|(k, v)| (k.to_string(), serde_json::json!(v))).collect();
            serde_json::json!({"pane_id": id, "tab_id": tab, "foreground_cwd": cwd, "focused": false, "label": label, "tokens": tokens})
        }).collect();
        serde_json::json!({"result": {"panes": panes}}).to_string()
    }
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
        let a = allocate(&[false, false], 20, None);
        assert_eq!(a.heights, vec![10, 10]);
        assert_eq!(a.folded, vec![false, false]);
    }
    #[test] fn allocate_folds_from_the_bottom_when_short() {
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
        let a = allocate(&[false, false], 20, Some(&[3, 17]));
        assert_eq!(a.heights, vec![10, 10]);
    }
    #[test] fn allocate_honors_dragged_heights_with_an_auto_folded_group() {
        assert_eq!(allocate(&[false; 3], 12, None).heights, vec![5, 5, 2]);
        let a = allocate(&[false; 3], 12, Some(&[4, 6, 2]));
        assert_eq!(a.heights, vec![4, 6, 2]);
        assert_eq!(a.folded, vec![false, false, true]);
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
        assert_eq!(a.heights, vec![15, 5]);
        drag(&mut a, 1, -50);
        assert_eq!(a.heights, vec![4, 16]);
    }
}
