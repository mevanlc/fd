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
        }
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
    validate_names(include_names)?;
    validate_names(exclude_names)?;

    if include_names.is_empty() && exclude_names.is_empty() && matchset_files.is_empty() {
        return Ok(SelectedMatchsets {
            include: Vec::new(),
            exclude: Vec::new(),
        });
    }

    let registry = Registry::assemble(matchset_files, no_user_matchsets)?;

    Ok(SelectedMatchsets {
        include: registry.select(include_names)?,
        exclude: registry.select(exclude_names)?,
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

fn validate_names(names: &[String]) -> Result<()> {
    for name in names {
        if name.trim().is_empty() {
            bail!("matchset names must not be empty");
        }
    }
    Ok(())
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
                    let matcher =
                        compile_matcher(&pattern, pattern_kind, mode).with_context(|| {
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
        for name in ["vcs", "build_output", "cache", "package", "noise"] {
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
        let user = r#""vcs" { (d) name literal full { ".jj" } }"#.parse::<KdlDocument>().unwrap();
        registry.merge(
            Registry::parse(&user, &Provenance::UserFile(PathBuf::from("matchsets.kdl"))).unwrap(),
        );

        let vcs = &registry.sets["vcs"];
        assert_eq!(vcs.clauses.len(), 1);
        assert_eq!(vcs.shadows.len(), 1);
        assert!(matches!(vcs.provenance, Provenance::UserFile(_)));
    }
}
