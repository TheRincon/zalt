//! Every archive the writer produces, judged by software that is not this
//! crate: libzstd's reference seekable reader, the probe's independent reading
//! of the Central Index, and `zstd(1)` and `tar`.
//!
//! §14.1 defines a conformant writer by exactly these: its output must pass
//! `zstd -t`, `zstd -dc | tar -t` and an indexed round trip. A test that only
//! read archives back through `zalt-archive`'s own reader would prove the
//! writer and reader agree — which a shared misreading of the spec satisfies.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use zalt_archive::read::Archive;
use zalt_archive::{Entry, FramePolicy, Kind, ReadLimits, Writer, WriterOptions};
use zlta_seekable_probe::{index as probe_index, seekable::RefSeekable, stock};

fn pseudo(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|i| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if i % 4 == 0 {
                b' '
            } else {
                b'a' + (s >> 27) as u8
            }
        })
        .collect()
}

/// A tree with every placement and kind, paths the probe (UTF-8 only) can read.
fn members() -> Vec<(Entry, Vec<u8>)> {
    let mut out = Vec::new();
    let mut dir = Entry::new(Kind::Directory, "proj");
    dir.mode = 0o755;
    out.push((dir, vec![]));
    for i in 0..40 {
        let content = pseudo(300 + i * 97, i as u32);
        let mut e = Entry::new(Kind::File, format!("proj/src/file_{i:03}.txt").as_bytes());
        e.size = content.len() as u64;
        e.mtime_sec = 1_700_000_000 + i as i64;
        out.push((e, content));
    }
    for (name, n) in [("proj/medium.bin", 30_000), ("proj/large.bin", 200_000)] {
        let content = pseudo(n, n as u32);
        let mut e = Entry::new(Kind::File, name.as_bytes());
        e.size = content.len() as u64;
        out.push((e, content));
    }
    let mut link = Entry::new(Kind::Symlink, "proj/latest");
    link.link = Some(b"src/file_000.txt".to_vec());
    out.push((link, vec![]));
    let mut hard = Entry::new(Kind::HardLink, "proj/again.txt");
    hard.link = Some(b"proj/src/file_001.txt".to_vec());
    out.push((hard, vec![]));
    let deep = format!("proj/{}/end.txt", "nested_directory".repeat(12));
    let mut e = Entry::new(Kind::File, deep.as_bytes());
    e.size = 4;
    out.push((e, b"deep".to_vec()));
    let mut e = Entry::new(Kind::File, "proj/données/résumé.txt".as_bytes());
    e.size = 6;
    out.push((e, b"accent".to_vec()));
    let mut e = Entry::new(Kind::File, "proj/empty");
    e.mtime_nsec = 5;
    out.push((e, vec![]));
    out
}

/// Small frames, so dozens of frames and boundaries come from a few hundred KB.
fn options(level: i32) -> WriterOptions {
    WriterOptions {
        level,
        policy: FramePolicy {
            small_file_threshold: 8 << 10,
            solid_target: 16 << 10,
            large_chunk: 32 << 10,
        },
        threads: 4,
        creator: None,
        hash: zalt_archive::Hash::default(),
    }
}

fn write(members: &[(Entry, Vec<u8>)], options: WriterOptions) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), options).unwrap();
    for (e, c) in members {
        w.add(e.clone(), &mut c.as_slice()).unwrap();
    }
    w.finish().unwrap().0
}

fn open(bytes: &[u8]) -> Archive<[u8]> {
    Archive::open(bytes.to_vec().into_boxed_slice(), ReadLimits::default()).unwrap()
}

#[test]
fn the_reference_seekable_reader_reads_every_archive() {
    for (name, members, level) in [
        ("tree, level 3", members(), 3),
        ("tree, level 19", members(), 19),
        ("empty", vec![], 3),
    ] {
        let bytes = write(&members, options(level));
        reference_reader_agrees(name, &bytes);
    }
}

