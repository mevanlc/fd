//! Process-level batch tests. Gates prove overlap without relying on runtime speed.
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const WAIT_FILE: &str = r#"
wait_file() {
    attempts=0
    until test -f "$1"; do
        attempts=$((attempts + 1))
        test "$attempts" -le 1000 || exit 99
        sleep 0.01
    done
}
"#;

fn wait_for<T>(description: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct RunningFd {
    child: Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
}

impl RunningFd {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        fn capture(
            mut pipe: impl Read + Send + 'static,
            bytes: Arc<Mutex<Vec<u8>>>,
        ) -> JoinHandle<()> {
            thread::spawn(move || {
                let mut buffer = [0; 4096];
                loop {
                    let count = pipe.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.lock().unwrap().extend_from_slice(&buffer[..count]);
                }
            })
        }
        let readers = vec![
            capture(child.stdout.take().unwrap(), Arc::clone(&stdout)),
            capture(child.stderr.take().unwrap(), Arc::clone(&stderr)),
        ];
        Self {
            child,
            stdout,
            stderr,
            readers,
        }
    }

    fn finish(mut self) -> Output {
        let status = wait_for("fd to exit", || self.child.try_wait().unwrap());
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
        Output {
            status,
            stdout: self.stdout.lock().unwrap().clone(),
            stderr: self.stderr.lock().unwrap().clone(),
        }
    }
}

impl Drop for RunningFd {
    fn drop(&mut self) {
        // Also clean up when a test assertion or deadline fails.
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

fn fixture(count: usize) -> TempDir {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join("input")).unwrap();
    for i in 0..count {
        fs::write(root.path().join(format!("input/{i}.txt")), "").unwrap();
    }
    root
}

fn fd(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fd"));
    command.current_dir(root).stdin(Stdio::null()).args([
        "--no-global-ignore-file",
        "--no-user-matchsets",
        "-j1",
        "-tf",
        "-e",
        "txt",
        ".",
        "input",
    ]);
    command
}

fn ready_jobs(root: &Path) -> Vec<String> {
    let mut jobs: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter_map(|name| {
            name.to_str()
                .unwrap()
                .strip_prefix("ready.")
                .map(str::to_owned)
        })
        .collect();
    jobs.sort();
    jobs
}

#[test]
fn parallel_batches_share_limit_and_group_output() {
    let root = fixture(5);
    let script = format!(
        r#"{WAIT_FILE}
job=$1
template=$2
shift 2
slot=
for candidate in 1 2; do
    if mkdir "slot.$candidate" 2>/dev/null; then
        slot=$candidate
        break
    fi
done
if test -z "$slot"; then touch overflow; exit 98; fi
printf '%s:start\n' "$job"
printf '%s:start\n' "$job" >&2
printf '%s\n' "$@" > "args.$job"
printf '%s\n' "$template" > "tag.$job"
mv "tag.$job" "ready.$job"
wait_file release
printf '%s:end\n' "$job"
printf '%s:end\n' "$job" >&2
rmdir "slot.$slot"
"#
    );
    let mut command = fd(root.path());
    command.args(["-0", "--batch-size", "2", "--batch-threads", "2"]);
    for template in ["first", "second"] {
        command.args([
            "-X", "sh", "-c", &script, "--", "{#}", template, "{}", "end{#}", ";",
        ]);
    }
    let mut running = RunningFd::spawn(&mut command);
    let first_jobs = wait_for("two concurrent batch processes", || {
        let jobs = ready_jobs(root.path());
        (jobs.len() >= 2).then_some(jobs)
    });
    assert_eq!(first_jobs.len(), 2);
    let templates: BTreeSet<_> = first_jobs
        .iter()
        .map(|job| fs::read_to_string(root.path().join(format!("ready.{job}"))).unwrap())
        .collect();
    assert_eq!(
        templates,
        BTreeSet::from(["first\n".into(), "second\n".into()])
    );
    assert!(running.child.try_wait().unwrap().is_none());
    assert!(running.stdout.lock().unwrap().is_empty());
    assert!(running.stderr.lock().unwrap().is_empty());
    fs::write(root.path().join("release"), "").unwrap();
    let output = running.finish();
    assert!(output.status.success(), "{output:?}");
    assert!(!root.path().join("overflow").exists());
    let jobs = ready_jobs(root.path());
    assert_eq!(jobs.len(), 6);
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8(bytes.clone()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 12);
        let mut emitted = BTreeSet::new();
        for pair in lines.as_chunks::<2>().0 {
            let job = pair[0].strip_suffix(":start").unwrap();
            assert_eq!(pair[1], format!("{job}:end"));
            assert!(emitted.insert(job.to_owned()));
        }
        assert_eq!(emitted, jobs.iter().cloned().collect());
    }
    for template in ["first", "second"] {
        let mut paths = Vec::new();
        let mut sizes = Vec::new();
        for job in &jobs {
            if fs::read_to_string(root.path().join(format!("ready.{job}")))
                .unwrap()
                .trim()
                != template
            {
                continue;
            }
            let args = fs::read_to_string(root.path().join(format!("args.{job}"))).unwrap();
            let mut args: Vec<_> = args.lines().map(str::to_owned).collect();
            assert_eq!(args.pop().unwrap(), format!("end{job}"));
            sizes.push(args.len());
            paths.extend(args);
        }
        sizes.sort();
        paths.sort();
        assert_eq!(sizes, [1, 2, 2]);
        assert_eq!(
            paths,
            (0..5).map(|i| format!("input/{i}.txt")).collect::<Vec<_>>()
        );
    }
}

