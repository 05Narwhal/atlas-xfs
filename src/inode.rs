// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! Inodes, the extent map of a data fork (in the inode or in a B-tree of
//! blocks), mapped reads and symbolic link targets.
//!
//! Every number is checked before use. The B-tree walk is bounded by a block
//! budget and a visited set, so a tree that loops or fans out forever ends in
//! an error.

use std::collections::HashSet;

use super::bytes::{be16, be32, be64, le32, slice, u8_at};
use super::crc::crc32c_zeroed;
use super::sb::Geometry;
use super::{Inner, bad_crc, invalid, limit, other, out_of_range};
use atlas_fs_core::error::{CorruptKind, Corruption, Error, Location, Result};
use atlas_fs_core::fs::{NodeKind, Timestamp};

/// "IN".
const DI_MAGIC: u16 = 0x494E;
const CORE_V2: usize = 100;
const CORE_V3: usize = 176;
const CRC_AT: usize = 100;
const DIFLAG_REALTIME: u16 = 1;
const DIFLAG2_BIGTIME: u64 = 1 << 3;
const DIFLAG2_NREXT64: u64 = 1 << 4;
/// Seconds from 1901-12-13 (where the big timestamps start) to 1970.
const BIGTIME_EPOCH_OFFSET: i64 = 1 << 31;

const MODE_TYPE: u16 = 0o170_000;
pub(super) const S_DIR: u16 = 0o040_000;
pub(super) const S_REG: u16 = 0o100_000;
pub(super) const S_LNK: u16 = 0o120_000;

/// Most extents one file may have. A file with more is refused.
pub(super) const MAX_EXTENTS: usize = 1 << 21;
/// Most blocks of one extent B-tree the walk visits.
const MAX_BMBT_BLOCKS: usize = 1 << 16;
/// Deepest extent B-tree the format produces (and then some).
const MAX_BMBT_LEVEL: u16 = 9;
/// Longest symbolic link target read.
pub(super) const MAX_LINK_BYTES: usize = 4096;
/// Blocks a remote symbolic link may span, at most.
const MAX_LINK_BLOCKS: usize = 64;

const BMAP_MAGIC: u32 = 0x424D_4150;
const BMA3_MAGIC: u32 = 0x424D_4133;
const SYMLINK_MAGIC: u32 = 0x5853_4C4D;
/// Length of the long-form B-tree block header, without and with the
/// version 5 fields.
const BMBT_HDR_V4: usize = 24;
const BMBT_HDR_V5: usize = 72;
const SYMLINK_HDR_V5: usize = 56;
/// Reads at least this long go through `read_bulk`.
const BULK_BYTES: usize = 1 << 20;

/// How an inode stores its data fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    /// Device number.
    Dev,
    /// The data is in the inode (short directories, short links).
    Local,
    /// An array of extent records in the inode.
    Extents,
    /// The root of an extent B-tree in the inode.
    Btree,
    /// Unused.
    Uuid,
}

/// The fields of an inode that readers use, copied out of its buffer.
#[derive(Debug, Clone)]
pub(super) struct Core {
    pub ino: u64,
    pub mode: u16,
    pub format: Format,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: Timestamp,
    pub mtime: Timestamp,
    pub crtime: Option<Timestamp>,
    pub size: u64,
    pub nextents: u64,
    flags: u16,
    fork_start: usize,
    fork_end: usize,
}

impl Core {
    pub fn kind(&self) -> NodeKind {
        match self.mode & MODE_TYPE {
            S_DIR => NodeKind::Directory,
            S_REG => NodeKind::File,
            S_LNK => NodeKind::Symlink,
            _ => NodeKind::Other,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.mode & MODE_TYPE == S_DIR
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & MODE_TYPE == S_LNK
    }

    /// The data fork inside the inode buffer `buf` this was read from.
    pub fn fork<'a>(&self, buf: &'a [u8]) -> Result<&'a [u8]> {
        buf.get(self.fork_start..self.fork_end)
            .ok_or_else(|| other("an inode's data fork is outside the inode"))
    }
}

