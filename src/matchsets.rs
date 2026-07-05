use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use etcetera::BaseStrategy;
use faccess::PathExt;
use globset::GlobBuilder;
use kdl::{KdlDocument, KdlNode};
use regex::bytes::{Regex, RegexBuilder};

use crate::bash_cond;
use crate::config::Config;
use crate::dir_entry::DirEntry;
use crate::filesystem;

static BUILTIN_MATCHSETS: &str = include_str!("matchset_builtins.kdl");

pub struct SelectedMatchsets {
    pub include: Vec<CompiledMatchset>,
    pub exclude: Vec<CompiledMatchset>,
}

#[derive(Clone)]
pub struct CompiledMatchset {
    name: String,
    provenance: Provenance,
    shadows: Vec<Provenance>,
    summary: String,
    clauses: Vec<CompiledMatchClause>,
}

#[derive(Clone)]
enum Provenance {
    Builtin,
    UserFile(PathBuf),
    ExtraFile(PathBuf),
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builtin => write!(f, "builtin"),
            Self::UserFile(path) => write!(f, "user {}", path.display()),
            Self::ExtraFile(path) => write!(f, "--matchset-file {}", path.display()),
        }
    }
}

#[derive(Clone)]
struct CompiledMatchClause {
    entry_type: EntryType,
    subject: Subject,
    matcher: Matcher,
}

/// A conjunction of optional type predicates; an entry must satisfy every
/// present predicate. Parsed from a clause's KDL type annotation, e.g.
/// `(f,-e)`. The default (no annotation) matches every entry kind.
#[derive(Clone, Default)]
struct EntryType {
    structural: Option<StructuralConstraint>,
    executable: Option<bool>,
    empty: Option<bool>,
}

#[derive(Clone)]
enum StructuralConstraint {
    Is(Structural),
    IsNot(Vec<Structural>),
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Structural {
    File,
    Directory,
    Symlink,
    Socket,
    Pipe,
    BlockDevice,
    CharDevice,
}

#[derive(Copy, Clone)]
enum Subject {
    Name,
    Path,
}

#[derive(Copy, Clone)]
enum Mode {
    Full,
    Sub,
}

#[derive(Copy, Clone)]
enum PatternKind {
    Literal,
    Glob,
    Regex,
}

#[derive(Clone)]
enum Matcher {
    Regex(Regex),
    Bash(bash_cond::Condition),
    /// A pattern led by a location variable (`$<home>/...`, `$<vroot>/...`):
    /// the last `depth` components of the entry's absolute path must match
    /// `tail`, and the remaining prefix must be the anchor location.
    Anchored {
        anchor: Anchor,
        tail: Regex,
        depth: usize,
    },
}

#[derive(Clone)]
enum Anchor {
    /// The user's home directory (as reported plus canonicalized, so both
    /// spellings of a symlinked home match).
    Home(Vec<PathBuf>),
    /// Any volume root: a path with no parent, or one whose device number
    /// differs from its parent's.
    VolumeRoot,
}

impl CompiledMatchset {
    pub fn matches(&self, entry: &DirEntry, context_dir: &Path, config: &Config) -> Result<bool> {
        for clause in &self.clauses {
            if clause.matches(entry, context_dir, config)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl CompiledMatchClause {
    fn matches(&self, entry: &DirEntry, context_dir: &Path, config: &Config) -> Result<bool> {
        if !self.entry_type.matches(entry) {
            return Ok(false);
        }

        match &self.matcher {
            Matcher::Regex(regex) => {
                let subject = self.subject.resolve(entry.path());
                Ok(regex.is_match(&filesystem::osstr_to_bytes(subject)))
            }
            Matcher::Bash(condition) => condition.evaluate(entry.path(), context_dir, config),
            Matcher::Anchored {
                anchor,
                tail,
                depth,
            } => Ok(matches_anchored(entry.path(), anchor, tail, *depth)),
        }
    }
}

fn matches_anchored(path: &Path, anchor: &Anchor, tail: &Regex, depth: usize) -> bool {
    let Ok(absolute) = std::path::absolute(path) else {
        return false;
    };
    let absolute = normalize_lexically(&absolute);

    let mut prefix = absolute.as_path();
    for _ in 0..depth {
        match prefix.parent() {
            Some(parent) => prefix = parent,
            None => return false,
        }
    }

    let tail_subject = absolute
        .strip_prefix(prefix)
        .expect("prefix is an ancestor of the absolute path");
    if !tail.is_match(&filesystem::osstr_to_bytes(tail_subject.as_os_str())) {
        return false;
    }

    match anchor {
        Anchor::Home(homes) => homes.iter().any(|home| home.as_path() == prefix),
        Anchor::VolumeRoot => is_volume_root(prefix),
    }
}

/// Resolve `.` and `..` components lexically (no filesystem access).
/// Anchoring is lexical by design; symlinked spellings are handled by
/// matching against both the reported and canonicalized anchor paths.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component);
                }
            }
            _ => normalized.push(component),
        }
    }
    normalized
}

