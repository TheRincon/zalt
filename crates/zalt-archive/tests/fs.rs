//! Archive a real directory tree, extract it, and try to escape the root.

#![cfg(feature = "native-fs")]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use zalt_archive::fs::{ExtractOptions, Report, add_tree, extract};
use zalt_archive::read::Archive;
use zalt_archive::{Entry, Error, Kind, ReadLimits, Writer, WriterOptions};

/// A scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "zalt-archive-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Flags such as `uchg` block removal; clear them all first.
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("chflags")
            .args(["-R", "0"])
            .arg(&self.0)
            .status();
        // Directories extracted read-only must be made writable to be removed.
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.0)
            .status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set_mtime(path: &Path, secs: u64, nanos: u32) {
    let t = UNIX_EPOCH + Duration::new(secs, nanos);
    fs::File::open(path).unwrap().set_modified(t).unwrap();
}

/// A tree with one of everything the portable profile carries.
fn build_tree(root: &Path) {
    let proj = root.join("proj");
    fs::create_dir_all(proj.join("src/deep")).unwrap();
    fs::write(proj.join("src/a.txt"), b"alpha\n".repeat(100)).unwrap();
    fs::write(
        proj.join("src/deep/b.bin"),
        (0..300_000u32)
            .map(|i| (i * 31 % 251) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(proj.join("empty"), b"").unwrap();
    fs::write(proj.join("donne\u{301}es.txt"), b"NFD name\n").unwrap();
    fs::write(proj.join("tool"), b"#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(proj.join("tool"), fs::Permissions::from_mode(0o4755)).unwrap();
    std::os::unix::fs::symlink("src/a.txt", proj.join("latest")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", proj.join("absolute_link")).unwrap();
    fs::hard_link(proj.join("src/a.txt"), proj.join("a_again.txt")).unwrap();
    set_mtime(&proj.join("src/a.txt"), 1_600_000_000, 123_456_789);
    fs::set_permissions(proj.join("src"), fs::Permissions::from_mode(0o750)).unwrap();
}

fn archive_tree(root: &Path, options: WriterOptions) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), options).unwrap();
    let cwd = std::env::current_dir().unwrap();
    // Archive names come from the path as given, as with tar; archive `proj`
    // relative to its parent.
    std::env::set_current_dir(root).unwrap();
    let result = add_tree(&mut w, Path::new("proj"), &mut |w| {
        panic!("unexpected warning: {w}")
    });
    std::env::set_current_dir(cwd).unwrap();
    result.unwrap();
    w.finish().unwrap().0
}

fn open(bytes: Vec<u8>) -> Archive<[u8]> {
    Archive::open(bytes.into_boxed_slice(), ReadLimits::default()).unwrap()
}

fn extract_all(
    archive: &Archive<[u8]>,
    dest: &Path,
    options: &ExtractOptions,
) -> zalt_archive::Result<()> {
    extract(archive, dest, &|_| true, options, &mut |_, _| {}).map(|_| ())
}

/// One test touches the process's working directory; the rest must not run
/// beside it.
static CWD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_tree_comes_back_as_it_was() {
    let _cwd = CWD.lock().unwrap_or_else(|e| e.into_inner());
    let src = Scratch::new("src");
    build_tree(&src.0);
    let bytes = archive_tree(
        &src.0,
        WriterOptions {
            level: 3,
            ..WriterOptions::default()
        },
    );
    let archive = open(bytes);
    archive.verify().unwrap();

    let names: Vec<_> = archive
        .entries()
        .iter()
        .map(|e| e.path_lossy().into_owned())
        .collect();
    // Byte-wise order, each directory before its contents.
    let mut sorted = names.clone();
    sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(names, sorted);
    assert_eq!(names[0], "proj");

    let dest = Scratch::new("dest");
    extract_all(&archive, &dest.0, &ExtractOptions::default()).unwrap();
    let (a, b) = (src.0.join("proj"), dest.0.join("proj"));

    for f in [
        "src/a.txt",
        "src/deep/b.bin",
        "empty",
        "donne\u{301}es.txt",
        "tool",
        "a_again.txt",
    ] {
        assert_eq!(
            fs::read(a.join(f)).unwrap(),
            fs::read(b.join(f)).unwrap(),
            "{f}"
        );
    }
    assert_eq!(
        fs::read_link(b.join("latest")).unwrap(),
        Path::new("src/a.txt")
    );
    assert_eq!(
        fs::read_link(b.join("absolute_link")).unwrap(),
        Path::new("/etc/hosts")
    );
    // The hardlink is a link again, not a copy.
    assert_eq!(
        fs::metadata(b.join("a_again.txt")).unwrap().ino(),
        fs::metadata(b.join("src/a.txt")).unwrap().ino()
    );
    // Modes: kept, except setuid, which §13 strips without opt-in.
    assert_eq!(fs::metadata(b.join("src")).unwrap().mode() & 0o7777, 0o750);
    assert_eq!(fs::metadata(b.join("tool")).unwrap().mode() & 0o7777, 0o755);
    // Nanosecond mtimes survive.
    let m = fs::metadata(b.join("src/a.txt")).unwrap();
    assert_eq!((m.mtime(), m.mtime_nsec()), (1_600_000_000, 123_456_789));
}

#[test]
fn setuid_is_kept_only_when_asked() {
    let _cwd = CWD.lock().unwrap_or_else(|e| e.into_inner());
    let src = Scratch::new("src");
    build_tree(&src.0);
    let archive = open(archive_tree(&src.0, WriterOptions::default()));
    let dest = Scratch::new("dest");
    extract_all(
        &archive,
        &dest.0,
        &ExtractOptions {
            keep_setid: true,
            ..ExtractOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        fs::metadata(dest.0.join("proj/tool")).unwrap().mode() & 0o7777,
        0o4755
    );
}

/// Build an archive from hand-made members, bypassing the filesystem.
fn archive_of(members: Vec<(Entry, Vec<u8>)>) -> Archive<[u8]> {
    let mut w = Writer::new(Vec::new(), WriterOptions::default()).unwrap();
    for (e, content) in members {
        w.add(e, &mut content.as_slice()).unwrap();
    }
    open(w.finish().unwrap().0)
}

fn file(path: &str, content: &[u8]) -> (Entry, Vec<u8>) {
    let mut e = Entry::new(Kind::File, path.as_bytes());
    e.size = content.len() as u64;
    (e, content.to_vec())
}

fn symlink(path: &str, target: &str) -> (Entry, Vec<u8>) {
    let mut e = Entry::new(Kind::Symlink, path.as_bytes());
    e.link = Some(target.as_bytes().to_vec());
    (e, vec![])
}

/// The classic escape: a symlink to somewhere else, then a file "inside" it.
/// Symlinks are created last, so the file's parent is made as a real
/// directory; the symlink then cannot replace it. Nothing is written outside.
#[test]
fn a_symlink_cannot_be_used_to_write_outside_the_root() {
    let outside = Scratch::new("outside");
    let archive = archive_of(vec![
        symlink("escape", outside.0.to_str().unwrap()),
        file("escape/planted.txt", b"should never exist outside"),
    ]);
    let dest = Scratch::new("dest");
    let result = extract_all(
        &archive,
        &dest.0,
        &ExtractOptions {
            overwrite: true,
            ..ExtractOptions::default()
        },
    );
    assert!(matches!(result, Err(Error::Refused(_))), "{result:?}");
    assert!(
        !outside.0.join("planted.txt").exists(),
        "a write escaped the root"
    );
}

/// A symlink that was already in the destination before extraction began is
/// not followed either.
#[test]
fn a_symlink_already_in_the_destination_is_not_followed() {
    let outside = Scratch::new("outside");
    let dest = Scratch::new("dest");
    std::os::unix::fs::symlink(&outside.0, dest.0.join("proj")).unwrap();
    let archive = archive_of(vec![file("proj/planted.txt", b"no")]);
    let result = extract_all(&archive, &dest.0, &ExtractOptions::default());
    assert!(matches!(result, Err(Error::Refused(_))), "{result:?}");
    assert!(
        !outside.0.join("planted.txt").exists(),
        "a write escaped the root"
    );
}

#[test]
fn a_hardlink_must_point_at_a_file_this_extraction_made() {
    let mut link = Entry::new(Kind::HardLink, "stolen");
    link.link = Some(b"not/there".to_vec());
    let archive = archive_of(vec![(link, vec![])]);
    let dest = Scratch::new("dest");
    assert!(matches!(
        extract_all(&archive, &dest.0, &ExtractOptions::default()),
        Err(Error::Refused(_))
    ));

    // Nor at a symlink this extraction made, which could point anywhere.
    let mut link = Entry::new(Kind::HardLink, "b");
    link.link = Some(b"a".to_vec());
    let archive = archive_of(vec![symlink("a", "/etc/hosts"), (link, vec![])]);
    let dest = Scratch::new("dest");
    assert!(extract_all(&archive, &dest.0, &ExtractOptions::default()).is_err());
}

#[test]
fn existing_files_are_kept_unless_overwriting_was_asked_for() {
    let archive = archive_of(vec![file("f", b"new")]);
    let dest = Scratch::new("dest");
    fs::write(dest.0.join("f"), b"old").unwrap();
    assert!(extract_all(&archive, &dest.0, &ExtractOptions::default()).is_err());
    assert_eq!(fs::read(dest.0.join("f")).unwrap(), b"old");
    extract_all(
        &archive,
        &dest.0,
        &ExtractOptions {
            overwrite: true,
            ..ExtractOptions::default()
        },
    )
    .unwrap();
    assert_eq!(fs::read(dest.0.join("f")).unwrap(), b"new");
}

#[test]
fn the_size_limit_is_checked_before_anything_is_written() {
    let archive = archive_of(vec![file("a", &[1u8; 600]), file("b", &[2u8; 600])]);
    let dest = Scratch::new("dest");
    let tight = ExtractOptions {
        max_total: 1000,
        ..ExtractOptions::default()
    };
    assert!(matches!(
        extract_all(&archive, &dest.0, &tight),
        Err(Error::Refused(_))
    ));
    assert!(
        !dest.0.join("a").exists(),
        "nothing written before the refusal"
    );
}

#[test]
fn fifos_are_made_and_device_nodes_need_opt_in() {
    use std::os::unix::fs::FileTypeExt;
    let mut fifo = Entry::new(Kind::Fifo, "pipe");
    fifo.mode = 0o640;
    fifo.mtime_sec = 1_234_567_890;
    let mut dev = Entry::new(Kind::CharDevice, "null");
    dev.device = Some((3, 2));
    let archive = archive_of(vec![(fifo, vec![]), (dev, vec![]), file("ok", b"fine")]);
    let dest = Scratch::new("dest");
    let mut skipped = Vec::new();
    let summary = extract(
        &archive,
        &dest.0,
        &|_| true,
        &ExtractOptions::default(),
        &mut |e, what| {
            if let Report::Skipped(_) = what {
                skipped.push(e.path_lossy().into_owned());
            }
        },
    )
    .unwrap();
    assert_eq!(skipped, ["null"]);
    assert_eq!((summary.entries, summary.skipped), (2, 1));
    assert!(fs::symlink_metadata(dest.0.join("null")).is_err());
    let pipe = fs::symlink_metadata(dest.0.join("pipe")).unwrap();
    assert!(pipe.file_type().is_fifo());
    assert_eq!((pipe.mode() & 0o7777, pipe.mtime()), (0o640, 1_234_567_890));
    // No temporary node left behind.
    let names: Vec<_> = fs::read_dir(&dest.0)
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn extracting_a_selection_touches_nothing_else() {
    let archive = archive_of(vec![file("keep/a", b"a"), file("skip/b", b"b")]);
    let dest = Scratch::new("dest");
    extract(
        &archive,
        &dest.0,
        &|e| e.path.starts_with(b"keep/"),
        &ExtractOptions::default(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(dest.0.join("keep/a").exists());
    assert!(!dest.0.join("skip").exists());
}

// ----------------------------------------------------------- macOS metadata

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::os::macos::fs::MetadataExt as _;
    use zalt_archive::fs::{CreateOptions, add_tree_with};
    use zalt_archive::meta::{self, flag};

    const RESOURCE_FORK: &str = "com.apple.ResourceFork";

    /// Every xattr but the resource fork, by the `xattr` crate — a reader
    /// independent of the crate's own; and the fork through
    /// `..namedfork/rsrc`, which `xattr` cannot read whole (it stops at
    /// 4 KiB) and which goes through neither.
    fn xattrs_of(path: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<_> = xattr::list(path)
            .unwrap()
            .map(|n| n.to_string_lossy().into_owned())
            // Every file this process creates gets one; it is not ours to compare.
            .filter(|n| n.as_bytes() != meta::XATTR_PROVENANCE)
            .filter(|n| n != RESOURCE_FORK)
            .map(|n| {
                let v = xattr::get(path, &n).unwrap().unwrap();
                (n, v)
            })
            .collect();
        if let Ok(fork) = fs::read(path.join("..namedfork/rsrc")) {
            if !fork.is_empty() {
                out.push((RESOURCE_FORK.into(), fork));
            }
        }
        out.sort();
        out
    }

    fn acl_of(path: &Path) -> String {
        exacl::to_string(&exacl::getfacl(path, exacl::AclOption::SYMLINK_ACL).unwrap()).unwrap()
    }

    fn deny(mut e: exacl::AclEntry) -> exacl::AclEntry {
        e.allow = false;
        e
    }

    /// Everything §9.1 lists for macOS, on real files, archived by `add_tree`
    /// and brought back by `extract`.
    #[test]
    fn macos_metadata_round_trips() {
        let _cwd = CWD.lock().unwrap_or_else(|e| e.into_inner());
        let src = Scratch::new("meta-src");
        let proj = src.0.join("proj");
        fs::create_dir_all(proj.join("shared")).unwrap();

        let doc = proj.join("doc.txt");
        fs::write(&doc, b"document\n").unwrap();
        xattr::set(&doc, "com.zalt.text", b"plain value").unwrap();
        xattr::set(&doc, "com.zalt.bin", &[0x00, 0xff, 0x10, b'\n', b'=']).unwrap();
        // A resource fork larger than any solid frame, so its pax header
        // spans frames.
        let fork: Vec<u8> = (0..5_000_000u32).map(|i| (i * 7 % 253) as u8).collect();
        xattr::set(&doc, RESOURCE_FORK, &fork).unwrap();
        exacl::setfacl(
            &[&doc],
            &[
                deny(exacl::AclEntry::allow_user(
                    "root",
                    exacl::Perm::DELETE,
                    None,
                )),
                exacl::AclEntry::allow_group(
                    "everyone",
                    exacl::Perm::READ | exacl::Perm::WRITE | exacl::Perm::APPEND,
                    None,
                ),
                exacl::AclEntry::allow_user("4242", exacl::Perm::READEXTATTR, None),
            ],
            None,
        )
        .unwrap();
        // Inheritance flags only mean something on a directory.
        exacl::setfacl(
            &[proj.join("shared")],
            &[exacl::AclEntry::allow_group(
                "staff",
                exacl::Perm::READ | exacl::Perm::EXECUTE,
                exacl::Flag::FILE_INHERIT | exacl::Flag::DIRECTORY_INHERIT,
            )],
            None,
        )
        .unwrap();

        let hidden = proj.join("hidden.txt");
        fs::write(&hidden, b"h").unwrap();
        let locked = proj.join("locked.txt");
        fs::write(&locked, b"cannot change me").unwrap();
        // A hardlink to a locked file: its flag must wait until the link exists.
        fs::hard_link(&locked, proj.join("locked_link.txt")).unwrap();

        let born = UNIX_EPOCH + Duration::new(1_000_000_000, 250_000_000);
        let file = fs::File::options().write(true).open(&hidden).unwrap();
        file.set_times(std::os::macos::fs::FileTimesExt::set_created(
            fs::FileTimes::new(),
            born,
        ))
        .unwrap();
        drop(file);
        nix::unistd::mkfifo(
            &proj.join("pipe"),
            nix::sys::stat::Mode::from_bits_truncate(0o620),
        )
        .unwrap();
        nix::unistd::chflags(
            &hidden,
            nix::sys::stat::FileFlag::UF_HIDDEN | nix::sys::stat::FileFlag::UF_NODUMP,
        )
        .unwrap();
        nix::unistd::chflags(&locked, nix::sys::stat::FileFlag::UF_IMMUTABLE).unwrap();

        // Archive.
        let mut w = Writer::new(Vec::new(), WriterOptions::default()).unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&src.0).unwrap();
        let mut warnings = Vec::new();
        let result = add_tree_with(
            &mut w,
            Path::new("proj"),
            &CreateOptions::default(),
            &mut |m| warnings.push(m),
        );
        std::env::set_current_dir(cwd).unwrap();
        result.unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let archive = open(w.finish().unwrap().0);
        archive.verify().unwrap();

        // The index summarises it — names, sizes, flags, birth time — and
        // leaves out what §9.3 says to.
        let entry = |p: &str| archive.find(p.as_bytes()).unwrap();
        let d = entry("proj/doc.txt");
        let names: Vec<&[u8]> = d.xattrs.iter().map(|(n, _)| n.as_slice()).collect();
        assert_eq!(
            names,
            [
                &b"com.apple.ResourceFork"[..],
                b"com.zalt.bin",
                b"com.zalt.text"
            ],
            "sorted, and no com.apple.provenance"
        );
        assert_eq!(d.xattrs[0].1, fork.len() as u64);
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .unwrap();
        assert_eq!(d.names.as_ref().unwrap().0, me.name.as_bytes());
        let h = entry("proj/hidden.txt");
        assert_eq!(h.fflags, Some(flag::UF_HIDDEN | flag::UF_NODUMP));
        assert_eq!(h.btime, Some((1_000_000_000, 250_000_000)));
        assert_eq!(entry("proj/locked.txt").fflags, Some(flag::UF_IMMUTABLE));
        assert_eq!(entry("proj/pipe").kind, Kind::Fifo);
        assert_eq!(entry("proj/locked_link.txt").kind, Kind::HardLink);

        // Extract, and compare file by file.
        let dest = Scratch::new("meta-dest");
        let mut reported = Vec::new();
        let summary = extract(
            &archive,
            &dest.0,
            &|_| true,
            &ExtractOptions::default(),
            &mut |e, r| {
                if r != Report::Extracted {
                    reported.push(format!("{}: {r:?}", e.path_lossy()));
                }
            },
        )
        .unwrap();
        assert!(reported.is_empty(), "{reported:?}");
        assert_eq!(summary.warnings, 0);
        let out = dest.0.join("proj");
        for f in ["doc.txt", "hidden.txt", "locked.txt", "shared"] {
            assert_eq!(
                xattrs_of(&proj.join(f)),
                xattrs_of(&out.join(f)),
                "{f} xattrs"
            );
            assert_eq!(acl_of(&proj.join(f)), acl_of(&out.join(f)), "{f} ACL");
            let (a, b) = (
                fs::symlink_metadata(proj.join(f)).unwrap(),
                fs::symlink_metadata(out.join(f)).unwrap(),
            );
            assert_eq!(
                a.st_flags() & meta::RECORDED_FLAGS as u32,
                b.st_flags(),
                "{f} flags"
            );
            assert_eq!(a.mode(), b.mode(), "{f} mode");
            assert_eq!(
                (a.mtime(), a.mtime_nsec()),
                (b.mtime(), b.mtime_nsec()),
                "{f} mtime"
            );
        }
        assert!(acl_of(&out.join("doc.txt")).contains("deny::user:root:delete"));
        assert_eq!(
            fs::metadata(out.join("hidden.txt"))
                .unwrap()
                .created()
                .unwrap(),
            born
        );
        assert!(std::os::unix::fs::FileTypeExt::is_fifo(
            &fs::symlink_metadata(out.join("pipe")).unwrap().file_type()
        ));
        assert_eq!(
            fs::metadata(out.join("locked_link.txt")).unwrap().ino(),
            fs::metadata(out.join("locked.txt")).unwrap().ino()
        );
        // And the lock holds.
        assert!(fs::write(out.join("locked.txt"), b"changed").is_err());
    }

    /// Each kind of metadata has its own off switch, on both sides.
    #[test]
    fn metadata_can_be_left_out() {
        let _cwd = CWD.lock().unwrap_or_else(|e| e.into_inner());
        let src = Scratch::new("off-src");
        let f = src.0.join("proj");
        fs::create_dir(&f).unwrap();
        let f = f.join("f");
        fs::write(&f, b"x").unwrap();
        xattr::set(&f, "com.zalt.k", b"v").unwrap();
        nix::unistd::chflags(&f, nix::sys::stat::FileFlag::UF_HIDDEN).unwrap();
        let mut w = Writer::new(Vec::new(), WriterOptions::default()).unwrap();
        let none = CreateOptions {
            xattrs: false,
            acls: false,
            fflags: false,
            keep_provenance: false,
        };
        add_tree_with(&mut w, &src.0.join("proj"), &none, &mut |m| panic!("{m}")).unwrap();
        let archive = open(w.finish().unwrap().0);
        let e = archive.entries().last().unwrap();
        assert!(e.xattrs.is_empty() && e.fflags.is_none(), "{e:?}");

        // Now archived with everything, extracted with nothing.
        let mut w = Writer::new(Vec::new(), WriterOptions::default()).unwrap();
        add_tree_with(
            &mut w,
            &src.0.join("proj"),
            &CreateOptions::default(),
            &mut |m| panic!("{m}"),
        )
        .unwrap();
        let archive = open(w.finish().unwrap().0);
        let dest = Scratch::new("off-dest");
        let bare = ExtractOptions {
            xattrs: false,
            acls: false,
            fflags: false,
            ..ExtractOptions::default()
        };
        extract(&archive, &dest.0, &|_| true, &bare, &mut |_, _| {}).unwrap();
        let out = dest.0.join(src.0.strip_prefix("/").unwrap()).join("proj/f");
        assert!(xattr::get(&out, "com.zalt.k").unwrap().is_none());
        assert_eq!(fs::metadata(&out).unwrap().st_flags(), 0);
    }

    /// §9.3: a quarantined archive passes its quarantine to what it extracts,
    /// unless a member carries its own.
    #[test]
    fn quarantine_follows_the_archive() {
        let mut own = Entry::new(Kind::File, "own");
        own.size = 1;
        let mut w = Writer::new(Vec::new(), WriterOptions::default()).unwrap();
        let mut dir = Entry::new(Kind::Directory, "dir");
        dir.mode = 0o755;
        w.add(dir, &mut std::io::empty()).unwrap();
        let mut plain = Entry::new(Kind::File, "dir/plain");
        plain.size = 1;
        w.add(plain, &mut &b"p"[..]).unwrap();
        let extras = zalt_archive::tar::Extras {
            xattrs: vec![(meta::XATTR_QUARANTINE.to_vec(), b"0001;own".to_vec())],
            acl: None,
        };
        w.add_with(own, &extras, &mut &b"o"[..]).unwrap();
        let bytes = w.finish().unwrap().0;

        let scratch = Scratch::new("quarantine");
        let path = scratch.0.join("a.tar.zst");
        fs::write(&path, &bytes).unwrap();
        xattr::set(&path, "com.apple.quarantine", b"0081;archive").unwrap();
        let q = zalt_archive::fs::archive_quarantine(&fs::File::open(&path).unwrap());
        assert_eq!(q.as_deref(), Some(&b"0081;archive"[..]));

        let dest = scratch.0.join("out");
        let options = ExtractOptions {
            quarantine: q,
            ..ExtractOptions::default()
        };
        extract(&open(bytes), &dest, &|_| true, &options, &mut |_, _| {}).unwrap();
        let get = |p: &str| xattr::get(dest.join(p), "com.apple.quarantine").unwrap();
        assert_eq!(get("dir/plain").as_deref(), Some(&b"0081;archive"[..]));
        assert_eq!(get("dir").as_deref(), Some(&b"0081;archive"[..]));
        assert_eq!(get("own").as_deref(), Some(&b"0001;own"[..]));
    }
}

// ---------------------------------------------------------------- the race

/// A directory in the destination is swapped for a symlink to `outside`, and
/// back, as fast as possible, while something extracts into it.
struct Swapper {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<u64>>,
}

impl Swapper {
    fn start(dir: PathBuf, outside: PathBuf) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            // An atomic exchange (`renameatx_np(RENAME_SWAP)`), so `dir` is
            // never missing — only ever a real directory or a symlink to
            // `outside` — and each state is held for a while: a symlink that
            // exists for a microsecond is one a quiet machine almost never
            // lands in. This is what a real attacker would do.
            let parked = dir.with_extension("link");
            std::os::unix::fs::symlink(&outside, &parked).unwrap();
            let hold = || {
                let until = std::time::Instant::now() + Duration::from_micros(30);
                while std::time::Instant::now() < until {
                    std::hint::spin_loop();
                }
            };
            let swap = || {
                let cwd = rustix::fs::CWD;
                rustix::fs::renameat_with(
                    cwd,
                    &dir,
                    cwd,
                    &parked,
                    rustix::fs::RenameFlags::EXCHANGE,
                )
                .is_ok()
            };
            let mut swaps = 0;
            while !flag.load(Ordering::Relaxed) {
                if swap() {
                    swaps += 1;
                    hold();
                    swap();
                }
                hold();
            }
            swaps
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap()
    }
}

/// What extraction did before descriptors: check each component by path,
/// then create by path. Correct when nothing moves; here, the control that
/// shows the swapper really does break path-based extraction.
fn extract_by_path(dest: &Path, paths: &[String]) {
    'files: for p in paths {
        let target = dest.join(p);
        let mut at = dest.to_path_buf();
        let parts: Vec<_> = Path::new(p).components().collect();
        for c in &parts[..parts.len() - 1] {
            at.push(c);
            match fs::symlink_metadata(&at) {
                Ok(m) if m.file_type().is_symlink() || !m.is_dir() => continue 'files,
                Ok(_) => {}
                Err(_) => {
                    if fs::create_dir(&at).is_err() {
                        continue 'files;
                    }
                }
            }
        }
        let _ = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target);
    }
}

