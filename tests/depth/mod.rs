use std::fs;

use crate::testenv::TestEnv;

fn fixture() -> TestEnv {
    TestEnv::new(&["alpha/nested/deep", "beta", "one/two"], &["alpha/file"])
}

#[test]
fn zero_depth_spellings_and_roots() {
    let te = fixture();
    for flags in [
        vec!["-d0"],
        vec!["--max-depth", "0"],
        vec!["--exact-depth", "0"],
    ] {
        te.assert_output(&flags, "./");
        let mut args = flags;
        args.extend([".", "alpha", "beta"]);
        te.assert_output(&args, "alpha/\nbeta/");
    }
    te.assert_output(&["-d0", ".", "alpha", "alpha"], "alpha/\nalpha/");
    te.assert_output(
        &["-d0", ".", "alpha", "alpha/nested"],
        "alpha/\nalpha/nested/",
    );
    te.assert_output(&["-d0", ".", "--search-path", "alpha"], "alpha/");
}

#[test]
fn zero_depth_special_paths() {
    let te = fixture();
    for root in [".", "./", "alpha/.."] {
        let expected = if root == "alpha/.." {
            "alpha/../"
        } else {
            "./"
        };
        te.assert_output(&["-d0", ".", root], expected);
    }
    te.assert_output_subdirectory("alpha", &["-d0", ".", ".."], "../");
    let absolute = crate::get_absolute_root_path(&te);
    te.assert_output(&["-d0", ".", &absolute], &format!("{absolute}/"));
    te.assert_output(
        &["-d0", "--absolute-path", ".", "alpha"],
        &format!("{absolute}/alpha/"),
    );
    let root_path = te.test_root();
    let root = root_path.ancestors().last().unwrap().to_str().unwrap();
    te.assert_output(&["-d0", ".", root], root);
}

#[test]
fn zero_depth_preserves_other_depths() {
    let te = fixture();
    te.assert_output(&["-d0", "--min-depth", "1", ".", "alpha"], "");
    for flags in [
        vec!["-d1"],
        vec!["--exact-depth", "1"],
        vec!["-d1", "--min-depth", "0"],
    ] {
        let mut args = flags;
        args.extend([".", "alpha"]);
        te.assert_output(&args, "alpha/file\nalpha/nested/");
    }
    for flags in [vec![], vec!["--min-depth", "0"]] {
        let mut args = flags;
        args.extend([".", "alpha"]);
        te.assert_output(&args, "alpha/file\nalpha/nested/\nalpha/nested/deep/");
    }
}

#[test]
fn zero_depth_filters_roots() {
    let te = fixture();
    te.assert_output(&["-d0", "^alpha$", "alpha", "beta"], "alpha/");
    te.assert_output(&["-d0", "--glob", "a*", "alpha", "beta"], "alpha/");
    te.assert_output(&["-d0", "--exact", "alpha", "alpha", "beta"], "alpha/");
    te.assert_output(
        &["-d0", "--full-path", r"[/\\]alpha$", "alpha", "beta"],
        "alpha/",
    );
    te.assert_output(&["-d0", "-td", ".", "alpha"], "alpha/");
    te.assert_output(&["-d0", "-tf", ".", "alpha"], "");
    te.assert_output(&["-d0", "-t", "empty", ".", "alpha", "beta"], "beta/");
    te.assert_output(
        &["-d0", "--exclude-if", "${/} == beta", ".", "alpha", "beta"],
        "alpha/",
    );
    te.assert_output(
        &["-d0", "--bash", "${/} == alpha", "alpha", "beta"],
        "alpha/",
    );
    te.assert_output(&["-d0", "--prune-if", "-d ${}", ".", "alpha"], "alpha/");
    te.assert_output(
        &["-d0", "--ignore-contain", "file", ".", "alpha", "beta"],
        "beta/",
    );
    let old = filetime::FileTime::from_unix_time(946_684_800, 0);
    filetime::set_file_mtime(te.test_root().join("alpha"), old).unwrap();
    te.assert_output(
        &[
            "-d0",
            "--changed-before",
            "2001-01-01",
            ".",
            "alpha",
            "beta",
        ],
        "alpha/",
    );
}