/// libzstd's reference seekable reader and ours derive the same frame map
/// from `bytes`, and read the same tar stream from it, three ways.
fn reference_reader_agrees(name: &str, bytes: &[u8]) {
    {
        let bytes = bytes.to_vec();
        let ours = open(&bytes);
        let mut theirs = RefSeekable::open(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));

        // The same frame map, derived independently from the same table.
        assert_eq!(theirs.num_frames() as usize, ours.frames().len(), "{name}");
        for (i, f) in ours.frames().iter().enumerate() {
            let i = i as u32;
            assert_eq!(theirs.frame_c_offset(i), f.file_offset, "{name}: frame {i}");
            assert_eq!(
                theirs.frame_c_size(i),
                f.compressed_size,
                "{name}: frame {i}"
            );
            assert_eq!(
                theirs.frame_d_offset(i),
                f.stream_offset,
                "{name}: frame {i}"
            );
            assert_eq!(
                theirs.frame_d_size(i),
                f.decompressed_size,
                "{name}: frame {i}"
            );
        }
        assert_eq!(theirs.stream_len(), ours.tar_stream_size(), "{name}");

        // The same tar stream, read three ways: libzstd's seekable reader,
        // libzstd's plain streaming decoder, and ours.
        let stream = stock::decompress_stream(&bytes).unwrap();
        assert_eq!(
            theirs.read_at(0, stream.len()).unwrap(),
            stream,
            "{name}: seekable"
        );
        assert_eq!(
            ours.read_range(0, stream.len() as u64).unwrap(),
            stream,
            "{name}: ours"
        );

        // Every frame, and ranges in a fixed pseudo-random order.
        for i in 0..theirs.num_frames() {
            theirs
                .decompress_frame(i)
                .unwrap_or_else(|e| panic!("{name}: frame {i}: {e}"));
        }
        let mut s = 0x5EEDu64;
        for _ in 0..200 {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let start = (s >> 33) % stream.len() as u64;
            let len = 1 + ((s >> 13) % 70_000).min(stream.len() as u64 - start - 1);
            let got = theirs.read_at(start, len as usize).unwrap();
            assert_eq!(
                got,
                &stream[start as usize..(start + len) as usize],
                "{name}: {start}+{len}"
            );
        }
    }
}

#[test]
fn an_independent_reading_of_section_7_agrees_with_our_index() {
    index_agrees("tree, level 9", &write(&members(), options(9)));
}

/// The probe's independent decoder of §7 reads the same records from
/// `bytes`'s Central Index as ours does.
fn index_agrees(name: &str, bytes: &[u8]) {
    let ours = open(bytes);
    let last = *ours.frames().last().unwrap();
    let frame =
        &bytes[last.file_offset as usize..(last.file_offset + last.compressed_size) as usize];
    let theirs = probe_index::decode(&frame[8..], 64 << 20)
        .unwrap_or_else(|e| panic!("{name}: the probe cannot read our Central Index: {e}"));

    assert_eq!(theirs.tar_stream_size, ours.tar_stream_size());
    assert_eq!(theirs.entries.len(), ours.entries().len());
    for (t, o) in theirs.entries.iter().zip(ours.entries()) {
        assert_eq!(t.typeflag, o.kind.typeflag());
        assert_eq!(t.path.as_bytes(), o.path.as_slice());
        assert_eq!(t.header_offset, o.header_offset, "{}", t.path);
        assert_eq!(t.data_offset(), o.data_offset, "{}", t.path);
        assert_eq!(t.size, o.size, "{}", t.path);
        assert_eq!(
            t.link.as_deref().map(str::as_bytes),
            o.link.as_deref(),
            "{}",
            t.path
        );
        assert_eq!(t.mode, o.mode, "{}", t.path);
        assert_eq!(
            (t.mtime_sec, t.mtime_nsec),
            (o.mtime_sec, o.mtime_nsec),
            "{}",
            t.path
        );
        assert_eq!(t.hash, o.hash, "{}", t.path);
    }

    // Every digest is the hash of the content as libzstd decodes it — read
    // past our reader entirely, and hashed by the BLAKE3 authors' crate or
    // xxHash's own C.
    let stream = stock::decompress_stream(bytes).unwrap();
    for t in &theirs.entries {
        let Some(digest) = &t.hash else { continue };
        let from = t.data_offset() as usize;
        let content = &stream[from..from + t.size as usize];
        let expect = match theirs.hash_algorithm {
            1 => zlta_seekable_probe::xxh3::xxh3_128(content).to_vec(),
            2 => blake3::hash(content).as_bytes().to_vec(),
            n => panic!("{name}: Hash_Algorithm {n} written"),
        };
        assert_eq!(digest, &expect, "{name}: {}", t.path);
    }
}