fn bad_inode_magic(found: u16) -> Error {
    Corruption::new(
        "XFS inode",
        Location::Unspecified,
        CorruptKind::BadSignature {
            expected: u64::from(DI_MAGIC),
            found: u64::from(found),
        },
    )
    .into()
}

fn timestamp(buf: &[u8], at: usize, big: bool) -> Result<Timestamp> {
    if big {
        let v = be64(buf, at)?;
        let seconds = i64::try_from(v / 1_000_000_000)
            .unwrap_or(i64::MAX)
            .saturating_sub(BIGTIME_EPOCH_OFFSET);
        // v % 1e9 is below 2^30.
        let nanos = u32::try_from(v % 1_000_000_000).unwrap_or(0);
        Ok(Timestamp::from_unix(seconds, nanos))
    } else {
        let seconds = i64::from(be32(buf, at)?.cast_signed());
        let nanos = be32(buf, at.saturating_add(4))?.min(999_999_999);
        Ok(Timestamp::from_unix(seconds, nanos))
    }
}

/// Checks and reads the core of the inode `ino` held in `buf`.
pub(super) fn parse_core(geo: &Geometry, ino: u64, buf: &[u8]) -> Result<Core> {
    let magic = be16(buf, 0)?;
    if magic != DI_MAGIC {
        return Err(bad_inode_magic(magic));
    }
    let mode = be16(buf, 2)?;
    let version = u8_at(buf, 4)?;
    let format = match u8_at(buf, 5)? {
        0 => Format::Dev,
        1 => Format::Local,
        2 => Format::Extents,
        3 => Format::Btree,
        4 => Format::Uuid,
        other => return Err(invalid("di_format", u64::from(other))),
    };
    let core_len = match version {
        1 | 2 => CORE_V2,
        3 => CORE_V3,
        other => return Err(invalid("di_version", u64::from(other))),
    };
    if (version == 3) != geo.v5 {
        return Err(invalid("di_version", u64::from(version)));
    }
    let mut flags2 = 0;
    let mut crtime = None;
    if version == 3 {
        let stored = le32(buf, CRC_AT)?;
        if crc32c_zeroed(buf, CRC_AT) != Some(stored) {
            return Err(bad_crc("XFS inode", ino));
        }
        let own = be64(buf, 152)?;
        if own != ino {
            return Err(invalid("di_ino", own));
        }
        flags2 = be64(buf, 120)?;
    }
    let big = flags2 & DIFLAG2_BIGTIME != 0;
    if version == 3 {
        crtime = Some(timestamp(buf, 144, big)?);
    }
    let nlink = if version == 1 {
        u32::from(be16(buf, 6)?)
    } else {
        be32(buf, 16)?
    };
    if flags2 & DIFLAG2_NREXT64 != 0 && !geo.nrext64 {
        return Err(invalid("di_flags2", flags2));
    }
    let nextents = if flags2 & DIFLAG2_NREXT64 != 0 {
        be64(buf, 24)?
    } else {
        u64::from(be32(buf, 76)?)
    };
    let size = be64(buf, 56)?;
    if i64::try_from(size).is_err() {
        return Err(invalid("di_size", size));
    }
    let literal = buf
        .len()
        .checked_sub(core_len)
        .ok_or_else(|| other("an inode is shorter than its core"))?;
    let forkoff = usize::from(u8_at(buf, 82)?);
    let fork_len = if forkoff == 0 {
        literal
    } else {
        forkoff
            .checked_mul(8)
            .filter(|&n| n <= literal)
            .ok_or_else(|| {
                out_of_range(
                    "di_forkoff",
                    forkoff as u64,
                    "inode literal area",
                    literal as u64,
                )
            })?
    };
    Ok(Core {
        ino,
        mode,
        format,
        nlink,
        uid: be32(buf, 8)?,
        gid: be32(buf, 12)?,
        atime: timestamp(buf, 32, big)?,
        mtime: timestamp(buf, 40, big)?,
        crtime,
        size,
        nextents,
        flags: be16(buf, 90)?,
        fork_start: core_len,
        fork_end: core_len.saturating_add(fork_len),
    })
}

/// One extent: `count` blocks of the file from block `off`, on disk from
/// block `start` (an AG number and a block in it, packed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Run {
    pub off: u64,
    pub start: u64,
    pub count: u64,
    pub unwritten: bool,
}

