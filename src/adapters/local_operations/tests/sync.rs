// SPDX-License-Identifier: MIT

use std::path::{Path, PathBuf};

use rustix::io::Errno;

use super::super::{MountFlushHint, SyncDestination, removable_sync_roots, sync_filesystem};
use super::fs_quirks::Op;
use super::*;

fn hint(root: &str, removable: bool) -> MountFlushHint {
    MountFlushHint {
        root: PathBuf::from(root),
        removable,
    }
}

#[test]
fn removable_copy_roots_are_synced_once() {
    let hints = [
        hint("/media/usb", true),
        hint("/home", false),
        hint("/", false),
    ];

    let removable = removable_sync_roots(
        &[SyncDestination::Local(Path::new("/media/usb/photos/a.jpg"))],
        &hints,
    );
    assert_eq!(removable, vec![PathBuf::from("/media/usb")]);

    let internal = removable_sync_roots(
        &[SyncDestination::Local(Path::new("/home/user/a.jpg"))],
        &hints,
    );
    assert!(internal.is_empty());

    let once = removable_sync_roots(
        &[
            SyncDestination::Local(Path::new("/media/usb/a.jpg")),
            SyncDestination::Local(Path::new("/media/usb/nested/b.jpg")),
        ],
        &hints,
    );
    assert_eq!(once, vec![PathBuf::from("/media/usb")]);

    let unknown = removable_sync_roots(
        &[SyncDestination::Local(Path::new("/var/tmp/file"))],
        &[hint("/media/usb", true)],
    );
    assert!(unknown.is_empty());

    let remote = removable_sync_roots(&[SyncDestination::NonLocal], &hints);
    assert!(remote.is_empty());
}

#[test]
fn sync_filesystem_on_a_local_directory_succeeds() {
    let directory = tempfile::tempdir().expect("temp directory");
    sync_filesystem(directory.path()).expect("syncfs on a local directory");
}

#[test]
fn bytes_already_written_are_flushed_before_cancellation_and_cancel_is_not_pasted()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().join("usb");
    fs::create_dir(&destination)?;
    let source = root.path().join("photo.bin");
    fs::write(&source, b"pixels")?;
    let _guard = RemovableFlushGuard::install(root.path(), None);
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let handle = Rc::new(RefCell::new(None));
    let slot = handle.clone();
    let operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(99),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| {
            let cancel = matches!(
                &event,
                OperationEvent::TransferProgress {
                    completed_items, ..
                } if *completed_items >= 1
            );
            emitted.borrow_mut().push(event);
            if cancel {
                slot.borrow_mut().take();
            }
        }),
    );
    *handle.borrow_mut() = Some(operation);
    pump_until_transfer(&events);

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::Cancelled { .. })
    ));
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| matches!(event, OperationEvent::Pasted { .. }))
    );
    assert_eq!(fs::read(destination.join("photo.bin"))?, b"pixels");
    assert!(
        !sync_probe_observations().is_empty(),
        "written bytes are synced before cancellation is reported"
    );
    Ok(())
}

#[test]
fn a_cancel_after_the_flush_starts_does_not_report_success() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().join("usb");
    fs::create_dir(&destination)?;
    let source = root.path().join("photo.bin");
    fs::write(&source, b"pixels")?;
    let _guard = RemovableFlushGuard::install(root.path(), None);
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let handle = Rc::new(RefCell::new(None));
    let slot = handle.clone();
    let operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(100),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| {
            let cancel = matches!(event, OperationEvent::FlushingToDevice { .. });
            emitted.borrow_mut().push(event);
            if cancel {
                slot.borrow_mut().take();
            }
        }),
    );
    *handle.borrow_mut() = Some(operation);
    pump_until_transfer(&events);

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::Cancelled { .. })
    ));
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| matches!(event, OperationEvent::Pasted { .. }))
    );
    assert_eq!(fs::read(destination.join("photo.bin"))?, b"pixels");
    assert!(!sync_probe_observations().is_empty());
    Ok(())
}

