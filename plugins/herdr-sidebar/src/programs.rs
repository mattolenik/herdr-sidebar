//! Per-filetype programs: `[[preview]]` rules whose command renders a file
//! to stdout for the built-in viewer to show (`leaf --inline`, `dawn -P`,
//! `bat`, `viu -b` …), and `[[editor]]` rules whose command opens the file in
//! its own terminal tab (`nvim`, `dawn` …). Rules come from a global
//! `preview-programs.toml` in the plugin state directory plus an optional
//! `.herdr-sidebar.toml` at the Explorer root. Every rule lists its programs
//! in PREFERENCE order and the first one that exists on this machine's PATH
//! wins, so one rules file serves machines with different tools installed.
//! This module only decides WHICH command runs; the viewer (`render_with`)
//! and the editor-tab path (`actions::open_in_editor_tab_with`) run it.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// Global rules, next to `state.json`.
pub const GLOBAL_FILE: &str = "preview-programs.toml";
/// Per-project rules at the Explorer root (consulted first, when enabled).
pub const PROJECT_FILE: &str = ".herdr-sidebar.toml";

/// Placeholders a `[[preview]]` command may use.
const PREVIEW_PLACEHOLDERS: &[&str] = &["file", "width", "height", "theme", "line"];
/// Placeholders an `[[editor]]` command may use.
const EDITOR_PLACEHOLDERS: &[&str] = &["file", "line"];

/// Seed written on first load. The plugin itself knows no tool: the three
/// active rules are suggestions the user can change or delete.
pub const TEMPLATE: &str = r#"# herdr-sidebar: per-filetype preview and editor programs.
#
# A clicked file is matched against these rules in order; the first rule whose
# pattern matches wins. A `.herdr-sidebar.toml` at the sidebar's root is
# consulted BEFORE this file when "Project preview rules" is on in Settings.
#
#   patterns   glob(s) — without a `/` they match the file NAME anywhere in
#              the tree (`*.md`); with a `/` they match the root-relative path
#              (`docs/**/*.rst`). Matching is case-insensitive.
#   command    one command line, or a list in PREFERENCE order: the first
#              program found on PATH is used. Commands run directly, never
#              through a shell.
#
# [[preview]] commands render the file to STDOUT (ANSI colours welcome) and the
# built-in preview pane shows the result — so Esc, pinning, placement and
# replacement all work as for any preview. The file is also piped on stdin for
# tools that read `-`. Placeholders: {file} (appended when absent), {width} and
# {height} (the pane body in cells), {theme} (light|dark), {line}.
# Add `mode = "external"` to give the program the preview pane's real terminal
# instead: it can then draw Kitty graphics or run interactively. The pane keeps
# what it drew until q / Esc, the next click, or a resize (which re-runs it).
# Files no rule matches use the built-in source preview.
#
# [[editor]] commands get a real terminal. `e` inside a preview runs the
# editor IN the preview pane and reloads the document when it exits; the `m`
# menu ("Edit with …") and a click while "Use editor on click" is on open it
# in its own tab. Without a rule, `e` uses the global "Custom editor…".
# Placeholders: {file} (appended when absent), {line} (a search hit's line,
# else 1).
#
# The rules below are suggested defaults — the plugin has no opinion about
# tools. Change or delete them freely.

[[preview]]
patterns = ["*.md", "*.markdown"]
command = "glow -s {theme} -w {width} -"

[[preview]]
patterns = ["*.png", "*.jpg", "*.jpeg", "*.gif", "*.webp", "*.bmp", "*.tif", "*.tiff"]
mode = "external"
command = "viu -s -h {height} {file}"

[[preview]]
patterns = ["*.pdf"]
mode = "external"
command = ["pdfterm {file}", "tdf {file}"]

# More examples:
#
# [[preview]]
# patterns = ["*.md", "*.markdown"]
# command = ["dawn -P {file}", "leaf --inline ansi:{width} {file}", "glow -s {theme} -w {width} -"]
#
# [[preview]]
# patterns = ["*.json", "*.yaml", "*.toml"]
# command = "bat --color=always --style=plain --terminal-width {width} {file}"
#
# [[editor]]
# patterns = ["*.md"]
# command = ["dawn {file}", "nvim {file}"]
"#;

