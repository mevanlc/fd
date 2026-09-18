mod count;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::anyhow;

use crate::config::Config;
use crate::dir_entry::DirEntry;
use crate::error::print_error;
use crate::exit_codes::ExitCode;
use crate::{output, walk};

/// A summary to produce instead of the regular search results (`--summary`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarySpec {
    /// Summarize the file extensions of the search results (`fext`).
    FileExtensions(FextOptions),
    CountChildren,
    CountDescendants,
}

/// Options for the `fext` summary, with all `@` (auto) settings resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FextOptions {
    /// Treat case variations of an extension as the same extension.
    pub case_insensitive: bool,
    /// Include dotfiles, whose entire filename counts as the extension.
    pub include_dotfiles: bool,
    /// Sort by ascending count (descending if false).
    pub sort_ascending: bool,
}

/// A single option in a summary-spec: enabled (`x`), disabled (`-x`) or auto (`@x`).
#[derive(Debug, Clone, Copy)]
enum Setting {
    Auto,
    Enabled,
    Disabled,
}

impl Setting {
    fn resolve(self, auto: bool) -> bool {
        match self {
            Setting::Auto => auto,
            Setting::Enabled => true,
            Setting::Disabled => false,
        }
    }
}

impl FromStr for SummarySpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, options) = match s.split_once(':') {
            Some((name, options)) => (name, options),
            None => (s, ""),
        };

        match name {
            "fext" => Ok(SummarySpec::FileExtensions(parse_fext_options(options)?)),
            "count-children" | "count-descendants" => {
                if !options.is_empty() {
                    return Err(anyhow!("summary '{name}' does not accept options"));
                }
                Ok(if name == "count-children" {
                    Self::CountChildren
                } else {
                    Self::CountDescendants
                })
            }
            _ => Err(anyhow!(
                "unknown summary type '{name}' (expected 'fext', 'count-children' or 'count-descendants')"
            )),
        }
    }
}

fn parse_fext_options(options: &str) -> anyhow::Result<FextOptions> {
    let mut case_insensitive = Setting::Auto;
    let mut include_dotfiles = Setting::Auto;
    let mut sort_ascending = Setting::Auto;

    let mut chars = options.chars();
    while let Some(c) = chars.next() {
        let (setting, option) = match c {
            '-' => (Setting::Disabled, chars.next()),
            '@' => (Setting::Auto, chars.next()),
            _ => (Setting::Enabled, Some(c)),
        };

        let slot = match option {
            Some('i') => &mut case_insensitive,
            Some('d') => &mut include_dotfiles,
            Some('s') => &mut sort_ascending,
            Some(other) => {
                return Err(anyhow!(
                    "unknown summary option '{other}' (expected 'i', 'd' or 's')"
                ));
            }
            None => return Err(anyhow!("missing summary option after '{c}'")),
        };
        *slot = setting;
    }

    Ok(FextOptions {
        case_insensitive: case_insensitive
            .resolve(cfg!(any(target_os = "macos", target_os = "windows"))),
        include_dotfiles: include_dotfiles.resolve(true),
        sort_ascending: sort_ascending.resolve(true),
    })
}

/// Label used for entries that have no file extension.
const NO_EXTENSION: &str = "(none)";

/// Accumulates search results and renders the requested summary.
pub struct Summarizer {
    spec: SummarySpec,
    counts: HashMap<String, u64>,
    directories: BTreeSet<PathBuf>,
}

impl Summarizer {
    pub fn new(spec: &SummarySpec) -> Self {
        Self {
            spec: spec.clone(),
            counts: HashMap::new(),
            directories: BTreeSet::new(),
        }
    }