#[test]
fn failed_final_flush_reports_that_writes_may_still_be_pending() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().join("usb");
    fs::create_dir(&destination)?;
    let source = root.path().join("photo.bin");
    fs::write(&source, b"pixels")?;
    let _guard = RemovableFlushGuard::install(root.path(), Some(io::ErrorKind::Other));
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(106),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );
    pump_until_transfer(&events);
    assert!(matches!(terminal_transfer(&events.borrow()),
        Some(OperationEvent::TransferFailed { message, .. })
        if message.contains("writes may still be pending") && message.contains("wait for safe eject")));
    Ok(())
}

#[test]
fn failed_flush_after_cancel_reports_that_writes_may_still_be_pending() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().join("usb");
    fs::create_dir(&destination)?;
    let _guard = RemovableFlushGuard::install(root.path(), Some(io::ErrorKind::Other));
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let emit: Rc<dyn Fn(OperationEvent)> = Rc::new(move |event| emitted.borrow_mut().push(event));
    glib::MainContext::default().block_on(stop_transfer(
        &[destination],
        &emit,
        OperationRequestId(105),
        TransferStop {
            completed: Vec::new(),
            failed: Vec::new(),
            not_attempted: Vec::new(),
            affected_locations: HashSet::new(),
            failure: None,
            skip_flush: false,
        },
    ));
    assert!(
        matches!(events.borrow().last(), Some(OperationEvent::TransferFailed { message, .. })
        if message.contains("writes may still be pending") && message.contains("wait for safe eject"))
    );
    assert!(!sync_probe_observations().is_empty());
    Ok(())
}

#[test]
fn fat32_limit_failure_does_not_start_an_uncancellable_device_flush() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().join("usb");
    fs::create_dir(&destination)?;
    let _guard = RemovableFlushGuard::install(root.path(), None);
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let emit: Rc<dyn Fn(OperationEvent)> = Rc::new(move |event| emitted.borrow_mut().push(event));
    glib::MainContext::default().block_on(stop_transfer(
        &[destination],
        &emit,
        OperationRequestId(104),
        TransferStop {
            completed: Vec::new(),
            failed: Vec::new(),
            not_attempted: Vec::new(),
            affected_locations: HashSet::new(),
            failure: Some("A file exceeds the FAT32 limit".to_owned()),
            skip_flush: true,
        },
    ));
    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::TransferFailed { .. })
    ));
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| matches!(event, OperationEvent::FlushingToDevice { .. }))
    );
    assert!(sync_probe_observations().is_empty());
    Ok(())
}

#[test]
fn bytes_already_written_are_flushed_before_transfer_failure() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().join("usb");
    fs::create_dir(&destination)?;
    let source = root.path().join("photo.bin");
    fs::write(&source, b"pixels")?;
    let _guard = RemovableFlushGuard::install(root.path(), None);
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(101),
            destination: Location::local(&destination),
            items: vec![
                PasteItem {
                    source: Location::local(&source),
                    conflict: TransferConflict::FailIfExists,
                },
                PasteItem {
                    source: Location::local(root.path().join("missing.bin")),
                    conflict: TransferConflict::FailIfExists,
                },
            ],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );
    pump_until_transfer(&events);

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::TransferFailed { .. })
    ));
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| matches!(event, OperationEvent::Pasted { .. }))
    );
    assert_eq!(fs::read(destination.join("photo.bin"))?, b"pixels");
    assert!(!sync_probe_observations().is_empty());
    Ok(())
}

/// What one barrier in a destination saw of a move of files.
#[derive(Debug, PartialEq)]
struct Seen {
    /// Moved sources already gone.
    deleted: Vec<String>,
    /// Copies already made.
    copied: Vec<String>,
    /// Whether the progress dock showed the device flush.
    flushing: bool,
}

/// Records, at each `SyncFs` in the destination, what [`Seen`] holds.
struct Barriers {
    seen: Arc<std::sync::Mutex<Vec<Seen>>>,
    flushing: Arc<std::sync::atomic::AtomicBool>,
}

impl Barriers {
    fn new() -> Self {
        Self {
            seen: Arc::default(),
            flushing: Arc::default(),
        }
    }

