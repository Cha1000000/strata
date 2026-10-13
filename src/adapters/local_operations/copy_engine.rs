// SPDX-License-Identifier: MIT

//! Copies native files through descriptors on a worker thread. A copy never
//! replaces an entry, and never shows a partial file under its final name.

mod data;
mod metadata;
pub(super) mod stage;

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
};

use gtk::{gio, glib};
use rustix::{
    fs::{AtFlags, Mode},
    io::Errno,
};

use super::{
    Reported, TransferProgressTracker, cancelled_local_operation, copy_failure_after_cleanup,
    fs_ops::{Entry, EntryStat, FsOps, fs},
    io_error, open_local_parent_directory, os_error_detail, placement,
    placement::{ENOTSUPP, PlaceError},
    placement_error, run_cancellable,
};
use crate::adapters::volume::{
    Family, MountCapabilities, MountKey, family, learn_mount_capability, mount_capabilities,
};
use data::Data;
use metadata::Snapshot;

/// Counters that workers advance and the main thread's follower reads.
#[derive(Default)]
pub(super) struct SharedProgress {
    bytes: AtomicU64,
    files: AtomicUsize,
}

impl SharedProgress {
    pub(super) fn reported(&self) -> Reported {
        Reported {
            bytes: self.bytes.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
        }
    }

    fn add_bytes(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn add_file(&self) {
        self.files.fetch_add(1, Ordering::Relaxed);
    }
}

/// Copies the regular file at `source` to `target`, a name that must be free,
/// and returns what it added to `progress`.
pub(super) async fn copy_new_file(
    source: PathBuf,
    target: PathBuf,
    progress: Option<Rc<TransferProgressTracker>>,
    cancellable: &gio::Cancellable,
) -> Result<Reported, glib::Error> {
    let shared = Arc::new(SharedProgress::default());
    let follower = progress.map(|progress| {
        if let Some(name) = source.file_name() {
            progress.set_current_file(name.to_string_lossy().into_owned());
        }
        progress.follow(&shared)
    });
    let counters = shared.clone();
    let copied = run_cancellable(cancellable, move |cancelled| {
        copy_file(fs(), &source, &target, &counters, &cancelled)
    })
    .await;
    let reported = follower.map_or_else(|| shared.reported(), |follower| follower.stop());
    copied.map_err(|_| io_error("Copy worker stopped unexpectedly"))??;
    Ok(reported)
}

fn split(path: &Path) -> Result<(OwnedFd, OsString), glib::Error> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io_error(crate::i18n::tr("Invalid local filesystem target")));
    };
    let parent = open_local_parent_directory(parent).map_err(io_error)?;
    Ok((parent, name.to_os_string()))
}

fn copy_file(
    fs: &dyn FsOps,
    source: &Path,
    target: &Path,
    progress: &SharedProgress,
    cancelled: &AtomicBool,
) -> Result<(), glib::Error> {
    let (source_parent, source_name) = split(source)?;
    let (target_parent, target_name) = split(target)?;
    let mut job = Job {
        fs,
        item: &source_name,
        dir: target_parent.as_fd(),
        name: &target_name,
        cancelled,
        progress,
        data: Data::default(),
        target: Mount::new(fs, target_parent.as_fd()),
        refused_xattrs: HashSet::new(),
    };
    job.copy_from(Entry::new(&source_parent, &source_name))
        .map_err(|failure| failure.into_error(&source_name))
}

/// Why a copy stopped.
enum Failure {
    Cancelled,
    Os(Errno),
    Place(PlaceError),
    Changed,
    Error(glib::Error),
}

impl From<Errno> for Failure {
    fn from(error: Errno) -> Self {
        Self::Os(error)
    }
}

