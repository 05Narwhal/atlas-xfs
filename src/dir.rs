// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! Reading the entries of a directory: the short form kept in the inode, and
//! the block, leaf and node forms, whose entries all sit in data blocks at the
//! start of the file. The hash index (leaf and free blocks, 32 GiB into the
//! file) is not needed to list a directory and is never read.

use std::sync::Arc;

use super::Inner;
use super::bytes::{be16, be32, be64, le32, slice, u8_at};
use super::crc::crc32c_zeroed;
use super::inode::{Core, Format, Run};
use super::{bad_crc, invalid, limit, other};
use atlas_fs_core::error::{CorruptKind, Corruption, Location, Result};

const BLOCK_MAGIC_V4: u32 = 0x5844_3242;
const DATA_MAGIC_V4: u32 = 0x5844_3244;
const BLOCK_MAGIC_V5: u32 = 0x5844_4233;
const DATA_MAGIC_V5: u32 = 0x5844_4433;
const HDR_V4: usize = 16;
const HDR_V5: usize = 64;
/// Directory data lives below this byte offset of the file; the hash index is
/// above it.
const LEAF_OFFSET_BYTES: u64 = 1 << 35;
/// Most directory blocks one walk reads.
const MAX_DIR_BLOCKS: u64 = 1 << 23;
/// The first two bytes of an unused entry.
const FREE_TAG: u16 = 0xFFFF;

/// An entry found by the walker; its name is in the caller's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Entry {
    pub ino: u64,
    /// The file type the entry records, when the volume has them.
    pub ftype: Option<u8>,
}

/// One step of a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Entry(Entry),
    /// The block allowance of this call is used up; call again.
    Pause,
    Done,
}

enum State {
    Short {
        fork: Vec<u8>,
        pos: usize,
        left: u32,
        wide: bool,
        parent: Option<u64>,
    },
    Blocks {
        runs: Arc<Vec<Run>>,
        run_idx: usize,
        next_blk: u64,
        buf: Vec<u8>,
        pos: usize,
        end: usize,
        blocks: u64,
    },
    Done,
}

/// Walks the entries of one directory, `.` and `..` included.
pub(super) struct DirWalker {
    ino: u64,
    state: State,
}

impl DirWalker {
    /// Starts a walk of the directory `core`, whose inode buffer is `ibuf`.
    pub fn open(inner: &Inner, core: &Core, ibuf: &[u8]) -> Result<Self> {
        if !core.is_dir() {
            return Err(atlas_fs_core::error::Error::NotADirectory { node: core.ino });
        }
        let state = match core.format {
            Format::Local => {
                let fork = core.fork(ibuf)?;
                let size = usize::try_from(core.size)
                    .ok()
                    .filter(|&n| n <= fork.len())
                    .ok_or_else(|| invalid("di_size", core.size))?;
                let fork = slice(fork, 0, size)?;
                let count = u32::from(u8_at(fork, 0)?);
                let wide = u8_at(fork, 1)? != 0;
                let header = if wide { 10 } else { 6 };
                let parent = if wide {
                    be64(fork, 2)?
                } else {
                    u64::from(be32(fork, 2)?)
                };
                State::Short {
                    fork: slice(fork, 0, size)?.to_vec(),
                    pos: header,
                    left: count,
                    wide,
                    parent: Some(parent),
                }
            }
            Format::Extents | Format::Btree => State::Blocks {
                runs: Arc::new(inner.load_runs(core, ibuf)?),
                run_idx: 0,
                next_blk: 0,
                buf: Vec::new(),
                pos: 0,
                end: 0,
                blocks: 0,
            },
            Format::Dev | Format::Uuid => return Err(invalid("di_format", 0)),
        };
        Ok(Self {
            ino: core.ino,
            state,
        })
    }

    /// The next entry, with its name in `name`. Reads at most `allow`
    /// directory blocks (the count goes down as it reads).
    pub fn next(&mut self, inner: &Inner, name: &mut Vec<u8>, allow: &mut u32) -> Result<Step> {
        let step = self.step(inner, name, allow);
        if !matches!(step, Ok(Step::Entry(_) | Step::Pause)) {
            self.state = State::Done;
        }
        step
    }

