use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
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

pub struct SelectedMatchsets {
    pub include: Vec<CompiledMatchset>,
    pub exclude: Vec<CompiledMatchset>,
}

pub struct CompiledMatchset {
    name: String,
    clauses: Vec<CompiledMatchClause>,
}

struct CompiledMatchClause {
    entry_type: EntryType,
    subject: Subject,
    matcher: Matcher,
}

#[derive(Copy, Clone)]
enum EntryType {
    File,
    Directory,
    Symlink,
    Executable,
    Empty,
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
    Bash,
}

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
    fn parse(value: &str) -> Result<Self> {
        match value {
            "f" | "file" => Ok(Self::File),
            "d" | "dir" | "directory" => Ok(Self::Directory),
            "l" | "symlink" => Ok(Self::Symlink),
            "x" | "executable" => Ok(Self::Executable),
            "e" | "empty" => Ok(Self::Empty),
            "s" | "socket" => Ok(Self::Socket),
            "p" | "pipe" => Ok(Self::Pipe),
            "b" | "block-device" => Ok(Self::BlockDevice),
            "c" | "char-device" => Ok(Self::CharDevice),
            _ => bail!("invalid matchset entry type '{value}'"),
        }
    }

    fn matches(self, entry: &DirEntry) -> bool {
        let Some(file_type) = entry.file_type() else {
            return false;
        };

        match self {
            Self::File => file_type.is_file(),
            Self::Directory => file_type.is_dir(),
            Self::Symlink => file_type.is_symlink(),
            Self::Executable => file_type.is_file() && entry.path().executable(),
            Self::Empty => filesystem::is_empty(entry),
            Self::Socket => filesystem::is_socket(file_type),
            Self::Pipe => filesystem::is_pipe(file_type),
            Self::BlockDevice => filesystem::is_block_device(file_type),
            Self::CharDevice => filesystem::is_char_device(file_type),
        }
    }
}

impl Subject {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "name" => Ok(Self::Name),
            "path" => Ok(Self::Path),
            _ => bail!("invalid matchset subject '{value}'"),
        }
    }

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
            "bash" => Ok(Self::Bash),
            _ => bail!("invalid matchset pattern kind '{value}'"),
        }
    }
}

pub fn load_selected(
    include_names: &[String],
    exclude_names: &[String],
    no_user_matchsets: bool,
    case_sensitive: bool,
) -> Result<SelectedMatchsets> {
    validate_names(include_names)?;
    validate_names(exclude_names)?;

    if include_names.is_empty() && exclude_names.is_empty() {
        return Ok(SelectedMatchsets {
            include: Vec::new(),
            exclude: Vec::new(),
        });
    }

    let registry = if no_user_matchsets {
        Registry::default()
    } else {
        let Some(path) = default_matchsets_path()? else {
            bail!("could not determine fd config directory for matchsets");
        };

        if !path.is_file() {
            bail!("matchset file not found: {}", path.display());
        }

        Registry::load(&path, case_sensitive)?
    };

    Ok(SelectedMatchsets {
        include: registry.select(include_names)?,
        exclude: registry.select(exclude_names)?,
    })
}

fn validate_names(names: &[String]) -> Result<()> {
    for name in names {
        if name.trim().is_empty() {
            bail!("matchset names must not be empty");
        }
    }
    Ok(())
}

fn default_matchsets_path() -> Result<Option<PathBuf>> {
    Ok(etcetera::choose_base_strategy()
        .ok()
        .map(|base| base.config_dir().join("fd").join("matchsets.kdl")))
}

#[derive(Default)]
struct Registry {
    sets: HashMap<String, CompiledMatchset>,
}

impl Registry {
    fn load(path: &Path, case_sensitive: bool) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("could not read matchset file '{}'", path.display()))?;
        let document = source
            .parse::<KdlDocument>()
            .with_context(|| format!("could not parse matchset file '{}'", path.display()))?;

        Self::parse(&document, case_sensitive)
            .with_context(|| format!("invalid matchset file '{}'", path.display()))
    }

    fn parse(document: &KdlDocument, case_sensitive: bool) -> Result<Self> {
        let mut sets = HashMap::new();

        for node in document.nodes() {
            let name = node.name().value().to_string();
            if name.trim().is_empty() {
                bail!("matchset names must not be empty");
            }
            if sets.contains_key(&name) {
                bail!("duplicate matchset '{name}'");
            }

            let children = node
                .children()
                .ok_or_else(|| anyhow!("matchset '{name}' must contain match clauses"))?;
            let mut clauses = Vec::new();
            for child in children.nodes() {
                clauses.extend(parse_clause_group(&name, child, case_sensitive)?);
            }
            if clauses.is_empty() {
                bail!("matchset '{name}' must contain at least one pattern");
            }
            sets.insert(name.clone(), CompiledMatchset { name, clauses });
        }

        Ok(Self { sets })
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

impl Clone for CompiledMatchset {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            clauses: self.clauses.clone(),
        }
    }
}

impl Clone for CompiledMatchClause {
    fn clone(&self) -> Self {
        Self {
            entry_type: self.entry_type,
            subject: self.subject,
            matcher: self.matcher.clone(),
        }
    }
}

impl Clone for Matcher {
    fn clone(&self) -> Self {
        match self {
            Self::Regex(regex) => Self::Regex(regex.clone()),
            Self::Bash(expr) => Self::Bash(expr.clone()),
        }
    }
}

