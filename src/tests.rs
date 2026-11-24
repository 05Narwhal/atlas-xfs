// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

use std::collections::BTreeSet;

use super::test_image::{MTIME, Patched, load, pat};
use super::*;
use atlas_fs_core::fs::Timestamp;

fn mounted(name: &str) -> Xfs {
    mount(load(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn list(fs: &Xfs, dir: NodeId) -> Vec<(String, NodeId, NodeKind, Option<u64>, EntryFlags)> {
    let mut stream = fs.read_dir(dir).unwrap();
    let mut batch = DirBatch::default();
    let mut out = Vec::new();
    loop {
        batch.clear();
        let progress = stream.next_batch(&mut batch, 7).unwrap();
        out.extend(
            batch
                .iter()
                .map(|e| (e.name.to_owned(), e.node, e.kind, e.size, e.flags)),
        );
        if progress == DirProgress::Done {
            return out;
        }
    }
}

fn names(fs: &Xfs, dir: NodeId) -> BTreeSet<String> {
    list(fs, dir).into_iter().map(|e| e.0).collect()
}

fn walk_to(fs: &Xfs, path: &str) -> NodeId {
    let mut node = fs.root();
    for part in path.split('/') {
        node = fs
            .lookup(node, part)
            .unwrap()
            .unwrap_or_else(|| panic!("{path}: no {part}"));
    }
    node
}

fn read_all(fs: &Xfs, path: &str) -> Vec<u8> {
    let source = fs.open(walk_to(fs, path), Stream::Data).unwrap();
    let mut buf = vec![0u8; usize::try_from(source.len()).unwrap()];
    source.read_at(0, &mut buf).unwrap();
    buf
}

const MAIN: [&str; 2] = ["v4_main", "v5_main"];
const ALL: [&str; 4] = ["v4_main", "v4_tiny", "v5_main", "v5_tiny"];

#[test]
fn both_versions_list_the_root_and_read_files() {
    for name in MAIN {
        let fs = mounted(name);
        let info = fs.info();
        assert_eq!(info.name, "atlas", "{name}");
        assert_eq!(info.format, Some("XFS"));
        assert_eq!(info.total_bytes, Some(300 << 20));
        assert!(fs.identity().is_some());
        assert!(fs.superblock_checksum_ok());
        assert!(!fs.case_insensitive());

        let root = list(&fs, fs.root());
        let by_name = |n: &str| {
            root.iter()
                .find(|e| e.0 == n)
                .unwrap_or_else(|| panic!("{n}"))
        };
        assert_eq!(by_name("hello.txt").2, NodeKind::File);
        assert_eq!(by_name("hello.txt").3, Some(10));
        assert_eq!(by_name("sf").2, NodeKind::Directory);
        assert_eq!(by_name("sf").3, None);
        assert_eq!(by_name("fifo").2, NodeKind::Other);
        assert_eq!(by_name("null").2, NodeKind::Other);
        assert_eq!(by_name("empty").3, Some(0));
        assert_eq!(root.len(), 17, "{name}: {:?}", names(&fs, fs.root()));

        let hello = walk_to(&fs, "hello.txt");
        assert_eq!(read_all(&fs, "hello.txt"), b"hello xfs\n");
        assert_eq!(walk_to(&fs, "hello-link.txt"), hello);
        assert_eq!(fs.link_count(hello).unwrap(), 2 + 40 + 300 + 1300);
        let meta = fs.metadata(hello).unwrap();
        assert_eq!(
            meta.modified,
            Some(Timestamp::from_unix(MTIME, 123_456_789))
        );
        assert_eq!(meta.size, Some(10));
        assert_eq!(meta.created.is_some(), name == "v5_main");
        assert!(meta.owner.is_some_and(|o| o.mode & 0o170_000 == 0o100_000));

        assert_eq!(read_all(&fs, "big.bin"), pat(100_000, 3));
        assert_eq!(read_all(&fs, "sf/c/d.txt"), b"delta");
        assert_eq!(read_all(&fs, "unicode/héllo wörld.txt"), b"unicode");
        assert_eq!(read_all(&fs, "deep/1/2/3/4/5/6/7/8/9/file.txt"), b"deep");
        assert_eq!(read_all(&fs, "empty"), b"");
        let long = "L".repeat(255);
        assert_eq!(
            names(&fs, walk_to(&fs, "longname")),
            BTreeSet::from([long.clone()])
        );
        assert_eq!(read_all(&fs, &format!("longname/{long}")), b"long name");

        assert!(matches!(
            fs.open(hello, Stream::Resource),
            Err(Error::NoSuchStream { .. })
        ));
        assert!(matches!(
            fs.open(fs.root(), Stream::Data),
            Err(Error::IsADirectory { .. })
        ));
        assert!(matches!(
            fs.open(walk_to(&fs, "fifo"), Stream::Data),
            Err(Error::NoSuchStream { .. })
        ));
        assert!(matches!(
            fs.metadata(NodeId(u64::MAX)),
            Err(Error::NotFound { .. })
        ));
        assert_eq!(fs.lookup(fs.root(), "nope").unwrap(), None);
        assert!(matches!(
            fs.lookup(hello, "x"),
            Err(Error::NotADirectory { .. })
        ));
    }
}

#[test]
fn symbolic_links_of_every_kind() {
    for name in ALL {
        let fs = mounted(name);
        let target = |path: &str| {
            let meta = fs.metadata(walk_to(&fs, path)).unwrap();
            assert_eq!(meta.kind, NodeKind::Symlink, "{name} {path}");
            meta.link_target.unwrap()
        };
        assert_eq!(target("links/short"), "../hello.txt");
        assert_eq!(target("links/dirlink"), "../sf");
        assert_eq!(target("links/dangling"), "nowhere/at/all");
        let long = ["abcdefgh"; 44].join("/");
        assert_eq!(target("links/long"), long);
        assert_eq!(read_all(&fs, "links/long"), long.as_bytes());
        assert_eq!(read_all(&fs, "links/short"), b"../hello.txt");

        let links = list(&fs, walk_to(&fs, "links"));
        let flag = |n: &str| links.iter().find(|e| e.0 == n).unwrap().4;
        assert!(
            flag("dirlink").contains(EntryFlags::LINKS_TO_DIRECTORY),
            "{name}"
        );
        assert!(!flag("short").contains(EntryFlags::LINKS_TO_DIRECTORY));
        assert!(!flag("dangling").contains(EntryFlags::LINKS_TO_DIRECTORY));
        // Metadata of a link found through its folder, and a listing of it.
        let dirlink = walk_to(&fs, "links/dirlink");
        assert!(
            fs.metadata(dirlink)
                .unwrap()
                .flags
                .contains(EntryFlags::LINKS_TO_DIRECTORY)
        );
        assert_eq!(
            names(&fs, dirlink),
            BTreeSet::from(["a.txt".to_owned(), "b.txt".to_owned(), "c".to_owned()])
        );
        let dangling = walk_to(&fs, "links/dangling");
        assert!(matches!(
            fs.read_dir(dangling),
            Err(Error::NotADirectory { .. })
        ));
    }
}

#[test]
fn every_directory_form_lists_every_entry() {
    for name in ALL {
        let fs = mounted(name);
        let blk: BTreeSet<String> = (0..40).map(|i| format!("blk-{i:04}")).collect();
        assert_eq!(names(&fs, walk_to(&fs, "blk")), blk, "{name}");
        assert_eq!(names(&fs, walk_to(&fs, "sf")).len(), 3);
    }
    for name in MAIN {
        let fs = mounted(name);
        let leaf: BTreeSet<String> = (0..300)
            .map(|i| format!("leaf-entry-with-a-long-name-{i:04}"))
            .collect();
        assert_eq!(names(&fs, walk_to(&fs, "leaf")), leaf, "{name}");
        let node: BTreeSet<String> = (0..1300).map(|i| format!("n{i:05}")).collect();
        let listed = list(&fs, walk_to(&fs, "node"));
        assert_eq!(listed.len(), 1300, "{name}");
        assert_eq!(
            listed.into_iter().map(|e| e.0).collect::<BTreeSet<_>>(),
            node
        );
        // The forms are what the names say: the folder is in the inode, or
        // its blocks have a hash index 32 GiB into the file.
        let mut ibuf = Vec::new();
        let sf = fs
            .inner
            .load_inode(walk_to(&fs, "sf").0, &mut ibuf)
            .unwrap();
        assert_eq!(sf.format, inode::Format::Local, "{name}");
        for dir in ["leaf", "node"] {
            let core = fs.inner.load_inode(walk_to(&fs, dir).0, &mut ibuf).unwrap();
            let runs = fs.inner.load_runs(&core, &ibuf).unwrap();
            let leaf_fsb = (1u64 << 35) >> fs.inner.geo.block_size.trailing_zeros();
            assert!(runs.iter().any(|r| r.off >= leaf_fsb), "{name} {dir}");
            assert!(runs.iter().any(|r| r.off < leaf_fsb), "{name} {dir}");
        }
        assert!(fs.lookup(walk_to(&fs, "node"), "n01299").unwrap().is_some());
        assert!(fs.lookup(walk_to(&fs, "node"), "n01300").unwrap().is_none());
    }
}

#[test]
fn fragmented_unwritten_and_sparse_files_read_correctly() {
    for name in MAIN {
        let fs = mounted(name);
        // 600 extents with a hole after each: the extent B-tree.
        let frag = read_all(&fs, "frag.bin");
        assert_eq!(frag.len(), 1199 * 4096, "{name}");
        for block in 0..1199usize {
            let got = &frag[block * 4096..(block + 1) * 4096];
            if block % 2 == 0 {
                assert_eq!(got, pat(4096, (block & 0xFF) as u8), "{name} block {block}");
            } else {
                assert!(got.iter().all(|&b| b == 0), "{name} block {block}");
            }
        }
        let extents = fs.extents(walk_to(&fs, "frag.bin"), Stream::Data).unwrap();
        // The kernel that filled the version 5 image merged most of them.
        assert_eq!(
            extents.len() == 600,
            name == "v4_main",
            "{name}: {}",
            extents.len()
        );
        assert!(extents.iter().map(|e| e.len).sum::<u64>() >= 600 * 4096);
        let mut ibuf = Vec::new();
        let core = fs
            .inner
            .load_inode(walk_to(&fs, "frag.bin").0, &mut ibuf)
            .unwrap();
        assert_eq!(core.format, inode::Format::Btree, "{name}");

        // Preallocated, the first block written: the rest is unwritten.
        let unwritten = read_all(&fs, "unwritten.bin");
        assert_eq!(unwritten.len(), 65_536);
        assert_eq!(unwritten[..4096], pat(4096, 9)[..]);
        assert!(unwritten[4096..].iter().all(|&b| b == 0));
        let on_disk: u64 = fs
            .extents(walk_to(&fs, "unwritten.bin"), Stream::Data)
            .unwrap()
            .iter()
            .map(|e| e.len)
            .sum();
        // mkfs.xfs -p (how the version 4 images were filled) copies only the
        // data, so there the rest is a hole instead of unwritten extents.
        assert_eq!(
            on_disk,
            if name == "v5_main" { 65_536 } else { 4096 },
            "{name}"
        );

        // 5 GiB, sparse: reads across the hole and the tail.
        let huge = fs
            .open(walk_to(&fs, "huge-sparse.bin"), Stream::Data)
            .unwrap();
        let five_gib = 5u64 << 30;
        assert_eq!(huge.len(), five_gib);
        let mut tail = vec![0u8; 8192];
        huge.read_at(five_gib - 8192, &mut tail).unwrap();
        assert!(tail[..4096].iter().all(|&b| b == 0));
        assert_eq!(tail[4096..], pat(4096, 5)[..]);
        let mut head = vec![1u8; 4096];
        huge.read_at(4 << 30, &mut head).unwrap();
        assert!(head.iter().all(|&b| b == 0));
        assert!(huge.read_at(five_gib - 1, &mut [0u8; 2]).is_err());
    }
}

/// Lists, looks up, describes and reads what it can of `fs`, within bounds.
fn exercise(fs: &Xfs) {
    let mut budget = 300usize;
    let mut stack = vec![(fs.root(), 0usize)];
    let mut buf = vec![0u8; 8192];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(mut stream) = fs.read_dir(dir) else {
            continue;
        };
        let mut batch = DirBatch::default();
        loop {
            batch.clear();
            let progress = stream.next_batch(&mut batch, 64);
            for entry in batch.iter() {
                if budget == 0 {
                    return;
                }
                budget -= 1;
                let _ = fs.lookup(dir, entry.name);
                let _ = fs.metadata(entry.node);
                let _ = fs.extents(entry.node, Stream::Data);
                if let Ok(source) = fs.open(entry.node, Stream::Data) {
                    let n = usize::try_from(source.len()).unwrap_or(0).min(buf.len());
                    let _ = source.read_at(0, &mut buf[..n]);
                    let _ = source.read_at(source.len().saturating_sub(8), &mut buf[..8.min(n)]);
                }
                if entry.is_browsable() && depth < 4 {
                    stack.push((entry.node, depth + 1));
                }
            }
            match progress {
                Ok(DirProgress::More) => {}
                Ok(DirProgress::Done) | Err(_) => break,
            }
        }
    }
}

#[test]
fn damaged_volumes_fail_without_panicking() {
    // A small deterministic generator: the cases are the same every run.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut mounted_ok = 0;
    for (name, cases) in [("v4_tiny", 200), ("v5_tiny", 100), ("v4_main", 40)] {
        let base = load(name);
        let spans = base.spans();
        let total: usize = spans.iter().map(|s| s.1).sum();
        for case in 0..cases {
            let mut patches = Vec::new();
            for _ in 0..=(next() % 4) {
                // A byte of data that is not zero: metadata or file content.
                let mut pick = (next() % total as u64) as usize;
                for &(offset, len) in &spans {
                    if pick < len {
                        patches.push((offset + pick as u64, (next() % 255) as u8 + 1));
                        break;
                    }
                    pick -= len;
                }
            }
            // Every tenth case also cuts the image short.
            let len = if case % 10 == 9 {
                next() % base.len()
            } else {
                base.len()
            };
            let source = Arc::new(Patched {
                base: base.clone(),
                patches,
                len,
            });
            if let Ok(fs) = mount(source) {
                mounted_ok += 1;
                exercise(&fs);
            }
        }
    }
    // The damage often lands where it does no harm, so some still mount.
    assert!(mounted_ok > 0);
    // And so does damage to the very first bytes, and the empty source.
    for at in 0..232u64 {
        let source = Arc::new(Patched {
            base: load("v5_tiny"),
            patches: vec![(at, 0xFF)],
            len: 4096,
        });
        let _ = mount(source);
    }
    assert!(mount(Arc::new(MemSource::new(Vec::new()))).is_err());
    assert!(mount(Arc::new(MemSource::new(vec![0u8; 512]))).is_err());
}