    /// One racer per barrier, running `also` at the first.
    fn racers(
        &self,
        sources: &Path,
        names: &[&str],
        destination: &Path,
        mut also: Option<Box<dyn FnMut() + Send>>,
    ) -> Vec<(Op, &'static str, usize, super::fs_quirks::Racer)> {
        (0..names.len())
            .map(|_| {
                let (seen, flushing) = (self.seen.clone(), self.flushing.clone());
                let (sources, destination) = (sources.to_path_buf(), destination.to_path_buf());
                let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
                let mut also = also.take();
                let racer: super::fs_quirks::Racer = Box::new(move || {
                    if let Some(also) = &mut also {
                        also();
                    }
                    let deleted = names
                        .iter()
                        .filter(|name| !sources.join(name).exists())
                        .cloned()
                        .collect();
                    let copied = names
                        .iter()
                        .filter(|name| destination.join(name).exists())
                        .cloned()
                        .collect();
                    seen.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(Seen {
                            deleted,
                            copied,
                            flushing: flushing.load(std::sync::atomic::Ordering::SeqCst),
                        });
                });
                (Op::SyncFs, "*", 0, racer)
            })
            .collect()
    }

    fn seen(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().unwrap_or_else(|error| error.into_inner()))
    }
}

/// Pastes a move of `sources` into `destination`, recording events, the
/// device flush status for `barriers`, and each item reported moved while
/// its source still exists; `cancel_after` items, the move is cancelled.
fn paste_moves(
    id: u64,
    destination: &Path,
    sources: &[PathBuf],
    barriers: &Barriers,
    cancel_after: Option<usize>,
) -> (
    Rc<RefCell<Option<LoadHandle>>>,
    Rc<RefCell<Vec<OperationEvent>>>,
    Rc<RefCell<Vec<Location>>>,
) {
    let events = Rc::new(RefCell::new(Vec::new()));
    let early = Rc::new(RefCell::new(Vec::new()));
    let handle = Rc::new(RefCell::new(None));
    let (emitted, reported_early, slot) = (events.clone(), early.clone(), handle.clone());
    let flushing = barriers.flushing.clone();
    let operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(id),
            destination: Location::local(destination),
            items: sources
                .iter()
                .map(|source| PasteItem {
                    source: Location::local(source),
                    conflict: TransferConflict::FailIfExists,
                })
                .collect(),
            move_sources: true,
        },
        Rc::new(move |event| {
            match &event {
                OperationEvent::FlushingToDevice { .. } => {
                    flushing.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                OperationEvent::ItemMoved { from, .. }
                    if from.native_path().is_some_and(Path::exists) =>
                {
                    reported_early.borrow_mut().push(from.clone());
                }
                OperationEvent::TransferProgress {
                    completed_items, ..
                } if Some(*completed_items) == cancel_after => {
                    slot.borrow_mut().take();
                }
                _ => {}
            }
            emitted.borrow_mut().push(event);
        }),
    );
    *handle.borrow_mut() = Some(operation);
    (handle, events, early)
}

fn moved_count(events: &[OperationEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, OperationEvent::ItemMoved { .. }))
        .count()
}

/// Resets the batch limits a test set.
struct LimitsGuard;

impl LimitsGuard {
    fn set(limits: super::super::owed::Limits) -> Self {
        super::super::owed::set_batch_limits_for_test(Some(limits));
        Self
    }
}

impl Drop for LimitsGuard {
    fn drop(&mut self) {
        super::super::owed::set_batch_limits_for_test(None);
    }
}

