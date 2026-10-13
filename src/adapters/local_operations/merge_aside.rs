// SPDX-License-Identifier: MIT

//! Where Trash is unavailable, a merge moves each original it overwrites
//! into one hidden folder inside the merge target, under its relative path,
//! and deletes that folder only once the merge has succeeded. An original
//! whose name the merge left free goes back to it first.

use std::{
    ffi::OsString,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    sync::Arc,
};

use gtk::{gio, glib};
use rustix::{
    fs::{AtFlags, Mode, OFlags, ResolveFlags},
    io::Errno,
};

use super::{
    LocalFileIdentity, PinnedEntry, flush_removable_destination,
    fs_ops::{Entry, fs},
    io_error, open_local_parent_directory, parallel_delete_local,
    placement::{self, Feasibility, MERGE_PREFIX, PlaceError},
    placement_reason, retry_local_open, run_blocking, was_cancelled,
};

struct Folder {
    name: OsString,
    identity: LocalFileIdentity,
    handle: Arc<OwnedFd>,
}

struct Mirror {
    relative: PathBuf,
    parent: Arc<OwnedFd>,
    name: OsString,
    handle: Arc<OwnedFd>,
}

struct Moved {
    parent: Arc<OwnedFd>,
    aside_parent: Arc<OwnedFd>,
    name: OsString,
    identity: LocalFileIdentity,
}

pub(super) struct MergeAside {
    target_path: PathBuf,
    target: Option<Arc<OwnedFd>>,
    folder: Option<Folder>,
    /// In creation order, so parents precede their children.
    mirrors: Vec<Mirror>,
    moved: Vec<Moved>,
}

/// Originals left in the folder after a failed merge.
struct Kept {
    folder: PathBuf,
    reason: PlaceError,
}

fn could_not_set_aside(name: &str, error: PlaceError) -> glib::Error {
    io_error(rust_i18n::t!(
        "Could not move “%{name}” out of the way before merging: %{error}",
        name = name,
        error = crate::services::error_detail(placement_reason(error))
    ))
}

fn open_beneath(dir: &OwnedFd, relative: &Path) -> Result<OwnedFd, PlaceError> {
    retry_local_open(|| {
        rustix::fs::openat2(
            dir,
            relative,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_XDEV,
        )
    })
    .map_err(|error| match error {
        Errno::XDEV => PlaceError::CrossDevice,
        error => PlaceError::Io(error),
    })
}

fn create_folder(target: &OwnedFd) -> Result<Folder, PlaceError> {
    let (name, identity) = placement::reserve_hidden_name(MERGE_PREFIX, |name| {
        let entry = Entry::new(target, name);
        fs().mkdir(entry, Mode::from_raw_mode(0o700))
            .map_err(placement::creation_error)?;
        fs().stat(entry, false)
            .map(|stat| stat.identity)
            .map_err(PlaceError::Io)
    })
    .map_err(|error| match error {
        PlaceError::InvalidName => PlaceError::Unsupported,
        error => error,
    })?;
    let opened = open_beneath(target, Path::new(&name)).and_then(|handle| {
        match fs().stat_fd(handle.as_fd()) {
            Ok(stat) if stat.identity == identity => Ok(handle),
            Ok(_) => Err(PlaceError::Changed),
            Err(error) => Err(PlaceError::Io(error)),
        }
    });
    match opened {
        Ok(handle) => Ok(Folder {
            name,
            identity,
            handle: Arc::new(handle),
        }),
        Err(error) => {
            placement::remove_if_unchanged(
                fs(),
                Entry::new(target, &name),
                identity,
                AtFlags::REMOVEDIR,
            );
            Err(error)
        }
    }
}

impl MergeAside {
    fn new(target_path: PathBuf) -> Self {
        Self {
            target_path,
            target: None,
            folder: None,
            mirrors: Vec::new(),
            moved: Vec::new(),
        }
    }

    fn used(&self) -> bool {
        self.folder.is_some()
    }

    fn folder_path(&self) -> Option<PathBuf> {
        self.folder
            .as_ref()
            .map(|folder| self.target_path.join(&folder.name))
    }

