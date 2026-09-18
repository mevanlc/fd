//! Unfiltered counting. Report paths are deliberately separate from filesystem identities.
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub(super) enum CountError {
    Filesystem { path: PathBuf, message: String },
    Interrupted,
}

impl CountError {
    pub(super) fn filesystem(path: &Path, error: impl fmt::Display) -> Self {
        Self::Filesystem {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for CountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filesystem { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Interrupted => f.write_str("interrupted"),
        }
    }
}

type Result<T> = std::result::Result<T, CountError>;

fn check_interrupt(interrupt: &AtomicBool) -> Result<()> {
    if interrupt.load(Ordering::Relaxed) {
        Err(CountError::Interrupted)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = path.metadata()?;
        if !metadata.is_dir() {
            return Err(io::Error::other("no longer a directory"));
        }
        Self::from_metadata(path, &metadata)
    }

    #[cfg(unix)]
    fn from_metadata(_path: &Path, metadata: &fs::Metadata) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    #[cfg(windows)]
    fn from_metadata(path: &Path, _metadata: &fs::Metadata) -> io::Result<Self> {
        use winapi_util::{Handle, file};
        let information = file::information(Handle::from_path_any(path)?)?;
        Ok(Self {
            device: information.volume_serial_number(),
            inode: information.file_index(),
        })
    }
}

struct Child {
    name: OsString,
    identity: Identity,
}

#[derive(Default)]
struct Listing {
    count: u64,
    directories: Vec<Child>,
    errors: Vec<CountError>,
}

#[derive(Debug, Default)]
pub(super) struct Count {
    pub total: u64,
    pub errors: Vec<CountError>,
}

trait Reader {
    fn identity(&self, path: &Path) -> Result<Identity>;
    fn read(&self, path: &Path, interrupt: &AtomicBool) -> Result<Listing>;
}

struct FsReader {
    follow: bool,
    recursive: bool,
}

impl Reader for FsReader {
    fn identity(&self, path: &Path) -> Result<Identity> {
        Identity::read(path).map_err(|error| CountError::filesystem(path, error))
    }

    fn read(&self, path: &Path, interrupt: &AtomicBool) -> Result<Listing> {
        match fs::read_dir(path) {
            Ok(entries) => self.read_entries(path, entries, interrupt),
            Err(error) => {
                check_interrupt(interrupt)?;
                Ok(Listing {
                    errors: vec![CountError::filesystem(path, error)],
                    ..Listing::default()
                })
            }
        }
    }
}

impl FsReader {
    fn read_entries(
        &self,
        path: &Path,
        entries: impl IntoIterator<Item = io::Result<fs::DirEntry>>,
        interrupt: &AtomicBool,
    ) -> Result<Listing> {
        let mut listing = Listing::default();
        for entry in entries {
            check_interrupt(interrupt)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    listing.errors.push(CountError::filesystem(path, error));
                    continue;
                }
            };
            // An enumerated entry counts even if it disappears before we can inspect it.
            listing.count = add(listing.count, 1, path)?;
            if !self.recursive {
                continue;
            }
            match self.child_identity(&entry) {
                Ok(Some(identity)) => {
                    listing.directories.push(Child {
                        name: entry.file_name(),
                        identity,
                    });
                }
                Ok(None) => {}
                Err(error) => listing
                    .errors
                    .push(CountError::filesystem(&entry.path(), error)),
            }
        }
        check_interrupt(interrupt)?;
        Ok(listing)
    }

    fn child_identity(&self, entry: &fs::DirEntry) -> io::Result<Option<Identity>> {
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            Identity::read(&path).map(Some)
        } else if file_type.is_symlink() && self.follow {
            match path.metadata() {
                Ok(metadata) if metadata.is_dir() => {
                    Identity::from_metadata(&path, &metadata).map(Some)
                }
                Ok(_) => Ok(None),
                // Dangling links and unresolvable link loops still count as entries.
                Err(error) if leaf_symlink_error(&error) => Ok(None),
                Err(error) => Err(error),
            }
        } else {
            Ok(None)
        }
    }
}

fn leaf_symlink_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) {
        return true;
    }
    false
}

pub(super) struct Counter<'a> {
    engine: Engine<'a, FsReader>,
}

impl<'a> Counter<'a> {
    pub(super) fn new(
        follow: bool,
        recursive: bool,
        one_file_system: bool,
        interrupt: &'a AtomicBool,
    ) -> Self {
        Self {
            engine: Engine::new(
                FsReader { follow, recursive },
                recursive,
                one_file_system,
                interrupt,
            ),
        }
    }