const NO_LIMITS: super::super::owed::Limits = super::super::owed::Limits {
    items: usize::MAX,
    bytes: u64::MAX,
    age: Duration::MAX,
};

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn the_barrier_before_deleting_a_moved_source_follows_the_destination_family()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // The family, whether the device is removable, whether the paste
    // moves, whether each written file is synced before it is closed, and
    // the barrier's calls.
    let rows: [(&str, &str, bool, bool, bool, &[Op]); 7] = [
        ("ext4", "rw", false, true, false, &[Op::SyncFs]),
        (
            "vfat",
            "rw,flush",
            false,
            true,
            false,
            &[Op::SyncFs, Op::SyncDir],
        ),
        ("fuseblk", "rw", false, true, true, &[Op::SyncDir]),
        ("cifs", "rw,serverino", false, true, true, &[]),
        ("9p", "rw", false, true, true, &[Op::SyncFs]),
        ("ext4", "rw", true, true, false, &[Op::SyncFs, Op::SyncDir]),
        ("ext4", "rw", false, false, false, &[]),
    ];
    for (id, (fs_type, options, removable, moving, each_file, barrier)) in (300..).zip(rows) {
        let row = format!("{fs_type} removable: {removable} moving: {moving}");
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let folder = sources.join("folder");
        fs::create_dir_all(&folder)?;
        fs::write(folder.join("a.txt"), b"a")?;
        fs::write(folder.join("b.txt"), b"bb")?;
        let destination = root.path().join("destination");
        fs::create_dir(&destination)?;
        let _removable = removable.then(|| RemovableFlushGuard::install(&destination, None));
        let destination_quirks = FsQuirks::install(
            &destination,
            Quirks {
                mount_type: Some((fs_type, options)),
                ..Quirks::default()
            },
        );
        let at_first_unlink = Arc::new(std::sync::Mutex::new(None));
        let (record, read) = (at_first_unlink.clone(), destination_quirks.reader());
        let _sources = FsQuirks::install(
            &sources,
            Quirks {
                before: vec![(
                    Op::Unlink,
                    "*",
                    0,
                    Box::new(move || {
                        *record.lock().unwrap_or_else(|error| error.into_inner()) = Some(read());
                    }),
                )],
                ..Quirks::default()
            },
        );

        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();
        let _operation = LocalOperationProvider.paste(
            PasteRequest {
                id: OperationRequestId(id),
                destination: Location::local(&destination),
                items: vec![PasteItem {
                    source: Location::local(&folder),
                    conflict: TransferConflict::FailIfExists,
                }],
                move_sources: moving,
            },
            Rc::new(move |event| emitted.borrow_mut().push(event)),
        );
        pump_until_transfer(&events);

        assert!(
            matches!(
                terminal_transfer(&events.borrow()),
                Some(OperationEvent::Pasted { .. })
            ),
            "{row}: {:?}",
            events.borrow()
        );
        assert_eq!(fs::read(destination.join("folder/b.txt"))?, b"bb", "{row}");
        assert_eq!(folder.exists(), !moving, "{row}");
        let calls = if moving {
            at_first_unlink
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .ok_or("the source was deleted")?
        } else {
            destination_quirks.calls()
        };
        let position = |op: Op, name: &str| {
            calls
                .iter()
                .position(|call| call.op == op && call.names.iter().any(|named| named == name))
        };
        for name in ["a.txt", "b.txt"] {
            let synced = position(Op::SyncData, name);
            assert_eq!(synced.is_some(), each_file, "{row}: {name}");
            if let Some(synced) = synced {
                assert!(
                    position(Op::Close, name).is_some_and(|closed| synced < closed),
                    "{row}: {name} is synced before it is closed"
                );
            }
        }
        let reached: Vec<Op> = calls
            .iter()
            .map(|call| call.op)
            .filter(|op| matches!(op, Op::SyncFs | Op::SyncDir))
            .collect();
        assert_eq!(reached, barrier, "{row}");
    }
    Ok(())
}

