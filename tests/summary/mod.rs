use std::fs;
#[cfg(unix)]
use std::process::Command;

use crate::testenv::TestEnv;

const MODES: [&str; 2] = ["count-children", "count-descendants"];

fn fixture() -> TestEnv {
    TestEnv::new(
        &["report/empty", "report/nested/deep"],
        &[
            "report/a.rs",
            "report/b.txt",
            "report/.hidden",
            "report/nested/c.rs",
            "report/nested/deep/d.txt",
        ],
    )
}

fn assert_report(te: &TestEnv, mode: &str, args: &[&str], expected: &str) {
    let mut arguments = vec!["--summary", mode, "--color=never", "--path-separator=/"];
    arguments.extend_from_slice(args);
    crate::assert_exact_output(te, &arguments, expected);
}

#[test]
fn complete_rows_and_default_order() {
    let te = fixture();
    assert_report(
        &te,
        MODES[0],
        &["", "report"],
        "0\treport/empty/\n1\treport/nested/deep/\n2\treport/nested/\n5\treport/\n",
    );
    assert_report(
        &te,
        MODES[1],
        &["", "report"],
        "0\treport/empty/\n1\treport/nested/deep/\n3\treport/nested/\n8\treport/\n",
    );
    for mode in MODES {
        assert_report(&te, mode, &["does-not-exist"], "");
    }
    assert_report(&te, "fext", &["does-not-exist"], "");
}

#[test]
fn selection_filters_do_not_change_counts() {
    let te = fixture().matchsets_file(
        r#"
        "pick" { (d) name literal full { "report"; }; }
        "skip" { name literal full { "nested"; "b.txt"; }; }
    "#,
    );
    fs::write(
        te.test_root().join(".fdignore"),
        "report/nested\nreport/b.txt\n",
    )
    .unwrap();
    fs::write(te.test_root().join("report/a.rs"), "nonempty").unwrap();
    for (mode, count) in MODES.into_iter().zip([5, 8]) {
        let expected = format!("{count}\treport/\n");
        for flags in [
            vec![],
            vec!["-H", "-I"],
            vec!["-t", "d"],
            vec!["--max-depth", "1"],
            vec!["--min-depth", "1"],
            vec!["--exact-depth", "1"],
            vec!["-E", "nested", "-E", "*.txt"],
            vec!["--prune"],
            vec!["--prune-if", "-d ${}"],
            vec!["--exclude-if", "${/} == b.txt"],
            vec!["-m", "pick"],
            vec!["-M", "skip"],
            vec!["--ignore-contain", "c.rs"],
            vec!["--max-results", "1"],
            vec!["-1"],
        ] {
            let mut args = vec!["--exact", "report"];
            args.extend(flags);
            assert_report(&te, mode, &args, &expected);
        }
        // The introduced parent does not itself satisfy these file-only predicates.
        assert_report(
            &te,
            mode,
            &[
                "--exact",
                "a.rs",
                "-tf",
                "-e",
                "rs",
                "--size",
                "+0b",
                "--changed-within",
                "1h",
            ],
            &expected,
        );
        assert_report(&te, mode, &["--bash", "${/} == a.rs"], &expected);
    }
}

#[test]
fn duplicate_and_overlapping_search_paths() {
    let te = fixture();
    let absolute = crate::get_absolute_root_path(&te);
    for (mode, count) in MODES.into_iter().zip([5, 8]) {
        assert_report(
            &te,
            mode,
            &["^(report|a.rs|b.txt)$", ".", "./.", &absolute, "report"],
            &format!("{count}\treport/\n"),
        );
    }
}