#[test]
fn zero_depth_matchsets() {
    let te = fixture().matchsets_file(r#""pick" { (d) name literal full { "alpha"; }; }"#);
    te.assert_output(&["-d0", "-m", "pick", ".", "alpha", "beta"], "alpha/");
    te.assert_output(&["-d0", "-M", "pick", ".", "alpha", "beta"], "beta/");
}

#[test]
fn zero_depth_retains_root_ignore_exemptions() {
    let te = TestEnv::new(
        &[
            ".hidden",
            "fdignored.foo",
            "gitignored.foo",
            "excluded",
            "global",
        ],
        &[],
    )
    .global_ignore_file("global");
    te.assert_output(
        &[
            "-d0",
            "-E",
            "excluded",
            ".",
            ".hidden",
            "fdignored.foo",
            "gitignored.foo",
            "excluded",
            "global",
        ],
        ".hidden/\nfdignored.foo/\ngitignored.foo/\nexcluded/\nglobal/",
    );
    te.assert_output(&["-d1", "-E", "excluded"], "symlink");
}

#[test]
fn zero_depth_directory_symlink() {
    let te = fixture();
    for flags in [vec![], vec!["-L"]] {
        let mut args = vec!["-d0", "-td"];
        args.extend(flags);
        args.extend([".", "symlink"]);
        te.assert_output(&args, "symlink/");
    }
}

#[test]
fn zero_depth_rejects_non_directory_inputs() {
    let te = fixture();
    te.assert_failure_with_error(
        &["-d0", ".", "alpha/file"],
        "[fd error]: Search path 'alpha/file' is not a directory.",
    );
    te.assert_failure_with_error(
        &["-d0", ".", "missing"],
        "[fd error]: Search path 'missing' is not a directory.",
    );
}

#[test]
fn zero_depth_current_directory_variables() {
    let te = fixture();
    // Exercise both compiled predicates and the general evaluator after './' stripping.
    te.assert_output(&["-d0", "--bash", "${fd_path} == ."], "./");
    te.assert_output(
        &[
            "-d0",
            "--bash",
            "${#fd_path} -eq 1 && ${fd_name} == . && -d ${}",
        ],
        "./",
    );
    te.assert_output(&["-d0", "--format", "{}|{/}|{.}|{/.}|{//}"], ".|.|.|.|.");
    te.assert_output(&["-d0", "--format", "{.}", ".", "alpha/.."], "alpha/..");
}

#[test]
fn zero_depth_output_separators() {
    let te = fixture();
    let ansi = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    for color in ["never", "always"] {
        for root in ["alpha/", "./"] {
            let expected = if root == "./" { ".@\n" } else { "alpha@\n" };
            let output = te.assert_success_and_get_output(
                ".",
                &["-d0", "--color", color, "--path-separator=@", ".", root],
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert_eq!(ansi.replace_all(&text, ""), expected);
        }
    }
    let output = te.assert_success_and_get_output(
        ".",
        &[
            "-d0",
            "--color=never",
            "--path-separator=/",
            "-l",
            ".",
            "alpha/",
        ],
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .ends_with(" alpha/\n")
    );
    crate::assert_exact_output(
        &te,
        &["-d0", "-0", "--path-separator=/", ".", "alpha/"],
        "alpha/\0",
    );
}

#[test]
fn zero_depth_sorting_limits_and_quiet() {
    let te = fixture();
    crate::assert_exact_output(
        &te,
        &["-d0", "-Rn", "--path-separator=/", ".", "beta", "alpha"],
        "alpha/\nbeta/\n",
    );
    let output =
        te.assert_success_and_get_output(".", &["-d0", "--max-results", "1", ".", "alpha", "beta"]);
    assert_eq!(String::from_utf8(output.stdout).unwrap().lines().count(), 1);
    te.assert_output(&["-d0", "-q", ".", "alpha"], "");
    te.assert_failure(&["-d0", "-q", "nomatch", "alpha"]);
}

#[test]
fn zero_depth_directory_summaries() {
    let te = fixture();
    for (mode, count) in [("count-children", 2), ("count-descendants", 3)] {
        crate::assert_exact_output(
            &te,
            &["-d0", "--summary", mode, "--path-separator=/", ".", "alpha"],
            &format!("{count}\talpha/\n"),
        );
    }
}

#[cfg(unix)]
#[test]
fn zero_depth_exec_roots() {
    let te = fixture();
    for mode in ["-x", "-X"] {
        te.assert_output(
            &["-d0", ".", "alpha", "beta", mode, "printf", "<%s>\n"],
            "<alpha>\n<beta>",
        );
        te.assert_output(&["-d0", mode, "printf", "<%s>\n"], "<./>");
        te.assert_output(
            &["-d0", "--strip-cwd-prefix", mode, "printf", "<%s>\n"],
            "<.>",
        );
        te.assert_output(
            &["-d0", "nomatch", "alpha", mode, "sh", "-c", "exit 27"],
            "",
        );
    }
    te.assert_output(
        &[
            "-d0",
            "--batch-size",
            "1",
            "--batch-threads",
            "2",
            ".",
            "alpha",
            "beta",
            "-X",
            "sh",
            "-c",
            r#"test "$#" -eq 1 || exit 1; printf '<%s>\n' "$@""#,
            "sh",
        ],
        "<alpha>\n<beta>",
    );
}

#[test]
fn zero_depth_empty_directory_is_a_result() {
    let te = fixture();
    // Changing descendants cannot change root selection.
    fs::remove_dir(te.test_root().join("alpha/nested/deep")).unwrap();
    te.assert_output(&["-d0", ".", "alpha/nested"], "alpha/nested/");
}