    fn step(&mut self, inner: &Inner, name: &mut Vec<u8>, allow: &mut u32) -> Result<Step> {
        let ino = self.ino;
        match &mut self.state {
            State::Done => Ok(Step::Done),
            State::Short {
                fork,
                pos,
                left,
                wide,
                parent,
            } => {
                if let Some(p) = parent.take() {
                    name.clear();
                    name.extend_from_slice(b"..");
                    return Ok(Step::Entry(Entry {
                        ino: p,
                        ftype: inner.geo.ftype.then_some(2),
                    }));
                }
                if *left == 0 {
                    return Ok(Step::Done);
                }
                let namelen = usize::from(u8_at(fork, *pos)?);
                if namelen == 0 {
                    return Err(invalid("namelen", 0));
                }
                // namelen, the two-byte offset, then the name.
                let mut at = pos
                    .checked_add(3)
                    .ok_or_else(|| other("a position overflows"))?;
                name.clear();
                name.extend_from_slice(slice(fork, at, namelen)?);
                at = at.saturating_add(namelen);
                let ftype = if inner.geo.ftype {
                    let t = u8_at(fork, at)?;
                    at = at.saturating_add(1);
                    Some(t)
                } else {
                    None
                };
                let (target, width) = if *wide {
                    (be64(fork, at)?, 8)
                } else {
                    (u64::from(be32(fork, at)?), 4)
                };
                *pos = at.saturating_add(width);
                *left = left.saturating_sub(1);
                Ok(Step::Entry(Entry { ino: target, ftype }))
            }
            State::Blocks {
                runs,
                run_idx,
                next_blk,
                buf,
                pos,
                end,
                blocks,
            } => loop {
                if *pos >= *end {
                    if *allow == 0 {
                        return Ok(Step::Pause);
                    }
                    let leaf_fsb = LEAF_OFFSET_BYTES >> inner.geo.block_size.trailing_zeros();
                    let Some(blk) =
                        next_dir_block(runs, run_idx, next_blk, inner.geo.dir_blk_log, leaf_fsb)
                    else {
                        return Ok(Step::Done);
                    };
                    *allow = allow.saturating_sub(1);
                    *blocks = blocks.saturating_add(1);
                    if *blocks > MAX_DIR_BLOCKS {
                        return Err(limit("directory blocks", MAX_DIR_BLOCKS));
                    }
                    (*pos, *end) = load_block(inner, ino, runs, blk, buf)?;
                    continue;
                }
                let tag = be16(buf, *pos)?;
                if tag == FREE_TAG {
                    let len = usize::from(be16(buf, pos.saturating_add(2))?);
                    if len < 8 || len % 8 != 0 || pos.saturating_add(len) > *end {
                        return Err(invalid("unused entry length", len as u64));
                    }
                    *pos = pos.saturating_add(len);
                    continue;
                }
                let room = end.saturating_sub(*pos);
                if room < 8 {
                    return Err(invalid("directory entry", u64::try_from(room).unwrap_or(0)));
                }
                let target = be64(buf, *pos)?;
                let namelen = usize::from(u8_at(buf, pos.saturating_add(8))?);
                if namelen == 0 {
                    return Err(invalid("namelen", 0));
                }
                name.clear();
                name.extend_from_slice(slice(buf, pos.saturating_add(9), namelen)?);
                let ftype = if inner.geo.ftype {
                    Some(u8_at(buf, pos.saturating_add(9).saturating_add(namelen))?)
                } else {
                    None
                };
                // Inode number, length, name, file type, tag; padded to 8.
                let size = namelen
                    .saturating_add(11)
                    .saturating_add(usize::from(inner.geo.ftype))
                    .div_ceil(8)
                    .saturating_mul(8);
                if size > room {
                    return Err(invalid("directory entry length", size as u64));
                }
                *pos = pos.saturating_add(size);
                return Ok(Step::Entry(Entry { ino: target, ftype }));
            },
        }
    }
}

/// The number of the next directory block that has any of its file blocks
/// mapped, at or after `*next`, below `leaf_fsb`. Runs are in file order.
fn next_dir_block(
    runs: &[Run],
    idx: &mut usize,
    next: &mut u64,
    dir_blk_log: u8,
    leaf_fsb: u64,
) -> Option<u64> {
    while let Some(r) = runs.get(*idx) {
        if r.off >= leaf_fsb {
            return None;
        }
        // Validated runs: off + count stays below 2^55.
        let end = r.off.saturating_add(r.count).min(leaf_fsb);
        let first = r.off >> dir_blk_log;
        let last = end.saturating_sub(1) >> dir_blk_log;
        let blk = first.max(*next);
        if blk <= last {
            *next = blk.saturating_add(1);
            return Some(blk);
        }
        *idx = idx.saturating_add(1);
    }
    None
}

/// Reads directory block `blk` into `buf`, checks it, and returns where its
/// entries start and end.
fn load_block(
    inner: &Inner,
    ino: u64,
    runs: &[Run],
    blk: u64,
    buf: &mut Vec<u8>,
) -> Result<(usize, usize)> {
    let geo = &inner.geo;
    let size = geo
        .block_size
        .checked_shl(u32::from(geo.dir_blk_log))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| invalid("sb_dirblklog", u64::from(geo.dir_blk_log)))?;
    // blk is below 2^26: the leaf offset bounds it.
    let at = blk
        .checked_mul(geo.dir_block_fsblocks())
        .and_then(|fsb| fsb.checked_mul(geo.block_size))
        .ok_or_else(|| other("a directory offset overflows"))?;
    buf.clear();
    buf.resize(size, 0);
    inner.read_mapped(runs, at, buf, false)?;
    let magic = be32(buf, 0)?;
    let (block_form, header) = match (geo.v5, magic) {
        (false, BLOCK_MAGIC_V4) => (true, HDR_V4),
        (false, DATA_MAGIC_V4) => (false, HDR_V4),
        (true, BLOCK_MAGIC_V5) => (true, HDR_V5),
        (true, DATA_MAGIC_V5) => (false, HDR_V5),
        _ => {
            return Err(Corruption::new(
                "XFS directory block",
                Location::Block(blk),
                CorruptKind::BadSignature {
                    expected: u64::from(if geo.v5 { DATA_MAGIC_V5 } else { DATA_MAGIC_V4 }),
                    found: u64::from(magic),
                },
            )
            .into());
        }
    };
    if geo.v5 {
        if crc32c_zeroed(buf, 4) != Some(le32(buf, 4)?) {
            return Err(bad_crc("XFS directory block", blk));
        }
        if be64(buf, 40)? != ino {
            return Err(invalid("directory block owner", be64(buf, 40)?));
        }
    }
    let end = if block_form {
        // The tail: the number of leaf entries and of stale ones, with the
        // entries themselves just before.
        let count = u64::from(be32(buf, size.saturating_sub(8))?);
        let leaf_bytes = count * 8;
        usize::try_from(leaf_bytes)
            .ok()
            .and_then(|n| size.checked_sub(8)?.checked_sub(n))
            .filter(|&end| end >= header)
            .ok_or_else(|| invalid("block leaf count", count))?
    } else {
        size
    };
    Ok((header, end))
}
