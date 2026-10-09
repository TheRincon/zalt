//! Files on disk: archiving a tree with its metadata, and extracting one under
//! §13's rules. Needs the `native-fs` feature.
//!
//! ## Extraction never acts on a path it checked earlier
//!
//! Every entry is created relative to an **open directory descriptor**, one
//! path component at a time:
//!
//! - The destination is opened once, as a `cap_std::fs::Dir`. Each directory
//!   below it is opened from its parent's descriptor with `O_NOFOLLOW`
//!   (`cap_fs_ext::DirExt::open_dir_nofollow`), so a symlink — planted before
//!   extraction or swapped in during it — is refused at the component where
//!   it sits, never followed.
//! - The entry itself is then created by a single name in that descriptor:
//!   `O_CREAT | O_EXCL` for files, `symlinkat`, `linkat`, `renameat`. There is
//!   no path left to resolve, so there is nothing to race. `tests/fs.rs` swaps
//!   a directory for a symlink in a tight loop during extraction to show it,
//!   with a path-based control that the same loop does break.
//! - Metadata goes through the **file's own descriptor**: `fchmod`, `fchown`,
//!   `futimens`, `fsetxattr`. Flags and ACLs have only path-based calls on
//!   macOS, so they are applied through `/dev/fd/N`, which the kernel resolves
//!   to the descriptor's vnode — a file renamed away from its name still
//!   receives them (verified; see `flags_and_acls_follow_the_descriptor`).
//!
//! The one step that cannot be descriptor-relative on macOS is creating a
//! FIFO or device node: the system has `mkfifoat` and `mknodat` since macOS
//! 13, but neither `nix` nor `rustix` exposes them there, and this crate is
//! `forbid(unsafe_code)`. Those nodes are made under an unguessable temporary
//! name by path, then found through the parent's descriptor — or extraction
//! stops — and renamed into place by `renameat`. A swap in that window can
//! make an empty node appear elsewhere under a random name; it cannot make
//! one appear under a name the archive chose.
//!
//! ## The rest of §13
//!
//! - **Paths** are validated from the index at open and again from the tar
//!   header when the member is read.
//! - **Links**: symlinks are created last, so nothing this extraction writes
//!   passes through one it made; a hardlink's target is a validated archive
//!   path that must already be a regular file this extraction made.
//! - **Special files**: device nodes only on [`ExtractOptions::devices`];
//!   setuid, setgid and sticky bits only on [`ExtractOptions::keep_setid`].
//! - **Resource limits**: the total size is checked before the first byte is
//!   written; xattr values are bounded by [`ReadLimits::max_header_bytes`].
//!
//! [`ReadLimits::max_header_bytes`]: crate::ReadLimits::max_header_bytes

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::index::{Entry, Kind, validate_path};
use crate::meta;
use crate::read::{Archive, Source};
use crate::tar::{Extras, Member};
use crate::write::Writer;
use crate::{Error, Result};

fn io(at: &Path, e: std::io::Error) -> Error {
    Error::Io(at.display().to_string(), e)
}

/// A time as the index records it: whole seconds and nanoseconds, with
/// times before 1970 as negative seconds plus a positive fraction.
fn split_time(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(before) => {
            let d = before.duration();
            match d.subsec_nanos() {
                0 => (-(d.as_secs() as i64), 0),
                n => (-(d.as_secs() as i64) - 1, 1_000_000_000 - n),
            }
        }
    }
}

/// The inverse of [`split_time`], saturating rather than panicking: the value
/// comes from the archive, and `SystemTime`'s operators panic on overflow,
/// which would make a hostile timestamp a way to crash the reader.
fn join_time(sec: i64, nsec: u32) -> SystemTime {
    let nanos = Duration::from_nanos(u64::from(nsec));
    let whole = Duration::from_secs(sec.unsigned_abs());
    let base = if sec >= 0 {
        UNIX_EPOCH.checked_add(whole)
    } else {
        UNIX_EPOCH.checked_sub(whole)
    };
    base.and_then(|t| t.checked_add(nanos))
        .unwrap_or(UNIX_EPOCH)
}

// ------------------------------------------------------------------ create

/// What [`add_tree_with`] records beyond the basics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateOptions {
    /// Extended attributes (§9.2). `com.apple.decmpfs` is never recorded.
    pub xattrs: bool,
    /// ACLs, as `SCHILY.acl.ace` (macOS).
    pub acls: bool,
    /// BSD file flags (macOS).
    pub fflags: bool,
    /// Record `com.apple.provenance`. Off by default: it names the process
    /// that created the file on *this* machine, which means nothing anywhere
    /// else (§9.3 leaves it to writer policy).
    pub keep_provenance: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            xattrs: true,
            acls: true,
            fflags: true,
            keep_provenance: false,
        }
    }
}

/// The archive name a filesystem path is stored under: its normal components
/// joined with `/`, the way `tar` stores what it was given — a leading `/` or
/// `./` dropped, `..` refused.
pub fn archive_name(path: &Path) -> Result<Vec<u8>> {
    let mut name = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(part) => {
                if !name.is_empty() {
                    name.push(b'/');
                }
                name.extend_from_slice(part.as_bytes());
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                return Err(Error::Refused(format!(
                    "{} has a `..` component; archive it from its parent instead",
                    path.display()
                )));
            }
        }
    }
    validate_path(&name)?;
    Ok(name)
}

/// [`add_tree_with`] and the default [`CreateOptions`].
pub fn add_tree<W: Write>(
    writer: &mut Writer<W>,
    path: &Path,
    warn: &mut dyn FnMut(String),
) -> Result<()> {
    add_tree_with(writer, path, &CreateOptions::default(), warn)
}