fn decode_run(rec: &[u8]) -> Result<Run> {
    let l0 = be64(rec, 0)?;
    let l1 = be64(rec, 8)?;
    Ok(Run {
        off: (l0 & 0x7FFF_FFFF_FFFF_FFFF) >> 9,
        start: ((l0 & 0x1FF) << 43) | (l1 >> 21),
        count: l1 & 0x1F_FFFF,
        unwritten: l0 >> 63 != 0,
    })
}

/// Collects the runs of one fork, checking each and the order.
struct Collector<'g> {
    geo: &'g Geometry,
    out: Vec<Run>,
    next_off: u64,
}

impl Collector<'_> {
    fn push(&mut self, rec: &[u8]) -> Result<()> {
        if self.out.len() >= MAX_EXTENTS {
            return Err(limit("extents of a file", MAX_EXTENTS as u64));
        }
        let run = decode_run(rec)?;
        if run.count == 0 {
            return Err(invalid("br_blockcount", 0));
        }
        if run.off < self.next_off {
            return Err(invalid("br_startoff", run.off));
        }
        if !self.geo.extent_ok(run.start, run.count) {
            return Err(out_of_range(
                "br_startblock",
                run.start,
                "data blocks",
                self.geo.data_blocks,
            ));
        }
        // off < 2^54 and count < 2^21.
        self.next_off = run.off.saturating_add(run.count);
        self.out.push(run);
        Ok(())
    }
}

impl Inner {
    /// Reads and checks the inode `ino` into `buf`.
    pub(super) fn load_inode(&self, ino: u64, buf: &mut Vec<u8>) -> Result<Core> {
        let at = self.geo.inode_offset(ino)?;
        let len = usize::try_from(self.geo.inode_size)
            .map_err(|_| invalid("sb_inodesize", self.geo.inode_size))?;
        buf.clear();
        buf.resize(len, 0);
        self.source.read_at(at, buf)?;
        parse_core(&self.geo, ino, buf)
    }

    fn block_len(&self) -> Result<usize> {
        usize::try_from(self.geo.block_size)
            .map_err(|_| invalid("sb_blocksize", self.geo.block_size))
    }

    /// The extent map of the data fork of `core` (whose inode buffer is
    /// `ibuf`), in file order. Empty for forks that hold no extents.
    pub(super) fn load_runs(&self, core: &Core, ibuf: &[u8]) -> Result<Vec<Run>> {
        if core.flags & DIFLAG_REALTIME != 0 {
            return Err(Error::Unsupported("XFS realtime files".into()));
        }
        let fork = core.fork(ibuf)?;
        let mut runs = Collector {
            geo: &self.geo,
            out: Vec::new(),
            next_off: 0,
        };
        match core.format {
            Format::Extents => {
                let n = usize::try_from(core.nextents)
                    .ok()
                    .filter(|&n| n <= MAX_EXTENTS)
                    .ok_or_else(|| limit("extents of a file", MAX_EXTENTS as u64))?;
                if n.checked_mul(16).is_none_or(|need| need > fork.len()) {
                    return Err(out_of_range(
                        "di_nextents",
                        core.nextents,
                        "data fork records",
                        u64::try_from(fork.len() / 16).unwrap_or(0),
                    ));
                }
                runs.out.reserve(n.min(4096));
                for i in 0..n {
                    runs.push(slice(fork, i.saturating_mul(16), 16)?)?;
                }
            }
            Format::Btree => self.walk_bmbt(core, fork, &mut runs)?,
            Format::Local | Format::Dev | Format::Uuid => {}
        }
        Ok(runs.out)
    }

