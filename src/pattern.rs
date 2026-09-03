//! The compiled form of a user-supplied search pattern.
//!
//! fd matches against raw path bytes, which may not be valid UTF-8 on Unix, so
//! both engines here are byte-oriented. The default engine is the `regex`
//! crate, which has no lookaround or backreferences. Passing `--pcre2` selects
//! PCRE2 instead, which supports both.

use anyhow::Result;
use regex::bytes::Regex;

#[cfg(any(windows, test))]
use std::borrow::Cow;

/// Appended to a compilation failure from the default engine. The suggested
/// options are all alternatives to writing a regular expression at all.
const REGEX_ERROR_HINT: &str = concat!(
    "Note: You can search for literal substrings with '--fixed-strings' or literal ",
    "strings with '--exact' options (instead of a regular expression). Alternatively, ",
    "you can also use the '--glob' option to match on a glob pattern.",
);

/// Appended to a compilation failure from PCRE2. `--fixed-strings`, `--exact`
/// and `--glob` all conflict with `--pcre2`, so the hint has to mention dropping
/// it.
#[cfg(feature = "pcre2")]
const PCRE2_ERROR_HINT: &str = concat!(
    "Note: This pattern was compiled with PCRE2 ('--pcre2'). Drop '--pcre2' to use ",
    "fd's default regex engine, or search for literal text with the '--fixed-strings' ",
    "or '--exact' options.",
);

/// A search pattern compiled by one of fd's regex engines.
pub enum Pattern {
    Regex(RegexMatcher),
    #[cfg(feature = "pcre2")]
    Pcre2(pcre2::bytes::Regex),
}

/// A byte regex together with the candidate representation it expects.
///
/// `globset` normalizes Windows path separators to `/` before applying the
/// regex generated for a glob. fd uses that generated regex directly, so it
/// must retain the glob origin and perform the same normalization itself.
#[derive(Clone)]
pub(crate) enum RegexMatcher {
    Native(Regex),
    Glob(Regex),
}

impl RegexMatcher {
    pub(crate) fn native(regex: Regex) -> Self {
        Self::Native(regex)
    }

    pub(crate) fn glob(regex: Regex) -> Self {
        Self::Glob(regex)
    }

    pub(crate) fn is_match(&self, haystack: &[u8]) -> bool {
        match self {
            Self::Native(regex) => regex.is_match(haystack),
            Self::Glob(regex) => is_glob_match(regex, haystack),
        }
    }
}

impl Pattern {
    /// Compile `pattern`, using PCRE2 when `use_pcre2` is set.
    pub fn build(
        pattern: &str,
        case_sensitive: bool,
        use_pcre2: bool,
        is_glob: bool,
    ) -> Result<Self> {
        if use_pcre2 {
            build_pcre2(pattern, case_sensitive)
        } else {
            build_regex(pattern, case_sensitive, is_glob)
        }
    }

    /// Test the pattern against a subject.
    ///
    /// PCRE2 can fail at match time (for example by exhausting its backtracking
    /// limit on a pathological pattern), so this is fallible even though the
    /// default engine never errors.
    pub fn is_match(&self, haystack: &[u8]) -> Result<bool> {
        match self {
            Pattern::Regex(re) => Ok(re.is_match(haystack)),
            #[cfg(feature = "pcre2")]
            Pattern::Pcre2(re) => re.is_match(haystack).map_err(Into::into),
        }
    }
}

fn build_regex(pattern: &str, case_sensitive: bool, is_glob: bool) -> Result<Pattern> {
    regex::bytes::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .dot_matches_new_line(true)
        .build()
        .map(|regex| {
            let matcher = if is_glob {
                RegexMatcher::glob(regex)
            } else {
                RegexMatcher::native(regex)
            };
            Pattern::Regex(matcher)
        })
        .map_err(|e| anyhow::anyhow!("{e}\n\n{REGEX_ERROR_HINT}"))
}

#[cfg(not(windows))]
fn is_glob_match(regex: &Regex, haystack: &[u8]) -> bool {
    regex.is_match(haystack)
}

#[cfg(windows)]
fn is_glob_match(regex: &Regex, haystack: &[u8]) -> bool {
    regex.is_match(normalize_glob_candidate(haystack).as_ref())
}

/// Mirror `globset::Candidate`'s Windows path normalization without changing
/// the native path used by any other matcher or filesystem operation.
#[cfg(any(windows, test))]
fn normalize_glob_candidate(candidate: &[u8]) -> Cow<'_, [u8]> {
    if !candidate.contains(&b'\\') {
        return Cow::Borrowed(candidate);
    }

    Cow::Owned(
        candidate
            .iter()
            .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
            .collect(),
    )
}

#[cfg(feature = "pcre2")]
fn build_pcre2(pattern: &str, case_sensitive: bool) -> Result<Pattern> {
    pcre2::bytes::RegexBuilder::new()
        .caseless(!case_sensitive)
        // Mirrors the default engine's dot_matches_new_line(true).
        .dotall(true)
        // Unicode mode, to line up with the default engine: '.' matches a whole
        // codepoint and \w/\d/\s are Unicode-aware.
        //
        // `ucp` is what makes this safe on filenames that are not valid UTF-8.
        // It is not merely PCRE2_UCP: the pcre2 crate also sets PCRE2_UTF and
        // PCRE2_MATCH_INVALID_UTF alongside it, and the latter downgrades an
        // ill-formed subject from a match-time *error* to a region that simply
        // never matches. Setting `.utf(true)` without `.ucp(true)` omits
        // PCRE2_MATCH_INVALID_UTF and does error on such filenames.
        //
        // Unlike the default engine, invalid bytes cannot be targeted
        // deliberately -- there is no PCRE2 equivalent of `(?-u)`.
        .utf(true)
        .ucp(true)
        .jit_if_available(true)
        // PCRE2 defaults to a 32KB JIT stack; ripgrep uses 10MB and notes that
        // 1MB should be enough for anything.
        .max_jit_stack_size(Some(10 * (1 << 20)))
        .build(pattern)
        .map(Pattern::Pcre2)
        .map_err(|e| anyhow::anyhow!("{e}\n\n{PCRE2_ERROR_HINT}"))
}

#[cfg(not(feature = "pcre2"))]
fn build_pcre2(_pattern: &str, _case_sensitive: bool) -> Result<Pattern> {
    anyhow::bail!(
        "PCRE2 is not available in this build of fd. \
         Rebuild fd with '--features pcre2' to use '--pcre2'."
    )
}

#[cfg(test)]
mod tests {
    use super::normalize_glob_candidate;
    use std::borrow::Cow;

    #[test]
    fn windows_glob_candidate_normalization_is_borrowed_when_possible() {
        let slash_path = b"C:/work/repo/src/main.rs";
        assert!(matches!(
            normalize_glob_candidate(slash_path),
            Cow::Borrowed(_)
        ));

        let backslash_path = br"C:\work\repo\src\main.rs";
        let normalized = normalize_glob_candidate(backslash_path);
        assert!(matches!(normalized, Cow::Owned(_)));
        assert_eq!(normalized.as_ref(), slash_path);
    }
}