/// Which table a rule came from; decides the placeholders it may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Preview,
    Editor,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Kind::Preview => "preview",
            Kind::Editor => "editor",
        }
    }

    fn placeholders(self) -> &'static [&'static str] {
        match self {
            Kind::Preview => PREVIEW_PLACEHOLDERS,
            Kind::Editor => EDITOR_PLACEHOLDERS,
        }
    }
}

/// One rule as the user wrote it: patterns plus candidate commands in
/// preference order (each a full command line or a bare program name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub patterns: Vec<String>,
    pub candidates: Vec<String>,
    /// `[[preview]]` only: `mode = "external"` — the program gets the
    /// preview pane's real terminal (Kitty graphics, interactive tools)
    /// instead of rendering to stdout for the viewer.
    pub external: bool,
}

/// The candidate that exists on this machine, chosen at use time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub command: String,
    /// Program basename for labels ("Edit with nvim").
    pub name: String,
    pub external: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RuleFile {
    preview: Vec<RawRule>,
    editor: Vec<RawRule>,
    /// The phase-0 table name. Kept only to tell the user to rename it.
    rule: Vec<toml::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRule {
    patterns: Vec<String>,
    pattern: Option<String>,
    command: OneOrMany,
    mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl Default for OneOrMany {
    fn default() -> Self {
        OneOrMany::Many(Vec::new())
    }
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            OneOrMany::One(one) => vec![one],
            OneOrMany::Many(many) => many,
        }
    }
}

struct Compiled {
    rule: Rule,
    /// Patterns without a `/`, matched against the file name.
    names: GlobSet,
    /// Patterns with a `/`, matched against the root-relative slash path.
    paths: GlobSet,
}

#[derive(Default)]
struct Layer {
    previews: Vec<Compiled>,
    editors: Vec<Compiled>,
}

/// The compiled rule set (project layer first, then global) plus every
/// problem found while loading, so a bad project file never disables the
/// global one and callers can surface the first error as a notice.
#[derive(Default)]
pub struct Rules {
    layers: Vec<Layer>,
    pub errors: Vec<String>,
}

impl Rules {
    /// First `[[preview]]` rule, in precedence order, matching `file`.
    pub fn preview_for(&self, root: &Path, file: &Path) -> Option<&Rule> {
        self.find(Kind::Preview, root, file)
    }

    /// First `[[editor]]` rule, in precedence order, matching `file`.
    pub fn editor_for(&self, root: &Path, file: &Path) -> Option<&Rule> {
        self.find(Kind::Editor, root, file)
    }

    fn find(&self, kind: Kind, root: &Path, file: &Path) -> Option<&Rule> {
        let name = file.file_name()?.to_string_lossy();
        let rel = file
            .strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"));
        self.layers
            .iter()
            .flat_map(|layer| match kind {
                Kind::Preview => layer.previews.iter(),
                Kind::Editor => layer.editors.iter(),
            })
            .find(|compiled| {
                compiled.names.is_match(&*name)
                    || rel.as_deref().is_some_and(|r| compiled.paths.is_match(r))
            })
            .map(|compiled| &compiled.rule)
    }

    pub fn preview_count(&self) -> usize {
        self.layers.iter().map(|layer| layer.previews.len()).sum()
    }

    pub fn editor_count(&self) -> usize {
        self.layers.iter().map(|layer| layer.editors.len()).sum()
    }