fn is_volume_root(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        // Filesystem root, drive root (C:\), or UNC share root.
        return true;
    };

    #[cfg(unix)]
    {
        fn device_num(path: &Path) -> std::io::Result<u64> {
            use std::os::unix::fs::MetadataExt;
            path.metadata().map(|metadata| metadata.dev())
        }
        match (device_num(path), device_num(parent)) {
            (Ok(own), Ok(parents)) => own != parents,
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        // On Windows, only drive and UNC roots (no parent) are recognized;
        // junction-style mount points are not detected.
        let _ = parent;
        false
    }
}

impl EntryType {
    fn parse(annotation: &str) -> Result<Self> {
        let mut executable = None;
        let mut empty = None;
        let mut positive_structural: Option<Structural> = None;
        let mut negated_structurals: Vec<Structural> = Vec::new();

        for token in annotation.split(',') {
            let token = token.trim();
            if token.is_empty() {
                bail!("empty type predicate in '({annotation})'");
            }
            let (name, positive) = match token.strip_prefix('-') {
                Some(rest) => (rest, false),
                None => (token, true),
            };
            if let Some(structural) = Structural::parse(name) {
                if positive {
                    if positive_structural.is_some() {
                        bail!(
                            "type constraint '({annotation})' has more than one positive structural type"
                        );
                    }
                    positive_structural = Some(structural);
                } else if negated_structurals.contains(&structural) {
                    bail!("duplicate type predicate '{token}' in '({annotation})'");
                } else {
                    negated_structurals.push(structural);
                }
            } else {
                let slot = match name {
                    "x" | "executable" => &mut executable,
                    "e" | "empty" => &mut empty,
                    _ => bail!("invalid type predicate '{name}' in '({annotation})'"),
                };
                if slot.is_some() {
                    bail!("conflicting or duplicate type predicate '{token}' in '({annotation})'");
                }
                *slot = Some(positive);
            }
        }

        let structural = match (positive_structural, negated_structurals.is_empty()) {
            (Some(_), false) => bail!(
                "type constraint '({annotation})' combines a positive structural type with negated ones"
            ),
            (Some(structural), true) => Some(StructuralConstraint::Is(structural)),
            (None, false) => Some(StructuralConstraint::IsNot(negated_structurals)),
            (None, true) => None,
        };

        Ok(Self {
            structural,
            executable,
            empty,
        })
    }

    fn matches(&self, entry: &DirEntry) -> bool {
        if let Some(constraint) = &self.structural {
            let file_type = entry.file_type();
            let matched = match constraint {
                StructuralConstraint::Is(structural) => {
                    file_type.is_some_and(|ft| structural.matches(ft))
                }
                StructuralConstraint::IsNot(list) => {
                    !file_type.is_some_and(|ft| list.iter().any(|s| s.matches(ft)))
                }
            };
            if !matched {
                return false;
            }
        }
        if let Some(positive) = self.executable
            && entry.path().executable() != positive
        {
            return false;
        }
        if let Some(positive) = self.empty
            && filesystem::is_empty(entry) != positive
        {
            return false;
        }
        true
    }
}

impl Structural {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "f" | "file" => Some(Self::File),
            "d" | "dir" | "directory" => Some(Self::Directory),
            "l" | "symlink" => Some(Self::Symlink),
            "s" | "socket" => Some(Self::Socket),
            "p" | "pipe" => Some(Self::Pipe),
            "b" | "block-device" => Some(Self::BlockDevice),
            "c" | "char-device" => Some(Self::CharDevice),
            _ => None,
        }
    }

    fn matches(self, file_type: fs::FileType) -> bool {
        match self {
            Self::File => file_type.is_file(),
            Self::Directory => file_type.is_dir(),
            Self::Symlink => file_type.is_symlink(),
            Self::Socket => filesystem::is_socket(file_type),
            Self::Pipe => filesystem::is_pipe(file_type),
            Self::BlockDevice => filesystem::is_block_device(file_type),
            Self::CharDevice => filesystem::is_char_device(file_type),
        }
    }
}

