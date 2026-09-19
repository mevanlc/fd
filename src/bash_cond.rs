use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bash_condexp::pattern::{CompiledGlob, GlobOptions, compile_glob, compile_regex};
use bash_condexp::{
    AccessMode, BinaryOp, Evaluator, Expr, FileStat, FileSystem, MapEnv, ParseOptions, Primary,
    StdFs, Word, WordPart, parse_with_options,
};
use regex::Regex;

use crate::config::Config;
use crate::filesystem::strip_current_dir;

pub fn parse_expr(input: &str, option: &str) -> Result<Expr> {
    parse_with_options(input, ParseOptions::new().punctuation_variables(true))
        .with_context(|| format!("Invalid {option} conditional expression"))
}

#[derive(Clone)]
pub enum Condition {
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    Not(Box<Condition>),
    Fast(FastCondition),
    Generic { expr: Expr, case_sensitive: bool },
}

impl Condition {
    pub fn compile(expr: Expr, case_sensitive: bool) -> Result<Self> {
        // Arithmetic and substring indices can assign variables. Keep the whole
        // expression in one environment so later branches observe those writes.
        // Conservatively include all structured expansions, including nested ones.
        if needs_shared_env(&expr) {
            return Ok(Self::Generic {
                expr,
                case_sensitive,
            });
        }
        Self::compile_stateless(expr, case_sensitive)
    }

    fn compile_stateless(expr: Expr, case_sensitive: bool) -> Result<Self> {
        match expr {
            Expr::And(left, right) => Ok(Self::And(
                Box::new(Self::compile_stateless(*left, case_sensitive)?),
                Box::new(Self::compile_stateless(*right, case_sensitive)?),
            )),
            Expr::Or(left, right) => Ok(Self::Or(
                Box::new(Self::compile_stateless(*left, case_sensitive)?),
                Box::new(Self::compile_stateless(*right, case_sensitive)?),
            )),
            Expr::Not(inner) => Ok(Self::Not(Box::new(Self::compile_stateless(
                *inner,
                case_sensitive,
            )?))),
            expr => Self::compile_primary(expr, case_sensitive),
        }
    }