    pub(super) fn count(&mut self, path: &Path) -> Result<Count> {
        self.engine.count(path)
    }
}

struct Engine<'a, R> {
    reader: R,
    recursive: bool,
    one_file_system: bool,
    interrupt: &'a AtomicBool,
    listings: HashMap<Identity, Arc<Listing>>,
    totals: HashMap<(Identity, Option<u64>), u64>,
}

struct Frame {
    identity: Identity,
    path: PathBuf,
    listing: Arc<Listing>,
    next: usize,
    total: u64,
    cacheable: bool,
}

impl<R: Reader> Engine<'_, R> {
    fn new(
        reader: R,
        recursive: bool,
        one_file_system: bool,
        interrupt: &AtomicBool,
    ) -> Engine<'_, R> {
        Engine {
            reader,
            recursive,
            one_file_system,
            interrupt,
            listings: HashMap::new(),
            totals: HashMap::new(),
        }
    }

    fn frame(
        &mut self,
        path: PathBuf,
        identity: Identity,
        errors: &mut Vec<CountError>,
    ) -> Result<Frame> {
        let listing = match self.listings.get(&identity) {
            Some(listing) => Arc::clone(listing),
            None => {
                let listing = Arc::new(self.reader.read(&path, self.interrupt)?);
                // Failed scans can depend on the path used, or recover on a later visit.
                if listing.errors.is_empty() {
                    self.listings.insert(identity, Arc::clone(&listing));
                }
                listing
            }
        };
        errors.extend(listing.errors.iter().cloned());
        Ok(Frame {
            identity,
            path,
            total: listing.count,
            cacheable: listing.errors.is_empty(),
            listing,
            next: 0,
        })
    }

    fn count(&mut self, path: &Path) -> Result<Count> {
        check_interrupt(self.interrupt)?;
        let identity = match self.reader.identity(path) {
            Ok(identity) => identity,
            Err(error @ CountError::Filesystem { .. }) => {
                return Ok(Count {
                    errors: vec![error],
                    ..Count::default()
                });
            }
            Err(error) => return Err(error),
        };
        let mut count = Count::default();
        let boundary = self.one_file_system.then_some(identity.device);
        if let Some(&total) = self.totals.get(&(identity, boundary)) {
            count.total = total;
            return Ok(count);
        }
        let root = self.frame(path.to_path_buf(), identity, &mut count.errors)?;
        check_interrupt(self.interrupt)?;
        if !self.recursive {
            count.total = root.total;
            return Ok(count);
        }
        let mut ancestors = HashSet::from([identity]);
        let mut stack = vec![root];
        loop {
            check_interrupt(self.interrupt)?;
            let current = stack.last_mut().expect("a root frame is always present");
            if let Some(child) = current.listing.directories.get(current.next) {
                current.next += 1;
                if boundary.is_some_and(|device| child.identity.device != device) {
                    continue;
                }
                if ancestors.contains(&child.identity) {
                    // The link entry is already in listing.count. Its target contributes nothing.
                    current.cacheable = false;
                    continue;
                }
                if let Some(&total) = self.totals.get(&(child.identity, boundary)) {
                    current.total = add(current.total, total, &current.path)?;
                    continue;
                }
                let frame = self.frame(
                    current.path.join(&child.name),
                    child.identity,
                    &mut count.errors,
                )?;
                ancestors.insert(child.identity);
                stack.push(frame);
            } else {
                let complete = stack.pop().unwrap();
                ancestors.remove(&complete.identity);
                // Ancestor cutoffs and read errors make totals depend on the route used.
                // Never cache a partial total as a successful count for another report.
                if complete.cacheable {
                    self.totals
                        .insert((complete.identity, boundary), complete.total);
                }
                if let Some(parent) = stack.last_mut() {
                    parent.total = add(parent.total, complete.total, &parent.path)?;
                    parent.cacheable &= complete.cacheable;
                } else {
                    count.total = complete.total;
                    return Ok(count);
                }
            }
        }
    }
}

