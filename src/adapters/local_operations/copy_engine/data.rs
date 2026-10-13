// SPDX-License-Identifier: MIT

//! The data ladder: one clone, then `copy_file_range` in chunks, then reads
//! and writes. Progress and cancellation are checked between calls.

use std::{
    os::fd::BorrowedFd,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rustix::io::Errno;

use super::{Failure, Mount, is_fatal, is_unsupported};
use crate::adapters::{
    local_operations::{
        fs_ops::{EntryStat, FsOps},
        placement::ENOTSUPP,
    },
    volume::Family,
};

const MIB: usize = 1 << 20;

/// Learned from `ENOSYS`, for the rest of the process.
static COPY_RANGE_MISSING: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(super) fn forget_learned() {
    COPY_RANGE_MISSING.store(false, Ordering::Relaxed);
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Network {
    Nfs,
    Smb,
    Fuse,
}

fn network(family: Family) -> Option<Network> {
    match family {
        Family::Nfs | Family::Nfs4 => Some(Network::Nfs),
        Family::Smb => Some(Network::Smb),
        Family::Fuse => Some(Network::Fuse),
        _ => None,
    }
}

fn kernel() -> (u32, u32, u32) {
    static KERNEL: OnceLock<(u32, u32, u32)> = OnceLock::new();
    *KERNEL.get_or_init(|| {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        let mut numbers = release
            .trim()
            .split(|character: char| !character.is_ascii_digit())
            .map(|number| number.parse().unwrap_or(0));
        (
            numbers.next().unwrap_or(0),
            numbers.next().unwrap_or(0),
            numbers.next().unwrap_or(0),
        )
    })
}

/// Whether the kernel has the client's fixes for stale caches after
/// server-side copies.
fn has_copy_fixes(network: Network) -> bool {
    let kernel = kernel();
    match network {
        Network::Nfs => kernel >= (5, 9, 0),
        Network::Fuse => kernel >= (5, 8, 0),
        // The December 2023 fixes for flushing, invalidation and file size:
        // 6.7, 6.6.7, and 6.1.68 with its backport regression fixed in 6.1.73.
        Network::Smb => {
            kernel >= (6, 7, 0)
                || ((6, 6, 7)..(6, 7, 0)).contains(&kernel)
                || ((6, 1, 73)..(6, 2, 0)).contains(&kernel)
        }
    }
}

/// Which steps of the ladder may run for one source and target.
pub(super) struct Route {
    clone: bool,
    /// The largest `copy_file_range` request, where it may run at all.
    copy_range: Option<usize>,
}

impl Route {
    pub(super) fn new(
        fs: &dyn FsOps,
        target: &Mount,
        source: BorrowedFd<'_>,
        source_stat: &EntryStat,
        parent: &EntryStat,
    ) -> Self {
        let target_network = network(target.family);
        // It stops at the reported size, which virtual files leave at 0
        // whatever they hold.
        let copy_range = source_stat.size > 0
            && match target_network {
                None => source_stat.identity.device == parent.identity.device,
                Some(target_network) => {
                    has_copy_fixes(target_network)
                        && network(Mount::new(fs, source).family) == Some(target_network)
                }
            }
            && !COPY_RANGE_MISSING.load(Ordering::Relaxed);
        Self {
            clone: source_stat.size > 0
                && target.capabilities().reflink != Some(false)
                && (target_network != Some(Network::Smb) || has_copy_fixes(Network::Smb)),
            // A larger NFS request becomes an asynchronous COPY that costs
            // seconds whatever its size.
            copy_range: copy_range.then_some(if target_network == Some(Network::Nfs) {
                2 * MIB
            } else {
                64 * MIB
            }),
        }
    }
}

enum Ranged {
    Done,
    /// Reads and writes continue from this offset.
    From(u64),
}

#[derive(Default)]
pub(super) struct Data {
    buffer: Vec<u8>,
    /// A small request took over a second, as on NFS with a small rsize.
    copy_range_slow: bool,
}

impl Data {
    #[expect(
        clippy::too_many_arguments,
        reason = "one ladder step per argument group, all borrowed from the job"
    )]
    pub(super) fn copy(
        &mut self,
        fs: &dyn FsOps,
        cancelled: &AtomicBool,
        target_mount: &Mount,
        route: &Route,
        (source, target): (BorrowedFd<'_>, BorrowedFd<'_>),
        size: u64,
        mut progress: impl FnMut(u64),
    ) -> Result<(), Failure> {
        if route.clone {
            match fs.reflink(target, source) {
                Ok(()) => {
                    progress(size);
                    return Ok(());
                }
                // Writeback may have reverted the page cache, so a fallback
                // could read wrong bytes.
                Err(error) if is_fatal(error) => return Err(error.into()),
                Err(error) => {
                    if is_unsupported(error) {
                        target_mount.learn(|capabilities| capabilities.reflink = Some(false));
                    }
                }
            }
        }
        let mut offset = 0;
        if let Some(largest) = route.copy_range.filter(|_| !self.copy_range_slow) {
            match self.copy_ranges(
                fs,
                cancelled,
                (source, target),
                size,
                largest,
                &mut progress,
            )? {
                Ranged::Done => return Ok(()),
                Ranged::From(copied) => offset = copied,
            }
        }
        self.read_write(
            fs,
            cancelled,
            (source, target),
            (offset, size),
            &mut progress,
        )
    }

    fn copy_ranges(
        &mut self,
        fs: &dyn FsOps,
        cancelled: &AtomicBool,
        (source, target): (BorrowedFd<'_>, BorrowedFd<'_>),
        size: u64,
        largest: usize,
        progress: &mut impl FnMut(u64),
    ) -> Result<Ranged, Failure> {
        let mut offset = 0;
        let mut chunk = MIB.min(largest);
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(Failure::Cancelled);
            }
            let started = Instant::now();
            let copied = match fs.copy_range(source, offset, target, offset, chunk) {
                Ok(copied) => copied,
                Err(Errno::INTR) => continue,
                Err(Errno::NOSYS) => {
                    COPY_RANGE_MISSING.store(true, Ordering::Relaxed);
                    return Ok(Ranged::From(offset));
                }
                Err(
                    Errno::XDEV
                    | Errno::OPNOTSUPP
                    | ENOTSUPP
                    | Errno::INVAL
                    | Errno::BADF
                    | Errno::TXTBSY
                    | Errno::PERM
                    | Errno::ACCESS
                    | Errno::NOENT,
                ) => return Ok(Ranged::From(offset)),
                Err(error) => return Err(error.into()),
            };
            let elapsed = started.elapsed();
            if copied == 0 {
                // A read confirms an early end.
                return Ok(if offset >= size {
                    Ranged::Done
                } else {
                    Ranged::From(offset)
                });
            }
            offset += copied as u64;
            progress(copied as u64);
            if offset >= size {
                return Ok(Ranged::Done);
            }
            if elapsed > Duration::from_secs(1) && chunk <= 2 * MIB {
                self.copy_range_slow = true;
                return Ok(Ranged::From(offset));
            }
            if elapsed > Duration::from_millis(250) {
                chunk = (chunk / 2).max(64 * 1024);
            } else if copied == chunk && elapsed < Duration::from_millis(100) {
                chunk = (chunk * 2).min(largest);
            }
        }
    }

    fn read_write(
        &mut self,
        fs: &dyn FsOps,
        cancelled: &AtomicBool,
        (source, target): (BorrowedFd<'_>, BorrowedFd<'_>),
        (mut offset, size): (u64, u64),
        progress: &mut impl FnMut(u64),
    ) -> Result<(), Failure> {
        // Sized by the file, so copying a small one maps no large buffer.
        let wanted = usize::try_from(size).map_or(MIB, |size| size.clamp(64 * 1024, MIB));
        if self.buffer.len() < wanted {
            self.buffer = vec![0; wanted];
        }
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(Failure::Cancelled);
            }
            let read = match fs.read_at(source, &mut self.buffer, offset) {
                Ok(read) => read,
                Err(Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            };
            if read == 0 {
                return Ok(());
            }
            let mut written = 0;
            while written < read {
                match fs.write_at(target, &self.buffer[written..read], offset + written as u64) {
                    Ok(0) => return Err(Errno::IO.into()),
                    Ok(count) => written += count,
                    Err(Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            offset += read as u64;
            progress(read as u64);
        }
    }
}