impl Failure {
    fn into_error(self, name: &OsStr) -> glib::Error {
        let name = name.to_string_lossy();
        match self {
            Self::Cancelled => cancelled_local_operation(),
            Self::Os(error) => {
                let message = rust_i18n::t!(
                    "Could not copy %{name}: %{error}",
                    name = name,
                    error = os_error_detail(error)
                );
                match error {
                    Errno::NOSPC | Errno::DQUOT => {
                        glib::Error::new(gio::IOErrorEnum::NoSpace, &message)
                    }
                    _ => io_error(message),
                }
            }
            Self::Place(error) => placement_error(error),
            Self::Changed => io_error(rust_i18n::t!(
                "%{name} changed while it was being copied",
                name = name
            )),
            Self::Error(error) => error,
        }
    }
}

/// The device or the connection is failing, so no step may skip the error.
fn is_fatal(error: Errno) -> bool {
    matches!(
        error,
        Errno::IO
            | Errno::STALE
            | Errno::NOTCONN
            | Errno::CONNABORTED
            | Errno::CONNRESET
            | Errno::TIMEDOUT
            | Errno::HOSTDOWN
            | Errno::HOSTUNREACH
            | Errno::NETDOWN
            | Errno::NETUNREACH
    )
}

fn is_unsupported(error: Errno) -> bool {
    matches!(error, Errno::OPNOTSUPP | Errno::NOTTY | ENOTSUPP)
}

/// A mount with what was learned about it, keyed as placement keys it.
struct Mount {
    key: MountKey,
    family: Family,
}

impl Mount {
    fn new(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> Self {
        let key = fs.mount_key(dir);
        let family = mount_capabilities(key).family.unwrap_or_else(|| {
            let family = fs
                .mount_type(dir)
                .map_or(Family::Other, |fs_type| family(&fs_type));
            learn_mount_capability(key, |capabilities| capabilities.family = Some(family));
            family
        });
        Self { key, family }
    }

    fn capabilities(&self) -> MountCapabilities {
        mount_capabilities(self.key)
    }

    fn learn(&self, learn: impl FnOnce(&mut MountCapabilities)) {
        learn_mount_capability(self.key, learn);
    }
}

const TMPFILE_LINK_UNKNOWN: u8 = 0;
const TMPFILE_LINK_EMPTY_PATH: u8 = 1;
const TMPFILE_LINK_PROC: u8 = 2;
const TMPFILE_LINK_UNAVAILABLE: u8 = 3;

/// How this process links unnamed files: `AT_EMPTY_PATH` needs Linux 6.10
/// unprivileged, and the `/proc` form needs `/proc`.
static TMPFILE_LINK: AtomicU8 = AtomicU8::new(TMPFILE_LINK_UNKNOWN);

#[cfg(test)]
pub(super) fn forget_learned_copy_support() {
    TMPFILE_LINK.store(TMPFILE_LINK_UNKNOWN, Ordering::Relaxed);
    data::forget_learned();
}

fn link_tmpfile(fs: &dyn FsOps, fd: BorrowedFd<'_>, to: Entry<'_>) -> Result<(), Errno> {
    let known = TMPFILE_LINK.load(Ordering::Relaxed);
    if known != TMPFILE_LINK_PROC {
        match fs.link_fd(fd, to, false) {
            Ok(()) => {
                TMPFILE_LINK.store(TMPFILE_LINK_EMPTY_PATH, Ordering::Relaxed);
                return Ok(());
            }
            // Once the plain form has worked, ENOENT is about the folder.
            Err(Errno::NOENT) if known == TMPFILE_LINK_UNKNOWN => {}
            Err(error) => return Err(error),
        }
    }
    match fs.link_fd(fd, to, true) {
        Ok(()) => {
            TMPFILE_LINK.store(TMPFILE_LINK_PROC, Ordering::Relaxed);
            Ok(())
        }
        Err(Errno::NOENT) if known == TMPFILE_LINK_UNKNOWN => {
            TMPFILE_LINK.store(TMPFILE_LINK_UNAVAILABLE, Ordering::Relaxed);
            Err(Errno::NOENT)
        }
        Err(error) => Err(error),
    }
}

/// An open source file and its snapshot from that descriptor.
struct Source {
    fd: OwnedFd,
    stat: EntryStat,
}

/// One top-level file copied into `dir` as `name`.
struct Job<'a> {
    fs: &'a dyn FsOps,
    /// The name errors give, the item's own.
    item: &'a OsStr,
    dir: BorrowedFd<'a>,
    name: &'a OsStr,
    cancelled: &'a AtomicBool,
    progress: &'a SharedProgress,
    data: Data,
    target: Mount,
    /// Names this job saw refused one by one, such as `security.*` as non-root.
    refused_xattrs: HashSet<OsString>,
}