/// Archive `path` and, if it is a directory, everything under it.
///
/// Entries are added in byte-wise path order, a directory before its contents
/// — §9.4's ordering and §12.4's condition for reproducible output; xattrs
/// are recorded in byte-wise name order for the same reason. Symlinks are
/// stored, not followed. A file with several links is stored once and
/// referred to by hardlink members after that (§9.4). Sockets are skipped and
/// reported through `warn`, as is metadata that could not be read.
pub fn add_tree_with<W: Write>(
    writer: &mut Writer<W>,
    path: &Path,
    options: &CreateOptions,
    warn: &mut dyn FnMut(String),
) -> Result<()> {
    let name = archive_name(path)?;
    let mut walk = Walk {
        options,
        links: HashMap::new(),
        users: HashMap::new(),
        groups: HashMap::new(),
        warn,
    };
    walk.add(writer, path, name)
}

struct Walk<'a> {
    options: &'a CreateOptions,
    links: HashMap<(u64, u64), Vec<u8>>,
    users: HashMap<u32, Vec<u8>>,
    groups: HashMap<u32, Vec<u8>>,
    warn: &'a mut dyn FnMut(String),
}

impl Walk<'_> {
    fn add<W: Write>(&mut self, writer: &mut Writer<W>, path: &Path, name: Vec<u8>) -> Result<()> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| io(path, e))?;
        let kind = meta.file_type();
        let mut entry = Entry::new(Kind::File, name.clone());
        entry.mode = meta.mode() & 0o7777;
        entry.uid = u64::from(meta.uid());
        entry.gid = u64::from(meta.gid());
        entry.mtime_sec = meta.mtime();
        entry.mtime_nsec = meta.mtime_nsec().clamp(0, 999_999_999) as u32;
        entry.btime = meta.created().ok().map(split_time);
        entry.names = Some(self.names(meta.uid(), meta.gid()));
        #[cfg(target_os = "macos")]
        if self.options.fflags {
            entry.fflags = Some(u64::from(std::os::macos::fs::MetadataExt::st_flags(&meta)));
        }

        if kind.is_dir() {
            entry.kind = Kind::Directory;
            let extras = self.extras(path, None)?;
            writer.add_with(entry, &extras, &mut std::io::empty())?;
            let mut children: Vec<_> = std::fs::read_dir(path)
                .map_err(|e| io(path, e))?
                .map(|d| d.map(|d| d.file_name()))
                .collect::<std::io::Result<_>>()
                .map_err(|e| io(path, e))?;
            // Byte-wise, not locale or Unicode order: the order must not depend
            // on anything but the bytes, or two machines archive one tree
            // differently.
            children.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            for child in children {
                let mut child_name = name.clone();
                child_name.push(b'/');
                child_name.extend_from_slice(child.as_bytes());
                self.add(writer, &path.join(&child), child_name)?;
            }
            return Ok(());
        }
        if kind.is_symlink() {
            entry.kind = Kind::Symlink;
            let target = std::fs::read_link(path).map_err(|e| io(path, e))?;
            entry.link = Some(target.as_os_str().as_bytes().to_vec());
            let extras = self.extras(path, None)?;
            return writer.add_with(entry, &extras, &mut std::io::empty());
        }
        if kind.is_file() {
            if meta.nlink() > 1 {
                let key = (meta.dev(), meta.ino());
                if let Some(first) = self.links.get(&key) {
                    // The inode's metadata was recorded with its first name.
                    entry.kind = Kind::HardLink;
                    entry.link = Some(first.clone());
                    entry.btime = None;
                    entry.fflags = None;
                    return writer.add(entry, &mut std::io::empty());
                }
                self.links.insert(key, name);
            }
            entry.size = meta.len();
            let mut file = std::fs::File::open(path).map_err(|e| io(path, e))?;
            let extras = self.extras(path, Some(&file))?;
            return writer.add_with(entry, &extras, &mut file);
        }
        if kind.is_fifo() || kind.is_char_device() || kind.is_block_device() {
            entry.kind = if kind.is_fifo() {
                Kind::Fifo
            } else if kind.is_char_device() {
                Kind::CharDevice
            } else {
                Kind::BlockDevice
            };
            if !kind.is_fifo() {
                match device_numbers(meta.rdev()) {
                    Some(d) => entry.device = Some(d),
                    None => {
                        (self.warn)(format!(
                            "{}: skipped — device numbers are not decoded on this platform",
                            path.display()
                        ));
                        return Ok(());
                    }
                }
            }
            // Opening a FIFO to read its xattrs would block; none are taken.
            return writer.add(entry, &mut std::io::empty());
        }
        (self.warn)(format!(
            "{}: skipped — sockets are not archived",
            path.display()
        ));
        Ok(())
    }

    /// Owner and group names, looked up once per id. An id with no name
    /// records an empty name, as tar does.
    fn names(&mut self, uid: u32, gid: u32) -> (Vec<u8>, Vec<u8>) {
        let user = self
            .users
            .entry(uid)
            .or_insert_with(|| {
                nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
                    .ok()
                    .flatten()
                    .map(|u| u.name.into_bytes())
                    .unwrap_or_default()
            })
            .clone();
        let group = self
            .groups
            .entry(gid)
            .or_insert_with(|| {
                nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid))
                    .ok()
                    .flatten()
                    .map(|g| g.name.into_bytes())
                    .unwrap_or_default()
            })
            .clone();
        (user, group)
    }

    /// The xattrs and ACL of `path`, read through `file` when it is open and
    /// never through a symlink.
    fn extras(&mut self, path: &Path, file: Option<&std::fs::File>) -> Result<Extras> {
        let mut extras = Extras::default();
        if self.options.xattrs {
            extras.xattrs = self.xattrs(path, file)?;
        }
        #[cfg(target_os = "macos")]
        if self.options.acls {
            extras.acl = macos::read_acl(path, self.warn);
        }
        Ok(extras)
    }

    fn xattrs(
        &mut self,
        path: &Path,
        file: Option<&std::fs::File>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let at = match file {
            Some(f) => XattrAt::File(f),
            None => XattrAt::Path(path),
        };
        let mut names = match list_xattrs(at) {
            Ok(names) => names,
            Err(e) if unsupported(&e) => return Ok(Vec::new()),
            Err(e) => return Err(io(path, e)),
        };
        names.sort();
        let mut out = Vec::with_capacity(names.len());
        let mut budget = MAX_XATTR_BYTES;
        for name in names {
            if name == meta::XATTR_DECMPFS
                || (name == meta::XATTR_PROVENANCE && !self.options.keep_provenance)
            {
                continue;
            }
            if !meta::xattr_name_is_portable(&name) {
                (self.warn)(format!(
                    "{}: xattr {:?} skipped — its name cannot be a pax keyword",
                    path.display(),
                    String::from_utf8_lossy(&name)
                ));
                continue;
            }
            match get_xattr(at, &name, budget) {
                Ok(Xattr::Value(v)) => {
                    budget -= v.len() as u64;
                    out.push((name, v));
                }
                Ok(Xattr::TooLarge) => (self.warn)(format!(
                    "{}: xattr {:?} skipped — over the {} MiB of xattrs one entry may carry",
                    path.display(),
                    String::from_utf8_lossy(&name),
                    MAX_XATTR_BYTES >> 20
                )),
                Ok(Xattr::Missing) => {} // removed since it was listed
                Err(e) => return Err(io(path, e)),
            }
        }
        Ok(out)
    }
}

