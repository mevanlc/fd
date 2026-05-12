use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bash_condexp::{AccessMode, Evaluator, Expr, FileStat, FileSystem, MapEnv, StdFs, parse};

use crate::config::Config;
use crate::filesystem::strip_current_dir;

pub fn parse_expr(input: &str, option: &str) -> Result<Expr> {
    parse(input).with_context(|| format!("Invalid {option} conditional expression"))
}

pub fn evaluate(
    expr: &Expr,
    entry_path: &Path,
    context_dir: &Path,
    config: &Config,
) -> Result<bool> {
    let current_path = if config.strip_cwd_prefix {
        strip_current_dir(entry_path)
    } else {
        entry_path
    };
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
