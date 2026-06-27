use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bash_condexp::{
    AccessMode, BinaryOp, Evaluator, Expr, FileStat, FileSystem, MapEnv, Primary, StdFs, Word,
    WordPart, parse,
};
use regex::Regex;

use crate::config::Config;
use crate::filesystem::strip_current_dir;

pub fn parse_expr(input: &str, option: &str) -> Result<Expr> {
    parse(input).with_context(|| format!("Invalid {option} conditional expression"))
}

#[derive(Clone)]
pub enum Condition {
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    Not(Box<Condition>),
    Fast(FastCondition),
    Generic(Expr),
}

impl Condition {
    pub fn compile(expr: Expr, case_sensitive: bool) -> Result<Self> {
        match expr {
            Expr::And(left, right) => Ok(Self::And(
                Box::new(Self::compile(*left, case_sensitive)?),
                Box::new(Self::compile(*right, case_sensitive)?),
            )),
            Expr::Or(left, right) => Ok(Self::Or(
                Box::new(Self::compile(*left, case_sensitive)?),
                Box::new(Self::compile(*right, case_sensitive)?),
            )),
            Expr::Not(inner) => Ok(Self::Not(Box::new(Self::compile(*inner, case_sensitive)?))),
            expr => Self::compile_primary(expr, case_sensitive),
        }
    }

    fn compile_primary(expr: Expr, case_sensitive: bool) -> Result<Self> {
        match FastCondition::compile(&expr, case_sensitive)? {
            Some(condition) => Ok(Self::Fast(condition)),
            None => Ok(Self::Generic(expr)),
        }
    }

    pub fn evaluate(&self, entry_path: &Path, context_dir: &Path, config: &Config) -> Result<bool> {
        match self {
            Self::And(left, right) => {
                if left.evaluate(entry_path, context_dir, config)? {
                    right.evaluate(entry_path, context_dir, config)
                } else {
                    Ok(false)
                }
            }
            Self::Or(left, right) => {
                if left.evaluate(entry_path, context_dir, config)? {
                    Ok(true)
                } else {
                    right.evaluate(entry_path, context_dir, config)
                }
            }
            Self::Not(inner) => Ok(!inner.evaluate(entry_path, context_dir, config)?),
            Self::Fast(condition) => Ok(condition.matches(current_path(entry_path, config))),
            Self::Generic(expr) => evaluate(expr, entry_path, context_dir, config),
        }
    }
}

#[derive(Clone)]
pub struct FastCondition {
    subject: Subject,
    matcher: FastMatcher,
}

#[derive(Clone)]
enum FastMatcher {
    Regex(Regex),
    RegexNot(Regex),
}

#[derive(Copy, Clone)]
enum Subject {
    Path,
    Basename,
    Parent,
    PathNoExt,
    BasenameNoExt,
}

impl FastCondition {
    fn compile(expr: &Expr, case_sensitive: bool) -> Result<Option<Self>> {
        let Expr::Primary(Primary::Binary { op, lhs, rhs }) = expr else {
            return Ok(None);
        };
        let Some(subject) = Subject::from_word(lhs) else {
            return Ok(None);
        };
        if word_contains_vars(rhs) {
            return Ok(None);
        }

        let nocase = !case_sensitive;
        let matcher =
            match op {
                BinaryOp::GlobMatch => FastMatcher::Regex(
                    match bash_condexp::pattern::compile_glob(rhs, nocase, |_| String::new()) {
                        Ok(regex) => regex,
                        Err(_) => return Ok(None),
                    },
                ),
                BinaryOp::GlobNotMatch => FastMatcher::RegexNot(
                    match bash_condexp::pattern::compile_glob(rhs, nocase, |_| String::new()) {
                        Ok(regex) => regex,
                        Err(_) => return Ok(None),
                    },
                ),
                BinaryOp::RegexMatch => FastMatcher::Regex(
                    match bash_condexp::pattern::compile_regex(rhs, nocase, |_| String::new()) {
                        Ok(regex) => regex,
                        Err(_) => return Ok(None),
                    },
                ),
                _ => return Ok(None),
            };

        Ok(Some(Self { subject, matcher }))
    }