/// Where xattrs are read from: an open file, or a path whose last component
/// is never followed.
#[derive(Clone, Copy)]
enum XattrAt<'a> {
    File(&'a std::fs::File),
    Path(&'a Path),
}

/// Most bytes of xattr values one archived entry carries. They are read
/// whole and written into one pax header, and the reader refuses headers over
/// [`ReadLimits::max_header_bytes`] (64 MiB by default); staying under it
/// means `add_tree` never writes a member its own reader would refuse.
///
/// [`ReadLimits::max_header_bytes`]: crate::ReadLimits::max_header_bytes
pub const MAX_XATTR_BYTES: u64 = 48 << 20;

/// Ask for a list or value with `call`: its size first (an empty buffer is a
/// size query on macOS and Linux both), then the read, then the size again.
///
/// The second size check catches both ways a value can outgrow its buffer:
/// ordinary xattrs fail with `ERANGE`, but macOS reads
/// `com.apple.ResourceFork` like a file and *succeeds* short. Assuming the
/// first — as the `xattr` crate does, with a 4 KiB first try — silently
/// truncates every resource fork. A value still changing after a few tries,
/// or a read that comes back short of a size that did not change, is an
/// error rather than a truncated value or an endless loop.
///
/// `None` if the size is over `max`, before anything that large is allocated.
fn sized_read(
    max: u64,
    mut call: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
) -> std::io::Result<Option<Vec<u8>>> {
    for _ in 0..4 {
        let size = call(&mut [])?;
        if size as u64 > max {
            return Ok(None);
        }
        let mut buf = vec![0u8; size];
        let n = match call(&mut buf) {
            Ok(n) => n,
            Err(rustix::io::Errno::RANGE) => continue, // grew between the calls
            Err(e) => return Err(e.into()),
        };
        let now = call(&mut [])?;
        if n == size && now == size {
            return Ok(Some(buf));
        }
        if n < size && now == size {
            return Err(std::io::Error::other(format!(
                "a {size}-byte value read back as {n} bytes"
            )));
        }
    }
    Err(std::io::Error::other(
        "the value kept changing while being read",
    ))
}

fn list_xattrs(at: XattrAt<'_>) -> std::io::Result<Vec<Vec<u8>>> {
    let list = match at {
        XattrAt::File(f) => sized_read(u64::MAX, |buf| rustix::fs::flistxattr(f, buf)),
        XattrAt::Path(p) => sized_read(u64::MAX, |buf| rustix::fs::llistxattr(p, buf)),
    }?
    .unwrap_or_default();
    Ok(list
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// What reading one xattr found.
enum Xattr {
    Value(Vec<u8>),
    /// Over the size it was allowed; the value was not read.
    TooLarge,
    Missing,
}

/// An xattr's whole value, if it is at most `max` bytes.
fn get_xattr(at: XattrAt<'_>, name: &[u8], max: u64) -> std::io::Result<Xattr> {
    let name = OsStr::from_bytes(name);
    let value = match at {
        XattrAt::File(f) => sized_read(max, |buf| rustix::fs::fgetxattr(f, name, buf)),
        XattrAt::Path(p) => sized_read(max, |buf| rustix::fs::lgetxattr(p, name, buf)),
    };
    match value {
        Ok(Some(v)) => Ok(Xattr::Value(v)),
        Ok(None) => Ok(Xattr::TooLarge),
        Err(e) if e.raw_os_error() == Some(NO_SUCH_XATTR.raw_os_error()) => Ok(Xattr::Missing),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "macos")]
const NO_SUCH_XATTR: rustix::io::Errno = rustix::io::Errno::NOATTR;
#[cfg(not(target_os = "macos"))]
const NO_SUCH_XATTR: rustix::io::Errno = rustix::io::Errno::NODATA;

fn set_xattr(file: &std::fs::File, name: &[u8], value: &[u8]) -> std::io::Result<()> {
    rustix::fs::fsetxattr(
        file,
        OsStr::from_bytes(name),
        value,
        rustix::fs::XattrFlags::empty(),
    )
    .map_err(Into::into)
}

fn unsupported(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::Unsupported
        || e.raw_os_error() == Some(nix::errno::Errno::ENOTSUP as i32)
}

/// `st_rdev` as (major, minor), in the platform's encoding.
fn device_numbers(rdev: u64) -> Option<(u64, u64)> {
    if cfg!(target_os = "macos") {
        // <sys/types.h>: major in the top 8 bits of 32, minor in the low 24.
        Some(((rdev >> 24) & 0xff, rdev & 0xff_ffff))
    } else {
        None
    }
}

// ----------------------------------------------------------------- extract

/// How extraction may treat the destination and what it creates.
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    /// Replace existing files and symlinks. Never replaces a directory with a
    /// file, and never follows anything it replaces.
    pub overwrite: bool,
    /// Keep setuid, setgid and sticky bits (§13: only on explicit opt-in).
    pub keep_setid: bool,
    /// Restore owner and group, which normally needs root.
    pub restore_owner: bool,
    /// Create device nodes (§13: only on explicit opt-in; normally needs
    /// root). FIFOs are not devices and are always created.
    pub devices: bool,
    /// Restore extended attributes. `com.apple.decmpfs` never is.
    pub xattrs: bool,
    /// Restore ACLs (macOS).
    pub acls: bool,
    /// Restore BSD file flags (macOS). Flags only root may set are reported,
    /// not fatal.
    pub fflags: bool,
    /// A `com.apple.quarantine` value to give every extracted file and
    /// directory that does not carry its own — §9.3's "when the archive itself
    /// is quarantined". See [`archive_quarantine`].
    pub quarantine: Option<Vec<u8>>,
    /// Most content bytes to write, across every member.
    pub max_total: u64,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            overwrite: false,
            keep_setid: false,
            restore_owner: false,
            devices: false,
            xattrs: true,
            acls: true,
            fflags: true,
            quarantine: None,
            max_total: 64 << 30,
        }
    }
}