#[test]
fn parent_of_top_level_file_and_path_controls() {
    let te = fixture();
    // A directory root is normally not itself a search result, but a file selects it.
    for (mode, count) in MODES.into_iter().zip([5, 8]) {
        let output = te.assert_success_and_get_output(
            "report",
            &["--summary", mode, "--exact", "a.rs", "--path-separator=/"],
        );
        assert_eq!(output.stdout, format!("{count}\t./\n").as_bytes());
        assert_report(
            &te,
            mode,
            &["--exact", "a.rs", "-0"],
            &format!("{count}\t./report/\0"),
        );
        assert_report(
            &te,
            mode,
            &["--exact", "a.rs", "--strip-cwd-prefix=never"],
            &format!("{count}\t./report/\n"),
        );
        assert_report(
            &te,
            mode,
            &["--exact", "a.rs", "--path-separator=|"],
            &format!("{count}\treport|\n"),
        );
        let path = format!(
            "{}/report",
            crate::get_absolute_root_path(&te).replace('\\', "/")
        );
        assert_report(
            &te,
            mode,
            &["--exact", "a.rs", "-a"],
            &format!("{count}\t{path}/\n"),
        );
    }
}

#[test]
fn explicit_metadata_sort_and_count_ties() {
    let te = fixture();
    fs::create_dir(te.test_root().join("report/another-empty")).unwrap();
    assert_report(
        &te,
        MODES[0],
        &["-td", "--exact", "report", "-R", "p"],
        "6\treport/\n",
    );
    assert_report(
        &te,
        MODES[0],
        &["-td", "", "report", "-R", "P"],
        "1\treport/nested/deep/\n2\treport/nested/\n0\treport/empty/\n0\treport/another-empty/\n",
    );
    assert_report(
        &te,
        MODES[0],
        &["empty$"],
        "0\treport/another-empty/\n0\treport/empty/\n",
    );
}

#[test]
fn output_conflicts_and_specs() {
    let te = fixture();
    for mode in ["fext", MODES[0], MODES[1]] {
        for flags in [
            vec!["-x", "echo"],
            vec!["-X", "echo"],
            vec!["-l"],
            vec!["--list-details"],
            vec!["--exec", "echo"],
            vec!["--exec-batch", "echo"],
            vec!["-l", "-x", "echo"],
            vec!["-l", "-X", "echo"],
            vec!["--format", "{}"],
            vec!["--quiet"],
            vec!["--has-results"],
        ] {
            let mut args = vec!["--summary", mode];
            args.extend(flags);
            te.assert_failure(&args);
        }
    }
    te.assert_failure(&["--summary", "count-children:s"]);
    te.assert_failure(&["--summary", "count-descendants:i"]);
    assert_report(
        &te,
        "count-children:",
        &["--exact", "report"],
        "5\treport/\n",
    );
}

#[cfg(unix)]
#[test]
fn links_cycles_and_special_entries() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    let te = fixture();
    let root = te.test_root();
    symlink("nested", root.join("report/alias")).unwrap();
    symlink("missing", root.join("report/dangling")).unwrap();
    symlink("..", root.join("report/nested/back")).unwrap();
    assert_report(&te, MODES[0], &["--exact", "report"], "7\treport/\n");
    assert_report(
        &te,
        MODES[0],
        &["--exact", "report", "-L", "--one-file-system"],
        "7\treport/\n",
    );
    assert_report(&te, MODES[1], &["--exact", "report"], "11\treport/\n");
    assert_report(&te, MODES[1], &["--exact", "report", "-L"], "15\treport/\n");
    assert_report(
        &te,
        MODES[1],
        &["--exact", "report", "-L", "--no-follow"],
        "11\treport/\n",
    );
    assert_report(
        &te,
        MODES[1],
        &["^(nested|alias)$", "-L", "--one-file-system"],
        "11\treport/alias/\n11\treport/nested/\n",
    );
    assert_report(&te, MODES[1], &["--exact", "alias"], "11\treport/\n");
    for mode in MODES {
        assert_report(
            &te,
            mode,
            &["--exact", "dangling", "-L"],
            if mode == MODES[0] {
                "7\treport/\n"
            } else {
                "15\treport/\n"
            },
        );
    }
    fs::hard_link(root.join("report/a.rs"), root.join("report/hard")).unwrap();
    let _socket = UnixListener::bind(root.join("report/socket")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(root.join("report/fifo"))
            .status()
            .unwrap()
            .success()
    );
    assert_report(&te, MODES[0], &["^(hard|socket|fifo)$"], "10\treport/\n");
}