impl Subject {
    fn resolve(self, path: &Path) -> &OsStr {
        match self {
            Self::Name => path.file_name().unwrap_or(path.as_os_str()),
            Self::Path => filesystem::strip_current_dir(path).as_os_str(),
        }
    }
}

impl Mode {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "full" => Ok(Self::Full),
            "sub" => Ok(Self::Sub),
            _ => bail!("invalid matchset mode '{value}'"),
        }
    }
}

impl PatternKind {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "literal" => Ok(Self::Literal),
            "glob" => Ok(Self::Glob),
            "regex" => Ok(Self::Regex),
            _ => bail!("invalid matchset pattern kind '{value}'"),
        }
    }
}

pub fn load_selected(
    include_names: &[String],
    exclude_names: &[String],
    matchset_files: &[PathBuf],
    no_user_matchsets: bool,
) -> Result<SelectedMatchsets> {
    if include_names.is_empty() && exclude_names.is_empty() && matchset_files.is_empty() {
        return Ok(SelectedMatchsets {
            include: Vec::new(),
            exclude: Vec::new(),
        });
    }

    let registry = Registry::assemble(matchset_files, no_user_matchsets)?;

    // Every mentioned name must be a known set, even ones only removed
    // again: a no-op removal is fine ("ensure this is not selected"), a
    // misspelled one should fail loudly.
    registry.validate_names(include_names.iter().chain(exclude_names))?;

    Ok(SelectedMatchsets {
        include: registry.select(&resolve_selection(include_names)?)?,
        exclude: registry.select(&resolve_selection(exclude_names)?)?,
    })
}

/// Print the assembled registry (for `--list-matchsets`).
pub fn print_list(matchset_files: &[PathBuf], no_user_matchsets: bool) -> Result<()> {
    let registry = Registry::assemble(matchset_files, no_user_matchsets)?;

    let mut sets: Vec<&CompiledMatchset> = registry.sets.values().collect();
    sets.sort_by(|a, b| a.name.cmp(&b.name));

    let rows: Vec<(&str, String, &str)> = sets
        .iter()
        .map(|set| {
            let mut source = set.provenance.to_string();
            if !set.shadows.is_empty() {
                let shadowed = set
                    .shadows
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                source.push_str(&format!(" (shadows {shadowed})"));
            }
            (set.name.as_str(), source, set.summary.as_str())
        })
        .collect();

    let name_width = rows
        .iter()
        .map(|(name, _, _)| name.len())
        .chain(["NAME".len()])
        .max()
        .unwrap();
    let source_width = rows
        .iter()
        .map(|(_, source, _)| source.len())
        .chain(["SOURCE".len()])
        .max()
        .unwrap();

    println!(
        "{:<name_width$}  {:<source_width$}  CLAUSES",
        "NAME", "SOURCE"
    );
    for (name, source, summary) in rows {
        println!("{name:<name_width$}  {source:<source_width$}  {summary}");
    }

    Ok(())
}

/// Fold a command-line selection left to right: a plain name adds the set
/// (once), a name with a trailing '-' ensures it is not selected (a no-op if
/// it never was, like rg's `-T`), and a bare '-' discards the selection
/// accumulated so far. This lets a later `-m`/`-M` occurrence undo one baked
/// into a shell alias without knowing what the alias selected.
fn resolve_selection(names: &[String]) -> Result<Vec<String>> {
    let mut selected: Vec<String> = Vec::new();
    for raw in names {
        let name = raw.trim();
        if name == "-" {
            selected.clear();
        } else if let Some(base) = name.strip_suffix('-') {
            if base.is_empty() {
                bail!("matchset names must not be empty");
            }
            selected.retain(|n| n != base);
        } else if name.is_empty() {
            bail!("matchset names must not be empty");
        } else if !selected.iter().any(|n| n == name) {
            selected.push(name.to_string());
        }
    }
    Ok(selected)
}