/// The archive file's own `com.apple.quarantine`, for
/// [`ExtractOptions::quarantine`].
pub fn archive_quarantine(archive: &std::fs::File) -> Option<Vec<u8>> {
    match get_xattr(XattrAt::File(archive), meta::XATTR_QUARANTINE, 4096) {
        Ok(Xattr::Value(v)) => Some(v),
        _ => None,
    }
}

/// What an extraction did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractSummary {
    pub entries: u64,
    pub bytes: u64,
    pub skipped: u64,
    /// Metadata that could not be restored, each also reported.
    pub warnings: u64,
}

/// What [`extract`] tells its caller about a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report<'a> {
    /// The member is on disk.
    Extracted,
    /// The member was not extracted, and why.
    Skipped(&'a str),
    /// The member is on disk, but some of its metadata is not.
    Warning(&'a str),
}

/// The destination, opened once, and the directories below it opened from
/// it.
struct Tree<'a> {
    root: cap_std::fs::Dir,
    dest: &'a Path,
    /// The last parent directory opened, by archive path: members arrive in
    /// path order, so consecutive ones usually share it.
    last: Option<(Vec<u8>, cap_std::fs::Dir)>,
}

/// An archive path's components, `.` dropped.
fn components(path: &[u8]) -> Vec<&[u8]> {
    path.split(|&b| b == b'/').filter(|p| *p != b".").collect()
}

impl<'a> Tree<'a> {
    fn open(dest: &'a Path) -> Result<Self> {
        std::fs::create_dir_all(dest).map_err(|e| io(dest, e))?;
        let root = cap_std::fs::Dir::open_ambient_dir(dest, cap_std::ambient_authority())
            .map_err(|e| io(dest, e))?;
        Ok(Self {
            root,
            dest,
            last: None,
        })
    }

    /// Where an archive path lands, for messages.
    fn shown(&self, parts: &[&[u8]]) -> PathBuf {
        let mut p = self.dest.to_path_buf();
        for part in parts {
            p.push(OsStr::from_bytes(part));
        }
        p
    }

    /// The directory that holds `path`, opened from the root one component at
    /// a time without following anything, missing ones created; and the
    /// final component. `None` for the destination itself.
    fn parent<'p>(&mut self, path: &'p [u8]) -> Result<Option<(cap_std::fs::Dir, &'p OsStr)>> {
        validate_path(path)?;
        let parts = components(path);
        let Some((name, dirs)) = parts.split_last() else {
            return Ok(None);
        };
        let key = dirs.join(&b'/');
        let name = OsStr::from_bytes(name);
        if let Some((k, d)) = &self.last {
            if *k == key {
                let d = d.try_clone().map_err(|e| io(self.dest, e))?;
                return Ok(Some((d, name)));
            }
        }
        let mut dir = self.root.try_clone().map_err(|e| io(self.dest, e))?;
        for i in 0..dirs.len() {
            dir = self.step(&dir, &dirs[..=i])?;
        }
        let keep = dir.try_clone().map_err(|e| io(self.dest, e))?;
        self.last = Some((key, keep));
        Ok(Some((dir, name)))
    }

    /// Open `parts`' last component in `dir`, creating it if missing, and
    /// refusing a symlink or a non-directory there.
    fn step(&self, dir: &cap_std::fs::Dir, parts: &[&[u8]]) -> Result<cap_std::fs::Dir> {
        let name = OsStr::from_bytes(parts[parts.len() - 1]);
        match cap_fs_ext::DirExt::open_dir_nofollow(dir, name) {
            Ok(d) => return Ok(d),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(self.refusal(dir, parts, e)),
        }
        match dir.create_dir(name) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io(&self.shown(parts), e)),
        }
        cap_fs_ext::DirExt::open_dir_nofollow(dir, name).map_err(|e| self.refusal(dir, parts, e))
    }

    /// Why opening a directory component failed, in §13's terms where it is
    /// one of §13's cases.
    fn refusal(&self, dir: &cap_std::fs::Dir, parts: &[&[u8]], e: std::io::Error) -> Error {
        let at = self.shown(parts);
        match dir.symlink_metadata(OsStr::from_bytes(parts[parts.len() - 1])) {
            Ok(m) if m.file_type().is_symlink() => Error::Refused(format!(
                "{} is a symlink; nothing is created through one (§13)",
                at.display()
            )),
            Ok(m) if !m.is_dir() => Error::Refused(format!("{} is not a directory", at.display())),
            _ => io(&at, e),
        }
    }
}