#[test]
fn batch_threads_do_not_split_batches_or_spawn_without_matches() {
    for count in [0, 5] {
        let root = fixture(count);
        let output = RunningFd::spawn(fd(root.path()).args([
            "--batch-threads",
            "4",
            "-X",
            "sh",
            "-c",
            "printf 'paths:%s\\n' \"$#\"",
            "--",
        ]))
        .finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            if count == 0 { "" } else { "paths:5\n" }
        );
    }
}

#[test]
fn failed_batch_status_does_not_skip_remaining_batches() {
    for threads in ["1", "3"] {
        let root = fixture(5);
        let output = RunningFd::spawn(fd(root.path()).args([
            "--batch-threads",
            threads,
            "--batch-size",
            "2",
            "-X",
            "sh",
            "-c",
            "printf '%s\\n' \"$@\"; exit 7",
            "--",
        ]))
        .finish();
        assert!(!output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let mut paths: Vec<_> = text.lines().collect();
        paths.sort();
        assert_eq!(
            paths,
            (0..5).map(|i| format!("input/{i}.txt")).collect::<Vec<_>>()
        );
    }
}

#[test]
fn serial_batches_keep_live_output_and_stdin_parallel_batches_get_eof() {
    for threads in [None, Some("1"), Some("2")] {
        let root = fixture(1);
        let mut command = fd(root.path());
        command.stdin(Stdio::piped());
        if let Some(threads) = threads {
            command.args(["--batch-threads", threads]);
        }
        command.args(["-X", "sh", "-c", r#"printf 'ready\n'; if read -r value; then printf '%s\n' "$value"; else printf 'eof\n'; fi"#]);
        let mut running = RunningFd::spawn(&mut command);
        if threads != Some("2") {
            wait_for("unbuffered serial output", || {
                running
                    .stdout
                    .lock()
                    .unwrap()
                    .starts_with(b"ready\n")
                    .then_some(())
            });
            assert!(running.child.try_wait().unwrap().is_none());
            running
                .child
                .stdin
                .take()
                .unwrap()
                .write_all(b"inherited\n")
                .unwrap();
        }
        // Leave fd's input pipe open in parallel mode: the child must still see EOF.
        let output = running.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            output.stdout,
            if threads == Some("2") {
                b"ready\neof\n".as_slice()
            } else {
                b"ready\ninherited\n".as_slice()
            }
        );
    }
}

#[test]
fn launch_failure_discards_queue_and_waits_for_running_batch() {
    let root = fixture(1);
    let blocker = format!("{WAIT_FILE}\ntouch ready.1\nwait_file release\ntouch finished\n");
    let failing_launcher =
        format!("{WAIT_FILE}\nwait_file ready.1\nexec ./missing-batch-command\n");
    // The first two jobs establish that another process is running when a launch fails.
    // A job-number executable lets the third job fail in fd itself, after a gate.
    fs::write(
        root.path().join("launcher1"),
        format!("#!/bin/sh\n{blocker}"),
    )
    .unwrap();
    fs::write(
        root.path().join("launcher2"),
        format!("#!/bin/sh\n{failing_launcher}"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    for name in ["launcher1", "launcher2"] {
        fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut command = fd(root.path());
    command.args([
        "--batch-threads",
        "2",
        "-X",
        "./launcher{#}",
        ";",
        "-X",
        "./launcher{#}",
        ";",
        "-X",
        "./missing-batch-command",
        ";",
    ]);
    for _ in 0..5 {
        command.args(["-X", "sh", "-c", "touch should-not-run", ";"]);
    }
    let mut running = RunningFd::spawn(&mut command);
    wait_for("fd's launch error", || {
        String::from_utf8_lossy(&running.stderr.lock().unwrap())
            .contains("[fd error]: Command not found:")
            .then_some(())
    });
    assert!(root.path().join("ready.1").exists());
    assert!(running.child.try_wait().unwrap().is_none());
    fs::write(root.path().join("release"), "").unwrap();
    let output = running.finish();
    assert!(!output.status.success());
    assert!(root.path().join("finished").exists());
    assert!(!root.path().join("should-not-run").exists());
}
