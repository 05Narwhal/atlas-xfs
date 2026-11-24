// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! Fuzzes mounting an XFS volume and walking it.
//!
//! The input is the volume. It is mounted, listed, looked up, described and
//! read within bounds. A damaged volume can loop or point anywhere, so the walk
//! is bounded in depth and in total entries. Errors are fine; a panic is the
//! bug.

#![no_main]

use std::sync::Arc;

use atlas_xfs as xfs;
use atlas_fs_core::fs::{
    DirBatch, DirProgress, FileSystem, MAX_ALLOCATION_BUCKETS, NodeId, Stream,
};
use atlas_fs_core::io::block::{BlockSource, MemSource};
use libfuzzer_sys::fuzz_target;

const BATCH: usize = 64;
const MAX_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 2000;
const READ_LIMIT: u64 = 256 * 1024;
const STEP: usize = 64 * 1024 + 1;

fuzz_target!(|data: &[u8]| {
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(data));
    let Ok(volume) = xfs::mount(source) else {
        return;
    };
    let _ = volume.info();
    let root = volume.root();
    let _ = volume.metadata(root);
    let _ = volume.allocation_map(MAX_ALLOCATION_BUCKETS);
    let _ = volume.allocation_map(3);
    // Ids a caller could not have been given must fail cleanly.
    for node in [1u64, 64, 128, 4096, u64::MAX] {
        let _ = volume.metadata(NodeId(node));
        let _ = volume.read_dir(NodeId(node));
        let _ = volume.open(NodeId(node), Stream::Data);
    }
    let mut budget = MAX_ENTRIES;
    walk(&volume, root, 0, &mut budget);
});

fn walk(volume: &xfs::Xfs, dir: NodeId, depth: usize, budget: &mut usize) {
    let Ok(mut stream) = volume.read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<NodeId> = Vec::new();
    let mut batch = DirBatch::new();
    loop {
        batch.clear();
        let progress = stream.next_batch(&mut batch, BATCH);
        for entry in batch.iter() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let _ = volume.lookup(dir, entry.name);
            let _ = volume.metadata(entry.node);
            let _ = volume.extents(entry.node, Stream::Data);
            read_some(volume, entry.node);
            if entry.is_browsable() {
                subdirs.push(entry.node);
            }
        }
        match progress {
            Ok(DirProgress::More) => {}
            Ok(DirProgress::Done) | Err(_) => break,
        }
    }
    if depth >= MAX_DEPTH {
        return;
    }
    for sub in subdirs {
        if *budget == 0 {
            return;
        }
        walk(volume, sub, depth + 1, budget);
    }
}

fn read_some(volume: &xfs::Xfs, node: NodeId) {
    let Ok(content) = volume.open(node, Stream::Data) else {
        return;
    };
    let end = content.len().min(READ_LIMIT);
    let mut buf = vec![0u8; STEP];
    let mut pos = 0;
    while pos < end {
        let n = usize::try_from(end - pos).map_or(STEP, |left| left.min(STEP));
        if content.read_at(pos, &mut buf[..n]).is_err() {
            break;
        }
        pos += n as u64;
    }
}