/// Clear the way for a new file, symlink or node called `name` in `dir`.
fn make_room(dir: &cap_std::fs::Dir, name: &OsStr, at: &Path, overwrite: bool) -> Result<()> {
    match dir.symlink_metadata(name) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(at, e)),
        Ok(m) if m.is_dir() => Err(Error::Refused(format!(
            "{} is a directory and will not be replaced",
            at.display()
        ))),
        Ok(_) if overwrite => {
            cap_fs_ext::DirExt::remove_file_or_symlink(dir, name).map_err(|e| io(at, e))
        }
        Ok(_) => Err(Error::Refused(format!(
            "{} exists; overwriting was not asked for",
            at.display()
        ))),
    }
}

fn mode_bits(m: &Member, options: &ExtractOptions) -> u32 {
    let mask = if options.keep_setid { 0o7777 } else { 0o777 };
    m.mode & mask
}

/// Collects metadata failures that do not stop extraction.
struct Warnings<'r> {
    report: &'r mut dyn FnMut(&Entry, Report<'_>),
    count: u64,
}

impl Warnings<'_> {
    fn warn(&mut self, e: &Entry, what: String) {
        self.count += 1;
        (self.report)(e, Report::Warning(&what));
    }
}

/// Apply a member's metadata to the file or directory open as `file`, in the
/// order that lets each step happen: xattrs and ACL while the mode still
/// allows writing them, then owner, mode and times, and flags last unless
/// the caller holds them back (`Some` in the return) because a locking flag
/// would block a hardlink still to come.
fn apply(
    file: &std::fs::File,
    e: &Entry,
    m: &Member,
    at: &Path,
    options: &ExtractOptions,
    hold_flags: bool,
    warnings: &mut Warnings<'_>,
) -> Result<Option<u64>> {
    if options.xattrs {
        for (name, value) in &m.xattrs {
            if name == meta::XATTR_DECMPFS {
                warnings.warn(e, "com.apple.decmpfs is never restored (§9.3)".into());
                continue;
            }
            if let Err(err) = set_xattr(file, name, value) {
                warnings.warn(e, format!("xattr {}: {err}", String::from_utf8_lossy(name)));
            }
        }
    }
    if let Some(q) = &options.quarantine {
        let own = options.xattrs && m.xattrs.iter().any(|(n, _)| n == meta::XATTR_QUARANTINE);
        if !own {
            if let Err(err) = set_xattr(file, meta::XATTR_QUARANTINE, q) {
                warnings.warn(e, format!("quarantine: {err}"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    if options.acls {
        if let Some(text) = &m.acl {
            if let Err(why) = macos::apply_acl(file, text) {
                warnings.warn(e, format!("ACL: {why}"));
            }
        }
    }
    if options.restore_owner {
        std::os::unix::fs::fchown(file, Some(m.uid as u32), Some(m.gid as u32))
            .map_err(|err| io(at, err))?;
    }
    file.set_permissions(std::fs::Permissions::from_mode(mode_bits(m, options)))
        .map_err(|err| io(at, err))?;
    let times = std::fs::FileTimes::new().set_modified(join_time(m.mtime_sec, m.mtime_nsec));
    #[cfg(target_os = "macos")]
    let times = match m.btime {
        Some((s, ns)) => std::os::macos::fs::FileTimesExt::set_created(times, join_time(s, ns)),
        None => times,
    };
    file.set_times(times).map_err(|err| io(at, err))?;

    let flags = m.fflags.filter(|&f| options.fflags && f != 0);
    match flags {
        Some(f) if hold_flags && f & meta::LOCKING_FLAGS != 0 => Ok(Some(f)),
        Some(f) => {
            set_flags(file, f, e, warnings);
            Ok(None)
        }
        None => Ok(None),
    }
}

fn set_flags(file: &std::fs::File, flags: u64, e: &Entry, warnings: &mut Warnings<'_>) {
    #[cfg(target_os = "macos")]
    if let Err(err) = macos::set_flags(file, flags) {
        warnings.warn(
            e,
            format!("file flags {}: {err}", meta::fflags_to_text(flags)),
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = file;
        warnings.warn(
            e,
            format!(
                "file flags {} are not restored on this platform",
                meta::fflags_to_text(flags)
            ),
        );
    }
}

/// Metadata a symlink or special file cannot take here, reported once.
fn note_unrestorable(e: &Entry, m: &Member, what: &str, warnings: &mut Warnings<'_>) {
    if !m.xattrs.is_empty() || m.acl.is_some() || m.fflags.is_some_and(|f| f != 0) {
        warnings.warn(
            e,
            format!("xattrs, ACLs and flags are not restored on {what}"),
        );
    }
}

/// Owner, mode and times for something that cannot be opened — a symlink, a
/// FIFO, a device node — through its parent's descriptor, never following.
fn apply_at(
    dir: &cap_std::fs::Dir,
    name: &OsStr,
    m: &Member,
    at: &Path,
    options: &ExtractOptions,
    set_mode: bool,
) -> Result<()> {
    if options.restore_owner {
        nix::unistd::fchownat(
            dir,
            name,
            Some(nix::unistd::Uid::from_raw(m.uid as u32)),
            Some(nix::unistd::Gid::from_raw(m.gid as u32)),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .map_err(|err| io(at, err.into()))?;
    }
    if set_mode {
        nix::sys::stat::fchmodat(
            dir,
            name,
            nix::sys::stat::Mode::from_bits_truncate(mode_bits(m, options) as _),
            nix::sys::stat::FchmodatFlags::NoFollowSymlink,
        )
        .map_err(|err| io(at, err.into()))?;
    }
    let mtime = cap_std::time::SystemTime::from_std(join_time(m.mtime_sec, m.mtime_nsec));
    cap_fs_ext::DirExt::set_symlink_times(
        dir,
        name,
        None,
        Some(cap_fs_ext::SystemTimeSpec::Absolute(mtime)),
    )
    .map_err(|err| io(at, err))
}

/// A name no archive member chose and no one can predict, for a node made by
/// path before it is moved into place by descriptor.
fn temp_name() -> std::ffi::OsString {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.write_u32(std::process::id());
    format!(".zalt-node-{:016x}.tmp", h.finish()).into()
}

/// Make a FIFO or device node called `name` in `dir`, whose path from the
/// process is `dir_path`. See the module notes for why this is the one step
/// done by path, and what keeps it from landing anywhere the archive chose.
fn make_node(
    dir: &cap_std::fs::Dir,
    dir_path: &Path,
    name: &OsStr,
    m: &Member,
    at: &Path,
) -> Result<()> {
    let tmp = temp_name();
    let by_path = dir_path.join(&tmp);
    let made = match m.kind {
        Kind::Fifo => nix::unistd::mkfifo(
            &by_path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        ),
        Kind::CharDevice | Kind::BlockDevice => {
            let Some((major, minor)) = m.device.and_then(|(a, b)| make_dev(a, b)) else {
                return Err(Error::Unsupported(format!(
                    "{}: device numbers this platform cannot encode",
                    at.display()
                )));
            };
            let kind = if m.kind == Kind::CharDevice {
                nix::sys::stat::SFlag::S_IFCHR
            } else {
                nix::sys::stat::SFlag::S_IFBLK
            };
            nix::sys::stat::mknod(
                &by_path,
                kind,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
                (major << 24 | minor) as _,
            )
        }
        _ => unreachable!("only special files are made as nodes"),
    };
    made.map_err(|err| io(at, err.into()))?;

    let landed = dir.symlink_metadata(&tmp).ok().is_some_and(|meta| {
        let t = meta.file_type();
        match m.kind {
            Kind::Fifo => cap_std::fs::FileTypeExt::is_fifo(&t),
            Kind::CharDevice => cap_std::fs::FileTypeExt::is_char_device(&t),
            _ => cap_std::fs::FileTypeExt::is_block_device(&t),
        }
    });
    if !landed {
        let _ = std::fs::remove_file(&by_path);
        return Err(Error::Refused(format!(
            "{}: the directory changed while the node was being made (§13)",
            at.display()
        )));
    }
    dir.rename(&tmp, dir, name).map_err(|err| {
        let _ = dir.remove_file(&tmp);
        io(at, err)
    })
}

/// (major, minor) in the platform's `dev_t` halves, if they fit.
fn make_dev(major: u64, minor: u64) -> Option<(u64, u64)> {
    (cfg!(target_os = "macos") && major <= 0xff && minor <= 0xff_ffff).then_some((major, minor))
}

/// Extract the members `select` accepts into `dest`, which is created if
/// missing. `report` hears about each member as it is done or skipped, and
/// about metadata that could not be restored.
pub fn extract<S: Source + ?Sized>(
    archive: &Archive<S>,
    dest: &Path,
    select: &dyn Fn(&Entry) -> bool,
    options: &ExtractOptions,
    report: &mut dyn FnMut(&Entry, Report<'_>),
) -> Result<ExtractSummary> {
    let chosen: Vec<&Entry> = archive.entries().iter().filter(|e| select(e)).collect();
    let total: u64 = chosen.iter().map(|e| e.size).sum();
    if total > options.max_total {
        return Err(Error::Refused(format!(
            "{total} bytes to extract, over the limit of {}",
            options.max_total
        )));
    }
    // Files a later hardlink will name: a locking flag set on one now would
    // make that link fail, so their flags wait until the links exist.
    let link_targets: HashSet<&[u8]> = chosen
        .iter()
        .filter(|e| e.kind == Kind::HardLink)
        .filter_map(|e| e.link.as_deref())
        .collect();

    let mut tree = Tree::open(dest)?;
    let mut summary = ExtractSummary::default();
    let mut warnings = Warnings { report, count: 0 };
    let mut symlinks = Vec::new();
    let mut directories = Vec::new();
    let mut held_flags: Vec<(&Entry, std::fs::File, u64)> = Vec::new();
    // Extracting everything reads the stream in order, so decode a window of
    // frames ahead, in parallel, whenever the next member lies past the last
    // one; the filesystem work then finds its frames already decoded. A
    // selection skips this: the frames between chosen members would be
    // decoded for nothing.
    let read_ahead = chosen.len() == archive.entries().len();
    let mut decoded_until = 0u64;

    for e in chosen {
        if read_ahead && e.data_offset + e.size > decoded_until {
            decoded_until = archive.prefetch(e.header_offset)?;
        }
        let Some((dir, name)) = tree.parent(&e.path)? else {
            continue; // "." as a directory entry: the destination itself
        };
        let at = tree.shown(&components(&e.path));
        match e.kind {
            Kind::Directory => {
                match dir.symlink_metadata(name) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => {
                        return Err(Error::Refused(format!(
                            "{} exists and is not a directory",
                            at.display()
                        )));
                    }
                    Err(_) => dir.create_dir(name).map_err(|err| io(&at, err))?,
                }
                // The headers now, while their frame is fresh: directories
                // are finished last, by when it may long be out of the cache.
                directories.push((e, archive.member(e)?));
            }
            Kind::File => {
                make_room(&dir, name, &at, options.overwrite)?;
                // `create_new` is `O_CREAT | O_EXCL`, relative to `dir`: it
                // refuses anything already there, a symlink included.
                let file = dir
                    .open_with(
                        name,
                        cap_std::fs::OpenOptions::new().write(true).create_new(true),
                    )
                    .map_err(|err| io(&at, err))?
                    .into_std();
                let mut out = BufWriter::new(file);
                let member = archive.copy_entry(e, &mut out)?;
                let file = out.into_inner().map_err(|err| io(&at, err.into_error()))?;
                let hold = link_targets.contains(e.path.as_slice());
                if let Some(f) = apply(&file, e, &member, &at, options, hold, &mut warnings)? {
                    held_flags.push((e, file, f));
                }
                summary.bytes += e.size;
            }
            Kind::HardLink => {
                let link = e.link.as_deref().unwrap_or_default();
                let not_ours = || {
                    Error::Refused(format!(
                        "hardlink {} points at {}, which is not a file this extraction made",
                        e.path_lossy(),
                        String::from_utf8_lossy(link)
                    ))
                };
                let (source_dir, source_name) = tree.parent(link)?.ok_or_else(not_ours)?;
                match source_dir.symlink_metadata(source_name) {
                    Ok(m) if m.file_type().is_file() => {}
                    _ => return Err(not_ours()),
                }
                // The hardlink's own parent again: resolving the target may
                // have replaced the cached one.
                let (dir, name) = tree.parent(&e.path)?.ok_or_else(not_ours)?;
                make_room(&dir, name, &at, options.overwrite)?;
                source_dir
                    .hard_link(source_name, &dir, name)
                    .map_err(|err| io(&at, err))?;
            }
            Kind::Symlink => {
                // Deferred, so no write this extraction makes can pass through
                // a symlink it made.
                symlinks.push((e, archive.member(e)?));
                continue;
            }
            Kind::Fifo | Kind::CharDevice | Kind::BlockDevice => {
                if e.kind != Kind::Fifo && !options.devices {
                    summary.skipped += 1;
                    (warnings.report)(
                        e,
                        Report::Skipped("device nodes need explicit opt-in (§13)"),
                    );
                    continue;
                }
                let member = archive.member(e)?;
                make_room(&dir, name, &at, options.overwrite)?;
                let parent_path = at.parent().unwrap_or(dest);
                make_node(&dir, parent_path, name, &member, &at)?;
                apply_at(&dir, name, &member, &at, options, true)?;
                note_unrestorable(e, &member, "FIFOs and device nodes", &mut warnings);
            }
        }
        summary.entries += 1;
        (warnings.report)(e, Report::Extracted);
    }

    for (e, member) in symlinks {
        let (dir, name) = tree.parent(&e.path)?.expect("a symlink is never the root");
        let at = tree.shown(&components(&e.path));
        make_room(&dir, name, &at, options.overwrite)?;
        let target = member.link.as_deref().unwrap_or_default();
        // `symlink_contents`, not `symlink`: cap-std's `symlink` refuses
        // absolute targets, which archives legitimately hold. Creating one is
        // harmless here — nothing this extraction does follows it.
        dir.symlink_contents(OsStr::from_bytes(target), name)
            .map_err(|err| io(&at, err))?;
        apply_at(&dir, name, &member, &at, options, false)?;
        note_unrestorable(e, &member, "symlinks", &mut warnings);
        summary.entries += 1;
        (warnings.report)(e, Report::Extracted);
    }

    for (e, file, flags) in held_flags {
        set_flags(&file, flags, e, &mut warnings);
    }

    // Directories last, deepest first: their modes may forbid writing into
    // them, and writing their contents would move their mtimes.
    for (e, member) in directories.into_iter().rev() {
        let (dir, name) = tree.parent(&e.path)?.expect("checked when created");
        let at = tree.shown(&components(&e.path));
        let handle = cap_fs_ext::DirExt::open_dir_nofollow(&dir, name)
            .map_err(|err| io(&at, err))?
            .into_std_file();
        apply(&handle, e, &member, &at, options, false, &mut warnings)?;
    }
    summary.warnings = warnings.count;
    Ok(summary)
}

// ------------------------------------------------------------------- macOS

/// ACLs and flags, the two pieces of macOS metadata with no descriptor-based
/// call in `exacl` or `nix`.
#[cfg(target_os = "macos")]
mod macos {
    use std::os::fd::AsRawFd;
    use std::path::Path;

    use crate::meta::{self, Ace, AceTag, AceType};

    /// The descriptor's own vnode, as a path the path-only calls accept. The
    /// kernel resolves `/dev/fd/N` to the open file, not to whatever is at its
    /// name now — the test `flags_and_acls_follow_the_descriptor` pins this.
    fn by_descriptor(file: &std::fs::File) -> String {
        format!("/dev/fd/{}", file.as_raw_fd())
    }

    pub fn set_flags(file: &std::fs::File, flags: u64) -> nix::Result<()> {
        let bits = nix::sys::stat::FileFlag::from_bits_truncate(flags as _);
        nix::unistd::chflags(by_descriptor(file).as_str(), bits)
    }

    /// [`meta::PERM_LETTERS`], letter by letter, as macOS permissions.
    const PERMS: [exacl::Perm; 14] = [
        exacl::Perm::READ,
        exacl::Perm::WRITE,
        exacl::Perm::EXECUTE,
        exacl::Perm::APPEND,
        exacl::Perm::DELETE_CHILD,
        exacl::Perm::DELETE,
        exacl::Perm::READATTR,
        exacl::Perm::WRITEATTR,
        exacl::Perm::READEXTATTR,
        exacl::Perm::WRITEEXTATTR,
        exacl::Perm::READSECURITY,
        exacl::Perm::WRITESECURITY,
        exacl::Perm::CHOWN,
        exacl::Perm::SYNC,
    ];

    /// [`meta::FLAG_LETTERS`], letter by letter; the two audit letters have
    /// no macOS counterpart.
    const FLAGS: [Option<exacl::Flag>; 7] = [
        Some(exacl::Flag::FILE_INHERIT),
        Some(exacl::Flag::DIRECTORY_INHERIT),
        Some(exacl::Flag::ONLY_INHERIT),
        Some(exacl::Flag::LIMIT_INHERIT),
        None,
        None,
        Some(exacl::Flag::INHERITED),
    ];

    /// `path`'s extended ACL as `SCHILY.acl.ace` text, never following a
    /// symlink. `None` when there is none or it cannot be read; a read error
    /// is reported.
    pub fn read_acl(path: &Path, warn: &mut dyn FnMut(String)) -> Option<Vec<u8>> {
        let entries = match exacl::getfacl(path, exacl::AclOption::SYMLINK_ACL) {
            Ok(entries) => entries,
            Err(e) if super::unsupported(&e) => return None,
            Err(e) => {
                warn(format!("{}: ACL not read: {e}", path.display()));
                return None;
            }
        };
        let mut aces = Vec::with_capacity(entries.len());
        for a in entries {
            let tag = match a.kind {
                exacl::AclEntryKind::User => AceTag::User,
                exacl::AclEntryKind::Group => AceTag::Group,
                _ => {
                    warn(format!(
                        "{}: ACL entry for {:?} skipped",
                        path.display(),
                        a.name
                    ));
                    continue;
                }
            };
            let mut perms = 0u16;
            for (i, p) in PERMS.iter().enumerate() {
                if a.perms.contains(*p) {
                    perms |= 1 << i;
                }
            }
            let mut flags = 0u8;
            for (i, f) in FLAGS.iter().enumerate() {
                if f.is_some_and(|f| a.flags.contains(f)) {
                    flags |= 1 << i;
                }
            }
            aces.push(Ace {
                tag,
                id: id_of(tag, &a.name),
                qualifier: a.name.into_bytes(),
                perms,
                flags,
                kind: if a.allow {
                    AceType::Allow
                } else {
                    AceType::Deny
                },
            });
        }
        (!aces.is_empty()).then(|| meta::format_acl(&aces))
    }

    /// The numeric id for a principal exacl named, for a reader without that
    /// name. exacl spells an id with no name as the number itself.
    fn id_of(tag: AceTag, name: &str) -> Option<u64> {
        if let Ok(n) = name.parse() {
            return Some(n);
        }
        match tag {
            AceTag::User => nix::unistd::User::from_name(name)
                .ok()
                .flatten()
                .map(|u| u64::from(u.uid.as_raw())),
            _ => nix::unistd::Group::from_name(name)
                .ok()
                .flatten()
                .map(|g| u64::from(g.gid.as_raw())),
        }
    }

    /// Set the extended ACL from `SCHILY.acl.ace` text. The three trivial
    /// entries restate the mode and macOS cannot store them; audit and alarm
    /// entries have no macOS meaning. Both are left out, and said so.
    pub fn apply_acl(file: &std::fs::File, text: &[u8]) -> Result<(), String> {
        let aces = meta::parse_acl(text).map_err(|e| e.to_string())?;
        let mut entries = Vec::new();
        let mut dropped = 0;
        for ace in aces.iter().filter(|a| !a.is_trivial()) {
            let allow = match ace.kind {
                AceType::Allow => true,
                AceType::Deny => false,
                AceType::Audit | AceType::Alarm => {
                    dropped += 1;
                    continue;
                }
            };
            let mut perms = exacl::Perm::empty();
            for (i, p) in PERMS.iter().enumerate() {
                if ace.perms & (1 << i) != 0 {
                    perms |= *p;
                }
            }
            let mut flags = exacl::Flag::empty();
            for (i, f) in FLAGS.iter().enumerate() {
                if let Some(f) = f.filter(|_| ace.flags & (1 << i) != 0) {
                    flags |= f;
                }
            }
            let name = principal(ace).ok_or_else(|| {
                format!(
                    "{} names no one this system knows",
                    String::from_utf8_lossy(&ace.qualifier)
                )
            })?;
            let kind = if ace.tag == AceTag::User {
                exacl::AclEntryKind::User
            } else {
                exacl::AclEntryKind::Group
            };
            entries.push(exacl::AclEntry {
                kind,
                name,
                perms,
                flags,
                allow,
            });
        }
        if !entries.is_empty() {
            exacl::setfacl(
                &[by_descriptor(file)],
                &entries,
                exacl::AclOption::SYMLINK_ACL,
            )
            .map_err(|e| e.to_string())?;
        }
        if dropped > 0 {
            return Err(format!(
                "{dropped} audit/alarm entries have no macOS meaning"
            ));
        }
        Ok(())
    }

    /// The name to give exacl: the archived name when this system has it,
    /// else the archived id — names travel between machines, ids do not.
    fn principal(ace: &Ace) -> Option<String> {
        let name = std::str::from_utf8(&ace.qualifier)
            .ok()
            .filter(|n| !n.is_empty());
        let known = name.is_some_and(|n| match ace.tag {
            AceTag::User => nix::unistd::User::from_name(n).ok().flatten().is_some(),
            _ => nix::unistd::Group::from_name(n).ok().flatten().is_some(),
        });
        if known {
            return name.map(str::to_owned);
        }
        ace.id
            .map(|id| id.to_string())
            .or_else(|| name.map(str::to_owned))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// The guarantee the module notes lean on: flags and ACLs applied through
    /// `/dev/fd/N` land on the open file, not on whatever is at its old name.
    /// If a macOS release ever resolved `/dev/fd/N` differently, this fails.
    #[test]
    fn flags_and_acls_follow_the_descriptor() {
        let dir = std::env::temp_dir().join(format!("zalt-fd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (named, moved) = (dir.join("named"), dir.join("moved"));
        let file = std::fs::File::create(&named).unwrap();
        std::fs::rename(&named, &moved).unwrap();
        std::fs::write(&named, b"a decoy now holds the old name").unwrap();

        macos::set_flags(&file, meta::flag::UF_HIDDEN).unwrap();
        macos::apply_acl(&file, b"group:staff:r::allow:20").unwrap();

        let flags =
            |p: &Path| std::os::macos::fs::MetadataExt::st_flags(&std::fs::metadata(p).unwrap());
        assert_eq!(flags(&moved), meta::flag::UF_HIDDEN as u32);
        assert_eq!(flags(&named), 0, "the decoy was not touched");
        let acl = |p: &Path| macos::read_acl(p, &mut |w| panic!("{w}")).unwrap_or_default();
        assert_eq!(acl(&moved), b"group:staff:r::allow:20");
        assert!(acl(&named).is_empty());
        let _ = std::process::Command::new("chflags")
            .args(["-R", "0"])
            .arg(&dir)
            .status();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