#[test]
fn owed_deletions_close_in_batches() -> Result<(), Box<dyn Error>> {
    use super::super::owed::Limits;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let names = ["a", "b", "c", "d", "e"];
    let rows: [(&str, Limits, [usize; 5], Vec<Seen>); 4] = [
        (
            "the end of the paste",
            NO_LIMITS,
            [1; 5],
            vec![Seen {
                deleted: Vec::new(),
                copied: strings(&names),
                flushing: true,
            }],
        ),
        (
            "items",
            Limits {
                items: 2,
                ..NO_LIMITS
            },
            [1; 5],
            vec![
                Seen {
                    deleted: Vec::new(),
                    copied: strings(&["a", "b"]),
                    flushing: false,
                },
                Seen {
                    deleted: strings(&["a", "b"]),
                    copied: strings(&["a", "b", "c", "d"]),
                    flushing: false,
                },
                Seen {
                    deleted: strings(&["a", "b", "c", "d"]),
                    copied: strings(&names),
                    flushing: true,
                },
            ],
        ),
        (
            "bytes",
            Limits {
                bytes: 10,
                ..NO_LIMITS
            },
            [12, 1, 1, 12, 1],
            vec![
                Seen {
                    deleted: Vec::new(),
                    copied: strings(&["a"]),
                    flushing: false,
                },
                Seen {
                    deleted: strings(&["a"]),
                    copied: strings(&["a", "b", "c", "d"]),
                    flushing: false,
                },
                Seen {
                    deleted: strings(&["a", "b", "c", "d"]),
                    copied: strings(&names),
                    flushing: true,
                },
            ],
        ),
        (
            "age",
            Limits {
                age: Duration::ZERO,
                ..NO_LIMITS
            },
            [1; 5],
            (0..names.len())
                .map(|index| Seen {
                    deleted: strings(&names[..index]),
                    copied: strings(&names[..=index]),
                    flushing: false,
                })
                .collect(),
        ),
    ];
    for (id, (row, limits, sizes, expected)) in (310..).zip(rows) {
        let _limits = LimitsGuard::set(limits);
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let destination = root.path().join("destination");
        fs::create_dir(&sources)?;
        fs::create_dir(&destination)?;
        let mut paths = Vec::new();
        for (name, size) in names.iter().zip(sizes) {
            fs::write(sources.join(name), vec![b'x'; size])?;
            paths.push(sources.join(name));
        }
        let barriers = Barriers::new();
        let _sources = FsQuirks::install(&sources, Quirks::default());
        let _destination = FsQuirks::install(
            &destination,
            Quirks {
                before: barriers.racers(&sources, &names, &destination, None),
                ..Quirks::ext4()
            },
        );

        let (_operation, events, early) = paste_moves(id, &destination, &paths, &barriers, None);
        pump_until_transfer(&events);

        assert!(
            matches!(
                terminal_transfer(&events.borrow()),
                Some(OperationEvent::Pasted { locations, .. })
                if locations.len() == names.len()
            ),
            "{row}: {:?}",
            events.borrow()
        );
        assert_eq!(barriers.seen(), expected, "{row}");
        assert!(paths.iter().all(|path| !path.exists()), "{row}");
        assert_eq!(moved_count(&events.borrow()), names.len(), "{row}");
        assert!(early.borrow().is_empty(), "{row}: {:?}", early.borrow());
    }
    Ok(())
}

#[test]
fn a_failed_barrier_deletes_nothing_in_its_batch_and_stops_the_move() -> Result<(), Box<dyn Error>>
{
    use super::super::owed::Limits;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let _limits = LimitsGuard::set(Limits {
        items: 2,
        ..NO_LIMITS
    });
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let destination = root.path().join("destination");
    fs::create_dir(&sources)?;
    fs::create_dir(&destination)?;
    let names = ["a", "b", "c", "d", "e"];
    let mut paths = Vec::new();
    for name in names {
        fs::write(sources.join(name), name)?;
        paths.push(sources.join(name));
    }
    let barriers = Barriers::new();
    let _sources = FsQuirks::install(&sources, Quirks::default());
    let _destination = FsQuirks::install(
        &destination,
        Quirks {
            before: barriers.racers(&sources, &names, &destination, None),
            fail: vec![(Op::SyncFs, "*", 1, Errno::IO)],
            ..Quirks::ext4()
        },
    );

    let (_operation, events, early) = paste_moves(320, &destination, &paths, &barriers, None);
    pump_until_transfer(&events);

    assert!(
        matches!(terminal_transfer(&events.borrow()),
            Some(OperationEvent::TransferFailed { completed_locations, message, .. })
            if completed_locations == &[Location::local(&paths[0]), Location::local(&paths[1])]
                && message.matches("writes may still be pending").count() == 1),
        "{:?}",
        events.borrow()
    );
    assert_eq!(barriers.seen().len(), 2);
    for (name, path) in names.iter().zip(&paths) {
        let moved = ["a", "b"].contains(name);
        assert_eq!(path.exists(), !moved, "{name}");
        assert_eq!(destination.join(name).exists(), *name != "e", "{name}");
    }
    assert_eq!(moved_count(&events.borrow()), 2);
    assert!(early.borrow().is_empty());
    Ok(())
}

