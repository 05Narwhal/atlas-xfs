// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! XFS, read-only: versions 4 and 5 (with checksums), shortform, block, leaf
//! and node directories, extent and B-tree data forks, big timestamps.
//!
//! A node id is the inode number. Metadata read from the disk is checked
//! before use: every length, offset and count, the checksum of version 5
//! blocks and inodes, and the owner and number each one records. The log is
//! never replayed, so a volume that was not unmounted cleanly may show the
//! state before its last changes.
//!
//! Not supported: realtime files, extended attributes, the free-space map
//! (so no shading on the disk strip), the metadata directory feature of the
//! newest kernels, and names that the hash index would find faster (lookups
//! scan the directory).
//!
//! ```no_run
//! use std::path::Path;
//! use std::sync::Arc;
//!
//! use atlas_fs_core::fs::{DirBatch, DirProgress, FileSystem};
//! use atlas_fs_core::io::file::FileSource;
//!
//! # fn main() -> Result<(), atlas_fs_core::error::Error> {
//! let source = Arc::new(FileSource::open(Path::new("volume.img"))?);
//! let volume = atlas_xfs::mount(source)?;
//! println!("{}", volume.info().name);
//!
//! let mut stream = volume.read_dir(volume.root())?;
//! let mut batch = DirBatch::new();
//! while stream.next_batch(&mut batch, 256)? == DirProgress::More {}
//! for entry in batch.iter() {
//!     println!("{}", entry.name);
//! }
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
// On-disk bytes are hostile. These lints make the usual ways of trusting
// them (unchecked indexing, arithmetic and casts, panicking shortcuts) fail
// the clippy gate. Unit tests and the test fixtures are exempt.
#![warn(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]
#![cfg_attr(
    any(test, feature = "test-util"),
    allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atlas_fs_core::error::{CorruptKind, Corruption, Error, Location, Result};
use atlas_fs_core::fs::{
    DirBatch, DirEntry, DirProgress, DirStream, EntryFlags, Extent, FileSystem, Metadata, NodeId,
    NodeKind, Owner, Stream, VolumeIdentity, VolumeInfo,
};
use atlas_fs_core::io::block::{BlockSource, MemSource, check_range};

mod bytes;
mod crc;
mod dir;
mod inode;
mod sb;

// Seed inputs for the fuzz targets; `volumes()` is shared with Disk Atlas, which adds the probe heads.
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub mod fuzz_seeds;
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
#[allow(dead_code, missing_docs)]
pub mod test_image;
#[cfg(test)]
mod tests;

use dir::{DirWalker, Step};
use inode::{Core, Run};
use sb::{Geometry, Superblock};

const STRUCTURE: &str = "XFS structure";
/// Directory blocks one `next_batch` call reads before it hands control back.
const DIR_BLOCKS_PER_CALL: u32 = 16;
/// Symbolic links remembered with the folder they were found in.
const MAX_PARENTS: usize = 65_536;
/// Path components, and links followed, when a link's target is resolved.
const MAX_COMPONENTS: u32 = 256;
const MAX_HOPS: u32 = 8;

pub(crate) fn other(what: &'static str) -> Error {
    Corruption::new(STRUCTURE, Location::Unspecified, CorruptKind::Other(what)).into()
}

pub(crate) fn invalid(field: &'static str, value: u64) -> Error {
    Corruption::new(
        STRUCTURE,
        Location::Unspecified,
        CorruptKind::InvalidValue { field, value },
    )
    .into()
}

pub(crate) fn out_of_range(
    field: &'static str,
    value: u64,
    limit_name: &'static str,
    limit: u64,
) -> Error {
    Corruption::new(
        STRUCTURE,
        Location::Unspecified,
        CorruptKind::OutOfRange {
            field,
            value,
            limit_name,
            limit,
        },
    )
    .into()
}

pub(crate) fn limit(what: &'static str, limit: u64) -> Error {
    Corruption::new(
        STRUCTURE,
        Location::Unspecified,
        CorruptKind::LimitExceeded { what, limit },
    )
    .into()
}

