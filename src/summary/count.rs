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

struct Listing {
    count: u64,
    directories: Vec<Child>,
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
        let read = || -> io::Result<Listing> {
            let mut listing = Listing {
                count: 0,
                directories: Vec::new(),
            };
            for entry in fs::read_dir(path)? {
                if interrupt.load(Ordering::Relaxed) {
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                let entry = entry?;
                listing.count = listing
                    .count
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("entry count exceeds u64"))?;
                if !self.recursive {
                    continue;
                }
                let file_type = entry.file_type()?;
                let child_path = entry.path();
                let identity = if file_type.is_dir() {
                    Some(Identity::read(&child_path)?)
                } else if file_type.is_symlink() && self.follow {
                    match child_path.metadata() {
                        Ok(metadata) if metadata.is_dir() => {
                            Some(Identity::from_metadata(&child_path, &metadata)?)
                        }
                        Ok(_) => None,
                        // Dangling links and unresolvable link loops still count as entries.
                        Err(error) if leaf_symlink_error(&error) => None,
                        Err(error) => return Err(error),
                    }
                } else {
                    None
                };
                if let Some(identity) = identity {
                    listing.directories.push(Child {
                        name: entry.file_name(),
                        identity,
                    });
                }
            }
            Ok(listing)
        };
        let result = read();
        check_interrupt(interrupt)?;
        result.map_err(|error| CountError::filesystem(path, error))
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

    pub(super) fn count(&mut self, path: &Path) -> Result<u64> {
        self.engine.count(path)
    }
}

struct Engine<'a, R> {
    reader: R,
    recursive: bool,
    one_file_system: bool,
    interrupt: &'a AtomicBool,
    listings: HashMap<Identity, Result<Arc<Listing>>>,
    totals: HashMap<(Identity, Option<u64>), u64>,
}

struct Frame {
    identity: Identity,
    path: PathBuf,
    listing: Arc<Listing>,
    next: usize,
    total: u64,
    contextual: bool,
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

    fn frame(&mut self, path: PathBuf, identity: Identity) -> Result<Frame> {
        let listing = self
            .listings
            .entry(identity)
            .or_insert_with(|| self.reader.read(&path, self.interrupt).map(Arc::new))
            .clone()?;
        Ok(Frame {
            identity,
            path,
            total: listing.count,
            listing,
            next: 0,
            contextual: false,
        })
    }

    fn count(&mut self, path: &Path) -> Result<u64> {
        check_interrupt(self.interrupt)?;
        let identity = self.reader.identity(path)?;
        let boundary = self.one_file_system.then_some(identity.device);
        if let Some(&total) = self.totals.get(&(identity, boundary)) {
            return Ok(total);
        }
        let root = self.frame(path.to_path_buf(), identity)?;
        check_interrupt(self.interrupt)?;
        if !self.recursive {
            return Ok(root.total);
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
                    current.contextual = true;
                    continue;
                }
                if let Some(&total) = self.totals.get(&(child.identity, boundary)) {
                    current.total = add(current.total, total, &current.path)?;
                    continue;
                }
                let frame = self.frame(current.path.join(&child.name), child.identity)?;
                ancestors.insert(child.identity);
                stack.push(frame);
            } else {
                let complete = stack.pop().unwrap();
                ancestors.remove(&complete.identity);
                // Any ancestor cutoff makes the total depend on the route used to reach it.
                // Such listings can be reused, but their totals must not be cached globally.
                if !complete.contextual {
                    self.totals
                        .insert((complete.identity, boundary), complete.total);
                }
                if let Some(parent) = stack.last_mut() {
                    parent.total = add(parent.total, complete.total, &parent.path)?;
                    parent.contextual |= complete.contextual;
                } else {
                    return Ok(complete.total);
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
                return Err(CountError::filesystem(path, "injected read failure"));
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
            })
        }
    }

    #[test]
    fn reuses_scans_and_totals_without_deduplicating_entry_contributions() {
        let reader = FakeReader::new(&[(1, 1, 3, &[2, 2, 3]), (2, 1, 4, &[]), (3, 1, 0, &[])]);
        let interrupt = AtomicBool::new(false);
        let mut engine = Engine::new(reader, true, false, &interrupt);
        assert_eq!(engine.count(Path::new("1")).unwrap(), 11);
        assert_eq!(engine.count(Path::new("2-alias")).unwrap(), 4);
        assert_eq!(engine.count(Path::new("1")).unwrap(), 11);
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
                assert_eq!(engine.count(Path::new(path)).unwrap(), total);
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
                    engine.count(Path::new("1")).unwrap(),
                    if recursive && !limited { 14 } else { 2 }
                );
                if limited {
                    assert!(!engine.reader.reads.borrow().contains_key(&2));
                }
                assert_eq!(
                    engine.count(Path::new("2")).unwrap(),
                    if recursive && !limited { 12 } else { 5 }
                );
            }
        }
    }

    #[test]
    fn read_failures_propagate_but_do_not_poison_complete_rows() {
        let reader = FakeReader::new(&[(1, 1, 2, &[2, 3]), (2, 1, 1, &[]), (3, 1, 8, &[])]);
        let interrupt = AtomicBool::new(false);
        let mut engine = Engine::new(reader, true, false, &interrupt);
        engine.reader.failures.insert(2);
        assert!(engine.count(Path::new("1")).is_err());
        assert!(engine.count(Path::new("2")).is_err());
        assert_eq!(engine.count(Path::new("3")).unwrap(), 8);
        assert_eq!(engine.reader.reads.borrow()[&2], 1);
        assert!(!engine.totals.contains_key(&(engine.reader.id(1), None)));
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