#[test]
fn cancelling_a_move_closes_its_pending_batch() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let _limits = LimitsGuard::set(NO_LIMITS);
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let destination = root.path().join("destination");
    fs::create_dir(&sources)?;
    fs::create_dir(&destination)?;
    let names = ["a", "b", "c"];
    let mut paths = Vec::new();
    for name in names {
        fs::write(sources.join(name), name)?;
        paths.push(sources.join(name));
    }
    let barriers = Barriers::new();
    let _sources = FsQuirks::install(&sources, Quirks::default());
    let _destination = FsQuirks::install(
        &destination,
        Quirks {
            before: barriers.racers(&sources, &names, &destination, None),
            ..Quirks::ext4()
        },
    );

    let (_operation, events, early) = paste_moves(330, &destination, &paths, &barriers, Some(2));
    pump_until_transfer(&events);

    assert!(
        matches!(terminal_transfer(&events.borrow()),
            Some(OperationEvent::Cancelled { result, .. })
            if result.completed == [Location::local(&paths[0]), Location::local(&paths[1])]
                && result.not_attempted == [Location::local(&paths[2])]),
        "{:?}",
        events.borrow()
    );
    assert_eq!(
        barriers.seen(),
        [Seen {
            deleted: Vec::new(),
            copied: strings(&["a", "b"]),
            flushing: true,
        }]
    );
    assert!(!paths[0].exists() && !paths[1].exists());
    assert!(paths[2].exists() && !destination.join("c").exists());
    assert_eq!(moved_count(&events.borrow()), 2);
    assert!(early.borrow().is_empty());
    Ok(())
}

#[test]
fn many_moved_files_are_copied_in_parallel_and_share_a_barrier() -> Result<(), Box<dyn Error>> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let _limits = LimitsGuard::set(NO_LIMITS);
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let destination = root.path().join("destination");
    fs::create_dir(&sources)?;
    fs::create_dir(&destination)?;
    let names: Vec<String> = (0..12).map(|index| format!("f{index:02}.txt")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut paths = Vec::new();
    for name in &names {
        fs::write(sources.join(name), name)?;
        paths.push(sources.join(name));
    }
    // The first copy waits for a second to start, as only parallel copies can.
    let (started, parallel) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let mut opened: Vec<(Op, &'static str, usize, super::fs_quirks::Racer)> = Vec::new();
    let (count, seen_parallel) = (started.clone(), parallel.clone());
    opened.push((
        Op::OpenSource,
        "*",
        0,
        Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            let start = Instant::now();
            while count.load(Ordering::SeqCst) < 2 && start.elapsed() < Duration::from_secs(3) {
                thread::sleep(Duration::from_millis(5));
            }
            seen_parallel.store(count.load(Ordering::SeqCst) >= 2, Ordering::SeqCst);
        }),
    ));
    for _ in 1..names.len() {
        let count = started.clone();
        opened.push((
            Op::OpenSource,
            "*",
            0,
            Box::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        ));
    }
    let barriers = Barriers::new();
    let changed = sources.join("f03.txt");
    let rewrite: Box<dyn FnMut() + Send> =
        Box::new(move || fs::write(&changed, b"rewritten after its copy").expect("rewrite"));
    let _sources = FsQuirks::install(
        &sources,
        Quirks {
            before: opened,
            ..Quirks::default()
        },
    );
    let _destination = FsQuirks::install(
        &destination,
        Quirks {
            before: barriers.racers(&sources, &names, &destination, Some(rewrite)),
            ..Quirks::ext4()
        },
    );

    let (_operation, events, early) = paste_moves(340, &destination, &paths, &barriers, None);
    pump_until_transfer(&events);

    assert!(
        matches!(
            terminal_transfer(&events.borrow()),
            Some(OperationEvent::Pasted { .. })
        ),
        "{:?}",
        events.borrow()
    );
    if thread::available_parallelism().map_or(1, usize::from) > 1 {
        assert!(parallel.load(Ordering::SeqCst), "the copies overlap");
    }
    assert_eq!(
        barriers.seen(),
        [Seen {
            deleted: Vec::new(),
            copied: strings(&names),
            flushing: true,
        }]
    );
    for (name, path) in names.iter().zip(&paths) {
        assert_eq!(path.exists(), *name == "f03.txt", "{name}");
        assert_eq!(fs::read(destination.join(name))?, name.as_bytes(), "{name}");
    }
    let kept: Vec<_> = events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            OperationEvent::ItemsReported { report, .. } => Some(report.kept.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        kept,
        [crate::services::KeptItem {
            location: Location::local(sources.join("f03.txt")),
            cause: crate::services::KeptCause::Changed,
        }]
    );
    assert!(
        early
            .borrow()
            .iter()
            .all(|from| from == &Location::local(sources.join("f03.txt")))
    );
    let last = events.borrow().iter().rev().find_map(|event| match event {
        OperationEvent::TransferProgress {
            completed_files,
            total_files,
            transferred_bytes,
            total_bytes,
            ..
        } => Some((
            *completed_files,
            *total_files,
            *transferred_bytes,
            *total_bytes,
        )),
        _ => None,
    });
    let bytes: u64 = names.iter().map(|name| name.len() as u64).sum();
    assert_eq!(last, Some((12, Some(12), bytes, Some(bytes))));
    Ok(())
}