    fn matches(&self, path: &Path) -> bool {
        let subject = self.subject.resolve(path);
        match &self.matcher {
            FastMatcher::Regex(regex) => regex.is_match(subject.as_ref()),
            FastMatcher::RegexNot(regex) => !regex.is_match(subject.as_ref()),
        }
    }
}

impl Subject {
    fn from_word(word: &Word) -> Option<Self> {
        let [part] = word.parts.as_slice() else {
            return None;
        };

        let name = match part {
            WordPart::Var(name) | WordPart::QuotedVar(name) => name.as_str(),
            _ => return None,
        };

        match name {
            "" => Some(Self::Path),
            "/" => Some(Self::Basename),
            "//" => Some(Self::Parent),
            "." => Some(Self::PathNoExt),
            "/." => Some(Self::BasenameNoExt),
            _ => None,
        }
    }

    fn resolve(self, path: &Path) -> Cow<'_, str> {
        match self {
            Self::Path => path.to_string_lossy(),
            Self::Basename => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy(),
            Self::Parent => path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_string_lossy(),
            Self::PathNoExt => Cow::Owned(path_to_string(&remove_extension(path))),
            Self::BasenameNoExt => {
                let basename = path.file_name().unwrap_or(path.as_os_str());
                Cow::Owned(path_to_string(&remove_extension(Path::new(basename))))
            }
        }
    }
}

fn word_contains_vars(word: &Word) -> bool {
    word.parts
        .iter()
        .any(|part| matches!(part, WordPart::Var(_) | WordPart::QuotedVar(_)))
}

pub fn evaluate(
    expr: &Expr,
    entry_path: &Path,
    context_dir: &Path,
    config: &Config,
) -> Result<bool> {
    let current_path = current_path(entry_path, config);
    let mut env = entry_env(current_path, config);
    let fs = ContextFs {
        context_dir,
        current_value: PathBuf::from(current_path),
        current_path: entry_path,
        inner: StdFs,
    };

    Evaluator::new(&mut env, &fs)
        .eval(expr)
        .context("Could not evaluate bash conditional expression")
}

fn current_path<'a>(entry_path: &'a Path, config: &Config) -> &'a Path {
    if config.strip_cwd_prefix {
        strip_current_dir(entry_path)
    } else {
        entry_path
    }
}

fn remove_extension(path: &Path) -> PathBuf {
    let Some(stem) = path.file_stem() else {
        return path.to_path_buf();
    };

    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(stem),
        _ => PathBuf::from(stem),
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn os_to_string(value: &std::ffi::OsStr) -> String {
    value.to_string_lossy().into_owned()
}

fn entry_env(path: &Path, config: &Config) -> MapEnv {
    let basename = path.file_name().unwrap_or(path.as_os_str());
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let no_ext = remove_extension(path);
    let basename_no_ext = remove_extension(Path::new(basename));

    MapEnv::new()
        .with_var("", path_to_string(path))
        .with_var("/", os_to_string(basename))
        .with_var("//", path_to_string(parent))
        .with_var(".", path_to_string(&no_ext))
        .with_var("/.", path_to_string(&basename_no_ext))
        .with_option("nocasematch", !config.case_sensitive)
}

struct ContextFs<'a> {
    context_dir: &'a Path,
    current_value: PathBuf,
    current_path: &'a Path,
    inner: StdFs,
}

impl ContextFs<'_> {
    fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else if path == self.current_value {
            self.current_path.to_path_buf()
        } else {
            self.context_dir.join(path)
        }
    }
}

impl FileSystem for ContextFs<'_> {
    fn stat(&self, path: &Path) -> std::io::Result<FileStat> {
        self.inner.stat(&self.resolve(path))
    }

    fn lstat(&self, path: &Path) -> std::io::Result<FileStat> {
        self.inner.lstat(&self.resolve(path))
    }

    fn access(&self, path: &Path, mode: AccessMode) -> bool {
        self.inner.access(&self.resolve(path), mode)
    }

    fn is_tty(&self, fd: i32) -> bool {
        self.inner.is_tty(fd)
    }

    fn effective_uid(&self) -> u32 {
        self.inner.effective_uid()
    }

    fn effective_gid(&self) -> u32 {
        self.inner.effective_gid()
    }
}