/// A version 5 checksum that does not match; `at` is a block or an inode.
pub(crate) fn bad_crc(structure: &'static str, at: u64) -> Error {
    Corruption::new(
        structure,
        Location::Object(at),
        CorruptKind::Other("checksum mismatch"),
    )
    .into()
}

/// What every handle, folder stream and open file shares.
struct Inner {
    source: Arc<dyn BlockSource>,
    geo: Geometry,
    /// Where symbolic links were found, so a link to a folder can be followed
    /// from the folder it sits in.
    parents: Mutex<HashMap<u64, u64>>,
}

/// A mounted XFS volume.
pub struct Xfs {
    inner: Arc<Inner>,
    info: VolumeInfo,
    identity: VolumeIdentity,
    crc_ok: bool,
}

const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<Xfs>();

/// Mounts the volume in `source`.
pub fn mount(source: Arc<dyn BlockSource>) -> Result<Xfs> {
    let want = usize::try_from(source.len().min(4096)).unwrap_or(4096);
    let mut head = vec![0u8; want];
    source.read_at(0, &mut head)?;
    let sb = Superblock::parse(&head)?;
    let inner = Inner {
        source,
        geo: sb.geo.clone(),
        parents: Mutex::new(HashMap::new()),
    };
    let mut buf = Vec::new();
    let root = inner.load_inode(sb.geo.root_ino, &mut buf)?;
    if !root.is_dir() {
        return Err(invalid("sb_rootino", sb.geo.root_ino));
    }
    let label_len = sb
        .label
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(sb.label.len());
    let label = String::from_utf8_lossy(sb.label.get(..label_len).unwrap_or(&[])).into_owned();
    let total = sb.geo.data_blocks.saturating_mul(sb.geo.block_size);
    let free = sb.free_blocks.min(sb.geo.data_blocks);
    let mut info = VolumeInfo::new(if label.is_empty() {
        "XFS volume".to_owned()
    } else {
        label
    });
    info.format = Some("XFS");
    info.total_bytes = Some(total);
    info.free_bytes = Some(free.saturating_mul(sb.geo.block_size));
    let mut change = [0u8; 16];
    change[..8].copy_from_slice(&sb.free_blocks.to_be_bytes());
    change[8..].copy_from_slice(&sb.icount.saturating_sub(sb.ifree).to_be_bytes());
    Ok(Xfs {
        inner: Arc::new(inner),
        info,
        identity: VolumeIdentity {
            id: sb.uuid,
            change,
        },
        crc_ok: !sb.crc_bad,
    })
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn kind_of_ftype(ftype: Option<u8>) -> NodeKind {
    match ftype {
        Some(1) => NodeKind::File,
        Some(2) => NodeKind::Directory,
        Some(7) => NodeKind::Symlink,
        _ => NodeKind::Other,
    }
}

fn name_eq(ci: bool, a: &[u8], b: &[u8]) -> bool {
    if ci {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

impl Inner {
    fn not_found(&self, node: u64, e: Error) -> Error {
        // An id that no inode can have is not a damaged disk.
        if self.geo.inode_offset(node).is_err() {
            Error::NotFound { node }
        } else {
            e
        }
    }

    /// The inode of a node a caller named.
    fn node_core(&self, node: u64, buf: &mut Vec<u8>) -> Result<Core> {
        self.load_inode(node, buf)
            .map_err(|e| self.not_found(node, e))
    }

    fn remember(&self, link: u64, parent: u64) {
        if let Ok(mut map) = self.parents.lock()
            && (map.len() < MAX_PARENTS || map.contains_key(&link))
        {
            map.insert(link, parent);
        }
    }

    fn parent_of(&self, link: u64) -> Option<u64> {
        self.parents.lock().ok()?.get(&link).copied()
    }

    /// The entry called `name` in the folder `dir`: `ino` and the file type.
    /// `.` is `dir` itself and `..` its parent.
    fn find(&self, dir: &Core, dbuf: &[u8], name: &[u8]) -> Result<Option<u64>> {
        if name == b"." {
            return Ok(Some(dir.ino));
        }
        let mut walker = DirWalker::open(self, dir, dbuf)?;
        let mut found = Vec::new();
        let mut allow = u32::MAX;
        loop {
            match walker.next(self, &mut found, &mut allow)? {
                Step::Entry(entry) if name_eq(self.geo.ci, &found, name) => {
                    return Ok(Some(entry.ino));
                }
                Step::Entry(_) | Step::Pause => {}
                Step::Done => return Ok(None),
            }
        }
    }

    /// Follows `target` from the folder `start`. `None` when something on the
    /// way does not exist or the path is too long or links too deep.
    fn resolve(&self, start: u64, target: &[u8]) -> Result<Option<u64>> {
        let mut cur = if target.first() == Some(&b'/') {
            self.geo.root_ino
        } else {
            start
        };
        let mut work: Vec<Vec<u8>> = components(target);
        let mut budget = MAX_COMPONENTS;
        let mut hops = 0;
        let mut buf = Vec::new();
        while let Some(part) = work.pop() {
            budget = match budget.checked_sub(1) {
                Some(b) => b,
                None => return Ok(None),
            };
            let dir = self.load_inode(cur, &mut buf)?;
            if !dir.is_dir() {
                return Ok(None);
            }
            let Some(next) = self.find(&dir, &buf, &part)? else {
                return Ok(None);
            };
            let mut nbuf = Vec::new();
            let core = self.load_inode(next, &mut nbuf)?;
            if core.is_symlink() {
                hops = u32::saturating_add(hops, 1);
                if hops > MAX_HOPS {
                    return Ok(None);
                }
                let link = self.read_link(&core, &nbuf)?;
                if link.first() == Some(&b'/') {
                    cur = self.geo.root_ino;
                }
                work.extend(components(&link));
            } else {
                cur = next;
            }
        }
        Ok(Some(cur))
    }

    /// True when the symbolic link `core` (found in the folder `parent`) leads
    /// to a folder.
    fn links_to_dir(&self, parent: u64, core: &Core, ibuf: &[u8]) -> bool {
        let Ok(target) = self.read_link(core, ibuf) else {
            return false;
        };
        let mut buf = Vec::new();
        match self.resolve(parent, &target) {
            Ok(Some(ino)) => self.load_inode(ino, &mut buf).is_ok_and(|c| c.is_dir()),
            _ => false,
        }
    }

    /// What a listing reports about the entry `ino` found in `dir`.
    fn entry_info(
        &self,
        dir: u64,
        ino: u64,
        ftype: Option<u8>,
        buf: &mut Vec<u8>,
    ) -> (
        NodeKind,
        Option<u64>,
        Option<atlas_fs_core::fs::Timestamp>,
        EntryFlags,
    ) {
        let Ok(core) = self.load_inode(ino, buf) else {
            return (kind_of_ftype(ftype), None, None, EntryFlags::EMPTY);
        };
        let kind = core.kind();
        let mut flags = EntryFlags::EMPTY;
        if kind == NodeKind::Symlink {
            self.remember(ino, dir);
            if self.links_to_dir(dir, &core, buf) {
                flags = flags.with(EntryFlags::LINKS_TO_DIRECTORY);
            }
        }
        (
            kind,
            (kind != NodeKind::Directory).then_some(core.size),
            Some(core.mtime),
            flags,
        )
    }
}

/// The parts of a path, last first, without empty parts and `.`.
fn components(path: &[u8]) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = path
        .split(|&b| b == b'/')
        .filter(|p| !p.is_empty() && *p != b".")
        .map(<[u8]>::to_vec)
        .collect();
    parts.reverse();
    parts
}

impl Xfs {
    /// The number of names that lead to `node`.
    pub fn link_count(&self, node: NodeId) -> Result<u32> {
        let mut buf = Vec::new();
        Ok(self.inner.node_core(node.0, &mut buf)?.nlink)
    }

    /// True when the volume's directory names compare without regard to
    /// ASCII case.
    pub fn case_insensitive(&self) -> bool {
        self.inner.geo.ci
    }

    /// False when the superblock's version 5 checksum did not match. The
    /// volume still mounts: every other block carries its own checksum.
    pub fn superblock_checksum_ok(&self) -> bool {
        self.crc_ok
    }
}

/// A folder listing.
struct XfsDir {
    inner: Arc<Inner>,
    walker: DirWalker,
    dir: u64,
    name: Vec<u8>,
    ibuf: Vec<u8>,
    done: bool,
}

impl DirStream for XfsDir {
    fn next_batch(&mut self, out: &mut DirBatch, max: usize) -> Result<DirProgress> {
        if self.done {
            return Ok(DirProgress::Done);
        }
        let mut allow = DIR_BLOCKS_PER_CALL;
        let mut added = 0usize;
        while added < max {
            let step = match self.walker.next(&self.inner, &mut self.name, &mut allow) {
                Ok(step) => step,
                Err(e) => {
                    self.done = true;
                    return Err(e);
                }
            };
            let entry = match step {
                Step::Done => {
                    self.done = true;
                    return Ok(DirProgress::Done);
                }
                Step::Pause => return Ok(DirProgress::More),
                Step::Entry(entry) => entry,
            };
            if self.name == b"." || self.name == b".." {
                continue;
            }
            let (kind, size, modified, flags) =
                self.inner
                    .entry_info(self.dir, entry.ino, entry.ftype, &mut self.ibuf);
            let shown = String::from_utf8_lossy(&self.name);
            out.push(DirEntry {
                node: NodeId(entry.ino),
                name: &shown,
                kind,
                size,
                resource_size: None,
                modified,
                flags,
            });
            added = added.saturating_add(1);
        }
        Ok(DirProgress::More)
    }
}

/// The data of one regular file.
struct XfsFile {
    inner: Arc<Inner>,
    runs: Vec<Run>,
    len: u64,
}

impl BlockSource for XfsFile {
    fn len(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        self.inner.read_mapped(&self.runs, offset, buf, false)
    }
}

impl FileSystem for Xfs {
    fn info(&self) -> VolumeInfo {
        self.info.clone()
    }

    fn root(&self) -> NodeId {
        NodeId(self.inner.geo.root_ino)
    }

    fn metadata(&self, node: NodeId) -> Result<Metadata> {
        let mut buf = Vec::new();
        let core = self.inner.node_core(node.0, &mut buf)?;
        let kind = core.kind();
        let mut flags = EntryFlags::EMPTY;
        let mut link_target = None;
        if kind == NodeKind::Symlink
            && let Ok(target) = self.inner.read_link(&core, &buf)
        {
            // A relative link can be followed only from the folder it was
            // listed in.
            let start = self
                .inner
                .parent_of(node.0)
                .or_else(|| (target.first() == Some(&b'/')).then_some(self.inner.geo.root_ino));
            if let Some(start) = start
                && self.inner.links_to_dir(start, &core, &buf)
            {
                flags = flags.with(EntryFlags::LINKS_TO_DIRECTORY);
            }
            link_target = Some(lossy(&target));
        }
        Ok(Metadata {
            node,
            kind,
            size: (kind != NodeKind::Directory).then_some(core.size),
            resource_size: None,
            created: core.crtime,
            modified: Some(core.mtime),
            accessed: Some(core.atime),
            flags,
            link_target,
            finder_codes: None,
            owner: Some(Owner {
                user: core.uid,
                group: core.gid,
                mode: core.mode,
            }),
            attributes: None,
            finder_info: None,
        })
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
        let name = name.as_bytes();
        if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
            return Ok(None);
        }
        let mut buf = Vec::new();
        let core = self.inner.node_core(dir.0, &mut buf)?;
        if !core.is_dir() {
            return Err(Error::NotADirectory { node: dir.0 });
        }
        let Some(ino) = self.inner.find(&core, &buf, name)? else {
            return Ok(None);
        };
        let mut found = Vec::new();
        if let Ok(c) = self.inner.load_inode(ino, &mut found)
            && c.is_symlink()
        {
            self.inner.remember(ino, dir.0);
        }
        Ok(Some(NodeId(ino)))
    }

    fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
        let mut buf = Vec::new();
        let mut core = self.inner.node_core(dir.0, &mut buf)?;
        let mut folder = dir.0;
        if core.is_symlink() {
            let target = self.inner.read_link(&core, &buf)?;
            let start = self
                .inner
                .parent_of(dir.0)
                .unwrap_or(self.inner.geo.root_ino);
            let Some(ino) = self.inner.resolve(start, &target)? else {
                return Err(Error::NotADirectory { node: dir.0 });
            };
            core = self.inner.load_inode(ino, &mut buf)?;
            folder = ino;
        }
        if !core.is_dir() {
            return Err(Error::NotADirectory { node: dir.0 });
        }
        let walker = DirWalker::open(&self.inner, &core, &buf)?;
        Ok(Box::new(XfsDir {
            inner: Arc::clone(&self.inner),
            walker,
            dir: folder,
            name: Vec::new(),
            ibuf: Vec::new(),
            done: false,
        }))
    }

    fn extents(&self, node: NodeId, stream: Stream) -> Result<Vec<Extent>> {
        if !matches!(stream, Stream::Data) {
            return Err(Error::NoSuchStream { node: node.0 });
        }
        let mut buf = Vec::new();
        let core = self.inner.node_core(node.0, &mut buf)?;
        match core.kind() {
            NodeKind::Directory => return Err(Error::IsADirectory { node: node.0 }),
            NodeKind::Symlink => {
                if !matches!(core.format, inode::Format::Extents | inode::Format::Btree) {
                    return Ok(Vec::new());
                }
            }
            NodeKind::File => {}
            _ => return Err(Error::NoSuchStream { node: node.0 }),
        }
        let runs = self.inner.load_runs(&core, &buf)?;
        let mut out: Vec<Extent> = Vec::with_capacity(runs.len().min(4096));
        for run in &runs {
            let offset = self.inner.geo.block_offset(run.start)?;
            let len = run
                .count
                .checked_mul(self.inner.geo.block_size)
                .ok_or_else(|| invalid("br_blockcount", run.count))?;
            match out.last_mut() {
                Some(last) if last.offset.checked_add(last.len) == Some(offset) => {
                    last.len = last
                        .len
                        .checked_add(len)
                        .ok_or_else(|| invalid("br_blockcount", run.count))?;
                }
                _ => out.push(Extent { offset, len }),
            }
        }
        Ok(out)
    }

    fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>> {
        if !matches!(stream, Stream::Data) {
            return Err(Error::NoSuchStream { node: node.0 });
        }
        let mut buf = Vec::new();
        let core = self.inner.node_core(node.0, &mut buf)?;
        match core.kind() {
            NodeKind::Directory => Err(Error::IsADirectory { node: node.0 }),
            NodeKind::Symlink => Ok(Arc::new(MemSource::new(self.inner.read_link(&core, &buf)?))),
            NodeKind::File => {
                if !matches!(core.format, inode::Format::Extents | inode::Format::Btree) {
                    return Err(invalid("di_format", 0));
                }
                let runs = self.inner.load_runs(&core, &buf)?;
                Ok(Arc::new(XfsFile {
                    inner: Arc::clone(&self.inner),
                    runs,
                    len: core.size,
                }))
            }
            _ => Err(Error::NoSuchStream { node: node.0 }),
        }
    }

    fn identity(&self) -> Option<VolumeIdentity> {
        Some(self.identity)
    }
}