fn parse_clause_group(
    set_name: &str,
    node: &KdlNode,
    case_sensitive: bool,
) -> Result<Vec<CompiledMatchClause>> {
    let atoms = clause_atoms(node)?;
    let (entry_type, subject, pattern_kind, mode) = parse_atoms(set_name, &atoms)?;
    let patterns = clause_patterns(node)
        .with_context(|| format!("invalid pattern list in matchset '{set_name}'"))?;

    if patterns.is_empty() {
        bail!("match clause in set '{set_name}' must contain at least one pattern");
    }

    patterns
        .into_iter()
        .map(|pattern| {
            let matcher = compile_matcher(&pattern, pattern_kind, mode, case_sensitive)
                .with_context(|| format!("invalid pattern '{pattern}' in matchset '{set_name}'"))?;
            Ok(CompiledMatchClause {
                entry_type,
                subject,
                matcher,
            })
        })
        .collect()
}

fn clause_atoms(node: &KdlNode) -> Result<Vec<String>> {
    let mut atoms = vec![node.name().value().to_string()];
    for entry in node.entries() {
        if entry.name().is_some() {
            bail!("match clause atoms must be positional values");
        }
        let Some(value) = entry.value().as_string() else {
            bail!("match clause atoms must be strings");
        };
        atoms.push(value.to_string());
    }
    Ok(atoms)
}

fn parse_atoms(
    set_name: &str,
    atoms: &[String],
) -> Result<(EntryType, Subject, PatternKind, Mode)> {
    match atoms {
        [entry_type, pattern_kind] if pattern_kind == "bash" => Ok((
            EntryType::parse(entry_type)
                .with_context(|| format!("invalid entry type in matchset '{set_name}'"))?,
            Subject::Name,
            PatternKind::Bash,
            Mode::Sub,
        )),
        [entry_type, subject, pattern_kind, mode] => Ok((
            EntryType::parse(entry_type)
                .with_context(|| format!("invalid entry type in matchset '{set_name}'"))?,
            Subject::parse(subject)
                .with_context(|| format!("invalid subject in matchset '{set_name}'"))?,
            PatternKind::parse(pattern_kind)
                .with_context(|| format!("invalid pattern kind in matchset '{set_name}'"))?,
            Mode::parse(mode).with_context(|| format!("invalid mode in matchset '{set_name}'"))?,
        )),
        _ => bail!(
            "match clause in set '{set_name}' must be '<type> bash' or '<type> <subject> <pattern-kind> <mode>'"
        ),
    }
}

fn clause_patterns(node: &KdlNode) -> Result<Vec<String>> {
    let Some(children) = node.children() else {
        return Ok(Vec::new());
    };

    let mut patterns = Vec::new();
    for child in children.nodes() {
        if !child.entries().is_empty() || child.children().is_some() {
            bail!("patterns must be child nodes with no arguments or children");
        }
        patterns.push(child.name().value().to_string());
    }
    Ok(patterns)
}

fn compile_matcher(
    pattern: &str,
    pattern_kind: PatternKind,
    mode: Mode,
    case_sensitive: bool,
) -> Result<Matcher> {
    match pattern_kind {
        PatternKind::Literal => {
            let regex = match mode {
                Mode::Full => format!("^{}$", regex::escape(pattern)),
                Mode::Sub => regex::escape(pattern),
            };
            build_regex(&regex, case_sensitive).map(Matcher::Regex)
        }
        PatternKind::Glob => {
            let glob = GlobBuilder::new(pattern).literal_separator(true).build()?;
            build_regex(glob.regex(), case_sensitive).map(Matcher::Regex)
        }
        PatternKind::Regex => {
            let regex = match mode {
                Mode::Full => format!("^(?:{pattern})$"),
                Mode::Sub => pattern.to_string(),
            };
            build_regex(&regex, case_sensitive).map(Matcher::Regex)
        }
        PatternKind::Bash => bash_cond::parse_expr(pattern, "matchset bash")
            .and_then(|expr| bash_cond::Condition::compile(expr, case_sensitive))
            .map(Matcher::Bash),
    }
}

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .dot_matches_new_line(true)
        .build()
        .with_context(|| format!("could not compile regex '{pattern}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_f_exclusions_fixture() {
        let document = include_str!("../tests/fixtures/matchsets/f-exclusions.kdl")
            .parse::<KdlDocument>()
            .unwrap();
        let registry = Registry::parse(&document, true).unwrap();

        assert!(registry.sets.contains_key("vcs"));
        assert!(registry.sets.contains_key("metadata"));
    }

    #[test]
    fn parses_matchsets_sketch_fixture() {
        let document = include_str!("../tests/fixtures/matchsets/matchsets-sketch.kdl")
            .parse::<KdlDocument>()
            .unwrap();
        let registry = Registry::parse(&document, true).unwrap();

        assert!(registry.sets.contains_key("vcs"));
        assert!(registry.sets.contains_key("build_output"));
        assert!(registry.sets.contains_key("cache"));
        assert!(registry.sets.contains_key("package"));
        assert!(registry.sets.contains_key("noise"));
    }

    #[test]
    fn rejects_duplicate_set_names() {
        let document = r#"
            "one" {
                file name literal full { "a" }
            }
            "one" {
                file name literal full { "b" }
            }
        "#
        .parse::<KdlDocument>()
        .unwrap();

        assert!(Registry::parse(&document, true).is_err());
    }
}