#[test]
fn names_of_one_file_moved_as_separate_items_are_all_deleted() -> Result<(), Box<dyn Error>> {
    use super::super::owed::Limits;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // Selected names of one file, or folders whose files share names (as
    // backup snapshots do), deleted in one batch or each item in its own,
    // after the next item's copy recorded fewer names.
    let items: [(&str, &[&str]); 2] = [
        ("selected names", &["a", "b", "c"]),
        ("folders", &["daily.0", "daily.1", "daily.2"]),
    ];
    let batches = [
        NO_LIMITS,
        Limits {
            items: 1,
            ..NO_LIMITS
        },
    ];
    for (id, (&(row, names), limits)) in (350..).zip(
        items
            .iter()
            .flat_map(|item| batches.iter().map(move |limits| (item, limits))),
    ) {
        let row = format!("{row}, a batch of {}", limits.items);
        let _limits = LimitsGuard::set(*limits);
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let destination = root.path().join("destination");
        fs::create_dir(&sources)?;
        fs::create_dir(&destination)?;
        if row.starts_with("selected") {
            fs::write(sources.join("a"), b"shared")?;
            for name in &names[1..] {
                fs::hard_link(sources.join("a"), sources.join(name))?;
            }
        } else {
            for name in names {
                fs::create_dir(sources.join(name))?;
            }
            for file in ["f1", "f2", "f3"] {
                let first = sources.join(names[0]).join(file);
                fs::write(&first, file)?;
                for name in &names[1..] {
                    fs::hard_link(&first, sources.join(name).join(file))?;
                }
            }
        }
        let paths: Vec<PathBuf> = names.iter().map(|name| sources.join(name)).collect();
        let _volumes = separate_volumes(&sources, &destination);

        let (_operation, events, _) = paste_moves(id, &destination, &paths, &Barriers::new(), None);
        pump_until_transfer(&events);

        assert!(
            matches!(
                terminal_transfer(&events.borrow()),
                Some(OperationEvent::Pasted { .. })
            ),
            "{row}: {:?}",
            events.borrow()
        );
        let kept: Vec<_> = events
            .borrow()
            .iter()
            .filter_map(|event| match event {
                OperationEvent::ItemsReported { report, .. } => Some(report.kept.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(kept, [], "{row}");
        assert_eq!(fs::read_dir(&sources)?.count(), 0, "{row}");
        assert_eq!(moved_count(&events.borrow()), names.len(), "{row}");
    }
    Ok(())
}