    fn target(&mut self) -> Result<Arc<OwnedFd>, glib::Error> {
        if let Some(target) = &self.target {
            return Ok(target.clone());
        }
        let target = Arc::new(open_local_parent_directory(&self.target_path).map_err(io_error)?);
        self.target = Some(target.clone());
        Ok(target)
    }

    /// Moves the file at `path`, inside the merge target, into the folder.
    fn set_aside(&mut self, path: &Path, name: &str) -> Result<(), glib::Error> {
        let failed = |error| could_not_set_aside(name, error);
        let (Some(relative), Some(file_name)) = (
            path.strip_prefix(&self.target_path)
                .ok()
                .and_then(Path::parent),
            path.file_name(),
        ) else {
            return Err(failed(PlaceError::Changed));
        };
        let target = self.target()?;
        if self.folder.is_none() {
            match placement::can_set_aside(fs(), target.as_fd(), false) {
                Ok(Feasibility::Yes) => {}
                Ok(Feasibility::No) => return Err(failed(PlaceError::Unsupported)),
                Err(error) => return Err(failed(error)),
            }
            self.folder = Some(create_folder(&target).map_err(failed)?);
        }
        let parent = if relative.as_os_str().is_empty() {
            target
        } else {
            Arc::new(open_beneath(&target, relative).map_err(failed)?)
        };
        let aside_parent = self.mirror(relative).map_err(failed)?;
        let entry = Entry::new(&*parent, file_name);
        let original = fs()
            .stat(entry, false)
            .map_err(|error| failed(PlaceError::Io(error)))?;
        if original.is_directory() {
            return Err(failed(PlaceError::Changed));
        }
        let identity = placement::move_aside(
            fs(),
            entry,
            Entry::new(&*aside_parent, file_name),
            original.identity,
        )
        .map_err(failed)?;
        self.moved.push(Moved {
            parent,
            aside_parent,
            name: file_name.to_os_string(),
            identity,
        });
        Ok(())
    }

    /// The folder's copy of the original's parent directory.
    fn mirror(&mut self, relative: &Path) -> Result<Arc<OwnedFd>, PlaceError> {
        let mut current = self
            .folder
            .as_ref()
            .map(|folder| folder.handle.clone())
            .ok_or(PlaceError::Changed)?;
        let mut walked = PathBuf::new();
        for component in relative.iter() {
            walked.push(component);
            if let Some(mirror) = self.mirrors.iter().find(|mirror| mirror.relative == walked) {
                current = mirror.handle.clone();
                continue;
            }
            let entry = Entry::new(&*current, component);
            fs().mkdir(entry, Mode::from_raw_mode(0o700))
                .map_err(placement::creation_error)?;
            let handle = match open_beneath(&current, Path::new(component)) {
                Ok(handle) => Arc::new(handle),
                Err(error) => {
                    let _ = fs().unlink(entry, AtFlags::REMOVEDIR);
                    return Err(error);
                }
            };
            self.mirrors.push(Mirror {
                relative: walked.clone(),
                parent: current,
                name: component.to_os_string(),
                handle: handle.clone(),
            });
            current = handle;
        }
        Ok(current)
    }

    /// Moves every original back, newest first, and removes the emptied
    /// folder. An original whose name was taken meanwhile stays in it.
    fn put_back(&mut self) -> Result<(), Kept> {
        let mut failure = None;
        for moved in std::mem::take(&mut self.moved).into_iter().rev() {
            if let Err(error) = placement::place(
                fs(),
                Entry::new(&*moved.aside_parent, &moved.name),
                Entry::new(&*moved.parent, &moved.name),
                Some(moved.identity),
            ) {
                failure.get_or_insert(error);
            }
        }
        for mirror in self.mirrors.drain(..).rev() {
            let _ = fs().unlink(
                Entry::new(&*mirror.parent, &mirror.name),
                AtFlags::REMOVEDIR,
            );
        }
        let (Some(target), Some(folder)) = (&self.target, &self.folder) else {
            return Ok(());
        };
        let entry = Entry::new(&**target, &folder.name);
        if placement::remove_if_unchanged(fs(), entry, folder.identity, AtFlags::REMOVEDIR)
            || fs().stat(entry, false).err() == Some(Errno::NOENT)
        {
            return Ok(());
        }
        Err(Kept {
            folder: self.target_path.join(&folder.name),
            reason: failure.unwrap_or(PlaceError::Unconfirmed),
        })
    }