    pub fn len(&self) -> usize {
        self.preview_count() + self.editor_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Settings-row value: `error`, `none`, or `2 previews · 1 editor`.
    pub fn summary(&self) -> String {
        if self.first_error().is_some() {
            return "error".into();
        }
        let plural = |n: usize, word: &str| match n {
            1 => format!("1 {word}"),
            n => format!("{n} {word}s"),
        };
        match (self.preview_count(), self.editor_count()) {
            (0, 0) => "none".into(),
            (p, 0) => plural(p, "preview"),
            (0, e) => plural(e, "editor"),
            (p, e) => format!("{} · {}", plural(p, "preview"), plural(e, "editor")),
        }
    }

    pub fn first_error(&self) -> Option<&str> {
        self.errors.first().map(String::as_str)
    }
}

impl Rule {
    /// The first candidate whose program exists, searching this process's
    /// PATH. `Err` lists the program names tried, for the notice.
    pub fn resolve(&self) -> Result<Resolved, Vec<String>> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|dir| dir.canonicalize().ok());
        self.resolve_in(&path, &pathext(), cwd.as_deref())
    }

    fn resolve_in(
        &self,
        path: &OsStr,
        pathext: &[String],
        cwd: Option<&Path>,
    ) -> Result<Resolved, Vec<String>> {
        let mut tried = Vec::new();
        for candidate in &self.candidates {
            let Some(program) = crate::actions::split_editor_command(candidate)
                .ok()
                .and_then(|argv| argv.into_iter().next())
            else {
                continue;
            };
            tried.push(display_name(candidate));
            if resolve_program(&program, path, pathext, cwd).is_some() {
                return Ok(Resolved {
                    command: candidate.clone(),
                    name: display_name(candidate),
                    external: self.external,
                });
            }
        }
        Err(tried)
    }
}

/// Where `program` (argv[0] of a command) lives: checked directly when it
/// carries a path separator, otherwise searched on `path`.
pub fn resolve_program(
    program: &str,
    path: &OsStr,
    pathext: &[String],
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    if has_separator(program) {
        let direct = Path::new(program);
        is_executable(direct).then(|| direct.to_path_buf())
    } else {
        find_executable_in(OsStr::new(program), path, pathext, cwd)
    }
}

/// `resolve_program` against this process's PATH and cwd.
pub fn resolve_program_here(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|dir| dir.canonicalize().ok());
    resolve_program(program, &path, &pathext(), cwd.as_deref())
}

/// Program basename of a command line: `leaf {file}` → `leaf`,
/// `C:\Tools\leaf.exe --x` → `leaf`.
pub fn display_name(command: &str) -> String {
    let program = crate::actions::split_editor_command(command)
        .ok()
        .and_then(|argv| argv.into_iter().next())
        .unwrap_or_else(|| command.trim().to_string());
    let base = Path::new(&program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or(program);
    match base
        .strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".EXE"))
    {
        Some(stem) if !stem.is_empty() => stem.to_string(),
        _ => base,
    }
}

fn has_separator(program: &str) -> bool {
    program.contains('/') || (cfg!(windows) && program.contains('\\'))
}

fn is_executable(candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        candidate
            .metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    true
}

