# Filesystem compatibility

Strata works on local disks, removable media, network mounts (NFS, SMB/CIFS), FUSE filesystems
(sshfs, ntfs-3g, exfat-fuse, rclone, gvfsd-fuse), virtualization shares (9p, virtiofs, vboxsf)
and stacked filesystems (overlayfs, eCryptfs, mergerfs). They disagree about renames, links,
case, names and Trash. This document records what Strata guarantees on every mount, how local
file operations adapt to what a mount supports, and how to test that.

GVfs locations (`smb://`, `sftp://`, `mtp://`) are not covered here: their semantics come from
the GVfs backend, and Strata uses GIO for them.

## Guarantees

- **Never replace a concurrent arrival.** An item that appears at a destination while Strata is
  working is never overwritten, on any mount. Replace only ever removes the item the user
  confirmed, identified by device, inode, type and, where the filesystem reports it, birth time.
- **Never remove a name on a guess.** A name is unlinked only after Strata has proven which inode
  it holds. When the outcome of a step cannot be proven, both names are kept and the operation
  reports that the result could not be confirmed.
- **Never block the interface.** No filesystem call runs on the GTK main thread, including probes,
  mount lookups and waits. A hung NFS or FUSE server stalls the operation, not the window.
- **Move metadata, not data.** Renames inside one filesystem never copy file contents, whatever
  the mount supports. Moves copy only when crossing devices, or on a mount that offers neither
  `RENAME_NOREPLACE` nor the fallback step below for that item.

## Placing an item without replacing

Every local operation that puts an item at a new name — move, rename, undo, restore from Trash,
publishing a staged copy, archive extraction and creation — goes through one routine,
`placement::place` in `src/adapters/local_operations/placement.rs`. It tries these steps in order
and stops at the first that applies:

| Step | Applies to | Operation | Why it cannot replace |
| --- | --- | --- | --- |
| 1 | everything | `renameat2(RENAME_NOREPLACE)` | the kernel refuses an existing target |
| 2 | files, symlinks, special files | `linkat`, then remove the old name once the new one reports the item's identity | creating a link fails if the name exists, on the server for NFS and SMB |
| 3 | folders | create an empty placeholder folder with `mkdirat`, then rename over it | a rename can only replace an empty folder; a filled or swapped placeholder fails with `ENOTEMPTY` or `ENOTDIR` |
| 4 | — | copy, then delete | only for a cross-device move (`EXDEV`), or a move no step above can place |

`EEXIST` and `ENOTEMPTY` always mean the destination is taken. `EINVAL` from step 1 has three
possible meanings, and Strata never treats it as "copy instead":

- moving a folder into itself, which is checked before any fallback;
- an invalid name on this filesystem (for example `:` on vfat), when `RENAME_NOREPLACE` is known to
  work here or the fallback step also returns `EINVAL`;
- `RENAME_NOREPLACE` unsupported, otherwise.

Residual limits:

- ext4, ntfs3, NFS and SMB give a freed inode number straight to the next item created. Where no
  birth time is reported (NFSv3, ntfs-3g, exfat-fuse), an item deleted and recreated at the same
  name with the same type passes for the original.
- Undo and restore find an item Strata moved to Trash (by Delete, Replace or Merge) by original
  path, device and inode number, without birth time. If another item trashed from the same path got
  that inode number, they can bring back that item instead; they never overwrite anything.
- A file stage's identity is read by name once GIO's copy into it finishes, so a process that
  replaced that randomly named stage at any point during the copy would have its file treated as
  the copy.
- On SMB2/3, gvfsd-fuse and FUSE daemons that ignore rename flags, the no-replace check is made
  by the client kernel, so it protects against local processes but not against another client
  creating the same name at the same moment.
- On a case-insensitive SMB share without POSIX extensions, the Linux cifs client turns server
  inode numbers off for the whole mount once a folder is looked up under a second spelling, as the
  check before a case-only folder rename does ("Autodisabling the use of server inode numbers" in
  the kernel log). Hard links then report different inode numbers, so Strata stops using them
  there, as on sshfs; `RENAME_NOREPLACE` still places items there.
- If the reply to the placeholder `mkdir` is lost on NFS, an empty folder can remain at the
  destination name.

Renaming a folder in the lower layer of an overlayfs mount without `redirect_dir`, or moving
across bind mounts or btrfs subvolumes, returns `EXDEV`. Those moves copy, and the copy uses a
reflink where both sides share a btrfs or XFS filesystem.

