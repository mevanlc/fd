use std::borrow::Cow;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender, bounded};
use etcetera::BaseStrategy;
use ignore::overrides::{Override, OverrideBuilder};
use ignore::{WalkBuilder, WalkParallel, WalkState};
use regex::bytes::Regex;

use crate::config::Config;
use crate::config::{SortBy, SortConfig, SortTextOptions};
use crate::dir_entry::DirEntry;
use crate::error::print_error;
use crate::exec;
use crate::exit_codes::{ExitCode, merge_exitcodes};
use crate::filesystem;
use crate::output;

/// The receiver thread can either be buffering results or directly streaming to the console.
#[derive(PartialEq)]
enum ReceiverMode {
    /// Receiver is still buffering in order to sort the results, if the search finishes fast
    /// enough.
    Buffering,

    /// Receiver is directly printing results to the output.
    Streaming,
}

/// The Worker threads can result in a valid entry having PathBuf or an error.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum WorkerResult {
    // Errors should be rare, so it's probably better to allow large_enum_variant than
    // to box the Entry variant
    Entry(DirEntry),
    Error(ignore::Error),
}

/// A batch of WorkerResults to send over a channel.
#[derive(Clone)]
struct Batch {
    items: Arc<Mutex<Option<Vec<WorkerResult>>>>,
}