/// Windows `PATHEXT` extensions tried for a bare program name; empty on unix.
fn pathext() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    std::env::var("PATHEXT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a bare program name against `path` (a `PATH`-style list): only
/// absolute entries count, entries inside `cwd` are skipped (Windows process
/// lookup otherwise searches the current directory first), and on unix the
/// file must carry an execute bit. On Windows a name without an extension
/// also tries each `PATHEXT` extension.
fn find_executable_in(
    name: &OsStr,
    path: &OsStr,
    pathext: &[String],
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    let base = name.to_string_lossy().into_owned();
    let mut names = vec![base.clone()];
    if !pathext.is_empty() && Path::new(&base).extension().is_none() {
        names.extend(pathext.iter().map(|ext| format!("{base}{ext}")));
    }
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .filter_map(|directory| directory.canonicalize().ok())
        .filter(|directory| !cwd.is_some_and(|cwd| directory.starts_with(cwd)))
        .flat_map(|directory| {
            names
                .iter()
                .map(move |name| directory.join(name))
                .collect::<Vec<_>>()
        })
        .find(|candidate| is_executable(candidate))
}

pub fn global_path() -> Option<PathBuf> {
    crate::state::state_dir().map(|dir| dir.join(GLOBAL_FILE))
}

pub fn project_path(root: &Path) -> PathBuf {
    root.join(PROJECT_FILE)
}

/// Load the rules for `root`: the project file first (only when
/// `include_project`), then the global file. Missing files are simply empty.
pub fn load(root: &Path, include_project: bool) -> Rules {
    // First run: seed the suggested defaults so previews work out of the box.
    let _ = ensure_global_file();
    let project = include_project.then(|| project_path(root));
    load_from(project.as_deref(), global_path().as_deref())
}

pub fn load_from(project: Option<&Path>, global: Option<&Path>) -> Rules {
    let mut rules = Rules::default();
    for file in [project, global].into_iter().flatten() {
        let label = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.display().to_string());
        match std::fs::read_to_string(file) {
            Ok(text) => rules
                .layers
                .push(parse_layer(&text, &label, &mut rules.errors)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => rules.errors.push(format!("{label}: {error}")),
        }
    }
    rules
}

fn parse_layer(text: &str, label: &str, errors: &mut Vec<String>) -> Layer {
    let file: RuleFile = match toml::from_str(text) {
        Ok(file) => file,
        Err(error) => {
            errors.push(format!("{label}: {}", toml_message(text, &error)));
            return Layer::default();
        }
    };
    if !file.rule.is_empty() {
        errors.push(format!(
            "{label}: rename [[rule]] to [[editor]] (previews now use [[preview]])"
        ));
    }
    let compile_all = |raws: Vec<RawRule>, kind: Kind, errors: &mut Vec<String>| {
        raws.into_iter()
            .enumerate()
            .filter_map(|(index, raw)| compile(raw, kind, index + 1, label, errors))
            .collect()
    };
    Layer {
        previews: compile_all(file.preview, Kind::Preview, errors),
        editors: compile_all(file.editor, Kind::Editor, errors),
    }
}

fn toml_message(text: &str, error: &toml::de::Error) -> String {
    let line = error
        .span()
        .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
    match line {
        Some(line) => format!("line {line}: {}", error.message()),
        None => error.message().to_string(),
    }
}

/// `{name}` tokens in a command that are not in `allowed`.
fn unknown_placeholders(command: &str, allowed: &[&str]) -> Vec<String> {
    let mut unknown: Vec<String> = Vec::new();
    for name in regex::Regex::new(r"\{(\w+)\}")
        .expect("static pattern")
        .captures_iter(command)
        .map(|c| c[1].to_string())
    {
        if !allowed.contains(&name.as_str()) && !unknown.contains(&name) {
            unknown.push(name);
        }
    }
    unknown
}

fn compile(
    raw: RawRule,
    kind: Kind,
    index: usize,
    label: &str,
    errors: &mut Vec<String>,
) -> Option<Compiled> {
    let table = kind.table();
    let external = match (kind, raw.mode.as_deref().map(str::trim)) {
        (_, None) | (Kind::Preview, Some("internal")) => false,
        (Kind::Preview, Some("external")) => true,
        (Kind::Preview, Some(other)) => {
            errors.push(format!(
                "{label} {table} {index}: mode must be \"internal\" or \"external\", not {other:?}"
            ));
            return None;
        }
        (Kind::Editor, Some(_)) => {
            errors.push(format!(
                "{label} {table} {index}: `mode` applies to [[preview]] rules only"
            ));
            return None;
        }
    };
    let mut patterns = raw.patterns;
    patterns.extend(raw.pattern);
    patterns.retain(|pattern| !pattern.trim().is_empty());
    if patterns.is_empty() {
        errors.push(format!("{label} {table} {index}: no patterns"));
        return None;
    }
    let mut candidates = Vec::new();
    for candidate in raw.command.into_vec() {
        match crate::actions::split_editor_command(&candidate) {
            Ok(argv) if !argv.is_empty() => {
                let unknown = unknown_placeholders(&candidate, kind.placeholders());
                if unknown.is_empty() {
                    candidates.push(candidate.trim().to_string());
                } else {
                    errors.push(format!(
                        "{label} {table} {index}: unknown placeholder {{{}}} in {candidate:?}",
                        unknown.join("}, {")
                    ));
                }
            }
            _ => errors.push(format!(
                "{label} {table} {index}: invalid command {candidate:?}"
            )),
        }
    }
    if candidates.is_empty() {
        errors.push(format!("{label} {table} {index}: no usable command"));
        return None;
    }
    let mut names = GlobSetBuilder::new();
    let mut paths = GlobSetBuilder::new();
    for pattern in &patterns {
        let glob = match GlobBuilder::new(pattern)
            .case_insensitive(true)
            .literal_separator(true)
            .build()
        {
            Ok(glob) => glob,
            Err(error) => {
                errors.push(format!(
                    "{label} {table} {index}: invalid pattern {pattern:?}: {error}"
                ));
                return None;
            }
        };
        if pattern.contains('/') {
            paths.add(glob);
        } else {
            names.add(glob);
        }
    }
    let (Ok(names), Ok(paths)) = (names.build(), paths.build()) else {
        errors.push(format!("{label} {table} {index}: invalid patterns"));
        return None;
    };
    Some(Compiled {
        rule: Rule {
            patterns,
            candidates,
            external,
        },
        names,
        paths,
    })
}

/// Create the global rules file with [`TEMPLATE`] unless it already exists.
/// `create_new` is atomic, so no state lock is needed.
pub fn ensure_global_file() -> io::Result<PathBuf> {
    let path = global_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no plugin state directory"))?;
    ensure_file(&path)?;
    Ok(path)
}

fn ensure_file(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(TEMPLATE.as_bytes())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-programs-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_exe(dir: &Path, name: &str) -> PathBuf {
        let file = dir.join(name);
        std::fs::write(&file, b"placeholder").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        file
    }

    fn parse(text: &str) -> Rules {
        let mut rules = Rules::default();
        let layer = parse_layer(text, "test.toml", &mut rules.errors);
        rules.layers.push(layer);
        rules
    }

    fn preview_rule(text: &str) -> Rule {
        let rules = parse(text);
        assert!(rules.errors.is_empty(), "{:?}", rules.errors);
        rules
            .preview_for(Path::new("/r"), Path::new("/r/x.md"))
            .unwrap()
            .clone()
    }

    const ROOT: &str = "/r";

    #[test]
    fn parses_both_tables_with_array_and_scalar_patterns_and_string_or_list_commands() {
        let rules = parse(
            r#"
            [[preview]]
            patterns = ["*.md", "*.markdown"]
            command = ["leaf --inline ansi:{width} {file}", "glow -s {theme} -w {width} -", "cat"]

            [[editor]]
            pattern = "*.rs"
            command = "nvim +{line} {file}"
            "#,
        );
        assert!(rules.errors.is_empty(), "{:?}", rules.errors);
        assert_eq!((rules.preview_count(), rules.editor_count()), (1, 1));
        let root = Path::new(ROOT);
        let md = rules
            .preview_for(root, Path::new("/r/a/b.markdown"))
            .unwrap();
        assert_eq!(md.candidates.len(), 3);
        assert!(
            rules
                .editor_for(root, Path::new("/r/a/b.markdown"))
                .is_none()
        );
        let rs = rules.editor_for(root, Path::new("/r/main.rs")).unwrap();
        assert_eq!(rs.patterns, vec!["*.rs"]);
        assert!(rules.preview_for(root, Path::new("/r/main.rs")).is_none());
        assert_eq!(rules.summary(), "1 preview · 1 editor");
    }

    #[test]
    fn preview_mode_is_validated_and_only_for_previews() {
        let rules = parse(
            r#"
            [[preview]]
            patterns = ["*.png"]
            mode = "external"
            command = "viu {file}"

            [[preview]]
            patterns = ["*.md"]
            mode = "internal"
            command = "glow"

            [[preview]]
            patterns = ["*.svg"]
            mode = "sideways"
            command = "x"

            [[editor]]
            patterns = ["*.md"]
            mode = "external"
            command = "vi"
            "#,
        );
        let root = Path::new(ROOT);
        assert!(
            rules
                .preview_for(root, Path::new("/r/a.png"))
                .unwrap()
                .external
        );
        assert!(
            !rules
                .preview_for(root, Path::new("/r/a.md"))
                .unwrap()
                .external
        );
        assert!(rules.preview_for(root, Path::new("/r/a.svg")).is_none());
        assert!(rules.editor_for(root, Path::new("/r/a.md")).is_none());
        assert_eq!(rules.errors.len(), 2, "{:?}", rules.errors);
        assert!(rules.errors[0].contains("mode must be"));
        assert!(rules.errors[1].contains("[[preview]] rules only"));
    }

    #[test]
    fn summary_wording() {
        assert_eq!(parse("").summary(), "none");
        assert_eq!(
            parse("[[preview]]\npatterns=[\"*\"]\ncommand=\"cat\"\n[[preview]]\npatterns=[\"*\"]\ncommand=\"cat\"\n").summary(),
            "2 previews"
        );
        assert_eq!(
            parse("[[editor]]\npatterns=[\"*\"]\ncommand=\"vi\"\n").summary(),
            "1 editor"
        );
        assert_eq!(parse("[[rule]]\npatterns=[\"*\"]\n").summary(), "error");
    }

    #[test]
    fn legacy_rule_table_is_a_rename_error() {
        let rules = parse("[[rule]]\npatterns = [\"*.md\"]\ncommand = \"cat\"\n");
        assert!(rules.is_empty());
        assert_eq!(rules.errors.len(), 1);
        assert!(
            rules.errors[0].contains("rename [[rule]] to [[editor]]"),
            "{}",
            rules.errors[0]
        );
    }

    #[test]
    fn unknown_placeholders_are_rejected_per_table() {
        let rules = parse(
            r#"
            [[preview]]
            patterns = ["*.md"]
            command = ["leaf --inline ansi:{witdh} {file}", "cat {file}"]

            [[editor]]
            patterns = ["*.md"]
            command = "nvim --columns {width} {file}"
            "#,
        );
        assert_eq!(rules.preview_count(), 1, "{:?}", rules.errors);
        assert_eq!(rules.editor_count(), 0);
        assert!(
            rules
                .errors
                .iter()
                .any(|e| e.contains("unknown placeholder {witdh}"))
        );
        assert!(
            rules
                .errors
                .iter()
                .any(|e| e.contains("unknown placeholder {width}"))
        );
        // JSON-ish braces that are not identifiers are left alone.
        assert!(unknown_placeholders("jq '{a: .b}' {file}", PREVIEW_PLACEHOLDERS).is_empty());
    }

    #[test]
    fn missing_files_yield_empty_rules_without_errors() {
        let dir = tmp("missing");
        let rules = load_from(Some(&dir.join("a.toml")), Some(&dir.join("b.toml")));
        assert!(rules.is_empty());
        assert!(rules.errors.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bad_toml_reports_the_file_and_keeps_the_other_layer() {
        let dir = tmp("bad");
        let project = dir.join(PROJECT_FILE);
        let global = dir.join(GLOBAL_FILE);
        std::fs::write(&project, "[[preview]]\npatterns = [\"*.md\"\n").unwrap();
        std::fs::write(
            &global,
            "[[preview]]\npatterns = [\"*.md\"]\ncommand = \"cat\"\n",
        )
        .unwrap();
        let rules = load_from(Some(&project), Some(&global));
        assert_eq!(rules.len(), 1);
        assert_eq!(rules.errors.len(), 1);
        assert!(
            rules.errors[0].starts_with(".herdr-sidebar.toml: line"),
            "{}",
            rules.errors[0]
        );
        assert!(rules.preview_for(&dir, &dir.join("x.md")).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_candidates_are_dropped_and_rules_without_usable_parts_are_skipped() {
        let rules = parse(
            r#"
            [[preview]]
            patterns = ["*.md"]
            command = ["\"unterminated", "", "cat"]

            [[preview]]
            patterns = ["*.rs"]
            command = ""

            [[preview]]
            command = "cat"

            [[preview]]
            patterns = ["*.txt"]
            command = "less"
            "#,
        );
        assert_eq!(rules.len(), 2);
        let root = Path::new(ROOT);
        assert_eq!(
            rules
                .preview_for(root, Path::new("/r/a.md"))
                .unwrap()
                .candidates,
            vec!["cat"]
        );
        assert!(rules.preview_for(root, Path::new("/r/a.rs")).is_none());
        assert!(rules.preview_for(root, Path::new("/r/a.txt")).is_some());
        assert_eq!(rules.errors.len(), 5, "{:?}", rules.errors);
        assert!(
            rules
                .errors
                .iter()
                .any(|e| e.contains("preview 2: no usable command"))
        );
        assert!(
            rules
                .errors
                .iter()
                .any(|e| e.contains("preview 3: no patterns"))
        );
    }

    #[test]
    fn name_patterns_match_anywhere_and_path_patterns_match_the_relative_path() {
        let rules = parse(
            r#"
            [[preview]]
            patterns = ["docs/*.md"]
            command = "a"

            [[preview]]
            patterns = ["*.md"]
            command = "b"
            "#,
        );
        let root = Path::new(ROOT);
        let at = |p: &str| {
            rules
                .preview_for(root, Path::new(p))
                .map(|r| r.candidates[0].as_str())
        };
        assert_eq!(at("/r/docs/x.md"), Some("a"));
        // literal_separator: `*` does not cross a directory.
        assert_eq!(at("/r/docs/sub/x.md"), Some("b"));
        assert_eq!(at("/r/x.md"), Some("b"));
        assert_eq!(at("/r/deep/er/x.md"), Some("b"));
        // Case-insensitive on every platform.
        assert_eq!(at("/r/README.MD"), Some("b"));
        // Outside the root there is no relative path, but names still match.
        assert_eq!(at("/elsewhere/x.md"), Some("b"));
        assert_eq!(at("/elsewhere/docs/x.md"), Some("b"));
    }

    #[test]
    fn project_rules_win_and_first_match_wins_within_a_layer() {
        let dir = tmp("precedence");
        let project = dir.join(PROJECT_FILE);
        let global = dir.join(GLOBAL_FILE);
        std::fs::write(
            &project,
            "[[preview]]\npatterns = [\"*.md\"]\ncommand = \"project\"\n",
        )
        .unwrap();
        std::fs::write(
            &global,
            "[[preview]]\npatterns = [\"*.md\"]\ncommand = \"global-first\"\n[[preview]]\npatterns = [\"*.md\"]\ncommand = \"global-second\"\n",
        )
        .unwrap();
        let both = load_from(Some(&project), Some(&global));
        assert_eq!(
            both.preview_for(&dir, &dir.join("x.md"))
                .unwrap()
                .candidates,
            vec!["project"]
        );
        let global_only = load_from(None, Some(&global));
        assert_eq!(
            global_only
                .preview_for(&dir, &dir.join("x.md"))
                .unwrap()
                .candidates,
            vec!["global-first"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn display_name_is_the_program_basename() {
        assert_eq!(display_name("leaf {file}"), "leaf");
        assert_eq!(display_name("leaf"), "leaf");
        assert_eq!(
            display_name(r#"C:\Tools\leaf.exe --x"#),
            if cfg!(windows) {
                "leaf"
            } else {
                r#"C:\Tools\leaf"#
            }
        );
        assert_eq!(display_name("/opt/tools/leaf.EXE"), "leaf");
        assert_eq!(display_name("\"/opt/my tool/bin/view\" {file}"), "view");
        assert_eq!(display_name("\"unterminated"), "\"unterminated");
    }

    #[test]
    fn resolution_prefers_the_first_candidate_found_on_path() {
        let dir = tmp("resolve");
        let bin_a = dir.join("a");
        let bin_b = dir.join("b");
        std::fs::create_dir_all(&bin_a).unwrap();
        std::fs::create_dir_all(&bin_b).unwrap();
        let exe = |name: &str| {
            if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_string()
            }
        };
        let mdview = fake_exe(&bin_b, &exe("mdview"));
        fake_exe(&bin_a, &exe("cat"));
        let path = std::env::join_paths([&bin_a, &bin_b]).unwrap();
        let pathext = vec![".exe".to_string()];

        let first = preview_rule(
            "[[preview]]\npatterns = [\"*.md\"]\ncommand = [\"leaf\", \"mdview {file}\", \"cat\"]\n",
        );
        let resolved = first.resolve_in(&path, &pathext, None).unwrap();
        assert_eq!(resolved.command, "mdview {file}");
        assert_eq!(resolved.name, "mdview");

        let none =
            preview_rule("[[preview]]\npatterns = [\"*.md\"]\ncommand = [\"leaf\", \"glow\"]\n");
        assert_eq!(
            none.resolve_in(&path, &pathext, None).unwrap_err(),
            vec!["leaf", "glow"]
        );

        // A directory inside `cwd` is never searched.
        assert!(
            preview_rule("[[preview]]\npatterns = [\"*\"]\ncommand = \"cat\"\n")
                .resolve_in(&path, &pathext, Some(&dir.canonicalize().unwrap()))
                .is_err()
        );

        // A relative PATH entry is ignored.
        let relative = std::env::join_paths([Path::new("a")]).unwrap();
        assert!(
            preview_rule("[[preview]]\npatterns = [\"*\"]\ncommand = \"cat\"\n")
                .resolve_in(&relative, &pathext, None)
                .is_err()
        );

        // argv[0] with a separator is checked directly, not searched.
        let direct = format!("\"{}\" --wait", mdview.display());
        let rule_direct = preview_rule(&format!(
            "[[preview]]\npatterns = [\"*.md\"]\ncommand = {direct:?}\n"
        ));
        let empty = std::env::join_paths::<[&Path; 0], _>([]).unwrap();
        assert!(rule_direct.resolve_in(&empty, &pathext, None).is_ok());
        let missing = preview_rule(&format!(
            "[[preview]]\npatterns = [\"*.md\"]\ncommand = \"{}/nope\"\n",
            bin_a.display().to_string().replace('\\', "/")
        ));
        assert!(missing.resolve_in(&path, &pathext, None).is_err());

        #[cfg(unix)]
        {
            // A non-executable file does not count.
            std::fs::write(bin_a.join("plain"), b"x").unwrap();
            assert!(
                preview_rule("[[preview]]\npatterns = [\"*\"]\ncommand = \"plain\"\n")
                    .resolve_in(&path, &pathext, None)
                    .is_err()
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn find_executable_tries_pathext_only_for_bare_names() {
        let dir = tmp("pathext");
        fake_exe(&dir, "tool.cmd");
        let path = std::env::join_paths([&dir]).unwrap();
        let pathext = vec![".exe".to_string(), ".cmd".to_string()];
        assert_eq!(
            find_executable_in(OsStr::new("tool"), &path, &pathext, None),
            Some(dir.canonicalize().unwrap().join("tool.cmd"))
        );
        assert_eq!(
            find_executable_in(OsStr::new("tool"), &path, &[], None),
            None
        );
        assert_eq!(
            find_executable_in(OsStr::new("tool.cmd"), &path, &[], None),
            Some(dir.canonicalize().unwrap().join("tool.cmd"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ensure_file_seeds_the_template_once_and_its_defaults_parse() {
        let dir = tmp("seed");
        let file = dir.join("nested").join(GLOBAL_FILE);
        ensure_file(&file).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), TEMPLATE);
        std::fs::write(&file, "# edited\n").unwrap();
        ensure_file(&file).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# edited\n");
        let seeded = parse(TEMPLATE);
        assert!(seeded.errors.is_empty(), "{:?}", seeded.errors);
        assert_eq!((seeded.preview_count(), seeded.editor_count()), (3, 0));
        let root = Path::new("/r");
        assert!(
            seeded
                .preview_for(root, Path::new("/r/a.PDF"))
                .unwrap()
                .external
        );
        assert!(
            !seeded
                .preview_for(root, Path::new("/r/a.md"))
                .unwrap()
                .external
        );
        // The commented examples themselves are valid once uncommented.
        let uncommented: String = TEMPLATE
            .lines()
            .filter(|l| {
                ["# [[preview]]", "# [[editor]]"].contains(l)
                    || l.starts_with("# patterns = ")
                    || l.starts_with("# pattern = ")
                    || l.starts_with("# mode = ")
                    || l.starts_with("# command = ")
            })
            .map(|l| format!("{}\n", &l[2..]))
            .collect();
        let examples = parse(&uncommented);
        assert!(examples.errors.is_empty(), "{:?}", examples.errors);
        assert_eq!((examples.preview_count(), examples.editor_count()), (2, 1));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