## What a mount supports

Strata learns capabilities per mount instead of trusting the filesystem type. The cache lives in
`src/adapters/volume/mount_capabilities.rs`. It is keyed by the mount id from `statx`:
`STATX_MNT_ID_UNIQUE`, which is never reused, on Linux 6.8 and later, otherwise `STATX_MNT_ID` with
the device number. Entries are dropped whenever Strata's mount monitor reports a change.

- It records whether `RENAME_NOREPLACE`, `RENAME_EXCHANGE`, hard links and renaming over an empty
  folder work.
- A capability is recorded only from a definitive answer (`ENOSYS`, `EOPNOTSUPP`, or a rejected
  exchange), from Strata's own placeholder, or from a probe on Strata's own temporary names
  (`.strata-probe-*`) in the destination. An `EINVAL` or `EPERM` from placing a user's item never
  marks a mount as unsupported, because it can also mean an invalid name or a protected file.
- The cache orders the steps above, each safe on its own, so a stale entry can make an operation
  slower or refuse it. The exception is the last rename step under [Rename](#rename), which the
  cache alone enables: a stale entry can let a rename take it, with that step's small window.
- The common case costs nothing extra: step 1 always runs first, and the cache is read only after
  it fails.

## Replace and merge

Before copying anything, Replace chooses how to remove the original, in this order:

1. **Trash**, when the location supports it. Undo restores the original.
2. **Atomic exchange** (`RENAME_EXCHANGE`) on in-kernel local filesystems: ext4, XFS, btrfs, f2fs,
   tmpfs, bcachefs, OpenZFS 2.2 and later, and vfat on Linux 6.0 and later. It is never used on
   FUSE, where some daemons ignore the flag and overwrite the target. The filesystem type is read
   fresh with `fstatfs` for every decision.
3. **Set aside**: the original is placed at a hidden `.strata-replaced-<id>` name with the routine
   above, the new item is placed at the original name, and the set-aside original is removed by
   identity. If placing fails, the original is put back; if its name was taken meanwhile, it is
   kept and the error names it. The original's name is briefly absent, as it is with Trash.
4. Otherwise Replace fails before any data is copied.

Copies are written into a hidden `.strata-replacement-<id>` stage before they are placed. A file
stage is created by the copy itself, exclusively, at a fresh name, so nothing is ever copied over
a stage.

When Replace is part of a move within one filesystem, the source itself is placed after the
original is out of the way, so nothing is copied. Without Trash, a replaced original is gone for
good: the replacement is not added to Undo, because undoing it could only delete the remaining
copy. A replacing move keeps its Undo, which moves the item back.

Merge uses the same order for files it overwrites. Without Trash, the originals go into one hidden
`.strata-merge-<id>` folder inside the merge target, are put back if the merge fails, and are
removed after it succeeds.

## Rename

Local renames go through the same routine instead of GIO's check-then-rename.

- **Case-only renames** (`notes.txt` to `Notes.txt`) on case-insensitive folders — vfat, exFAT,
  SMB shares, casefolded ext4, f2fs and tmpfs folders, ntfs3 with `nocase` — are done in two steps
  through a hidden `.strata-rename-<id>` name, with rollback. A plain rename is a silent no-op
  there, and a no-replace rename reports the name as taken. The SMB client caches the lookup of
  the new spelling for about a second, so it is revalidated before the second step. exfat-fuse
  gives each spelling its own inode number, so there the new spelling counts as the item itself
  only when the folder lists no such name, the item's own name is the only one that matches it
  ignoring case, and type, size and modification time agree.
- **Mounts with neither `RENAME_NOREPLACE` nor usable hard links** (vboxsf, rclone, s3fs, sshfs),
  confirmed by a probe, and files that refuse a hard link on a mount without `RENAME_NOREPLACE`
  (for example under `fs.protected_hardlinks`): a file rename first reserves the new name with an
  exclusive empty file, then renames over it; a folder rename checks that the name is free, then
  renames. Only a process that deletes Strata's reservation in that instant could lose an item.
  Rename never refuses for lack of a primitive.
- Undo of a rename never copies.

## Trash

GIO decides where Trash is available. It reports Trash as unsupported for:

- tmpfs, overlayfs, eCryptfs, gvfsd-fuse and other mounts GLib treats as system-internal;
- mounts whose mountinfo root is not `/`, which includes bind mounts and btrfs `subvol=` mounts,
  and nested btrfs subvolumes that have no mount entry;
- mounts with `x-gvfs-notrash`;
- mount roots where `.Trash-$uid` cannot be created or would be owned by another user, which is
  common on NFS (root squash, read-only export roots), CIFS mounted with `uid=` and sshfs without
  idmap;
- bind mounts of the home filesystem, where the move into the home Trash fails with `EXDEV`.

There, Delete offers permanent deletion and Replace and Merge use the order above.

GLib 2.86.0 to 2.88.4 can report success without trashing anything on FUSE filesystems that do not
report entry types (GLib issue #4060). Strata therefore checks that a trashed local item is gone
before reporting success or recording Undo.

## Hidden names

| Name | Purpose | After a crash |
| --- | --- | --- |
| `.strata-replacement-*` | staging copy before publication; after an exchange, the swapped-out original until it is deleted | **may be the user's original** |
| `.strata-compression-*` | archive being written; after an exchange, the replaced archive | **may be the user's original** |
| `.strata-replaced-*` | original set aside by Replace | **may be the only copy of the user's item** |
| `.strata-merge-*` | originals set aside by Merge | **may hold the only copies** |
| `.strata-rename-*` | middle step of a case-only rename | **is the user's item** |
| `.strata-extraction-*` | archive extraction staging | extracted output; see [Archives](archives.md) |
| `.strata-probe-*` | capability probe | empty; safe to delete |

The prefixes live in `placement.rs`. Names that may hold user data are never deleted or renamed automatically.

## Filesystem notes

Rename and link support decides which placement step runs. "Rejects flags" means
`renameat2` returns `EINVAL` for both `RENAME_NOREPLACE` and `RENAME_EXCHANGE`.

| Filesystem | No-replace | Exchange | Hard links | Notes |
| --- | --- | --- | --- | --- |
| ext4, XFS, btrfs, f2fs, tmpfs, bcachefs | yes | yes | yes | btrfs renames across subvolumes return `EXDEV`; reflinks still work |
| OpenZFS 2.2 and later | yes | yes | yes | the loaded module version counts, not the userspace tools |
| OpenZFS 2.1 and earlier (Ubuntu 22.04, Debian 12) | rejects flags | rejects flags | yes | |
| vfat | yes | Linux 6.0 and later | no | case-insensitive; no symlinks; trailing dots are dropped |
| exFAT (kernel) | yes | no | no | case-insensitive; no symlinks |
| ntfs3, ntfs (Linux 7.1 and later) | yes | no | yes | case-sensitive unless mounted with `nocase` |
| ntfs-3g, libfuse2 daemons | rejects flags | rejects flags | ntfs-3g: yes | the kernel refuses rename flags below FUSE protocol 7.23 |
| exfat-fuse (libfuse3) | enforced by the kernel only | ignored: overwrites | no | |
| NFS v3 and v4 | rejects flags | rejects flags | yes | unlinking an open file leaves `.nfsXXXX` until it is closed |
| SMB/CIFS | client-side only | no | server-dependent | usually case-insensitive; open files may block rename and delete |
| sshfs | rejects flags | rejects flags | not used | each link reports its own inode number; reports "exists" as `EPERM` |
| 9p (WSL2, QEMU), ceph, eCryptfs | rejects flags | rejects flags | yes | |
| vboxsf | rejects flags | rejects flags | no | |
| mergerfs | rejects flags | rejects flags | within a branch | |
| overlayfs | yes | yes | yes | lower-layer folders cannot be renamed without `redirect_dir` |

libfuse high-level daemons without `use_ino`, sshfs among them, give each hard link its own inode
number, so a link cannot prove that it holds the item. Once a probe on Strata's own names confirms
this, Strata stops using links on that mount: moves copy, renames use the reserved-name step above,
and an archive file cannot be published there. The link is removed by the identity it reported
right after linking, if its type, permissions, size and modification time match the item's.
Otherwise, or without that confirmation, the new name may be a racer's item, so both names are
kept and the result is reported as unconfirmed.

Archive extraction never sets a symbolic link's time on FUSE, where sshfs applies it to the target.

## Testing

### Unit tests

Unit tests inject each quirk deterministically through the syscall seam in
`src/adapters/local_operations/fs_ops.rs`. A test installs a scope over its temporary directory:

```rust
let _quirks = FsQuirks::install(root.path(), Quirks::without_rename_flags());
```

Quirks emulate rejected rename flags (NFS, ntfs-3g), missing hard links (vfat, vboxsf), sshfs
collisions reported as `EPERM`, gvfsd-fuse refusing to rename over an empty folder, case-insensitive
lookups, invalid names, unsupported or lying Trash, reused inode numbers, hard links with their own
inode numbers, missing birth times, lost replies and racing processes. The seam also asserts that
no call runs on the main context's thread. Run them with:

```bash
./scripts/test-headless.py local_operations::
./scripts/test-headless.py volume::
```

### Real filesystems

`scripts/fslab/fslab.sh` is a manual lab that CI never runs. It starts one privileged Docker
container that mounts real kernel filesystems over loopback, then runs syscall probes or
`scripts/test-headless.py` with the test fixtures placed on each mount.

| Mount | Filesystem |
| --- | --- |
| `tmpfs`, `ext4` | controls |
| `ext4-casefold` | ext4 with a `chattr +F` folder |
| `vfat`, `exfat`, `ntfs3` | kernel drivers with udisks' default options |
| `exfat-fuse`, `ntfs-3g` | FUSE drivers |
| `nfs42`, `nfs3` | kernel NFS server and client over loopback |
| `cifs`, `cifs-nocase`, `cifs-posix` | Samba with the kernel SMB 3.1.1 client |
| `sshfs` | sshfs speaking SFTP to a loopback `sftp-server` that runs as the test user, in a mount namespace without the client mount |

```sh
scripts/fslab/fslab.sh up                              # build if needed, start, mount everything
scripts/fslab/fslab.sh probe [MOUNT...]                # capability matrix in target/fslab/probe/
scripts/fslab/fslab.sh test FILTER [MOUNT...]          # the working tree, including uncommitted changes
scripts/fslab/fslab.sh test --rev REV FILTER [MOUNT...]
scripts/fslab/fslab.sh down                            # always run when finished
```

`test` sets `STRATA_TEST_TMPDIR` to a directory on each mount. `test-headless.py` passes that
absolute path to the tests as `TMPDIR`, which `tempfile::tempdir()` honours; nothing else is
inherited. Results, logs and leftovers per mount go to `target/fslab/runs/`. The summary flags a
mount on which the kernel cifs client turned server inode numbers off during the run, because
failures after that point can be artefacts of that mount state.

Unit tests layer quirks on a fully featured filesystem, so on a limited mount some of them fail by
design: a test expecting a plain rename sees the ladder link instead on NFS, and fixtures with
symlinks, FIFOs or Unix permissions cannot exist on vfat. Read failures against a run of the same
filter on `origin/main`.

Requirements and safety:

- Docker on amd64, as a non-root user in the `docker` group, and the host modules `loop`, `vfat`,
  `exfat`, `ntfs3`, `nfsd`, `nfs`, `cifs` and `fuse`. The image builds on the verified E2E base
  from `scripts/e2e_base.py`.
- The container is privileged: it loads host modules and attaches loop devices in the host
  kernel. It has no network; its NFS, SMB and SFTP servers listen only on its own loopback.
  Crates are fetched first by a separate unprivileged container, and the build runs offline.
- Images live on a container-private tmpfs and are attached blank, so the host's udev never sees a
  filesystem. `up`, `probe` and `test` check this. Only udiskie ignores the images' directory, so
  stop other automounters (GNOME Disks, KDE) while the lab is up.
- The NFS exports and the sshfs server's folder are container-private tmpfs mounts, not loop
  devices. After full runs, Linux 7.2 kept a reference to the NFSv4.2 export after nfsd had
  stopped, with no client, thread or process left, which pinned the export's loop device and its
  memory on the host until reboot. A leaked tmpfs export pins only its few remaining files. tmpfs
  has no UUID, so each export sets `fsid=`; it reports birth times, which NFSv4.2 passes on.
- `down` releases everything that is free and detaches only loop devices still backed by the
  lab's own images, deleting each image once its device is gone. If a mount or loop device stays
  busy, `down` lists what holds it and keeps the container running with that mount, its server and
  its loop device; run `down` again once those are released. `down --force` unmounts lazily
  instead, which can keep loop devices attached on the host, or a tmpfs export's memory, until
  reboot.
- The container runs its own copy of `scripts/fslab`, refreshed by every `fslab.sh` command, so
  switching branches or rebasing while the lab is up cannot break `down`.
- The NFS, SMB and SFTP servers share the kernel with their clients, so there is no latency,
  reconnect or multi-client race. ZFS, 9p, virtiofs, eCryptfs and FUSE network filesystems other
  than sshfs are not covered.

Never point tests at real user shares or removable drives.