    /// Moves each original whose name the merge left free back to it: the
    /// incoming item was left out, or kept at its source by a move.
    fn put_back_unreplaced(&mut self) -> Result<(), PlaceError> {
        let mut failure = Ok(());
        self.moved.retain(|moved| {
            let original = Entry::new(&*moved.parent, &moved.name);
            if fs().stat(original, false).err() != Some(Errno::NOENT) {
                return true;
            }
            let aside = Entry::new(&*moved.aside_parent, &moved.name);
            match placement::place(fs(), aside, original, Some(moved.identity)) {
                Ok(_) => false,
                Err(PlaceError::Exists) => true,
                Err(error) => {
                    failure = failure.and(Err(error));
                    true
                }
            }
        });
        failure
    }

    /// Deletes the folder and every original in it once the merge is on
    /// disk, flushing removable media first.
    async fn remove(self) -> Result<(), glib::Error> {
        let (Some(target), Some(folder)) = (self.target, self.folder) else {
            return Ok(());
        };
        let path = self.target_path.join(&folder.name);
        let removed = async {
            flush_removable_destination(&self.target_path).await?;
            parallel_delete_local(
                vec![PinnedEntry {
                    parent: target,
                    name: folder.name,
                    identity: Some(folder.identity),
                }],
                gio::Cancellable::new(),
            )
            .await
        }
        .await;
        removed.map_err(|error| {
            io_error(rust_i18n::t!(
                "The folders were merged, but the replaced originals could not be removed from “%{folder}”: %{error}",
                folder = path.display(),
                error = crate::services::gio_error_detail(&error)
            ))
        })
    }
}

/// Sets the original at `path` aside off the main thread, starting the
/// folder in `root` on first use.
pub(super) async fn set_aside(
    aside: &mut Option<MergeAside>,
    root: &Path,
    path: &Path,
    name: String,
) -> Result<(), glib::Error> {
    let mut step = aside
        .take()
        .unwrap_or_else(|| MergeAside::new(root.to_path_buf()));
    let path = path.to_path_buf();
    let (step, result) = run_blocking(move || {
        let result = step.set_aside(&path, &name);
        Ok((step, result))
    })
    .await?;
    *aside = Some(step);
    result
}

/// Undoes the asides of a failed merge, naming the folder that keeps any
/// original that could not be put back.
pub(super) async fn abandon(aside: Option<MergeAside>, error: glib::Error) -> glib::Error {
    let Some(mut aside) = aside.filter(MergeAside::used) else {
        return error;
    };
    let folder = aside.folder_path().unwrap_or_default();
    let kept = match gio::spawn_blocking(move || aside.put_back()).await {
        Ok(Ok(())) => return error,
        Ok(Err(kept)) => kept,
        Err(_) => Kept {
            folder,
            reason: PlaceError::Unconfirmed,
        },
    };
    let reason = if was_cancelled(&error) {
        crate::services::error_detail(placement_reason(kept.reason))
    } else {
        crate::services::gio_error_detail(&error)
    };
    io_error(rust_i18n::t!(
        "The merge stopped. Originals that could not be put back are in “%{folder}”: %{error}",
        folder = kept.folder.display(),
        error = reason
    ))
}

/// Deletes the originals of a successful merge, except those it left
/// nothing in place of. Returns whether the merged target was flushed on
/// the way.
pub(super) async fn finish(aside: Option<MergeAside>) -> Result<bool, glib::Error> {
    let Some(mut aside) = aside.filter(MergeAside::used) else {
        return Ok(false);
    };
    let (aside, put_back) = run_blocking(move || {
        let put_back = aside.put_back_unreplaced();
        Ok((aside, put_back))
    })
    .await?;
    if let Err(error) = put_back {
        return Err(io_error(rust_i18n::t!(
            "The folders were merged, but the replaced originals could not be removed from “%{folder}”: %{error}",
            folder = aside.folder_path().unwrap_or_default().display(),
            error = crate::services::error_detail(placement_reason(error))
        )));
    }
    aside.remove().await.map(|()| true)
}