#[test]
fn swapping_a_directory_for_a_symlink_mid_extraction_never_escapes() {
    // Each file under its own subdirectory, so every one is a fresh walk
    // through the swapped directory rather than a cached descriptor.
    let paths: Vec<String> = (0..300).map(|i| format!("d/s{i}/f")).collect();
    let members: Vec<_> = paths.iter().map(|p| file(p, b"x")).collect();
    let archive = archive_of(members);
    let escaped = |outside: &Path| fs::read_dir(outside).unwrap().count();

    // The control first: the same swapper must get the path-based extractor
    // to write outside, or this test proves nothing.
    let mut control_escapes = 0;
    let mut control_attempts = 0;
    for _ in 0..200 {
        control_attempts += 1;
        let (dest, outside) = (Scratch::new("race-ctl"), Scratch::new("race-ctl-out"));
        fs::create_dir(dest.0.join("d")).unwrap();
        let swapper = Swapper::start(dest.0.join("d"), outside.0.clone());
        extract_by_path(&dest.0, &paths);
        swapper.finish();
        control_escapes += escaped(&outside.0);
        if control_escapes > 0 {
            break;
        }
    }
    assert!(
        control_escapes > 0,
        "the swapper never broke the path-based control"
    );
    eprintln!("control: {control_escapes} files escaped after {control_attempts} attempt(s)");

    // Now the real extractor, under the same attack, many times. It may stop
    // with an error — refusing is the right answer — but never writes outside.
    let mut swaps = 0;
    for _ in 0..30 {
        let (dest, outside) = (Scratch::new("race"), Scratch::new("race-out"));
        fs::create_dir(dest.0.join("d")).unwrap();
        let swapper = Swapper::start(dest.0.join("d"), outside.0.clone());
        let _ = extract_all(&archive, &dest.0, &ExtractOptions::default());
        swaps += swapper.finish();
        assert_eq!(escaped(&outside.0), 0, "a write escaped the root");
    }
    assert!(swaps > 0, "the swapper never ran");
    eprintln!("cap-std extractor: 30 attempts under {swaps} swaps, 0 files escaped");
}

/// Extracting everything decodes each frame about once — files, and the
/// directories and symlinks finished after them included.
#[test]
fn extracting_everything_decodes_each_frame_about_once() {
    let mut members = Vec::new();
    for d in 0..30 {
        let mut dir = Entry::new(Kind::Directory, format!("t/d{d:02}").into_bytes());
        dir.mode = 0o755;
        members.push((dir, vec![]));
        for f in 0..10 {
            members.push(file(&format!("t/d{d:02}/f{f}"), &vec![b'a' + f as u8; 700]));
        }
        members.push(symlink(&format!("t/d{d:02}/link"), "f0"));
    }
    let mut top = Entry::new(Kind::Directory, "t");
    top.mode = 0o755;
    members.insert(0, (top, vec![]));
    let archive = archive_of(members);
    let dest = Scratch::new("decode-once");
    extract_all(&archive, &dest.0, &ExtractOptions::default()).unwrap();
    let ratio = archive.bytes_decoded() as f64 / archive.tar_stream_size() as f64;
    assert!(
        ratio <= 1.5,
        "extraction decoded {ratio:.1}x the tar stream"
    );
}
