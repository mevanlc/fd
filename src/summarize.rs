use std::collections::HashMap;
use std::io::{self, Write};
use std::str::FromStr;

use anyhow::anyhow;

use crate::dir_entry::DirEntry;

/// A summary to produce instead of the regular search results (`--summarize`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizeSpec {
    /// Summarize the file extensions of the search results (`fext`).
    FileExtensions(FextOptions),
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

impl FromStr for SummarizeSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, options) = match s.split_once(':') {
            Some((name, options)) => (name, options),
            None => (s, ""),
        };

        match name {
            "fext" => Ok(SummarizeSpec::FileExtensions(parse_fext_options(options)?)),
            _ => Err(anyhow!("unknown summary type '{name}' (expected 'fext')")),
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
    spec: SummarizeSpec,
    counts: HashMap<String, u64>,
}

impl Summarizer {
    pub fn new(spec: &SummarizeSpec) -> Self {
        Self {
            spec: spec.clone(),
            counts: HashMap::new(),
        }
    }

    pub fn record(&mut self, entry: &DirEntry) {
        let SummarizeSpec::FileExtensions(options) = &self.spec;

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

    pub fn write(&self, stdout: &mut impl Write) -> io::Result<()> {
        let SummarizeSpec::FileExtensions(options) = &self.spec;

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

        const HEADER: &str = "File Extensions Summary";
        writeln!(stdout, "{HEADER}")?;
        writeln!(stdout, "{}", "-".repeat(HEADER.len()))?;
        for (extension, count) in entries {
            writeln!(stdout, "{count:>width$} {extension}")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fext_options(spec: &str) -> FextOptions {
        match spec.parse().unwrap() {
            SummarizeSpec::FileExtensions(options) => options,
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
        assert!("bogus".parse::<SummarizeSpec>().is_err());
        assert!("fext:x".parse::<SummarizeSpec>().is_err());
        assert!("fext:-x".parse::<SummarizeSpec>().is_err());
        assert!("fext:i-".parse::<SummarizeSpec>().is_err());
        assert!("fext:@".parse::<SummarizeSpec>().is_err());
    }
}