/// The same, for an archive hashed with XXH3-128 rather than the default.
#[test]
fn an_independent_reading_agrees_on_xxh3_hashes() {
    let o = WriterOptions {
        hash: zalt_archive::Hash::Xxh3,
        ..options(3)
    };
    index_agrees("xxh3", &write(&members(), o));
}

// ------------------------------------------------------------ stock tools

fn tool(candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|t| {
            Command::new(t)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .map(|t| t.to_string())
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        let _ = Command::new("chflags")
            .args(["-R", "0"])
            .arg(&self.0)
            .status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// `zstd -t`, `zstd -dc | tar -t` and `tar -x` (§14.1).
///
/// GNU tar, where it is installed, because macOS's bsdtar lists NFD paths
/// in NFC — a real finding about §14.1's "with bsdtar", recorded in the spec
/// notes, and not something this test should paper over by normalizing.
/// Where only bsdtar exists the listing is compared on ASCII paths alone, and
/// the test says so.
#[test]
fn stock_zstd_and_tar_read_our_archives() {
    let Some(zstd) = tool(&["zstd"]) else {
        eprintln!("SKIPPED: zstd(1) is not on PATH");
        return;
    };
    let Some(tar) = tool(&["gtar", "tar"]) else {
        eprintln!("SKIPPED: no tar on PATH");
        return;
    };
    let gnu = String::from_utf8_lossy(&run(Command::new(&tar).arg("--version"))).contains("GNU");

    let dir =
        Scratch(std::env::temp_dir().join(format!("zalt-archive-oracle-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let members = members();
    let path = dir.0.join("a.tar.zst");
    std::fs::write(&path, write(&members, options(19))).unwrap();

    run(Command::new(&zstd).args(["-q", "-t"]).arg(&path));

    let stream = run(Command::new(&zstd).args(["-q", "-dc"]).arg(&path));
    let mut list = Command::new(&tar)
        .arg("-tf")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    list.stdin.take().unwrap().write_all(&stream).unwrap();
    let listing = list.wait_with_output().unwrap();
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let listed: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(|l| l.trim_end_matches('/').to_string())
        .collect();
    let ours: Vec<String> = members
        .iter()
        .map(|(e, _)| e.path_lossy().into_owned())
        .collect();
    if gnu {
        assert_eq!(listed, ours);
    } else {
        eprintln!("note: bsdtar normalizes non-ASCII paths; comparing ASCII paths only");
        let ascii = |v: &[String]| {
            v.iter()
                .filter(|p| p.is_ascii())
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(ascii(&listed), ascii(&ours));
    }

    let out = dir.0.join("x");
    std::fs::create_dir_all(&out).unwrap();
    run(Command::new(&tar)
        .arg("--use-compress-program")
        .arg(format!("{zstd} -d"))
        .arg("-xf")
        .arg(&path)
        .arg("-C")
        .arg(&out));
    for (e, content) in &members {
        if e.kind == Kind::File && e.path.is_ascii() {
            let got = std::fs::read(out.join(Path::new(&*e.path_lossy()))).unwrap();
            assert_eq!(&got, content, "{}", e.path_lossy());
        }
    }
    let link = std::fs::read_link(out.join("proj/latest")).unwrap();
    assert_eq!(link, Path::new("src/file_000.txt"));
}

// ----------------------------------------------- macOS metadata and bsdtar

/// §9.1's macOS keywords, held to what bsdtar — libarchive, the reference for
/// every one of them — writes and reads. Both directions: bsdtar restores
/// what we archived, and we read what bsdtar archived.
#[cfg(all(target_os = "macos", feature = "native-fs"))]
mod bsdtar_metadata {
    use super::*;
    use zalt_archive::fs::{CreateOptions, add_tree_with};
    use zalt_archive::meta::{self, AceType, flag};
    use zalt_archive::tar::{self, BLOCK, pad};

    const BSDTAR: &str = "/usr/bin/tar";

    /// A file with text and awkward binary xattrs, an ACL with a deny entry,
    /// and flags; a directory with an inheriting ACL.
    fn source(dir: &Path) -> PathBuf {
        let root = dir.join("src");
        std::fs::create_dir_all(root.join("shared")).unwrap();
        let f = root.join("f.txt");
        std::fs::write(&f, b"content\n").unwrap();
        xattr::set(&f, "com.zalt.text", b"plain value").unwrap();
        xattr::set(&f, "com.zalt.bin", b"\x00\xff\n=\x10").unwrap();
        let mut deny = exacl::AclEntry::allow_user("root", exacl::Perm::DELETE, None);
        deny.allow = false;
        exacl::setfacl(
            &[&f],
            &[
                deny,
                exacl::AclEntry::allow_group(
                    "staff",
                    exacl::Perm::EXECUTE | exacl::Perm::APPEND,
                    None,
                ),
            ],
            None,
        )
        .unwrap();
        exacl::setfacl(
            &[root.join("shared")],
            &[exacl::AclEntry::allow_group(
                "everyone",
                exacl::Perm::READ,
                exacl::Flag::FILE_INHERIT | exacl::Flag::DIRECTORY_INHERIT,
            )],
            None,
        )
        .unwrap();
        nix::unistd::chflags(
            &f,
            nix::sys::stat::FileFlag::UF_HIDDEN | nix::sys::stat::FileFlag::UF_NODUMP,
        )
        .unwrap();
        root
    }

    fn acl(path: &Path) -> String {
        exacl::to_string(&exacl::getfacl(path, None).unwrap()).unwrap()
    }

    fn flags(path: &Path) -> u32 {
        std::os::macos::fs::MetadataExt::st_flags(&std::fs::symlink_metadata(path).unwrap())
    }

    fn scratch(tag: &str) -> Scratch {
        let dir = Scratch(
            std::env::temp_dir().join(format!("zalt-archive-bsdtar-{tag}-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        dir
    }

    #[test]
    fn bsdtar_restores_the_metadata_we_archive() {
        let Some(zstd) = tool(&["zstd"]) else {
            eprintln!("SKIPPED: zstd(1) is not on PATH");
            return;
        };
        let dir = scratch("ours");
        let root = source(&dir.0);
        let mut w = Writer::new(Vec::new(), options(3)).unwrap();
        add_tree_with(&mut w, &root, &CreateOptions::default(), &mut |m| {
            panic!("{m}")
        })
        .unwrap();
        let path = dir.0.join("a.tar.zst");
        std::fs::write(&path, w.finish().unwrap().0).unwrap();

        let out = dir.0.join("x");
        std::fs::create_dir_all(&out).unwrap();
        run(Command::new(BSDTAR)
            .arg("--use-compress-program")
            .arg(format!("{zstd} -d"))
            .args(["-x", "--xattrs", "--acls", "--fflags", "-f"])
            .arg(&path)
            .arg("-C")
            .arg(&out));
        let got = out.join(root.strip_prefix("/").unwrap());

        let f = got.join("f.txt");
        assert_eq!(
            xattr::get(&f, "com.zalt.text").unwrap().unwrap(),
            b"plain value"
        );
        assert_eq!(
            xattr::get(&f, "com.zalt.bin").unwrap().unwrap(),
            b"\x00\xff\n=\x10"
        );
        assert_eq!(acl(&f), acl(&root.join("f.txt")));
        assert_eq!(acl(&got.join("shared")), acl(&root.join("shared")));
        assert_eq!(flags(&f), (flag::UF_HIDDEN | flag::UF_NODUMP) as u32);
    }

    /// Every member of a plain tar stream, as our parser reads it.
    fn members_of(stream: &[u8]) -> Vec<tar::Member> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + BLOCK <= stream.len() && stream[pos..pos + BLOCK].iter().any(|&b| b != 0) {
            let m = tar::parse_member(&stream[pos..]).unwrap();
            pos += (m.header_len + pad(m.size)) as usize;
            out.push(m);
        }
        out
    }

    #[test]
    fn we_read_the_metadata_bsdtar_archives() {
        let dir = scratch("theirs");
        let root = source(&dir.0);
        let stream = run(Command::new(BSDTAR)
            .args([
                "-c",
                "--format",
                "pax",
                "--no-mac-metadata",
                "--xattrs",
                "--acls",
                "--fflags",
            ])
            .arg("-f")
            .arg("-")
            .arg("-C")
            .arg(&root)
            .args(["f.txt", "shared"]));
        let members = members_of(&stream);
        let f = members.iter().find(|m| m.path == b"f.txt").unwrap();

        // bsdtar writes every xattr twice — SCHILY raw and LIBARCHIVE base64.
        // §9.2: both are read, and each attribute comes out once.
        let value = |name: &[u8]| {
            let found: Vec<_> = f.xattrs.iter().filter(|(n, _)| n == name).collect();
            assert_eq!(found.len(), 1, "{}", String::from_utf8_lossy(name));
            found[0].1.clone()
        };
        assert_eq!(value(b"com.zalt.text"), b"plain value");
        assert_eq!(value(b"com.zalt.bin"), b"\x00\xff\n=\x10");
        assert_eq!(f.fflags, Some(flag::UF_HIDDEN | flag::UF_NODUMP));
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .unwrap();
        assert_eq!(f.names.as_ref().unwrap().0, me.name.as_bytes());

        let aces = meta::parse_acl(f.acl.as_deref().unwrap()).unwrap();
        let extended: Vec<_> = aces.iter().filter(|a| !a.is_trivial()).collect();
        assert_eq!(extended.len(), 2);
        assert_eq!(
            (extended[0].qualifier.as_slice(), extended[0].kind),
            (&b"root"[..], AceType::Deny)
        );
        assert_eq!(extended[0].id, Some(0));
        let shared = members.iter().find(|m| m.path == b"shared").unwrap();
        let inherit = meta::parse_acl(shared.acl.as_deref().unwrap()).unwrap();
        assert_eq!(inherit[0].flags & 0b11, 0b11, "file and directory inherit");
    }
}

// ------------------------------------------------------- R2: append, recover

/// `members` written, then `more` appended through a real file.
fn appended(members: &[(Entry, Vec<u8>)], more: &[(Entry, Vec<u8>)], level: i32) -> Vec<u8> {
    let path =
        std::env::temp_dir().join(format!("zalt-oracle-append-{}-{level}", std::process::id()));
    std::fs::write(&path, write(members, options(level))).unwrap();
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut w = zalt_archive::append(file, options(level)).unwrap();
    for (e, content) in more {
        w.add(e.clone(), &mut content.as_slice()).unwrap();
    }
    let (out, _) = w.finish().unwrap();
    drop(out);
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

/// `bytes` with its trailer cut off at the Central Index, recovered.
fn recovered(bytes: &[u8]) -> Vec<u8> {
    let a = open(bytes);
    let index = a.frames()[a.frames().len() - 1].file_offset as usize;
    let (fixed, report) = zalt_archive::recover(&bytes[..index], Vec::new(), options(3)).unwrap();
    assert!(report.terminator_found, "{report:?}");
    fixed
}

#[test]
fn appended_and_recovered_archives_satisfy_the_reference_reader() {
    let all = members();
    let (first, rest) = all.split_at(all.len() / 2);
    let grown = appended(first, rest, 19);
    reference_reader_agrees("appended", &grown);
    index_agrees("appended", &grown);
    let fixed = recovered(&grown);
    reference_reader_agrees("recovered", &fixed);
    index_agrees("recovered", &fixed);
    assert_eq!(open(&fixed).entries(), open(&grown).entries());
}

/// An archive libzstd wrote, with vendor frames between its data frames (Q7),
/// as the probe's own writer lays it out.
fn libzstd_archive() -> (Vec<u8>, usize) {
    let mut t = zlta_seekable_probe::tar::TarWriter::new();
    t.add_dir("lib", 0o755, 1_700_000_000);
    for i in 0..12 {
        t.add_file(
            &format!("lib/f{i:02}.dat"),
            &pseudo(700 * i + 100, i as u32),
            0o644,
            1_700_000_000,
        );
    }
    t.add_file("lib/big.dat", &pseudo(40_000, 99), 0o644, 1_700_000_000);
    t.add_symlink("lib/latest", "f00.dat", 1_700_000_000);
    let (body, entries) = t.finish();
    let opts = zlta_seekable_probe::archive::WriterOptions {
        solid_target: 4096,
        chunk: 8192,
        vendor_every: Some(2),
        ..Default::default()
    };
    let mut enc = zlta_seekable_probe::encoder::ReferenceEncoder::new(19);
    let a = zlta_seekable_probe::archive::write(&body, &entries, &mut enc, &opts).unwrap();
    (a.bytes, entries.len())
}

#[test]
fn archives_libzstd_wrote_can_be_recovered_and_appended_to() {
    let (bytes, count) = libzstd_archive();
    let theirs = open(&bytes);
    assert_eq!(theirs.entries().len(), count);
    let vendor = theirs
        .frames()
        .iter()
        .filter(|f| f.magic == 0x184D_2A5F)
        .count();
    assert!(vendor > 0, "the vendor frames are there to be walked past");

    let fixed = recovered(&bytes);
    let a = open(&fixed);
    a.verify().unwrap();
    assert_eq!(a.entries().len(), count);
    for (x, y) in a.entries().iter().zip(theirs.entries()) {
        assert_eq!(
            a.entry_bytes(x).unwrap(),
            theirs.entry_bytes(y).unwrap(),
            "{}",
            x.path_lossy()
        );
    }
    reference_reader_agrees("libzstd archive, recovered", &fixed);

    let path = std::env::temp_dir().join(format!("zalt-oracle-libzstd-{}", std::process::id()));
    std::fs::write(&path, &bytes).unwrap();
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut w = zalt_archive::append(file, options(3)).unwrap();
    for (e, content) in members().iter().take(4) {
        w.add(e.clone(), &mut content.as_slice()).unwrap();
    }
    drop(w.finish().unwrap());
    let grown = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    open(&grown).verify().unwrap();
    reference_reader_agrees("libzstd archive, appended", &grown);
}

/// zalt-codec's frame walk against libzstd's `ZSTD_findFrameCompressedSize`,
/// on every frame of archives both encoders wrote — whole, and cut short.
#[test]
fn frame_lengths_agree_with_libzstd() {
    let all = members();
    let (first, rest) = all.split_at(all.len() / 2);
    let archives = [
        write(&all, options(3)),
        write(&all, options(19)),
        appended(first, rest, 1),
        libzstd_archive().0,
    ];
    let mut checked = 0;
    for bytes in &archives {
        let mut off = 0usize;
        while off < bytes.len() {
            let (_, theirs) = zlta_seekable_probe::frame::inspect(&bytes[off..]).unwrap();
            let ours = zalt_codec::frame::compressed_size(&bytes[off..]).unwrap();
            assert_eq!(ours as u64, theirs, "frame at {off}");
            let end = off + ours;
            for cut in [end - 1, end - 2, off + 4, off + 6, off + (ours / 2)] {
                if cut <= off || cut >= end {
                    continue;
                }
                assert!(
                    zalt_codec::frame::compressed_size(&bytes[off..cut]).is_err(),
                    "cut {cut}"
                );
                assert!(
                    zlta_seekable_probe::frame::inspect(&bytes[off..cut]).is_err(),
                    "cut {cut}"
                );
            }
            off = end;
            checked += 1;
        }
    }
    assert!(checked > 40, "{checked} frames");
}