fn add(left: u64, right: u64, path: &Path) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| CountError::filesystem(path, "entry count exceeds u64"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Node {
        device: u64,
        count: u64,
        children: Vec<u64>,
    }

    struct FakeReader {
        nodes: HashMap<u64, Node>,
        failures: HashSet<u64>,
        reads: RefCell<HashMap<u64, usize>>,
        interrupt_on_read: bool,
    }

    impl FakeReader {
        fn new(nodes: &[(u64, u64, u64, &[u64])]) -> Self {
            Self {
                nodes: nodes
                    .iter()
                    .map(|&(id, device, count, children)| {
                        (
                            id,
                            Node {
                                device,
                                count,
                                children: children.to_vec(),
                            },
                        )
                    })
                    .collect(),
                failures: HashSet::new(),
                reads: RefCell::new(HashMap::new()),
                interrupt_on_read: false,
            }
        }

        fn id(&self, inode: u64) -> Identity {
            Identity {
                inode,
                device: self.nodes[&inode].device,
            }
        }
    }

    impl Reader for FakeReader {
        fn identity(&self, path: &Path) -> Result<Identity> {
            let inode = path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .split('-')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            Ok(self.id(inode))
        }

        fn read(&self, path: &Path, interrupt: &AtomicBool) -> Result<Listing> {
            let id = self.identity(path)?.inode;
            *self.reads.borrow_mut().entry(id).or_default() += 1;
            if self.failures.contains(&id) {
                return Ok(Listing {
                    errors: vec![CountError::filesystem(path, "injected read failure")],
                    ..Listing::default()
                });
            }
            if self.interrupt_on_read {
                interrupt.store(true, Ordering::Relaxed);
            }
            let node = &self.nodes[&id];
            Ok(Listing {
                count: node.count,
                directories: node
                    .children
                    .iter()
                    .enumerate()
                    .map(|(index, &child)| Child {
                        name: format!("{child}-{index}").into(),
                        identity: self.id(child),
                    })
                    .collect(),
                errors: Vec::new(),
            })
        }
    }

    #[test]
    fn reuses_scans_and_totals_without_deduplicating_entry_contributions() {
        let reader = FakeReader::new(&[(1, 1, 3, &[2, 2, 3]), (2, 1, 4, &[]), (3, 1, 0, &[])]);
        let interrupt = AtomicBool::new(false);
        let mut engine = Engine::new(reader, true, false, &interrupt);
        assert_eq!(engine.count(Path::new("1")).unwrap().total, 11);
        assert_eq!(engine.count(Path::new("2-alias")).unwrap().total, 4);
        assert_eq!(engine.count(Path::new("1")).unwrap().total, 11);
        assert_eq!(
            *engine.reader.reads.borrow(),
            HashMap::from([(1, 1), (2, 1), (3, 1)])
        );
        assert_eq!(engine.totals.len(), 3);
    }

    #[test]
    fn cyclic_totals_are_independent_of_report_order() {
        for order in [[("1", 7), ("2", 5)], [("2", 5), ("1", 7)]] {
            let reader = FakeReader::new(&[(1, 1, 3, &[2, 2]), (2, 1, 2, &[1])]);
            let interrupt = AtomicBool::new(false);
            let mut engine = Engine::new(reader, true, false, &interrupt);
            for (path, total) in order {
                assert_eq!(engine.count(Path::new(path)).unwrap().total, total);
            }
            assert_eq!(
                *engine.reader.reads.borrow(),
                HashMap::from([(1, 1), (2, 1)])
            );
            assert!(engine.totals.is_empty());
        }
    }

    #[test]
    fn filesystem_boundary_counts_entry_and_resets_for_each_report_root() {
        let interrupt = AtomicBool::new(false);
        for recursive in [false, true] {
            for limited in [false, true] {
                let reader = FakeReader::new(&[(1, 1, 2, &[2]), (2, 2, 5, &[3]), (3, 1, 7, &[])]);
                let mut engine = Engine::new(reader, recursive, limited, &interrupt);
                assert_eq!(
                    engine.count(Path::new("1")).unwrap().total,
                    if recursive && !limited { 14 } else { 2 }
                );
                if limited {
                    assert!(!engine.reader.reads.borrow().contains_key(&2));
                }
                assert_eq!(
                    engine.count(Path::new("2")).unwrap().total,
                    if recursive && !limited { 12 } else { 5 }
                );
            }
        }
    }

    #[test]
    fn read_failures_preserve_siblings_and_do_not_cache_partial_totals() {
        let reader = FakeReader::new(&[
            (1, 1, 3, &[2, 3, 4]),
            (2, 1, 1, &[]),
            (3, 1, 8, &[]),
            (4, 1, 2, &[]),
        ]);
        let interrupt = AtomicBool::new(false);
        let mut engine = Engine::new(reader, true, false, &interrupt);
        engine.reader.failures.extend([2, 4]);
        for path in ["1", "1-alias"] {
            let count = engine.count(Path::new(path)).unwrap();
            assert_eq!(count.total, 11);
            assert_eq!(count.errors.len(), 2);
            assert!(
                count
                    .errors
                    .iter()
                    .all(|error| error.to_string().starts_with(path))
            );
        }
        let count = engine.count(Path::new("2")).unwrap();
        assert_eq!(count.total, 0);
        assert_eq!(count.errors.len(), 1);
        let count = engine.count(Path::new("3")).unwrap();
        assert_eq!(count.total, 8);
        assert!(count.errors.is_empty());
        assert!(!engine.totals.contains_key(&(engine.reader.id(1), None)));

        engine.reader.failures.clear();
        let count = engine.count(Path::new("1")).unwrap();
        assert_eq!(count.total, 14);
        assert!(count.errors.is_empty());
        assert_eq!(engine.reader.reads.borrow()[&1], 1);
        assert_eq!(engine.reader.reads.borrow()[&3], 1);
    }

    #[test]
    fn missing_report_directory_returns_zero_with_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("removed");
        fs::create_dir(&path).unwrap();
        fs::remove_dir(&path).unwrap();
        let interrupt = AtomicBool::new(false);
        for recursive in [false, true] {
            let mut counter = Counter::new(false, recursive, false, &interrupt);
            let count = counter.count(&path).unwrap();
            assert_eq!(count.total, 0);
            assert_eq!(count.errors.len(), 1);
            assert!(
                matches!(&count.errors[0], CountError::Filesystem { path: failed, .. } if failed == &path)
            );
        }
    }

    #[test]
    fn entry_read_errors_preserve_entries_before_and_after_them() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("first"), "").unwrap();
        fs::write(temp.path().join("last"), "").unwrap();
        let interrupt = AtomicBool::new(false);
        for recursive in [false, true] {
            let mut entries = fs::read_dir(temp.path()).unwrap();
            let entries = [
                entries.next().unwrap(),
                Err(io::ErrorKind::NotFound.into()),
                entries.next().unwrap(),
                Err(io::ErrorKind::PermissionDenied.into()),
            ];
            let listing = FsReader {
                follow: false,
                recursive,
            }
            .read_entries(temp.path(), entries, &interrupt)
            .unwrap();
            assert_eq!(listing.count, 2);
            assert_eq!(listing.errors.len(), 2);
        }
    }

    #[test]
    fn removed_child_still_counts_and_does_not_hide_readable_siblings() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["removed", "readable"] {
            fs::create_dir(temp.path().join(name)).unwrap();
        }
        let mut entries: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.file_name()));
        let removed = temp.path().join("removed");
        fs::remove_dir(&removed).unwrap();
        let interrupt = AtomicBool::new(false);
        let listing = FsReader {
            follow: false,
            recursive: true,
        }
        .read_entries(temp.path(), entries.into_iter().map(Ok), &interrupt)
        .unwrap();
        assert_eq!(listing.count, 2);
        assert_eq!(listing.directories.len(), 1);
        assert_eq!(listing.directories[0].name, "readable");
        assert_eq!(listing.errors.len(), 1);
        assert!(
            matches!(&listing.errors[0], CountError::Filesystem { path, .. } if path == &removed)
        );
    }

    #[test]
    fn overflow_is_incomplete_and_interruption_stops_counting() {
        let reader = FakeReader::new(&[(1, 1, u64::MAX, &[2]), (2, 1, 1, &[])]);
        let interrupt = AtomicBool::new(false);
        let mut engine = Engine::new(reader, true, false, &interrupt);
        assert!(
            engine
                .count(Path::new("1"))
                .unwrap_err()
                .to_string()
                .contains("exceeds u64")
        );
        let reader = FakeReader::new(&[(1, 1, 1, &[2]), (2, 1, 1, &[])]);
        let mut engine = Engine::new(reader, true, false, &interrupt);
        engine.reader.interrupt_on_read = true;
        assert!(matches!(
            engine.count(Path::new("1")),
            Err(CountError::Interrupted)
        ));
        assert_eq!(*engine.reader.reads.borrow(), HashMap::from([(1, 1)]));
    }
}