    pub fn record(&mut self, entry: &DirEntry) {
        let SummarySpec::FileExtensions(options) = &self.spec else {
            let path = if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                entry.path()
            } else {
                entry.path().parent().unwrap_or(Path::new("."))
            };
            self.directories.insert(if path.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                path.to_path_buf()
            });
            return;
        };

        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy();

        let extension = if name.starts_with('.') {
            if !options.include_dotfiles {
                return;
            }
            // For dotfiles, the entire filename is the extension.
            name.to_string()
        } else {
            match name.rfind('.') {
                Some(pos) if pos + 1 < name.len() => name[pos + 1..].to_string(),
                _ => NO_EXTENSION.to_string(),
            }
        };

        let extension = if options.case_insensitive {
            extension.to_lowercase()
        } else {
            extension
        };

        *self.counts.entry(extension).or_insert(0) += 1;
    }

    pub fn write(
        &self,
        stdout: &mut impl Write,
        config: &Config,
        interrupt: &AtomicBool,
    ) -> io::Result<ExitCode> {
        let SummarySpec::FileExtensions(options) = &self.spec else {
            return self.write_directories(stdout, config, interrupt);
        };

        let mut entries: Vec<_> = self.counts.iter().collect();
        entries.sort_by(|(ext_a, count_a), (ext_b, count_b)| {
            let by_count = if options.sort_ascending {
                count_a.cmp(count_b)
            } else {
                count_b.cmp(count_a)
            };
            by_count.then_with(|| ext_a.cmp(ext_b))
        });

        let width = entries
            .iter()
            .map(|(_, count)| count.to_string().len())
            .max()
            .unwrap_or(1);

        for (extension, count) in entries {
            writeln!(stdout, "{count:>width$} {extension}")?;
        }

        Ok(ExitCode::Success)
    }

    fn write_directories(
        &self,
        stdout: &mut impl Write,
        config: &Config,
        interrupt: &AtomicBool,
    ) -> io::Result<ExitCode> {
        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => {
                print_error(format!("Could not resolve report directories: {error}"));
                return Ok(ExitCode::GeneralError);
            }
        };
        let mut directories = BTreeMap::<PathBuf, &PathBuf>::new();
        for path in &self.directories {
            // Do not resolve links or collapse '..': both can change path semantics.
            let key = cwd
                .join(path)
                .components()
                .filter(|c| *c != Component::CurDir)
                .collect();
            directories
                .entry(key)
                .and_modify(|existing| {
                    if (path.components().count(), path)
                        < (existing.components().count(), *existing)
                    {
                        *existing = path;
                    }
                })
                .or_insert(path);
        }
        let mut counter = count::Counter::new(
            config.follow_links,
            self.spec == SummarySpec::CountDescendants,
            config.one_file_system,
            interrupt,
        );
        let mut rows = Vec::with_capacity(directories.len());
        let mut status = ExitCode::Success;
        for path in directories.into_values() {
            if interrupt.load(Ordering::Relaxed) {
                return Ok(ExitCode::KilledBySigint);
            }
            match counter.count(path) {
                Ok(count) => {
                    for error in count.errors {
                        print_error(format!(
                            "Could not fully count '{}': {error}",
                            path.display()
                        ));
                        status = ExitCode::GeneralError;
                    }
                    rows.push((count.total, DirEntry::directory(path.clone())));
                }
                Err(count::CountError::Interrupted) => return Ok(ExitCode::KilledBySigint),
                Err(error) => {
                    print_error(format!("Could not count '{}': {error}", path.display()));
                    status = ExitCode::GeneralError;
                }
            }
        }
        rows.sort_by(|(left_count, left), (right_count, right)| {
            if let Some(sort) = &config.sort {
                walk::compare_entries(left, right, sort)
            } else {
                left_count.cmp(right_count).then_with(|| left.cmp(right))
            }
        });
        for (count, entry) in rows {
            if interrupt.load(Ordering::Relaxed) {
                return Ok(ExitCode::KilledBySigint);
            }
            write!(stdout, "{count}\t")?;
            output::print_entry(stdout, &entry, config)?;
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn disappeared_report_directories_still_print_rows() {
        let temp = tempfile::tempdir().unwrap();
        let removed = temp.path().join("removed");
        let replaced = temp.path().join("replaced");
        let readable = temp.path().join("readable");
        for path in [&removed, &replaced, &readable] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(readable.join("file"), "").unwrap();
        let mut summary = Summarizer::new(&SummarySpec::CountChildren);
        for path in [&removed, &replaced, &readable] {
            summary.record(&DirEntry::directory(path.clone()));
        }
        std::fs::remove_dir(&removed).unwrap();
        std::fs::remove_dir(&replaced).unwrap();
        std::fs::write(&replaced, "").unwrap();

        let interrupt = AtomicBool::new(false);
        for spec in [SummarySpec::CountChildren, SummarySpec::CountDescendants] {
            summary.spec = spec;
            for sort in [None, Some("p"), Some("m")] {
                let mut args = vec![
                    "fd",
                    "--no-user-matchsets",
                    "--color=never",
                    "--path-separator=/",
                ];
                if let Some(sort) = sort {
                    args.extend(["--sort", sort]);
                }
                let opts = crate::cli::Opts::try_parse_from(args).unwrap();
                let config = crate::construct_config(opts, &[], Vec::new(), None, None).unwrap();
                let mut output = Vec::new();
                assert_eq!(
                    summary.write(&mut output, &config, &interrupt).unwrap(),
                    ExitCode::GeneralError
                );
                let output = String::from_utf8(output).unwrap();
                let mut lines: Vec<_> = output.lines().collect();
                lines.sort_unstable();
                assert_eq!(
                    lines,
                    [
                        format!("0\t{}/", removed.display()).replace('\\', "/"),
                        format!("0\t{}/", replaced.display()).replace('\\', "/"),
                        format!("1\t{}/", readable.display()).replace('\\', "/"),
                    ]
                );
            }
        }
    }

    fn fext_options(spec: &str) -> FextOptions {
        match spec.parse().unwrap() {
            SummarySpec::FileExtensions(options) => options,
            _ => panic!("expected fext"),
        }
    }

    #[test]
    fn parse_defaults() {
        let auto_case_insensitive = cfg!(any(target_os = "macos", target_os = "windows"));
        for spec in ["fext", "fext:", "fext:@i@d@s"] {
            let options = fext_options(spec);
            assert_eq!(options.case_insensitive, auto_case_insensitive);
            assert!(options.include_dotfiles);
            assert!(options.sort_ascending);
        }
    }

    #[test]
    fn parse_explicit_options() {
        let options = fext_options("fext:i-d-s");
        assert!(options.case_insensitive);
        assert!(!options.include_dotfiles);
        assert!(!options.sort_ascending);

        let options = fext_options("fext:-ids");
        assert!(!options.case_insensitive);
        assert!(options.include_dotfiles);
        assert!(options.sort_ascending);
    }

    #[test]
    fn parse_last_occurrence_wins() {
        let options = fext_options("fext:i-i");
        assert!(!options.case_insensitive);

        let options = fext_options("fext:-ss");
        assert!(options.sort_ascending);
    }

    #[test]
    fn parse_errors() {
        assert!("bogus".parse::<SummarySpec>().is_err());
        assert!("fext:x".parse::<SummarySpec>().is_err());
        assert!("fext:-x".parse::<SummarySpec>().is_err());
        assert!("fext:i-".parse::<SummarySpec>().is_err());
        assert!("fext:@".parse::<SummarySpec>().is_err());
    }
}