    fn compile_primary(expr: Expr, case_sensitive: bool) -> Result<Self> {
        match FastCondition::compile(&expr, case_sensitive)? {
            Some(condition) => Ok(Self::Fast(condition)),
            None => Ok(Self::Generic {
                expr,
                case_sensitive,
            }),
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
            Self::Generic {
                expr,
                case_sensitive,
            } => evaluate(
                expr,
                entry_path,
                context_dir,
                current_path(entry_path, config),
                *case_sensitive,
            ),
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
    Glob(CompiledGlob),
    GlobNot(CompiledGlob),
    Regex(Regex),
}

#[derive(Copy, Clone)]
enum Subject {
    Path,
    Basename,
    Parent,
    PathNoExt,
    BasenameNoExt,
}

const PATH_VARIABLES: [(&str, &str, Subject); 5] = [
    ("", "fd_path", Subject::Path),
    ("/", "fd_name", Subject::Basename),
    ("//", "fd_parent", Subject::Parent),
    (".", "fd_path_no_ext", Subject::PathNoExt),
    ("/.", "fd_name_no_ext", Subject::BasenameNoExt),
];

impl FastCondition {
    fn compile(expr: &Expr, case_sensitive: bool) -> Result<Option<Self>> {
        let Expr::Primary(Primary::Binary { op, lhs, rhs }) = expr else {
            return Ok(None);
        };
        let Some(subject) = Subject::from_word(lhs) else {
            return Ok(None);
        };
        if word_is_dynamic(rhs) {
            return Ok(None);
        }

        let nocase = !case_sensitive;
        let options = GlobOptions {
            case_insensitive: nocase,
            // Like [[ ... ]], conditional glob matching always enables extglob.
            extglob: true,
        };
        let matcher = match op {
            BinaryOp::GlobMatch => {
                FastMatcher::Glob(match compile_glob(rhs, options, |_| String::new()) {
                    Ok(glob) => glob,
                    Err(_) => return Ok(None),
                })
            }
            BinaryOp::GlobNotMatch => {
                FastMatcher::GlobNot(match compile_glob(rhs, options, |_| String::new()) {
                    Ok(glob) => glob,
                    Err(_) => return Ok(None),
                })
            }
            BinaryOp::RegexMatch => {
                FastMatcher::Regex(match compile_regex(rhs, nocase, |_| String::new()) {
                    Ok(regex) => regex,
                    Err(_) => return Ok(None),
                })
            }
            _ => return Ok(None),
        };

        Ok(Some(Self { subject, matcher }))
    }

    fn matches(&self, path: &Path) -> bool {
        let subject = self.subject.resolve(path);
        match &self.matcher {
            FastMatcher::Glob(glob) => glob.is_match(subject.as_ref()),
            FastMatcher::GlobNot(glob) => !glob.is_match(subject.as_ref()),
            FastMatcher::Regex(regex) => regex.is_match(subject.as_ref()),
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

        PATH_VARIABLES
            .iter()
            .find(|(punctuation, named, _)| name == *punctuation || name == *named)
            .map(|(_, _, subject)| *subject)
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

fn word_is_dynamic(word: &Word) -> bool {
    word.parts
        .iter()
        .any(|part| !matches!(part, WordPart::Literal(_) | WordPart::Quoted(_)))
}

fn needs_shared_env(expr: &Expr) -> bool {
    let has_expansion = |word: &Word| {
        word.parts
            .iter()
            .any(|part| matches!(part, WordPart::Expansion { .. }))
    };
    match expr {
        Expr::And(left, right) | Expr::Or(left, right) => {
            needs_shared_env(left) || needs_shared_env(right)
        }
        Expr::Not(inner) => needs_shared_env(inner),
        Expr::Primary(primary) => match primary {
            Primary::Unary { arg, .. } | Primary::StringNonEmpty(arg) => has_expansion(arg),
            Primary::Binary { op, lhs, rhs } => {
                matches!(
                    op,
                    BinaryOp::ArithEq
                        | BinaryOp::ArithNe
                        | BinaryOp::ArithLt
                        | BinaryOp::ArithLe
                        | BinaryOp::ArithGt
                        | BinaryOp::ArithGe
                ) || has_expansion(lhs)
                    || has_expansion(rhs)
            }
        },
    }
}

fn evaluate(
    expr: &Expr,
    entry_path: &Path,
    context_dir: &Path,
    current_value: &Path,
    case_sensitive: bool,
) -> Result<bool> {
    let mut env = entry_env(current_value, case_sensitive);
    let fs = ContextFs {
        context_dir,
        current_value: PathBuf::from(current_value),
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

fn entry_env(path: &Path, case_sensitive: bool) -> MapEnv {
    let mut env = MapEnv::new().with_option("nocasematch", !case_sensitive);
    for (punctuation, named, subject) in PATH_VARIABLES {
        let value = subject.resolve(path).into_owned();
        env = env
            .with_var(punctuation, value.clone())
            .with_var(named, value);
    }
    env
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

#[cfg(test)]
mod tests {
    use super::*;
    use bash_condexp::Env;

    fn eval(input: &str, path: &str) -> Result<bool> {
        let expr = parse_expr(input, "test")?;
        evaluate(
            &expr,
            Path::new(path),
            Path::new("."),
            Path::new(path),
            true,
        )
    }

    #[test]
    fn path_variable_pairs() {
        let temp = tempfile::tempdir().unwrap();
        for path in [
            PathBuf::from("a b/é🙂.txt"),
            PathBuf::from("./nested/file.tar.gz"),
            PathBuf::from(".hidden"),
            PathBuf::from("no-extension"),
            temp.path().join("nested/file.txt"),
        ] {
            let env = entry_env(&path, true);
            for (punctuation, named, subject) in PATH_VARIABLES {
                assert_eq!(env.var(punctuation), env.var(named), "{path:?}: {named}");
                assert_eq!(env.var(named), Some(subject.resolve(&path).as_ref()));
                for name in [punctuation, named] {
                    let expr = parse_expr(&format!("${{{name}}} == *"), "test").unwrap();
                    assert!(matches!(
                        Condition::compile(expr, true).unwrap(),
                        Condition::Fast(_)
                    ));
                }
            }
        }
        let env = entry_env(Path::new(".hidden"), true);
        assert_eq!(env.var("fd_parent"), Some("."));
        assert_eq!(env.var("fd_name_no_ext"), Some(".hidden"));
        let env = entry_env(Path::new("nested/file.tar.gz"), true);
        assert_eq!(env.var("fd_name_no_ext"), Some("file.tar"));
        assert_eq!(
            env.var("fd_path_no_ext"),
            Some(
                Path::new("nested")
                    .join("file.tar")
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }

    #[test]
    fn fast_matchers_agree_with_evaluator() {
        for (input, name, case_sensitive, expected) in [
            ("${fd_name} == @(foo|bar).txt", "foo.txt", true, true),
            ("${/} == ?(foo).txt", ".txt", true, true),
            ("${fd_name} == *(ab).txt", "abab.txt", true, true),
            ("${fd_name} == +(ab).txt", ".txt", true, false),
            ("${fd_name} == +(ab).txt", "ab.txt", true, true),
            ("${fd_name} == !(foo|bar).txt", "baz.txt", true, true),
            ("${fd_name} == !(foo|bar).txt", "foo.txt", true, false),
            ("${fd_name} != !(foo|bar).txt", "foo.txt", true, true),
            ("${fd_name} == +(@(a|b)|c).txt", "abc.txt", true, true),
            ("${fd_name} == @(foo|bar).txt", "FOO.TXT", false, true),
            ("${fd_name} == @(foo|bar).txt", "FOO.TXT", true, false),
            (r#""${fd_name}" == 'a*.txt'"#, "abc.txt", true, false),
            (r#"${fd_name} == 'a*.txt'"#, "a*.txt", true, true),
            (r#"${fd_name} == \*.txt"#, "*.txt", true, true),
            (r#"${fd_name} == '!(foo).txt'"#, "bar.txt", true, false),
            (r#"${fd_name} =~ ^foo[.]txt$"#, "foo.txt", true, true),
            (r#"${fd_name} =~ 'foo.txt'"#, "fooXtxt", true, false),
            (r#"${fd_name} =~ ^foo[.]txt$"#, "FOO.TXT", false, true),
            ("${fd_name} == ??.txt", "é🙂.txt", true, true),
        ] {
            let expr = parse_expr(input, "test").unwrap();
            let Condition::Fast(fast) = Condition::compile(expr.clone(), case_sensitive).unwrap()
            else {
                panic!("expected a fast matcher: {input}");
            };
            let path = Path::new(name);
            assert_eq!(fast.matches(path), expected, "fast: {input}, {name}");
            assert_eq!(
                evaluate(&expr, path, Path::new("."), path, case_sensitive).unwrap(),
                expected,
                "generic: {input}, {name}"
            );
        }
    }

    #[test]
    fn transforms_use_one_evaluator() {
        for (input, path) in [
            ("${fd_name#a*} == bcabc.txt", "abcabc.txt"),
            ("${fd_name##a*} == ''", "abcabc.txt"),
            ("${fd_name%.*} == abcabc", "abcabc.txt"),
            ("${fd_name%%b*} == a", "abcabc.txt"),
            ("${/#._} == photo.jpg", "._photo.jpg"),
            ("${/%.bak} == file", "file.bak"),
            ("${fd_name/a/X} == Xbcabc.txt", "abcabc.txt"),
            ("${fd_name//a/X} == XbcXbc.txt", "abcabc.txt"),
            ("${fd_name/#a/X} == Xbcabc.txt", "abcabc.txt"),
            ("${fd_name/%txt/md} == abcabc.md", "abcabc.txt"),
            ("${fd_name/a/[&]} == '[a]bcabc.txt'", "abcabc.txt"),
            (r"${fd_name/a/\&} == '&bcabc.txt'", "abcabc.txt"),
            ("${#fd_name} -eq 2", "é🙂"),
            ("${fd_name:1:1} == 🙂", "é🙂"),
            ("${fd_name: -3} == txt", "abcabc.txt"),
            ("${fd_name^} == Abcabc.txt", "abcabc.txt"),
            ("${fd_name^^} == ABCABC.TXT", "abcabc.txt"),
            ("${fd_name,,} == abc.txt", "ABC.TXT"),
            ("${fd_name,} == aBC.TXT", "ABC.TXT"),
            ("${fd_name^^@(a|b)} == ABcABc.txt", "abcabc.txt"),
            ("${missing:-${fd_name%.txt}} == abcabc", "abcabc.txt"),
            ("${missing-fallback} == fallback", "abcabc.txt"),
            (
                "${fd_name:+present} == present && ${missing+present} == ''",
                "abcabc.txt",
            ),
            ("${fd_name} == ${missing:-*.txt}", "abcabc.txt"),
            (r#"${fd_name} != "${missing:-*.txt}""#, "abcabc.txt"),
            (r#"${fd_name} == "${fd_name%.txt}.txt""#, "abcabc.txt"),
            (r#"${fd_name} =~ "${fd_name%.txt}""#, "abcabc.txt"),
            ("-n ${fd_name%.txt}", "abcabc.txt"),
            ("${fd_name%.txt}", "abcabc.txt"),
        ] {
            let expr = parse_expr(input, "test").unwrap();
            assert!(
                FastCondition::compile(&expr, true).unwrap().is_none(),
                "{input}"
            );
            assert!(
                matches!(
                    Condition::compile(expr, true).unwrap(),
                    Condition::Generic { .. }
                ),
                "{input}"
            );
            assert!(eval(input, path).unwrap(), "{input}");
        }
    }

    #[test]
    fn arithmetic_mutations_and_short_circuiting() {
        for input in [
            "'n=2' -eq 2 && n++ -eq 2 && $n == 3",
            "'n=2' -eq 0 || n -eq 2",
            "! [[ 'n=2' -eq 0 ]] && $n == 2",
            "${fd_name:n=1:1} == b && n -eq 1",
            "${fd_name:0:n=2} == ab && $n == 2",
            "${missing:-${fd_name:n=1:1}} == b && n -eq 1",
            "1 -eq 1 || 1/0 -eq 0",
            "! [[ 0 -eq 1 && 1/0 -eq 0 ]]",
            "[[ 1 -eq 1 || 'n=1' -eq 1 ]] && ! -v n",
            "'fd_name=7' -eq 7 && $fd_name == 7 && ${/} == abc.txt",
            "2**3+1 -eq 9 && 16#ff -eq 255",
        ] {
            let expr = parse_expr(input, "test").unwrap();
            assert!(
                matches!(
                    Condition::compile(expr, true).unwrap(),
                    Condition::Generic { .. }
                ),
                "{input}"
            );
            assert!(eval(input, "abc.txt").unwrap(), "{input}");
        }
    }

    #[test]
    fn bash_option_defaults_and_unsynthesized_special_parameters() {
        assert!(eval("! -o extglob && -o patsub_replacement", "abc.txt").unwrap());
        assert!(eval("${fd_name#@(abc)} == abc.txt", "abc.txt").unwrap());
        assert!(eval("${fd_name/@(abc)/X} == abc.txt", "abc.txt").unwrap());
        assert!(eval("${fd_name^^@(a|b)} == ABc.txt", "abc.txt").unwrap());
        assert!(eval("${#} == '' && ${?} == '' && ${1} == ''", "abc.txt").unwrap());
        assert!(eval("${missing:-fallback} == fallback", "abc.txt").unwrap());
    }

    #[test]
    fn malformed_and_invalid_expressions_report_errors() {
        for input in ["${fd_name", "${fd.name}", "${fd_name:=x}", "${fd_name:?x}"] {
            let error = parse_expr(input, "--bash").unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Invalid --bash conditional expression")
            );
        }
        for input in ["1/0 -eq 0", "${fd_name:1:-99} == x", "${fd_name} =~ ["] {
            let error = eval(input, "abc.txt").unwrap_err();
            assert_eq!(
                error.to_string(),
                "Could not evaluate bash conditional expression"
            );
        }
    }
}