#[cfg(unix)]
#[test]
fn native_filesystem_boundary() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let te = fixture();
    let Ok(device) = fs::metadata("/dev") else {
        return;
    };
    if device.dev() == fs::metadata(te.test_root()).unwrap().dev() {
        return;
    }
    symlink("/dev", te.test_root().join("report/device")).unwrap();
    // Only discovery is depth-limited. Counting must stop at the filesystem boundary.
    assert_report(
        &te,
        MODES[1],
        &[
            "--exact",
            "report",
            "--max-depth",
            "1",
            "-L",
            "--one-file-system",
        ],
        "9\treport/\n",
    );
    assert_report(
        &te,
        MODES[0],
        &[
            "--exact",
            "report",
            "--max-depth",
            "1",
            "-L",
            "--one-file-system",
        ],
        "6\treport/\n",
    );
}

#[test]
fn directory_alias_selection() {
    let te = fixture();
    let alias = te.test_root().join("report/alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink("nested", alias).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir("nested", alias).unwrap();
    for (mode, parent_count, target_count) in [(MODES[0], 6, 2), (MODES[1], 9, 3)] {
        assert_report(
            &te,
            mode,
            &["--exact", "alias"],
            &format!("{parent_count}\treport/\n"),
        );
        assert_report(
            &te,
            mode,
            &["^(alias|nested)$", "-L"],
            &format!("{target_count}\treport/alias/\n{target_count}\treport/nested/\n"),
        );
    }
}

#[cfg(unix)]
#[test]
fn unreadable_counts_keep_partial_rows_and_fail_without_show_errors() {
    use std::os::unix::fs::PermissionsExt;
    if nix::unistd::Uid::effective().is_root() {
        return;
    }
    let te = fixture();
    let blocked = te.test_root().join("report/nested");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    let run = |mode| {
        Command::new(te.test_exe())
            .current_dir(te.test_root())
            .args([
                "--no-global-ignore-file",
                "--no-user-matchsets",
                "--summary",
                mode,
                "--path-separator=/",
                "-td",
                "--max-depth",
                "2",
                "^(report|nested|empty)$",
            ])
            .output()
            .unwrap()
    };
    let children = run(MODES[0]);
    let descendants = run(MODES[1]);
    fs::set_permissions(blocked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(children.status.code(), Some(1));
    let expected = b"0\treport/empty/\n0\treport/nested/\n5\treport/\n";
    assert_eq!(children.stdout, expected);
    assert_eq!(descendants.status.code(), Some(1));
    assert_eq!(descendants.stdout, expected);
    for output in [children, descendants] {
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(errors.contains("Could not fully count"));
        assert!(errors.contains("report/nested"));
    }
}

#[cfg(unix)]
#[test]
fn raw_names_colors_and_hyperlinks() {
    use std::os::unix::ffi::OsStrExt;
    let te = fixture().env("LS_COLORS", "di=34");
    for mode in MODES {
        let output = te.assert_success_and_get_output(
            ".",
            &[
                "--summary",
                mode,
                "--exact",
                "report",
                "--color=always",
                "--hyperlink=always",
            ],
        );
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.starts_with(if mode == MODES[0] {
            "5\t\x1b]8;;file://"
        } else {
            "8\t\x1b]8;;file://"
        }));
        assert!(output.contains("\x1b[34m"));
    }
    let name = std::ffi::OsStr::from_bytes(b"strange\tline\n\xff");
    // macOS may reject invalid UTF-8; control characters still get native coverage there.
    let name = if cfg!(target_os = "macos") {
        std::ffi::OsStr::new("strange\tline\n")
    } else {
        name
    };
    fs::create_dir(te.test_root().join(name)).unwrap();
    let output = te.assert_success_and_get_output(
        ".",
        &[
            "--summary",
            MODES[0],
            "^strange",
            "-0",
            "--strip-cwd-prefix=always",
        ],
    );
    let mut expected = b"0\t".to_vec();
    expected.extend_from_slice(name.as_bytes());
    expected.extend_from_slice(b"/\0");
    assert_eq!(output.stdout, expected);
}