    /// Walks the extent B-tree whose root is `fork`, depth first and left to
    /// right, so the records come out in file order.
    fn walk_bmbt(&self, core: &Core, fork: &[u8], runs: &mut Collector<'_>) -> Result<()> {
        let level = be16(fork, 0)?;
        let numrecs = usize::from(be16(fork, 2)?);
        if level == 0 || level > MAX_BMBT_LEVEL {
            return Err(invalid("bb_level", u64::from(level)));
        }
        let max_root = fork.len().saturating_sub(4) / 16;
        if numrecs > max_root {
            return Err(out_of_range(
                "bb_numrecs",
                numrecs as u64,
                "root records",
                max_root as u64,
            ));
        }
        let mut stack: Vec<(u64, u16)> = Vec::new();
        let ptrs = max_root.saturating_mul(8).saturating_add(4);
        for i in (0..numrecs).rev() {
            stack.push((
                be64(fork, ptrs.saturating_add(i.saturating_mul(8)))?,
                level.saturating_sub(1),
            ));
        }
        let bs = self.block_len()?;
        let hdr = if self.geo.v5 {
            BMBT_HDR_V5
        } else {
            BMBT_HDR_V4
        };
        let want_magic = if self.geo.v5 { BMA3_MAGIC } else { BMAP_MAGIC };
        let max_recs = bs.saturating_sub(hdr) / 16;
        let mut block = vec![0u8; bs];
        let mut visited: HashSet<u64> = HashSet::new();
        while let Some((fsb, expected)) = stack.pop() {
            if !visited.insert(fsb) {
                return Err(Corruption::new(
                    "XFS extent B-tree",
                    Location::Block(fsb),
                    CorruptKind::Loop { what: "B-tree" },
                )
                .into());
            }
            if visited.len() > MAX_BMBT_BLOCKS || stack.len() > MAX_BMBT_BLOCKS {
                return Err(limit("blocks of an extent B-tree", MAX_BMBT_BLOCKS as u64));
            }
            if !self.geo.extent_ok(fsb, 1) {
                return Err(out_of_range(
                    "bmbt pointer",
                    fsb,
                    "data blocks",
                    self.geo.data_blocks,
                ));
            }
            self.source
                .read_at(self.geo.block_offset(fsb)?, &mut block)?;
            if be32(&block, 0)? != want_magic {
                return Err(Corruption::new(
                    "XFS extent B-tree",
                    Location::Block(fsb),
                    CorruptKind::BadSignature {
                        expected: u64::from(want_magic),
                        found: u64::from(be32(&block, 0)?),
                    },
                )
                .into());
            }
            if self.geo.v5 {
                if crc32c_zeroed(&block, 64) != Some(le32(&block, 64)?) {
                    return Err(bad_crc("XFS extent B-tree block", fsb));
                }
                if be64(&block, 56)? != core.ino {
                    return Err(invalid("bb_owner", be64(&block, 56)?));
                }
            }
            let found_level = be16(&block, 4)?;
            if found_level != expected {
                return Err(invalid("bb_level", u64::from(found_level)));
            }
            let n = usize::from(be16(&block, 6)?);
            if n > max_recs {
                return Err(out_of_range(
                    "bb_numrecs",
                    n as u64,
                    "block records",
                    max_recs as u64,
                ));
            }
            if expected == 0 {
                for i in 0..n {
                    runs.push(slice(&block, hdr.saturating_add(i.saturating_mul(16)), 16)?)?;
                }
            } else {
                let ptrs = hdr.saturating_add(max_recs.saturating_mul(8));
                for i in (0..n).rev() {
                    stack.push((
                        be64(&block, ptrs.saturating_add(i.saturating_mul(8)))?,
                        expected.saturating_sub(1),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Fills `buf` from the file bytes at `offset` described by `runs`.
    /// Holes and unwritten extents read as zeros, or fail when `strict`.
    pub(super) fn read_mapped(
        &self,
        runs: &[Run],
        offset: u64,
        buf: &mut [u8],
        strict: bool,
    ) -> Result<()> {
        let bs = self.geo.block_size;
        let mut offset = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let remaining = u64::try_from(buf.len().saturating_sub(done)).unwrap_or(u64::MAX);
            let blk = offset.checked_div(bs).unwrap_or(0);
            let inblk = offset.checked_rem(bs).unwrap_or(0);
            // Runs are validated: off + count stays below 2^55.
            let idx = runs.partition_point(|r| r.off.saturating_add(r.count) <= blk);
            let mut physical = None;
            let avail = match runs.get(idx) {
                Some(r) if r.off <= blk => {
                    let within = blk.saturating_sub(r.off);
                    if !r.unwritten {
                        let fsb = r
                            .start
                            .checked_add(within)
                            .ok_or_else(|| invalid("br_startblock", r.start))?;
                        physical = Some(
                            self.geo
                                .block_offset(fsb)?
                                .checked_add(inblk)
                                .ok_or_else(|| invalid("br_startblock", r.start))?,
                        );
                    }
                    r.count
                        .saturating_sub(within)
                        .saturating_mul(bs)
                        .saturating_sub(inblk)
                }
                Some(r) => r
                    .off
                    .saturating_sub(blk)
                    .saturating_mul(bs)
                    .saturating_sub(inblk),
                None => u64::MAX,
            };
            let n = usize::try_from(avail.min(remaining)).unwrap_or(buf.len().saturating_sub(done));
            let end = done
                .checked_add(n)
                .ok_or_else(|| other("a read length overflows"))?;
            let part = buf
                .get_mut(done..end)
                .ok_or_else(|| other("a read length is wrong"))?;
            match physical {
                Some(at) if n >= BULK_BYTES => self.source.read_bulk(at, part)?,
                Some(at) => self.source.read_at(at, part)?,
                None if strict => {
                    return Err(Corruption::new(
                        "XFS file",
                        Location::Byte(offset),
                        CorruptKind::Other("a block is missing"),
                    )
                    .into());
                }
                None => part.fill(0),
            }
            done = end;
            offset = offset
                .checked_add(n as u64)
                .ok_or_else(|| other("a file offset overflows"))?;
        }
        Ok(())
    }

    /// The target of the symbolic link `core` (whose inode buffer is `ibuf`).
    pub(super) fn read_link(&self, core: &Core, ibuf: &[u8]) -> Result<Vec<u8>> {
        let size = usize::try_from(core.size)
            .ok()
            .filter(|&n| n > 0 && n <= MAX_LINK_BYTES)
            .ok_or_else(|| invalid("di_size", core.size))?;
        match core.format {
            Format::Local => {
                let fork = core.fork(ibuf)?;
                Ok(slice(fork, 0, size)?.to_vec())
            }
            Format::Extents | Format::Btree => {
                let runs = self.load_runs(core, ibuf)?;
                let bs = self.block_len()?;
                let mut out: Vec<u8> = Vec::with_capacity(size);
                let mut block = vec![0u8; bs];
                for index in 0..MAX_LINK_BLOCKS {
                    if out.len() >= size {
                        break;
                    }
                    let at = u64::try_from(index)
                        .unwrap_or(0)
                        .saturating_mul(self.geo.block_size);
                    self.read_mapped(&runs, at, &mut block, true)?;
                    let want = size.saturating_sub(out.len());
                    let (start, take) = if self.geo.v5 {
                        if be32(&block, 0)? != SYMLINK_MAGIC {
                            return Err(invalid("sl_magic", u64::from(be32(&block, 0)?)));
                        }
                        if crc32c_zeroed(&block, 12) != Some(le32(&block, 12)?) {
                            return Err(bad_crc("XFS symbolic link block", core.ino));
                        }
                        if be64(&block, 32)? != core.ino {
                            return Err(invalid("sl_owner", be64(&block, 32)?));
                        }
                        let offset = usize::try_from(be32(&block, 4)?).unwrap_or(usize::MAX);
                        let bytes = usize::try_from(be32(&block, 8)?).unwrap_or(usize::MAX);
                        if offset != out.len() || bytes > bs.saturating_sub(SYMLINK_HDR_V5) {
                            return Err(invalid("sl_bytes", bytes as u64));
                        }
                        (SYMLINK_HDR_V5, bytes.min(want))
                    } else {
                        (0, bs.min(want))
                    };
                    if take == 0 {
                        return Err(other("a symbolic link block holds no bytes"));
                    }
                    out.extend_from_slice(slice(&block, start, take)?);
                }
                if out.len() < size {
                    return Err(other("a symbolic link is longer than its blocks"));
                }
                Ok(out)
            }
            Format::Dev | Format::Uuid => Err(invalid("di_format", 0)),
        }
    }
}
