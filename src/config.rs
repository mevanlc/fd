use std::{path::PathBuf, sync::Arc, time::Duration};

use lscolors::LsColors;
use regex::bytes::RegexSet;

use crate::bash_cond::Condition;
use crate::exec::CommandSet;
use crate::filetypes::FileTypes;
#[cfg(unix)]
use crate::filter::OwnerFilter;
use crate::filter::{SizeFilter, TimeFilter};
use crate::fmt::FormatTemplate;
use crate::matchsets::CompiledMatchset;
use crate::summarize::SummarizeSpec;

/// Configuration options for *fd*.
pub struct Config {
    /// Whether the search is case-sensitive or case-insensitive.
    pub case_sensitive: bool,

    /// Whether to compile search patterns with PCRE2 instead of the default engine.
    pub pcre2: bool,

    /// Cached current working directory for absolute path construction.
    /// Populated when full-path matching is active; `None` means search by filename only.
    pub full_path_base: Option<PathBuf>,

    /// Whether to ignore hidden files and directories (or not).
    pub ignore_hidden: bool,

    /// Whether to respect `.fdignore` files or not.
    pub read_fdignore: bool,

    /// Whether to respect ignore files in parent directories or not.
    pub read_parent_ignore: bool,

    /// Whether to respect VCS ignore files (`.gitignore`, ..) or not.
    pub read_vcsignore: bool,

    /// Whether to require a `.git` directory to respect gitignore files.
    pub require_git_to_read_vcsignore: bool,

    /// Whether to respect the global ignore file or not.
    pub read_global_ignore: bool,

    /// Whether to follow symlinks or not.
    pub follow_links: bool,

    /// Whether to limit the search to starting file system or not.
    pub one_file_system: bool,

    /// Whether elements of output should be separated by a null character
    pub null_separator: bool,

    /// The maximum search depth, or `None` if no maximum search depth should be set.
    ///
    /// A depth of `1` includes all files under the current directory, a depth of `2` also includes
    /// all files under subdirectories of the current directory, etc.
    pub max_depth: Option<usize>,

    /// The minimum depth for reported entries, or `None`.
    pub min_depth: Option<usize>,

    /// Whether to stop traversing into matching directories.
    pub prune: bool,

    /// Bash conditional expressions that all entries must satisfy.
    pub bash_patterns: Vec<Condition>,

    /// Bash conditional expression that skips descendants of matching directories.
    pub prune_if: Option<Condition>,

    /// Bash conditional expression that excludes matching entries.
    pub exclude_if: Option<Condition>,

    /// Matchsets that include matching entries.
    pub include_matchsets: Vec<CompiledMatchset>,

    /// Matchsets that exclude matching entries.
    pub exclude_matchsets: Vec<CompiledMatchset>,

    /// The number of threads to use.
    pub threads: usize,

    /// If true, the program doesn't print anything and will instead return an exit code of 0
    /// if there's at least one match. Otherwise, the exit code will be 1.
    pub quiet: bool,

    /// Time to buffer results internally before streaming to the console. This is useful to
    /// provide a sorted output, in case the total execution time is shorter than
    /// `max_buffer_time`.
    pub max_buffer_time: Option<Duration>,

    /// Sort criteria for output results. If this is set, all results are buffered until
    /// traversal finishes, then sorted.
    pub sort: Option<SortConfig>,

    /// `None` if the output should not be colorized. Otherwise, a `LsColors` instance that defines
    /// how to style different filetypes.
    pub ls_colors: Option<LsColors>,

    /// Whether or not we are writing to an interactive terminal
    #[cfg_attr(not(unix), allow(unused))]
    pub interactive_terminal: bool,

    /// Whether to print entries in a long listing format.
    pub list_details: bool,

    /// The type of file to search for. If set to `None`, all file types are displayed. If
    /// set to `Some(..)`, only the types that are specified are shown.
    pub file_types: Option<FileTypes>,

    /// The extension to search for. Only entries matching the extension will be included.
    ///
    /// The value (if present) will be a lowercase string without leading dots.
    pub extensions: Option<RegexSet>,

    /// A format string to use to format results, similarly to exec
    pub format: Option<FormatTemplate>,

    /// Print a summary of the search results instead of the results themselves.
    pub summarize: Option<SummarizeSpec>,

    /// If a value is supplied, each item found will be used to generate and execute commands.
    pub command: Option<Arc<CommandSet>>,

    /// Maximum number of search results to pass to each `command`. If zero, the number is
    /// unlimited.
    pub batch_size: usize,

    /// A list of glob patterns that should be excluded from the search.
    pub exclude_patterns: Vec<String>,

    /// A list of custom ignore files.
    pub ignore_files: Vec<PathBuf>,

    /// The given constraints on the size of returned files
    pub size_constraints: Vec<SizeFilter>,

    /// Constraints on last modification time of files
    pub time_constraints: Vec<TimeFilter>,

    #[cfg(unix)]
    /// User/group ownership constraint
    pub owner_constraint: Option<OwnerFilter>,

    /// Whether or not to display filesystem errors
    pub show_filesystem_errors: bool,

    /// The separator used to print file paths.
    pub path_separator: Option<String>,

    /// The actual separator, either the system default separator or `path_separator`
    pub actual_path_separator: String,

    /// The maximum number of search results
    pub max_results: Option<usize>,

    /// Whether or not to strip the './' prefix for search results
    pub strip_cwd_prefix: bool,

    /// Whether or not to use hyperlinks on paths
    pub hyperlink: bool,

    /// Names that should stop traversal down their parent. (e.g. https://bford.info/cachedir/).
    pub ignore_contain: Vec<String>,
}

impl Config {
    /// The separator used for paths leaving fd. On Windows, returning the
    /// native separator explicitly also normalizes separators retained from
    /// search roots such as `.`.
    pub fn effective_path_separator(&self) -> Option<&str> {
        if cfg!(windows) {
            Some(&self.actual_path_separator)
        } else {
            self.path_separator.as_deref()
        }
    }

    /// Check whether results are being printed.
    pub fn is_printing(&self) -> bool {
        self.command.is_none()
    }

    pub fn uses_bash_patterns(&self) -> bool {
        !self.bash_patterns.is_empty()
    }
}

/// Supported sort keys.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SortBy {
    Path,
    Basename,
    Extension,
    Type,
    Changed,
    Modified,
    Accessed,
    Born,
    Inode,
    Size,
}

/// Collation options for text-based sort keys.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct SortTextOptions {
    /// Compare text case-insensitively for p/P, n/N and e/E.
    pub case_insensitive: bool,
    /// Use natural-number sorting for p/P, n/N and e/E.
    pub natural: bool,
}

/// A sort key, with explicit ordering direction.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SortCriterion {
    pub by: SortBy,
    pub descending: bool,
}

/// Sort expression configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortConfig {
    pub criteria: Vec<SortCriterion>,
    pub text: SortTextOptions,
}