/// A stage this job created, which only it may remove.
struct Stage {
    name: OsString,
    created: EntryStat,
}

impl Job<'_> {
    fn check_cancelled(&self) -> Result<(), Failure> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Failure::Cancelled);
        }
        Ok(())
    }

    fn target(&self) -> Entry<'_> {
        Entry {
            dir: self.dir,
            name: self.name,
        }
    }

    fn copy_from(&mut self, at: Entry<'_>) -> Result<(), Failure> {
        self.check_cancelled()?;
        let source = match self.fs.open_source(at) {
            Ok(Some((fd, stat))) => Source { fd, stat },
            Ok(None) => return Err(Failure::Changed),
            Err(error) => {
                return Err(Failure::Error(io_error(rust_i18n::t!(
                    "%{name} changed while it was being copied: %{error}",
                    name = at.name.to_string_lossy(),
                    error = os_error_detail(error)
                ))));
            }
        };
        let parent = self.fs.stat_fd(self.dir)?;
        if stage::tmpfile_allowed(self.target.family)
            && self.target.capabilities().tmpfile != Some(false)
            && TMPFILE_LINK.load(Ordering::Relaxed) != TMPFILE_LINK_UNAVAILABLE
        {
            match self.fs.open_tmpfile(self.dir, Mode::from_raw_mode(0o600)) {
                Ok(fd) => {
                    self.target
                        .learn(|capabilities| capabilities.tmpfile = Some(true));
                    return self.through_tmpfile(&source, fd, &parent);
                }
                // Before Linux 3.11 the flag reads as O_DIRECTORY.
                Err(error) if is_unsupported(error) || error == Errno::ISDIR => {
                    self.target
                        .learn(|capabilities| capabilities.tmpfile = Some(false));
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.through_named_stage(&source, &parent)
    }

    /// Copies all of `source`, counting the bytes as progress when `counted`.
    fn copy_data(
        &mut self,
        source: BorrowedFd<'_>,
        source_stat: &EntryStat,
        target: BorrowedFd<'_>,
        parent: &EntryStat,
        counted: bool,
    ) -> Result<(), Failure> {
        let route = data::Route::new(self.fs, &self.target, source, source_stat, parent);
        let progress = counted.then_some(self.progress);
        self.data.copy(
            self.fs,
            self.cancelled,
            &self.target,
            &route,
            (source, target),
            source_stat.size,
            |bytes| {
                if let Some(progress) = progress {
                    progress.add_bytes(bytes);
                }
            },
        )
    }

    fn apply_metadata(
        &mut self,
        fd: BorrowedFd<'_>,
        created: &EntryStat,
        snapshot: &Snapshot,
        parent: &EntryStat,
    ) -> Result<(), Failure> {
        snapshot
            .apply(
                self.fs,
                fd,
                created,
                parent,
                &self.target,
                &mut self.refused_xattrs,
            )
            .map_err(Failure::Os)
    }

    fn through_tmpfile(
        &mut self,
        source: &Source,
        tmpfile: OwnedFd,
        parent: &EntryStat,
    ) -> Result<(), Failure> {
        self.copy_data(
            source.fd.as_fd(),
            &source.stat,
            tmpfile.as_fd(),
            parent,
            true,
        )?;
        let snapshot = Snapshot::read(self.fs, source.fd.as_fd(), &source.stat)?;
        let created = self.fs.stat_fd(tmpfile.as_fd())?;
        self.apply_metadata(tmpfile.as_fd(), &created, &snapshot, parent)?;
        self.check_cancelled()?;
        if let Err(error) = link_tmpfile(self.fs, tmpfile.as_fd(), self.target()) {
            return match placement::file_creation_error(error) {
                PlaceError::Exists => Err(Failure::Place(PlaceError::Exists)),
                _ if matches!(error, Errno::NOSPC | Errno::DQUOT) || is_fatal(error) => {
                    Err(error.into())
                }
                // The written bytes are copied out on the same device, so
                // the source is never read twice.
                _ => self.through_stage_from_tmpfile(tmpfile, &created, &snapshot, parent),
            };
        }
        if let Err(error) = self.fs.close(tmpfile) {
            placement::remove_if_unchanged(
                self.fs,
                self.target(),
                created.identity,
                AtFlags::empty(),
            );
            return Err(error.into());
        }
        self.progress.add_file();
        Ok(())
    }

    fn through_stage_from_tmpfile(
        &mut self,
        tmpfile: OwnedFd,
        tmpfile_stat: &EntryStat,
        snapshot: &Snapshot,
        parent: &EntryStat,
    ) -> Result<(), Failure> {
        let (fd, stage) = self.create_stage()?;
        let copied = self
            .copy_data(tmpfile.as_fd(), tmpfile_stat, fd.as_fd(), parent, false)
            .and_then(|()| self.apply_metadata(fd.as_fd(), &stage.created, snapshot, parent));
        drop(tmpfile);
        match copied {
            Ok(()) => self.publish_stage(fd, &stage, snapshot, parent),
            Err(failure) => {
                drop(fd);
                Err(self.discard(&stage, failure))
            }
        }
    }

    fn through_named_stage(&mut self, source: &Source, parent: &EntryStat) -> Result<(), Failure> {
        let (fd, stage) = self.create_stage()?;
        let copied = self
            .copy_data(source.fd.as_fd(), &source.stat, fd.as_fd(), parent, true)
            .and_then(|()| Ok(Snapshot::read(self.fs, source.fd.as_fd(), &source.stat)?))
            .and_then(|snapshot| {
                self.apply_metadata(fd.as_fd(), &stage.created, &snapshot, parent)?;
                Ok(snapshot)
            });
        match copied {
            Ok(snapshot) => self.publish_stage(fd, &stage, &snapshot, parent),
            Err(failure) => {
                drop(fd);
                Err(self.discard(&stage, failure))
            }
        }
    }

    fn create_stage(&self) -> Result<(OwnedFd, Stage), Failure> {
        let (name, (fd, created)) =
            stage::create_file(self.fs, self.dir).map_err(|error| match error {
                PlaceError::Io(error) => Failure::Os(error),
                error => Failure::Place(error),
            })?;
        Ok((fd, Stage { name, created }))
    }

    /// Closes the stage, settles its times and publishes it, discarding it on failure.
    fn publish_stage(
        &mut self,
        fd: OwnedFd,
        stage: &Stage,
        snapshot: &Snapshot,
        parent: &EntryStat,
    ) -> Result<(), Failure> {
        let settled = self
            .fs
            .close(fd)
            .map_err(Failure::Os)
            .and_then(|()| self.settle_times(stage, snapshot))
            .and_then(|()| self.check_cancelled());
        if let Err(failure) = settled {
            return Err(self.discard(stage, failure));
        }
        let placed = stage::publish(
            self.fs,
            self.dir,
            &stage.name,
            stage.created.identity,
            self.name,
        );
        match placed {
            Ok(_) => {}
            // Either name may hold the copy, so neither is removed.
            Err(PlaceError::Unconfirmed) => return Err(Failure::Place(PlaceError::Unconfirmed)),
            Err(PlaceError::Unsupported) => {
                if let Err(failure) = self.create_from_stage(stage, snapshot, parent) {
                    return Err(self.discard(stage, failure));
                }
                if let Err(error) = self.remove_stage(stage) {
                    return Err(Failure::Error(io_error(rust_i18n::t!(
                        "The item was copied, but its staging copy could not be removed: %{error}",
                        error = crate::services::gio_error_detail(&error)
                    ))));
                }
            }
            Err(error) => return Err(self.discard(stage, Failure::Place(error))),
        }
        self.progress.add_file();
        Ok(())
    }

    /// Where nothing can place the stage, creates the final name exclusively
    /// and fills it from the stage on the same device.
    fn create_from_stage(
        &mut self,
        stage: &Stage,
        snapshot: &Snapshot,
        parent: &EntryStat,
    ) -> Result<(), Failure> {
        let staged = Entry {
            dir: self.dir,
            name: &stage.name,
        };
        let (from, from_stat) = match self.fs.open_source(staged)? {
            Some((fd, stat)) if stat.identity == stage.created.identity => (fd, stat),
            _ => return Err(Failure::Changed),
        };
        let fd = self
            .fs
            .create_exclusive(self.target(), Mode::from_raw_mode(0o600))
            .map_err(|error| Failure::Place(placement::file_creation_error(error)))?;
        let created = self.fs.stat_fd(fd.as_fd())?;
        let filled = self
            .copy_data(from.as_fd(), &from_stat, fd.as_fd(), parent, false)
            .and_then(|()| self.apply_metadata(fd.as_fd(), &created, snapshot, parent))
            .and_then(|()| self.fs.close(fd).map_err(Failure::Os));
        if filled.is_err() {
            placement::remove_if_unchanged(
                self.fs,
                self.target(),
                created.identity,
                AtFlags::empty(),
            );
        }
        filled
    }

    /// Removes a stage this job created, unless something else took its name.
    fn remove_stage(&self, stage: &Stage) -> Result<(), glib::Error> {
        let entry = Entry {
            dir: self.dir,
            name: &stage.name,
        };
        match self.fs.stat(entry, true) {
            Ok(current) if current.identity == stage.created.identity => self
                .fs
                .unlink(entry, AtFlags::empty())
                .map_err(|error| io_error(os_error_detail(error))),
            Ok(_) => Err(placement_error(PlaceError::Changed)),
            Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(io_error(os_error_detail(error))),
        }
    }

    fn discard(&self, stage: &Stage, failure: Failure) -> Failure {
        match self.remove_stage(stage) {
            Ok(()) => failure,
            Err(cleanup) => Failure::Error(copy_failure_after_cleanup(
                failure.into_error(self.item),
                Err(cleanup),
            )),
        }
    }

    /// NFSv4 write delegations set the times at close, and FUSE daemons with
    /// a writeback cache and some SMB servers may too, so a closed stage gets
    /// its times again: always on NFSv4, and where a check finds them lost.
    fn settle_times(&self, stage: &Stage, snapshot: &Snapshot) -> Result<(), Failure> {
        let entry = Entry {
            dir: self.dir,
            name: &stage.name,
        };
        let lost = match self.target.family {
            Family::Nfs4 => true,
            Family::Fuse | Family::Smb => match self.fs.stat(entry, true) {
                Ok(current) => !snapshot.times_match(&current),
                Err(error) if is_fatal(error) => return Err(error.into()),
                Err(_) => false,
            },
            _ => false,
        };
        if !lost {
            return Ok(());
        }
        let reopened = match self.fs.open_source(entry) {
            Ok(Some((fd, stat))) if stat.identity == stage.created.identity => fd,
            Err(error) if is_fatal(error) => return Err(error.into()),
            _ => return Ok(()),
        };
        snapshot.apply_times(self.fs, reopened.as_fd())?;
        match self.fs.close(reopened) {
            Err(error) if is_fatal(error) => Err(error.into()),
            _ => Ok(()),
        }
    }
}