fn user_matchsets_path() -> Option<PathBuf> {
    etcetera::choose_base_strategy()
        .ok()
        .map(|base| base.config_dir().join("fd").join("matchsets.kdl"))
}

#[derive(Default)]
struct Registry {
    sets: HashMap<String, CompiledMatchset>,
}

impl Registry {
    /// Layer the three matchset sources: builtins, then the user matchset
    /// file, then `--matchset-file` files in command-line order. Later
    /// definitions shadow earlier ones with the same set name.
    fn assemble(matchset_files: &[PathBuf], no_user_matchsets: bool) -> Result<Self> {
        let mut registry = Self::builtins()?;

        if !no_user_matchsets
            && let Some(path) = user_matchsets_path()
            && path.is_file()
        {
            let provenance = Provenance::UserFile(path.clone());
            registry.merge(Self::load(&path, provenance)?);
        }

        for path in matchset_files {
            if !path.is_file() {
                bail!("matchset file not found: {}", path.display());
            }
            let provenance = Provenance::ExtraFile(path.clone());
            registry.merge(Self::load(path, provenance)?);
        }

        Ok(registry)
    }

    fn builtins() -> Result<Self> {
        let document = BUILTIN_MATCHSETS
            .parse::<KdlDocument>()
            .context("could not parse built-in matchsets (this is a bug)")?;
        Self::parse(&document, &Provenance::Builtin)
            .context("invalid built-in matchsets (this is a bug)")
    }

    fn load(path: &Path, provenance: Provenance) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("could not read matchset file '{}'", path.display()))?;
        let document = source
            .parse::<KdlDocument>()
            .with_context(|| format!("could not parse matchset file '{}'", path.display()))?;

        Self::parse(&document, &provenance)
            .with_context(|| format!("invalid matchset file '{}'", path.display()))
    }

    fn parse(document: &KdlDocument, provenance: &Provenance) -> Result<Self> {
        let mut sets = HashMap::new();

        for node in document.nodes() {
            let name = node.name().value().to_string();
            if name.trim().is_empty() {
                bail!("matchset names must not be empty");
            }
            if name.ends_with('-') {
                bail!(
                    "matchset name '{name}' must not end with '-' \
                     (reserved for removing a set from a command-line selection)"
                );
            }
            if node.ty().is_some() {
                bail!("matchset '{name}' must not have a type annotation");
            }
            if !node.entries().is_empty() {
                bail!("matchset '{name}' must not have arguments");
            }
            if sets.contains_key(&name) {
                bail!("duplicate matchset '{name}'");
            }

            let children = node
                .children()
                .ok_or_else(|| anyhow!("matchset '{name}' must contain match clauses"))?;
            let mut clauses = Vec::new();
            let mut summaries = Vec::new();
            for child in children.nodes() {
                let (group, summary) = parse_clause_group(&name, child)?;
                clauses.extend(group);
                summaries.push(summary);
            }
            if clauses.is_empty() {
                bail!("matchset '{name}' must contain at least one pattern");
            }
            sets.insert(
                name.clone(),
                CompiledMatchset {
                    name,
                    provenance: provenance.clone(),
                    shadows: Vec::new(),
                    summary: summaries.join(", "),
                    clauses,
                },
            );
        }

        Ok(Self { sets })
    }

    fn merge(&mut self, other: Self) {
        for (name, mut set) in other.sets {
            if let Some(shadowed) = self.sets.remove(&name) {
                set.shadows = shadowed.shadows;
                set.shadows.push(shadowed.provenance);
            }
            self.sets.insert(name, set);
        }
    }

    /// Check that every raw selection token names a known set, including
    /// removals (`name-`); the bare '-' clear marker carries no name.
    fn validate_names<'a>(&self, names: impl Iterator<Item = &'a String>) -> Result<()> {
        for raw in names {
            let name = raw.trim();
            if name == "-" {
                continue;
            }
            let base = name.strip_suffix('-').unwrap_or(name);
            if !self.sets.contains_key(base) {
                bail!("unknown matchset '{base}'");
            }
        }
        Ok(())
    }

    fn select(&self, names: &[String]) -> Result<Vec<CompiledMatchset>> {
        let mut selected = Vec::new();
        let mut seen = HashSet::new();

        for name in names {
            if !seen.insert(name) {
                continue;
            }
            let set = self
                .sets
                .get(name)
                .ok_or_else(|| anyhow!("unknown matchset '{name}'"))?;
            selected.push(set.clone());
        }

        Ok(selected)
    }
}

