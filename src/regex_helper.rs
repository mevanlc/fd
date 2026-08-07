use regex_syntax::ParserBuilder;
use regex_syntax::hir::Hir;

/// Determine if a regex pattern contains a literal uppercase character.
pub fn pattern_has_uppercase_char(pattern: &str) -> bool {
    let mut parser = ParserBuilder::new().utf8(false).build();

    parser
        .parse(pattern)
        .map(|hir| hir_has_uppercase_char(&hir))
        .unwrap_or_else(|_| raw_pattern_has_uppercase_char(pattern))
}

/// Determine if a regex pattern contains a literal uppercase character by
/// scanning the pattern text directly.
///
/// This is the fallback for patterns `regex-syntax` cannot parse. Under
/// `--pcre2` that is exactly the look-around and backreference patterns the
/// engine exists to support, and reporting those as lowercase would silently
/// turn smart case off for them.
fn raw_pattern_has_uppercase_char(pattern: &str) -> bool {
    let mut chars = pattern.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\\' {
            if c.is_uppercase() {
                return true;
            }
            continue;
        }

        // A backslash escapes the following character. Skip it, so that `\A`,
        // `\W`, `\S` and friends are not mistaken for literal uppercase.
        if chars.next() == Some('x') {
            // Hex escapes carry digits of their own; skip those too, so that
            // the `F` in `\x6F` does not register as uppercase.
            if chars.peek() == Some(&'{') {
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                }
            } else {
                for _ in 0..2 {
                    if chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        }
    }

    false
}

/// Determine if a regex expression contains a literal uppercase character.
fn hir_has_uppercase_char(hir: &Hir) -> bool {
    use regex_syntax::hir::*;

    match hir.kind() {
        HirKind::Literal(Literal(bytes)) => match std::str::from_utf8(bytes) {
            Ok(s) => s.chars().any(|c| c.is_uppercase()),
            Err(_) => bytes.iter().any(|b| char::from(*b).is_uppercase()),
        },
        HirKind::Class(Class::Unicode(ranges)) => ranges
            .iter()
            .any(|r| r.start().is_uppercase() || r.end().is_uppercase()),
        HirKind::Class(Class::Bytes(ranges)) => ranges
            .iter()
            .any(|r| char::from(r.start()).is_uppercase() || char::from(r.end()).is_uppercase()),
        HirKind::Capture(Capture { sub, .. }) | HirKind::Repetition(Repetition { sub, .. }) => {
            hir_has_uppercase_char(sub)
        }
        HirKind::Concat(hirs) | HirKind::Alternation(hirs) => {
            hirs.iter().any(hir_has_uppercase_char)
        }
        _ => false,
    }
}

/// Determine if a regex pattern only matches strings starting with a literal dot (hidden files)
pub fn pattern_matches_strings_with_leading_dot(pattern: &str) -> bool {
    let mut parser = ParserBuilder::new().utf8(false).build();

    parser
        .parse(pattern)
        .map(|hir| hir_matches_strings_with_leading_dot(&hir))
        .unwrap_or(false)
}

/// See above.
fn hir_matches_strings_with_leading_dot(hir: &Hir) -> bool {
    use regex_syntax::hir::*;

    // Note: this only really detects the simplest case where a regex starts with
    // "^\\.", i.e. a start text anchor and a literal dot character. There are a lot
    // of other patterns that ONLY match hidden files, e.g. ^(\\.foo|\\.bar) which are
    // not (yet) detected by this algorithm.
    match hir.kind() {
        HirKind::Concat(hirs) => {
            let mut hirs = hirs.iter();
            if let Some(hir) = hirs.next() {
                if hir.kind() != &HirKind::Look(Look::Start) {
                    return false;
                }
            } else {
                return false;
            }

            if let Some(hir) = hirs.next() {
                match hir.kind() {
                    HirKind::Literal(Literal(bytes)) => bytes.starts_with(b"."),
                    _ => false,
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

#[test]
fn pattern_has_uppercase_char_simple() {
    assert!(pattern_has_uppercase_char("A"));
    assert!(pattern_has_uppercase_char("foo.EXE"));

    assert!(!pattern_has_uppercase_char("a"));
    assert!(!pattern_has_uppercase_char("foo.exe123"));
}

#[test]
fn pattern_has_uppercase_char_advanced() {
    assert!(pattern_has_uppercase_char("foo.[a-zA-Z]"));

    assert!(!pattern_has_uppercase_char(r"\Acargo"));
    assert!(!pattern_has_uppercase_char(r"carg\x6F"));
}

/// Patterns using look-around or backreferences cannot be parsed by
/// `regex-syntax`, so these exercise the raw-scan fallback rather than the HIR
/// walk. They are only compilable by PCRE2, but smart case is decided before
/// the engine is chosen.
#[test]
fn pattern_has_uppercase_char_unparseable() {
    assert!(pattern_has_uppercase_char(r"(?<!Foo)bar"));
    assert!(pattern_has_uppercase_char(r"(\w+)_\1_Baz"));
    assert!(pattern_has_uppercase_char(r"foo(?=BAR)"));

    assert!(!pattern_has_uppercase_char(r"(?<!foo)bar"));
    assert!(!pattern_has_uppercase_char(r"(\w+)_\1"));
    // The uppercase letters here are all escape sequences, not literals.
    assert!(!pattern_has_uppercase_char(r"(?<!\W)\Acargo\B\S"));
    assert!(!pattern_has_uppercase_char(r"(?<!x)carg\x6F"));
    assert!(!pattern_has_uppercase_char(r"(?<!x)carg\x{6F}"));
}

#[test]
fn matches_strings_with_leading_dot_simple() {
    assert!(pattern_matches_strings_with_leading_dot("^\\.gitignore"));

    assert!(!pattern_matches_strings_with_leading_dot("^.gitignore"));
    assert!(!pattern_matches_strings_with_leading_dot("\\.gitignore"));
    assert!(!pattern_matches_strings_with_leading_dot("^gitignore"));
}