impl Batch {
    fn new() -> Self {
        Self {
            items: Arc::new(Mutex::new(Some(vec![]))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Vec<WorkerResult>>> {
        self.items.lock().unwrap()
    }
}

impl IntoIterator for Batch {
    type Item = WorkerResult;
    type IntoIter = std::vec::IntoIter<WorkerResult>;

    fn into_iter(self) -> Self::IntoIter {
        self.lock().take().unwrap().into_iter()
    }
}

/// Wrapper that sends batches of items at once over a channel.
struct BatchSender {
    batch: Batch,
    tx: Sender<Batch>,
    limit: usize,
}

impl BatchSender {
    fn new(tx: Sender<Batch>, limit: usize) -> Self {
        Self {
            batch: Batch::new(),
            tx,
            limit,
        }
    }

    /// Check if we need to flush a batch.
    fn needs_flush(&self, batch: Option<&Vec<WorkerResult>>) -> bool {
        match batch {
            // Limit the batch size to provide some backpressure
            Some(vec) => vec.len() >= self.limit,
            // Batch was already taken by the receiver, so make a new one
            None => true,
        }
    }

    /// Add an item to a batch.
    fn send(&mut self, item: WorkerResult) -> Result<(), SendError<()>> {
        let mut batch = self.batch.lock();

        if self.needs_flush(batch.as_ref()) {
            drop(batch);
            self.batch = Batch::new();
            batch = self.batch.lock();
        }

        let items = batch.as_mut().unwrap();
        items.push(item);

        if items.len() == 1 {
            // New batch, send it over the channel
            self.tx
                .send(self.batch.clone())
                .map_err(|_| SendError(()))?;
        }

        Ok(())
    }
}

/// Maximum size of the output buffer before flushing results to the console
const MAX_BUFFER_LENGTH: usize = 1000;
/// Default duration until output buffering switches to streaming.
const DEFAULT_MAX_BUFFER_TIME: Duration = Duration::from_millis(100);

/// Wrapper for the receiver thread's buffering behavior.
struct ReceiverBuffer<'a, W> {
    /// The configuration.
    config: &'a Config,
    /// For shutting down the senders.
    quit_flag: &'a AtomicBool,
    /// The ^C notifier.
    interrupt_flag: &'a AtomicBool,
    /// Receiver for worker results.
    rx: Receiver<Batch>,
    /// Standard output.
    stdout: W,
    /// The current buffer mode.
    mode: ReceiverMode,
    /// The deadline to switch to streaming mode.
    deadline: Instant,
    /// The buffer of quickly received paths.
    buffer: Vec<DirEntry>,
    /// Result count.
    num_results: usize,
}

impl<'a, W: Write> ReceiverBuffer<'a, W> {
    /// Create a new receiver buffer.
    fn new(state: &'a WorkerState, rx: Receiver<Batch>, stdout: W) -> Self {
        let config = &state.config;
        let quit_flag = state.quit_flag.as_ref();
        let interrupt_flag = state.interrupt_flag.as_ref();
        let max_buffer_time = config.max_buffer_time.unwrap_or(DEFAULT_MAX_BUFFER_TIME);
        let deadline = Instant::now() + max_buffer_time;

        Self {
            config,
            quit_flag,
            interrupt_flag,
            rx,
            stdout,
            mode: ReceiverMode::Buffering,
            deadline,
            buffer: Vec::with_capacity(MAX_BUFFER_LENGTH),
            num_results: 0,
        }
    }

    /// Process results until finished.
    fn process(&mut self) -> ExitCode {
        loop {
            if let Err(ec) = self.poll() {
                self.quit_flag.store(true, AtomicOrdering::Relaxed);
                return ec;
            }
        }
    }

    /// Receive the next worker result.
    fn recv(&self) -> Result<Batch, RecvTimeoutError> {
        if self.config.sort.is_some() {
            return Ok(self.rx.recv()?);
        }

        match self.mode {
            ReceiverMode::Buffering => {
                // Wait at most until we should switch to streaming
                self.rx.recv_deadline(self.deadline)
            }
            ReceiverMode::Streaming => {
                // Wait however long it takes for a result
                Ok(self.rx.recv()?)
            }
        }
    }

    /// Wait for a result or state change.
    fn poll(&mut self) -> Result<(), ExitCode> {
        match self.recv() {
            Ok(batch) => {
                for result in batch {
                    match result {
                        WorkerResult::Entry(dir_entry) => {
                            if self.config.quiet {
                                return Err(ExitCode::HasResults(true));
                            }

                            match self.mode {
                                ReceiverMode::Buffering => {
                                    self.buffer.push(dir_entry);
                                    if self.config.sort.is_none()
                                        && self.buffer.len() > MAX_BUFFER_LENGTH
                                    {
                                        self.stream()?;
                                    }
                                }
                                ReceiverMode::Streaming => {
                                    self.print(&dir_entry)?;
                                }
                            }

                            self.num_results += 1;
                            if let Some(max_results) = self.config.max_results
                                && self.num_results >= max_results
                            {
                                return self.stop();
                            }
                        }
                        WorkerResult::Error(err) => {
                            if self.config.show_filesystem_errors {
                                print_error(err.to_string());
                            }
                        }
                    }
                }

                // If we don't have another batch ready, flush before waiting
                if self.mode == ReceiverMode::Streaming && self.rx.is_empty() {
                    self.flush()?;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                self.stream()?;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return self.stop();
            }
        }

        Ok(())
    }

    /// Output a path.
    fn print(&mut self, entry: &DirEntry) -> Result<(), ExitCode> {
        if let Err(e) = output::print_entry(&mut self.stdout, entry, self.config)
            && e.kind() != ::std::io::ErrorKind::BrokenPipe
        {
            print_error(format!("Could not write to output: {e}"));
            return Err(ExitCode::GeneralError);
        }

        if self.interrupt_flag.load(AtomicOrdering::Relaxed) {
            // Ignore any errors on flush, because we're about to exit anyway
            let _ = self.flush();
            return Err(ExitCode::KilledBySigint);
        }

        Ok(())
    }

    /// Switch ourselves into streaming mode.
    fn stream(&mut self) -> Result<(), ExitCode> {
        self.mode = ReceiverMode::Streaming;

        let buffer = mem::take(&mut self.buffer);
        for path in buffer {
            self.print(&path)?;
        }

        self.flush()
    }

    /// Stop looping.
    fn stop(&mut self) -> Result<(), ExitCode> {
        if self.mode == ReceiverMode::Buffering {
            self.sort_buffer();
            self.stream()?;
        }

        if self.config.quiet {
            Err(ExitCode::HasResults(self.num_results > 0))
        } else {
            Err(ExitCode::Success)
        }
    }

    /// Flush stdout if necessary.
    fn flush(&mut self) -> Result<(), ExitCode> {
        if self.stdout.flush().is_err() {
            // Probably a broken pipe. Exit gracefully.
            return Err(ExitCode::GeneralError);
        }
        Ok(())
    }

    /// Sort buffered entries according to the configured sort order.
    fn sort_buffer(&mut self) {
        if let Some(sort) = &self.config.sort {
            self.buffer
                .sort_by(|left, right| compare_entries(left, right, sort));
        } else {
            self.buffer.sort();
        }
    }
}

fn compare_entries(left: &DirEntry, right: &DirEntry, sort: &SortConfig) -> Ordering {
    for criterion in &sort.criteria {
        let ord = compare_entries_by(left, right, criterion.by, sort.text);
        let ord = if criterion.descending {
            ord.reverse()
        } else {
            ord
        };

        if ord != Ordering::Equal {
            return ord;
        }
    }

    left.path().cmp(right.path())
}

fn compare_entries_by(
    left: &DirEntry,
    right: &DirEntry,
    by: SortBy,
    text: SortTextOptions,
) -> Ordering {
    match by {
        SortBy::Path => compare_text_values(
            Some(left.path().as_os_str()),
            Some(right.path().as_os_str()),
            text,
        ),
        SortBy::Basename => compare_text_values(
            basename_or_path(left.path()),
            basename_or_path(right.path()),
            text,
        ),
        SortBy::Extension => {
            compare_text_values(left.path().extension(), right.path().extension(), text)
        }
        SortBy::Type => compare_optional(type_rank_for(left), type_rank_for(right)),
        SortBy::Changed => compare_optional(changed_for(left), changed_for(right)),
        SortBy::Modified => compare_optional(modified_for(left), modified_for(right)),
        SortBy::Accessed => compare_optional(accessed_for(left), accessed_for(right)),
        SortBy::Born => compare_optional(born_for(left), born_for(right)),
        SortBy::Inode => compare_optional(inode_for(left), inode_for(right)),
        SortBy::Size => compare_optional(size_for(left), size_for(right)),
    }
}

fn basename_or_path(path: &std::path::Path) -> Option<&OsStr> {
    path.file_name().or(Some(path.as_os_str()))
}

fn entry_context_dir(path: &Path, is_dir: bool) -> &Path {
    if is_dir {
        path
    } else {
        path.parent().unwrap_or_else(|| Path::new("."))
    }
}

fn compare_optional<T: Ord>(left: Option<T>, right: Option<T>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn size_for(entry: &DirEntry) -> Option<u64> {
    entry.metadata().map(|metadata| metadata.len())
}

fn modified_for(entry: &DirEntry) -> Option<SystemTime> {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified().ok())
}

fn accessed_for(entry: &DirEntry) -> Option<SystemTime> {
    entry
        .metadata()
        .and_then(|metadata| metadata.accessed().ok())
}

fn born_for(entry: &DirEntry) -> Option<SystemTime> {
    entry
        .metadata()
        .and_then(|metadata| metadata.created().ok())
}

#[cfg(unix)]
fn changed_for(entry: &DirEntry) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    entry
        .metadata()
        .map(|metadata| (metadata.ctime(), metadata.ctime_nsec()))
}

#[cfg(windows)]
fn changed_for(entry: &DirEntry) -> Option<u64> {
    use std::os::windows::fs::MetadataExt;
    entry.metadata().map(|metadata| metadata.last_write_time())
}

#[cfg(not(any(unix, windows)))]
fn changed_for(_entry: &DirEntry) -> Option<u64> {
    None
}

#[cfg(unix)]
fn inode_for(entry: &DirEntry) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    entry.metadata().map(|metadata| metadata.ino())
}

#[cfg(not(unix))]
fn inode_for(_entry: &DirEntry) -> Option<u64> {
    None
}

fn type_rank_for(entry: &DirEntry) -> Option<u8> {
    let file_type = entry.file_type()?;
    let rank = if file_type.is_symlink() {
        0
    } else if file_type.is_dir() {
        1
    } else if file_type.is_file() {
        2
    } else if filesystem::is_block_device(file_type) {
        3
    } else if filesystem::is_char_device(file_type) {
        4
    } else if filesystem::is_socket(file_type) {
        5
    } else if filesystem::is_pipe(file_type) {
        6
    } else {
        7
    };
    Some(rank)
}

fn compare_text_values(
    left: Option<&OsStr>,
    right: Option<&OsStr>,
    options: SortTextOptions,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => compare_text_bytes(
            filesystem::osstr_to_bytes(left).as_ref(),
            filesystem::osstr_to_bytes(right).as_ref(),
            options,
        ),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn compare_text_bytes(left: &[u8], right: &[u8], options: SortTextOptions) -> Ordering {
    if options.natural {
        compare_natural(left, right, options.case_insensitive)
    } else {
        compare_lexical(left, right, options.case_insensitive)
    }
}

fn compare_lexical(left: &[u8], right: &[u8], case_insensitive: bool) -> Ordering {
    let mut idx = 0;
    while idx < left.len() && idx < right.len() {
        let mut l = left[idx];
        let mut r = right[idx];
        if case_insensitive {
            l = l.to_ascii_lowercase();
            r = r.to_ascii_lowercase();
        }
        match l.cmp(&r) {
            Ordering::Equal => idx += 1,
            non_eq => return non_eq,
        }
    }
    left.len().cmp(&right.len())
}

fn compare_natural(left: &[u8], right: &[u8], case_insensitive: bool) -> Ordering {
    let mut li = 0usize;
    let mut ri = 0usize;

    while li < left.len() && ri < right.len() {
        let ld = left[li].is_ascii_digit();
        let rd = right[ri].is_ascii_digit();

        if ld && rd {
            let l_start = li;
            let r_start = ri;

            while li < left.len() && left[li].is_ascii_digit() {
                li += 1;
            }
            while ri < right.len() && right[ri].is_ascii_digit() {
                ri += 1;
            }

            let l_digits = &left[l_start..li];
            let r_digits = &right[r_start..ri];
            let ord = compare_digit_chunks(l_digits, r_digits);
            if ord != Ordering::Equal {
                return ord;
            }
            continue;
        }

        let mut l = left[li];
        let mut r = right[ri];
        if case_insensitive {
            l = l.to_ascii_lowercase();
            r = r.to_ascii_lowercase();
        }
        match l.cmp(&r) {
            Ordering::Equal => {
                li += 1;
                ri += 1;
            }
            non_eq => return non_eq,
        }
    }

    left.len().cmp(&right.len())
}

fn compare_digit_chunks(left: &[u8], right: &[u8]) -> Ordering {
    let left_trimmed = left.iter().position(|b| *b != b'0').unwrap_or(left.len());
    let right_trimmed = right.iter().position(|b| *b != b'0').unwrap_or(right.len());

    let left_sig = &left[left_trimmed..];
    let right_sig = &right[right_trimmed..];

    let left_sig = if left_sig.is_empty() { b"0" } else { left_sig };
    let right_sig = if right_sig.is_empty() {
        b"0"
    } else {
        right_sig
    };

    match left_sig.len().cmp(&right_sig.len()) {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }

    match left_sig.cmp(right_sig) {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }

    left.len().cmp(&right.len())
}

/// State shared by the sender and receiver threads.
struct WorkerState {
    /// The search patterns.
    patterns: Vec<Regex>,
    /// The command line configuration.
    config: Config,
    /// Flag for cleanly shutting down the parallel walk
    quit_flag: Arc<AtomicBool>,
    /// Flag specifically for quitting due to ^C
    interrupt_flag: Arc<AtomicBool>,
}

impl WorkerState {
    fn new(patterns: Vec<Regex>, config: Config) -> Self {
        let quit_flag = Arc::new(AtomicBool::new(false));
        let interrupt_flag = Arc::new(AtomicBool::new(false));

        Self {
            patterns,
            config,
            quit_flag,
            interrupt_flag,
        }
    }

    fn build_overrides(&self, paths: &[PathBuf]) -> Result<Override> {
        let first_path = &paths[0];
        let config = &self.config;

        let mut builder = OverrideBuilder::new(first_path);

        for pattern in &config.exclude_patterns {
            builder
                .add(pattern)
                .map_err(|e| anyhow!("Malformed exclude pattern: {}", e))?;
        }

        builder
            .build()
            .map_err(|_| anyhow!("Mismatch in exclude patterns"))
    }

    fn build_walker(&self, paths: &[PathBuf]) -> Result<WalkParallel> {
        let first_path = &paths[0];
        let config = &self.config;
        let overrides = self.build_overrides(paths)?;

        let mut builder = WalkBuilder::new(first_path);
        builder
            .hidden(config.ignore_hidden)
            .ignore(config.read_fdignore)
            .parents(config.read_parent_ignore && (config.read_fdignore || config.read_vcsignore))
            .git_ignore(config.read_vcsignore)
            .git_global(config.read_vcsignore)
            .git_exclude(config.read_vcsignore)
            .require_git(config.require_git_to_read_vcsignore)
            .overrides(overrides)
            .follow_links(config.follow_links)
            // No need to check for supported platforms, option is unavailable on unsupported ones
            .same_file_system(config.one_file_system)
            .max_depth(config.max_depth);

        if config.read_fdignore {
            builder.add_custom_ignore_filename(".fdignore");
        }

        if config.read_global_ignore
            && let Ok(basedirs) = etcetera::choose_base_strategy()
        {
            let global_ignore_file = basedirs.config_dir().join("fd").join("ignore");
            if global_ignore_file.is_file() {
                let result = builder.add_ignore(global_ignore_file);
                match result {
                    Some(ignore::Error::Partial(_)) => (),
                    Some(err) => {
                        print_error(format!("Malformed pattern in global ignore file. {err}."));
                    }
                    None => (),
                }
            }
        }

        for ignore_file in &config.ignore_files {
            let result = builder.add_ignore(ignore_file);
            match result {
                Some(ignore::Error::Partial(_)) => (),
                Some(err) => {
                    print_error(format!("Malformed pattern in custom ignore file. {err}."));
                }
                None => (),
            }
        }

        for path in &paths[1..] {
            builder.add(path);
        }

        let walker = builder.threads(config.threads).build_parallel();
        Ok(walker)
    }

    /// Run the receiver work, either on this thread or a pool of background
    /// threads (for --exec).
    fn receive(&self, rx: Receiver<Batch>) -> ExitCode {
        let config = &self.config;

        // This will be set to `Some` if the `--exec` argument was supplied.
        if let Some(ref cmd) = config.command {
            if cmd.in_batch_mode() {
                exec::batch(rx.into_iter().flatten(), cmd, config)
            } else {
                thread::scope(|scope| {
                    // Each spawned job will store its thread handle in here.
                    let threads = config.threads;
                    let mut handles = Vec::with_capacity(threads);
                    for _ in 0..threads {
                        let rx = rx.clone();

                        // Spawn a job thread that will listen for and execute inputs.
                        let handle =
                            scope.spawn(|| exec::job(rx.into_iter().flatten(), cmd, config));

                        // Push the handle of the spawned thread into the vector for later joining.
                        handles.push(handle);
                    }
                    let exit_codes = handles.into_iter().map(|handle| handle.join().unwrap());
                    merge_exitcodes(exit_codes)
                })
            }
        } else {
            let stdout = io::stdout().lock();
            let stdout = io::BufWriter::new(stdout);

            ReceiverBuffer::new(self, rx, stdout).process()
        }
    }

    /// Spawn the sender threads.
    fn spawn_senders(&self, walker: WalkParallel, tx: Sender<Batch>) {
        walker.run(|| {
            let patterns = &self.patterns;
            let config = &self.config;
            let quit_flag = self.quit_flag.as_ref();

            let mut limit = 0x100;
            if let Some(cmd) = &config.command
                && !cmd.in_batch_mode()
                && config.threads > 1
            {
                // Evenly distribute work between multiple receivers
                limit = 1;
            }
            let mut tx = BatchSender::new(tx.clone(), limit);

            Box::new(move |entry| {
                if quit_flag.load(AtomicOrdering::Relaxed) {
                    return WalkState::Quit;
                }

                if let Ok(e) = &entry {
                    let entry_path = e.path();
                    if entry_path.is_dir()
                        && config
                            .ignore_contain
                            .iter()
                            .any(|ic| entry_path.join(ic).exists())
                    {
                        return WalkState::Skip;
                    }
                    if e.depth() == 0 {
                        // Skip the root directory entry.
                        return WalkState::Continue;
                    }
                }
                let entry = match entry {
                    Ok(e) => DirEntry::normal(e),
                    Err(ignore::Error::WithPath {
                        path,
                        err: inner_err,
                    }) => match inner_err.as_ref() {
                        ignore::Error::Io(io_error)
                            if io_error.kind() == io::ErrorKind::NotFound
                                && path
                                    .symlink_metadata()
                                    .ok()
                                    .is_some_and(|m| m.file_type().is_symlink()) =>
                        {
                            DirEntry::broken_symlink(path)
                        }
                        _ => {
                            return match tx.send(WorkerResult::Error(ignore::Error::WithPath {
                                path,
                                err: inner_err,
                            })) {
                                Ok(_) => WalkState::Continue,
                                Err(_) => WalkState::Quit,
                            };
                        }
                    },
                    Err(err) => {
                        return match tx.send(WorkerResult::Error(err)) {
                            Ok(_) => WalkState::Continue,
                            Err(_) => WalkState::Quit,
                        };
                    }
                };

                // Check the name first, since it doesn't require metadata
                let entry_path = entry.path();
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                let context_dir = entry_context_dir(entry_path, is_dir);

                if let Some(condition) = &config.exclude_if {
                    match condition.evaluate(entry_path, context_dir, config) {
                        Ok(true) if is_dir => return WalkState::Skip,
                        Ok(true) => return WalkState::Continue,
                        Ok(false) => {}
                        Err(err) => {
                            print_error(format!("{err:#}"));
                            return WalkState::Quit;
                        }
                    }
                }

                let prune_if_matched = if is_dir {
                    if let Some(expr) = &config.prune_if {
                        match expr.evaluate(entry_path, context_dir, config) {
                            Ok(result) => result,
                            Err(err) => {
                                print_error(format!("{err:#}"));
                                return WalkState::Quit;
                            }
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                if let Some(min_depth) = config.min_depth
                    && entry.depth().is_none_or(|d| d < min_depth)
                {
                    return WalkState::Continue;
                }

                if !config.include_match_sets.is_empty() {
                    let mut matched = false;
                    for set in &config.include_match_sets {
                        match set.matches(&entry, context_dir, config) {
                            Ok(true) => {
                                matched = true;
                                break;
                            }
                            Ok(false) => {}
                            Err(err) => {
                                print_error(format!("{err:#}"));
                                return WalkState::Quit;
                            }
                        }
                    }
                    if !matched {
                        return WalkState::Continue;
                    }
                }

                for set in &config.exclude_match_sets {
                    match set.matches(&entry, context_dir, config) {
                        Ok(true) if is_dir => return WalkState::Skip,
                        Ok(true) => return WalkState::Continue,
                        Ok(false) => {}
                        Err(err) => {
                            print_error(format!("{err:#}"));
                            return WalkState::Quit;
                        }
                    }
                }

                let search_str: Cow<OsStr> = if config.search_full_path {
                    let path_abs_buf = filesystem::path_absolute_form(entry_path)
                        .expect("Retrieving absolute path succeeds");
                    Cow::Owned(path_abs_buf.as_os_str().to_os_string())
                } else {
                    match entry_path.file_name() {
                        Some(filename) => Cow::Borrowed(filename),
                        None => unreachable!(
                            "Encountered file system entry without a file name. This should only \
                             happen for paths like 'foo/bar/..' or '/' which are not supposed to \
                             appear in a file system traversal."
                        ),
                    }
                };

                if !patterns
                    .iter()
                    .all(|pat| pat.is_match(&filesystem::osstr_to_bytes(search_str.as_ref())))
                {
                    return WalkState::Continue;
                }

                for condition in &config.bash_patterns {
                    match condition.evaluate(entry_path, context_dir, config) {
                        Ok(true) => {}
                        Ok(false) => return WalkState::Continue,
                        Err(err) => {
                            print_error(format!("{err:#}"));
                            return WalkState::Quit;
                        }
                    }
                }

                // Filter out unwanted extensions.
                if let Some(ref exts_regex) = config.extensions {
                    if let Some(path_str) = entry_path.file_name() {
                        if !exts_regex.is_match(&filesystem::osstr_to_bytes(path_str)) {
                            return WalkState::Continue;
                        }
                    } else {
                        return WalkState::Continue;
                    }
                }

                // Filter out unwanted file types.
                if let Some(ref file_types) = config.file_types
                    && file_types.should_ignore(&entry)
                {
                    return WalkState::Continue;
                }

                #[cfg(unix)]
                {
                    if let Some(ref owner_constraint) = config.owner_constraint {
                        if let Some(metadata) = entry.metadata() {
                            if !owner_constraint.matches(metadata) {
                                return WalkState::Continue;
                            }
                        } else {
                            return WalkState::Continue;
                        }
                    }
                }

                // Filter out unwanted sizes if it is a file and we have been given size constraints.
                if !config.size_constraints.is_empty() {
                    if entry_path.is_file() {
                        if let Some(metadata) = entry.metadata() {
                            let file_size = metadata.len();
                            if config
                                .size_constraints
                                .iter()
                                .any(|sc| !sc.is_within(file_size))
                            {
                                return WalkState::Continue;
                            }
                        } else {
                            return WalkState::Continue;
                        }
                    } else {
                        return WalkState::Continue;
                    }
                }

                // Filter out unwanted modification times
                if !config.time_constraints.is_empty() {
                    let mut matched = false;
                    if let Some(metadata) = entry.metadata()
                        && let Ok(modified) = metadata.modified()
                    {
                        matched = config
                            .time_constraints
                            .iter()
                            .all(|tf| tf.applies_to(&modified));
                    }
                    if !matched {
                        return WalkState::Continue;
                    }
                }

                if config.is_printing()
                    && let Some(ls_colors) = &config.ls_colors
                {
                    // Compute colors in parallel
                    entry.style(ls_colors);
                }

                let send_result = tx.send(WorkerResult::Entry(entry));

                if send_result.is_err() {
                    return WalkState::Quit;
                }

                // Apply pruning.
                if config.prune || prune_if_matched {
                    return WalkState::Skip;
                }

                WalkState::Continue
            })
        });
    }

    /// Perform the recursive scan.
    fn scan(&self, paths: &[PathBuf]) -> Result<ExitCode> {
        let config = &self.config;
        let walker = self.build_walker(paths)?;

        if config.ls_colors.is_some() && config.is_printing() {
            let quit_flag = Arc::clone(&self.quit_flag);
            let interrupt_flag = Arc::clone(&self.interrupt_flag);

            ctrlc::set_handler(move || {
                quit_flag.store(true, AtomicOrdering::Relaxed);

                if interrupt_flag.fetch_or(true, AtomicOrdering::Relaxed) {
                    // Ctrl-C has been pressed twice, exit NOW
                    ExitCode::KilledBySigint.exit();
                }
            })
            .unwrap();
        }

        let (tx, rx) = bounded(2 * config.threads);

        let exit_code = thread::scope(|scope| {
            // Spawn the receiver thread(s)
            let receiver = scope.spawn(|| self.receive(rx));

            // Spawn the sender threads.
            self.spawn_senders(walker, tx);

            receiver.join().unwrap()
        });

        if self.interrupt_flag.load(AtomicOrdering::Relaxed) {
            Ok(ExitCode::KilledBySigint)
        } else {
            Ok(exit_code)
        }
    }
}

/// Recursively scan the given search path for files / pathnames matching the patterns.
///
/// If the `--exec` argument was supplied, this will create a thread pool for executing
/// jobs in parallel from a given command line and the discovered paths. Otherwise, each
/// path will simply be written to standard output.
pub fn scan(paths: &[PathBuf], patterns: Vec<Regex>, config: Config) -> Result<ExitCode> {
    WorkerState::new(patterns, config).scan(paths)
}