/// Parse one clause node of the form
/// `[(<constraint>)] <name|path> <literal|glob|regex> <full|sub> { patterns }`
/// or `[(<constraint>)] bash { conditions }` into one compiled clause per
/// pattern, plus a human-readable summary for `--list-matchsets`.
fn parse_clause_group(
    set_name: &str,
    node: &KdlNode,
) -> Result<(Vec<CompiledMatchClause>, String)> {
    let entry_type = match node.ty() {
        Some(annotation) => EntryType::parse(annotation.value())
            .with_context(|| format!("invalid type constraint in matchset '{set_name}'"))?,
        None => EntryType::default(),
    };
    let annotation_prefix = node
        .ty()
        .map(|annotation| format!("({}) ", annotation.value()))
        .unwrap_or_default();

    let patterns = clause_patterns(node)
        .with_context(|| format!("invalid pattern list in matchset '{set_name}'"))?;
    if patterns.is_empty() {
        bail!("match clause in set '{set_name}' must contain at least one pattern");
    }

    match node.name().value() {
        "bash" => {
            if !node.entries().is_empty() {
                bail!("'bash' clause in set '{set_name}' takes no arguments");
            }
            let summary = format!("{} {annotation_prefix}bash", patterns.len());
            let clauses = patterns
                .into_iter()
                .map(|pattern| {
                    let matcher = bash_cond::parse_expr(&pattern, "matchset bash")
                        .and_then(|expr| bash_cond::Condition::compile(expr, true))
                        .with_context(|| {
                            format!("invalid pattern '{pattern}' in matchset '{set_name}'")
                        })?;
                    Ok(CompiledMatchClause {
                        entry_type: entry_type.clone(),
                        subject: Subject::Name,
                        matcher: Matcher::Bash(matcher),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((clauses, summary))
        }
        subject_name @ ("name" | "path") => {
            let subject = match subject_name {
                "name" => Subject::Name,
                _ => Subject::Path,
            };
            let atoms = clause_atoms(node)?;
            let [pattern_kind, mode] = atoms.as_slice() else {
                bail!(
                    "'{subject_name}' clause in set '{set_name}' must be '{subject_name} <literal|glob|regex> <full|sub>'"
                );
            };
            let summary = format!(
                "{} {annotation_prefix}{subject_name} {pattern_kind} {mode}",
                patterns.len()
            );
            let pattern_kind = PatternKind::parse(pattern_kind)
                .with_context(|| format!("invalid pattern kind in matchset '{set_name}'"))?;
            let mode = Mode::parse(mode)
                .with_context(|| format!("invalid mode in matchset '{set_name}'"))?;
            let clauses = patterns
                .into_iter()
                .map(|pattern| {
                    let matcher = compile_clause_pattern(&pattern, subject, pattern_kind, mode)
                        .with_context(|| {
                            format!("invalid pattern '{pattern}' in matchset '{set_name}'")
                        })?;
                    Ok(CompiledMatchClause {
                        entry_type: entry_type.clone(),
                        subject,
                        matcher,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((clauses, summary))
        }
        other => bail!(
            "invalid match clause '{other}' in set '{set_name}' (expected 'name', 'path', or 'bash')"
        ),
    }
}

fn clause_atoms(node: &KdlNode) -> Result<Vec<String>> {
    let mut atoms = Vec::new();
    for entry in node.entries() {
        if entry.name().is_some() {
            bail!("match clause arguments must be positional values");
        }
        let Some(value) = entry.value().as_string() else {
            bail!("match clause arguments must be strings");
        };
        atoms.push(value.to_string());
    }
    Ok(atoms)
}

fn clause_patterns(node: &KdlNode) -> Result<Vec<String>> {
    let Some(children) = node.children() else {
        return Ok(Vec::new());
    };

    let mut patterns = Vec::new();
    for child in children.nodes() {
        if !child.entries().is_empty() || child.children().is_some() || child.ty().is_some() {
            bail!("patterns must be child nodes with no arguments, annotations, or children");
        }
        patterns.push(child.name().value().to_string());
    }
    Ok(patterns)
}

/// Compile one pattern of a name/path clause, handling the location-variable
/// prefix (`$<home>/...`, `$<vroot>/...`) that anchors a `path` pattern to a
/// semantic location. A leading literal `$<` can be escaped as `$$<`; a plain
/// `$` anywhere (e.g. `$RECYCLE.BIN`) is literal as-is.
fn compile_clause_pattern(
    pattern: &str,
    subject: Subject,
    pattern_kind: PatternKind,
    mode: Mode,
) -> Result<Matcher> {
    let Some((variable, tail)) = parse_location_prefix(pattern)? else {
        let pattern = pattern
            .strip_prefix('$')
            .filter(|p| p.starts_with("$<"))
            .unwrap_or(pattern);
        return compile_matcher(pattern, pattern_kind, mode);
    };

    if matches!(subject, Subject::Name) {
        bail!("location variable '$<{variable}>' is only allowed in 'path' clauses");
    }
    if matches!(mode, Mode::Sub) {
        bail!("location variable '$<{variable}>' requires 'full' mode");
    }
    if matches!(pattern_kind, PatternKind::Regex) {
        bail!("location variable '$<{variable}>' is not supported in 'regex' clauses");
    }

    let components: Vec<&str> = tail.split('/').collect();
    if components.iter().any(|component| component.is_empty()) {
        bail!("empty path component after '$<{variable}>'");
    }
    let depth = components.len();

    // The tail is matched against the last `depth` components of the entry's
    // absolute path, which use the platform separator.
    let tail_regex = match pattern_kind {
        PatternKind::Literal => {
            let native = components.join(std::path::MAIN_SEPARATOR_STR);
            build_regex(&format!("^{}$", regex::escape(&native)))?
        }
        PatternKind::Glob => {
            let glob = GlobBuilder::new(tail).literal_separator(true).build()?;
            build_regex(glob.regex())?
        }
        PatternKind::Regex => unreachable!("rejected above"),
    };

    let anchor = match variable {
        "home" => Anchor::Home(home_anchor_paths()?),
        "vroot" => Anchor::VolumeRoot,
        _ => bail!("unknown location variable '$<{variable}>' (expected 'home' or 'vroot')"),
    };

    Ok(Matcher::Anchored {
        anchor,
        tail: tail_regex,
        depth,
    })
}

/// Split a leading `$<variable>/tail` off a pattern. Returns `None` for
/// patterns that do not start with `$<` (a lone `$` stays literal).
fn parse_location_prefix(pattern: &str) -> Result<Option<(&str, &str)>> {
    if pattern.starts_with("$$<") {
        // Escaped literal '$<'; the caller strips one '$'.
        return Ok(None);
    }
    let Some(rest) = pattern.strip_prefix("$<") else {
        return Ok(None);
    };
    let Some((variable, tail)) = rest.split_once('>') else {
        bail!("unterminated location variable (expected '$<name>/...')");
    };
    let Some(tail) = tail.strip_prefix('/') else {
        bail!("location variable '$<{variable}>' must be followed by '/' and a path");
    };
    if tail.is_empty() {
        bail!("location variable '$<{variable}>' must be followed by a non-empty path");
    }
    Ok(Some((variable, tail)))
}

/// The path(s) that count as `$<home>`: the reported home directory and its
/// canonicalized form, so entries reached through either spelling of a
/// symlinked home still anchor.
fn home_anchor_paths() -> Result<Vec<PathBuf>> {
    let home =
        etcetera::home_dir().context("could not resolve '$<home>' (no home directory found)")?;
    let mut paths = vec![home.clone()];
    if let Ok(canonical) = fs::canonicalize(&home)
        && canonical != home
    {
        paths.push(canonical);
    }
    Ok(paths)
}

// Matchset patterns always compile case-sensitive: a named set is a
// definition, so its meaning must not drift with the casing of an adjacent
// search pattern (no -s/-i/smart-case coupling).
fn compile_matcher(pattern: &str, pattern_kind: PatternKind, mode: Mode) -> Result<Matcher> {
    match pattern_kind {
        PatternKind::Literal => {
            let regex = match mode {
                Mode::Full => format!("^{}$", regex::escape(pattern)),
                Mode::Sub => regex::escape(pattern),
            };
            build_regex(&regex).map(Matcher::Regex)
        }
        PatternKind::Glob => {
            let glob = GlobBuilder::new(pattern).literal_separator(true).build()?;
            build_regex(glob.regex()).map(Matcher::Regex)
        }
        PatternKind::Regex => {
            let regex = match mode {
                Mode::Full => format!("^(?:{pattern})$"),
                Mode::Sub => pattern.to_string(),
            };
            build_regex(&regex).map(Matcher::Regex)
        }
    }
}

fn build_regex(pattern: &str) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .build()
        .with_context(|| format!("could not compile regex '{pattern}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_registry(source: &str) -> Result<Registry> {
        let document = source.parse::<KdlDocument>()?;
        Registry::parse(&document, &Provenance::Builtin)
    }

    #[test]
    fn builtin_matchsets_parse() {
        let registry = Registry::builtins().unwrap();
        for name in [
            "vcs_meta",
            "build_output",
            "cache",
            "package",
            "noise",
            "trash",
        ] {
            assert!(registry.sets.contains_key(name), "missing builtin '{name}'");
        }
    }

    #[test]
    fn parses_f_exclusions_fixture() {
        let registry =
            parse_registry(include_str!("../tests/fixtures/matchsets/f-exclusions.kdl")).unwrap();

        assert!(registry.sets.contains_key("vcs"));
        assert!(registry.sets.contains_key("metadata"));
    }

    #[test]
    fn parses_annotated_and_unannotated_clauses() {
        let registry = parse_registry(
            r#"
            "s" {
                (d) name literal full { "a" }
                (f,x,e) name glob full { "*" }
                (file,-empty) path regex sub { "b" }
                (-d,-l) name literal full { "c" }
                name literal full { "d" }
                (d) bash { "-f CACHEDIR.TAG" }
            }
        "#,
        )
        .unwrap();

        assert!(registry.sets.contains_key("s"));
    }

    #[test]
    fn rejects_invalid_type_constraints() {
        for annotation in [
            "d,f", "d,-f", "d,-d", "x,-x", "d,d", "x,x", "-d,-d", "bogus", "d,,x",
        ] {
            let source = format!(r#""s" {{ ({annotation}) name literal full {{ "a" }} }}"#);
            assert!(
                parse_registry(&source).is_err(),
                "annotation '({annotation})' should be rejected"
            );
        }
    }

    #[test]
    fn rejects_old_type_prefix_grammar() {
        assert!(parse_registry(r#""s" { dir name literal full { "a" } }"#).is_err());
    }

    #[test]
    fn rejects_set_names_with_trailing_hyphen() {
        let err = parse_registry(r#""foo-" { (f) name literal full { "a" } }"#)
            .err()
            .expect("trailing-hyphen set name should be rejected");
        assert!(err.to_string().contains("must not end with '-'"));
    }

    #[test]
    fn resolve_selection_adds_dedups_removes_and_readds() {
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            resolve_selection(&names(&["a", "b", "a", "a-"])).unwrap(),
            names(&["b"])
        );
        assert_eq!(
            resolve_selection(&names(&["a", "a-", "a"])).unwrap(),
            names(&["a"])
        );
        assert!(resolve_selection(&names(&[])).unwrap().is_empty());
        // a bare '-' clears the selection accumulated so far
        assert_eq!(
            resolve_selection(&names(&["a", "b", "-", "c"])).unwrap(),
            names(&["c"])
        );
        assert!(resolve_selection(&names(&["a", "-"])).unwrap().is_empty());
        // clearing an empty selection and removing an unselected name are
        // both no-ops ("ensure not selected", like rg's -T)
        assert!(resolve_selection(&names(&["-"])).unwrap().is_empty());
        assert!(resolve_selection(&names(&["a-"])).unwrap().is_empty());
        assert!(
            resolve_selection(&names(&["a", "-", "a-"]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn registry_validates_removal_names() {
        let registry = parse_registry(r#""real" { (f) name literal full { "a" } }"#).unwrap();

        assert!(
            registry
                .validate_names(["real-".to_string()].iter())
                .is_ok()
        );
        let err = registry
            .validate_names(["bogus-".to_string()].iter())
            .expect_err("unknown removal name should be rejected");
        assert_eq!(err.to_string(), "unknown matchset 'bogus'");
    }

    #[test]
    fn rejects_duplicate_set_names() {
        let source = r#"
            "one" {
                (f) name literal full { "a" }
            }
            "one" {
                (f) name literal full { "b" }
            }
        "#;

        assert!(parse_registry(source).is_err());
    }

    #[test]
    fn parses_location_variable_patterns() {
        let registry = parse_registry(
            r#"
            "s" {
                (d) path literal full { "$<home>/.Trash"; "$<vroot>/.Trashes" }
                (d) path glob full { "$<vroot>/.Trash-*" }
            }
        "#,
        )
        .unwrap();

        assert!(registry.sets.contains_key("s"));
    }

    #[test]
    fn rejects_invalid_location_variable_patterns() {
        for (clause, pattern) in [
            // unknown variable
            ("path literal full", "$<bogus>/.Trash"),
            // unterminated variable
            ("path literal full", "$<home/.Trash"),
            // no tail
            ("path literal full", "$<home>"),
            // no separator before the tail
            ("path literal full", "$<home>.Trash"),
            // empty path component
            ("path literal full", "$<home>//x"),
            // name clauses cannot anchor
            ("name literal full", "$<home>/.Trash"),
            // sub mode contradicts anchoring
            ("path literal sub", "$<home>/.Trash"),
            // regex tails are not supported
            ("path regex full", "$<home>/.+"),
        ] {
            let source = format!(r#""s" {{ (d) {clause} {{ "{pattern}" }} }}"#);
            assert!(
                parse_registry(&source).is_err(),
                "'{clause} {{ {pattern} }}' should be rejected"
            );
        }
    }

    #[test]
    fn dollar_is_literal_unless_it_introduces_a_variable() {
        // A plain '$' (e.g. $RECYCLE.BIN) needs no escaping; a leading
        // literal '$<' is written '$$<'.
        let registry = parse_registry(
            r#"
            "s" {
                (d) name literal full { "$RECYCLE.BIN" }
                (d) path literal full { "$$<not-a-variable>/x" }
            }
        "#,
        )
        .unwrap();

        assert!(registry.sets.contains_key("s"));
    }

    #[test]
    fn normalize_lexically_folds_dot_components() {
        assert_eq!(
            normalize_lexically(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize_lexically(Path::new("/a/..")), PathBuf::from("/"));
    }

    #[test]
    fn volume_root_detection() {
        // The filesystem root is always a volume root.
        assert!(is_volume_root(Path::new(if cfg!(windows) {
            "C:\\"
        } else {
            "/"
        })));

        // A freshly created plain subdirectory is on its parent's device.
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(!is_volume_root(&sub));
    }

    #[cfg(unix)]
    #[test]
    fn anchored_home_matching_uses_prefix_equality() {
        let anchor = Anchor::Home(vec![PathBuf::from("/home/me")]);
        let tail = build_regex(&format!("^{}$", regex::escape(".Trash"))).unwrap();

        assert!(matches_anchored(
            Path::new("/home/me/.Trash"),
            &anchor,
            &tail,
            1
        ));
        // same name, wrong location
        assert!(!matches_anchored(
            Path::new("/home/me/sub/.Trash"),
            &anchor,
            &tail,
            1
        ));
        assert!(!matches_anchored(
            Path::new("/srv/other/.Trash"),
            &anchor,
            &tail,
            1
        ));
        // lexical normalization applies before anchoring
        assert!(matches_anchored(
            Path::new("/home/me/sub/../.Trash"),
            &anchor,
            &tail,
            1
        ));
    }

    #[test]
    fn matchers_are_case_sensitive() {
        let Matcher::Regex(regex) =
            compile_matcher("Foo", PatternKind::Literal, Mode::Full).unwrap()
        else {
            panic!("expected a regex matcher");
        };
        assert!(regex.is_match(b"Foo"));
        assert!(!regex.is_match(b"foo"));
    }

    #[test]
    fn merge_shadows_by_name() {
        let mut registry = Registry::builtins().unwrap();
        let user =
            r#""vcs_meta" { (d) name literal full { ".jj" } }"#.parse::<KdlDocument>().unwrap();
        registry.merge(
            Registry::parse(&user, &Provenance::UserFile(PathBuf::from("matchsets.kdl"))).unwrap(),
        );

        let vcs_meta = &registry.sets["vcs_meta"];
        assert_eq!(vcs_meta.clauses.len(), 1);
        assert_eq!(vcs_meta.shadows.len(), 1);
        assert!(matches!(vcs_meta.provenance, Provenance::UserFile(_)));
    }
}
