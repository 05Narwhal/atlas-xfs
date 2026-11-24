// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! The superblock and the geometry derived from it: where a block or an
//! inode is on the source. Every number read from the disk is checked here, so
//! the rest of the reader can rely on the geometry.

use super::bytes::{be16, be32, be64, le32, slice, u8_at};
use super::crc::crc32c_zeroed;
use super::{invalid, out_of_range};
use atlas_fs_core::error::{CorruptKind, Corruption, Error, Location, Result};

/// "XFSB".
pub(super) const MAGIC: u32 = 0x5846_5342;

/// Size of the superblock fields up to `sb_bad_features2`, which a version 4
/// superblock must hold.
const V4_LEN: usize = 208;
/// Offset of `sb_crc`, and the length a version 5 superblock must have.
const CRC_OFFSET: usize = 224;
const V5_LEN: usize = 232;

const VERSION_DIRV2: u16 = 0x2000;
const VERSION_BORG: u16 = 0x4000;
const VERSION_MOREBITS: u16 = 0x8000;
const FEATURES2_FTYPE: u32 = 0x200;

const INCOMPAT_FTYPE: u32 = 1;
const INCOMPAT_SPINODES: u32 = 1 << 1;
const INCOMPAT_META_UUID: u32 = 1 << 2;
const INCOMPAT_BIGTIME: u32 = 1 << 3;
const INCOMPAT_NEEDSREPAIR: u32 = 1 << 4;
const INCOMPAT_NREXT64: u32 = 1 << 5;
const INCOMPAT_EXCHRANGE: u32 = 1 << 6;
const INCOMPAT_PARENT: u32 = 1 << 7;
/// What this reader understands. Anything else (the metadata directory
/// tree, newer layouts) changes where things are.
const INCOMPAT_KNOWN: u32 = INCOMPAT_FTYPE
    | INCOMPAT_SPINODES
    | INCOMPAT_META_UUID
    | INCOMPAT_BIGTIME
    | INCOMPAT_NEEDSREPAIR
    | INCOMPAT_NREXT64
    | INCOMPAT_EXCHRANGE
    | INCOMPAT_PARENT;

const MIN_BLOCK_LOG: u8 = 9;
const MAX_BLOCK_LOG: u8 = 16;
const MIN_SECT_LOG: u8 = 9;
const MAX_SECT_LOG: u8 = 15;
const MIN_INODE_LOG: u8 = 8;
const MAX_INODE_LOG: u8 = 11;
/// The smallest allocation group the format allows.
const MIN_AG_BLOCKS: u64 = 64;
const MAX_AG_BLOCKS: u64 = 1 << 31;
const MAX_AG_COUNT: u64 = 1 << 24;

/// The numbers every address calculation needs.
#[derive(Debug, Clone)]
pub(super) struct Geometry {
    pub block_size: u64,
    pub inode_size: u64,
    pub inop_log: u8,
    pub ag_blk_log: u8,
    pub ag_blocks: u64,
    pub ag_count: u64,
    pub data_blocks: u64,
    pub dir_blk_log: u8,
    pub root_ino: u64,
    pub v5: bool,
    pub ftype: bool,
    pub nrext64: bool,
    /// Directory names compare without regard to ASCII case.
    pub ci: bool,
}

/// `1 << log`, for a log the superblock checks keep far below 64.
fn bit(log: u8) -> u64 {
    1u64.checked_shl(u32::from(log)).unwrap_or(0)
}

/// The low `log` bits.
fn mask(log: u8) -> u64 {
    bit(log).wrapping_sub(1)
}

impl Geometry {
    /// Byte offset of block `fsb` (an AG number and a block in it, packed),
    /// when it is inside the data device.
    pub fn block_offset(&self, fsb: u64) -> Result<u64> {
        let agno = fsb.checked_shr(u32::from(self.ag_blk_log)).unwrap_or(0);
        let agbno = fsb & mask(self.ag_blk_log);
        self.agblock_offset(agno, agbno, fsb)
    }

    fn agblock_offset(&self, agno: u64, agbno: u64, original: u64) -> Result<u64> {
        if agno >= self.ag_count {
            return Err(out_of_range("AG number", agno, "AG count", self.ag_count));
        }
        if agbno >= self.ag_blocks {
            return Err(out_of_range(
                "block in AG",
                agbno,
                "AG blocks",
                self.ag_blocks,
            ));
        }
        let linear = agno
            .checked_mul(self.ag_blocks)
            .and_then(|v| v.checked_add(agbno))
            .ok_or_else(|| invalid("block number", original))?;
        if linear >= self.data_blocks {
            return Err(out_of_range(
                "block number",
                linear,
                "data blocks",
                self.data_blocks,
            ));
        }
        linear
            .checked_mul(self.block_size)
            .ok_or_else(|| invalid("block number", linear))
    }

    /// True when `count` blocks from `fsb` lie in one allocation group
    /// inside the data device.
    pub fn extent_ok(&self, fsb: u64, count: u64) -> bool {
        let agno = fsb.checked_shr(u32::from(self.ag_blk_log)).unwrap_or(0);
        let agbno = fsb & mask(self.ag_blk_log);
        count != 0
            && self.block_offset(fsb).is_ok()
            && agbno
                .checked_add(count)
                .is_some_and(|end| end <= self.ag_blocks)
            && agno
                .checked_mul(self.ag_blocks)
                .and_then(|v| v.checked_add(agbno))
                .and_then(|v| v.checked_add(count))
                .is_some_and(|end| end <= self.data_blocks)
    }

    /// Byte offset of inode `ino`, when it is inside the data device.
    pub fn inode_offset(&self, ino: u64) -> Result<u64> {
        let shift = u32::from(self.ag_blk_log).saturating_add(u32::from(self.inop_log));
        let agno = ino.checked_shr(shift).unwrap_or(0);
        let agbno = ino.checked_shr(u32::from(self.inop_log)).unwrap_or(0) & mask(self.ag_blk_log);
        let slot = ino & mask(self.inop_log);
        let block = self.agblock_offset(agno, agbno, ino)?;
        slot.checked_mul(self.inode_size)
            .and_then(|v| v.checked_add(block))
            .ok_or_else(|| invalid("inode number", ino))
    }

    /// Blocks in one directory block.
    pub fn dir_block_fsblocks(&self) -> u64 {
        bit(self.dir_blk_log)
    }
}

/// What mount keeps from the superblock.
#[derive(Debug, Clone)]
pub(super) struct Superblock {
    pub geo: Geometry,
    pub uuid: [u8; 16],
    pub label: [u8; 12],
    pub icount: u64,
    pub ifree: u64,
    pub free_blocks: u64,
    /// The version 5 checksum did not match (always false on version 4).
    pub crc_bad: bool,
}

fn check_log(field: &'static str, value: u64, log: u8, min: u8, max: u8) -> Result<()> {
    if log < min || log > max || value != 1u64 << log {
        return Err(invalid(field, value));
    }
    Ok(())
}

fn truncated(needed: usize, available: usize) -> Error {
    Corruption::new(
        "XFS superblock",
        Location::Byte(0),
        CorruptKind::Truncated {
            needed: needed as u64,
            available: available as u64,
        },
    )
    .into()
}

impl Superblock {
    /// Reads the superblock from `buf`: the first sector of the source or
    /// more (a version 5 checksum covers the whole sector).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let magic = be32(buf, 0)?;
        if magic != MAGIC {
            return Err(Corruption::new(
                "XFS superblock",
                Location::Byte(0),
                CorruptKind::BadSignature {
                    expected: u64::from(MAGIC),
                    found: u64::from(magic),
                },
            )
            .into());
        }
        if buf.len() < V4_LEN {
            return Err(truncated(V4_LEN, buf.len()));
        }
        let block_size = u64::from(be32(buf, 4)?);
        let data_blocks = be64(buf, 8)?;
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(slice(buf, 32, 16)?);
        let root_ino = be64(buf, 56)?;
        let ag_blocks = u64::from(be32(buf, 84)?);
        let ag_count = u64::from(be32(buf, 88)?);
        let version_num = be16(buf, 100)?;
        let sect_size = u64::from(be16(buf, 102)?);
        let inode_size = u64::from(be16(buf, 104)?);
        let inop_block = u64::from(be16(buf, 106)?);
        let mut label = [0u8; 12];
        label.copy_from_slice(slice(buf, 108, 12)?);
        let block_log = u8_at(buf, 120)?;
        let sect_log = u8_at(buf, 121)?;
        let inode_log = u8_at(buf, 122)?;
        let inop_log = u8_at(buf, 123)?;
        let ag_blk_log = u8_at(buf, 124)?;
        let in_progress = u8_at(buf, 126)?;
        let icount = be64(buf, 128)?;
        let ifree = be64(buf, 136)?;
        let free_blocks = be64(buf, 144)?;
        let dir_blk_log = u8_at(buf, 192)?;
        let features2 = be32(buf, 200)?;
        let bad_features2 = be32(buf, 204)?;

        let v5 = match version_num & 0xF {
            4 => false,
            5 => true,
            other => return Err(Error::Unsupported(format!("XFS version {other}").into())),
        };
        let mut ftype = false;
        let mut nrext64 = false;
        let mut crc_bad = false;
        if v5 {
            if buf.len() < V5_LEN {
                return Err(truncated(V5_LEN, buf.len()));
            }
            let incompat = be32(buf, 216)?;
            let unknown = incompat & !INCOMPAT_KNOWN;
            if unknown != 0 {
                return Err(Error::Unsupported(
                    format!("XFS incompatible features {unknown:#x}").into(),
                ));
            }
            ftype = incompat & INCOMPAT_FTYPE != 0;
            nrext64 = incompat & INCOMPAT_NREXT64 != 0;
            let stored = le32(buf, CRC_OFFSET)?;
            let sector = usize::try_from(sect_size)
                .ok()
                .and_then(|n| buf.get(..n))
                .unwrap_or(buf);
            crc_bad = crc32c_zeroed(sector, CRC_OFFSET) != Some(stored);
        } else {
            if version_num & VERSION_DIRV2 == 0 {
                return Err(Error::Unsupported(
                    "XFS volume with version 1 directories".into(),
                ));
            }
            if version_num & VERSION_MOREBITS != 0 {
                ftype = (features2 | bad_features2) & FEATURES2_FTYPE != 0;
            }
        }
        if in_progress != 0 {
            return Err(invalid("sb_inprogress", u64::from(in_progress)));
        }

        check_log(
            "sb_blocksize",
            block_size,
            block_log,
            MIN_BLOCK_LOG,
            MAX_BLOCK_LOG,
        )?;
        check_log(
            "sb_sectsize",
            sect_size,
            sect_log,
            MIN_SECT_LOG,
            MAX_SECT_LOG,
        )?;
        check_log(
            "sb_inodesize",
            inode_size,
            inode_log,
            MIN_INODE_LOG,
            MAX_INODE_LOG,
        )?;
        if sect_size > block_size || inode_size > block_size {
            return Err(invalid("sb_sectsize", sect_size));
        }
        if inop_block != block_size.checked_div(inode_size).unwrap_or(0)
            || Some(inop_log) != block_log.checked_sub(inode_log)
        {
            return Err(invalid("sb_inopblock", inop_block));
        }
        if u32::from(block_log).saturating_add(u32::from(dir_blk_log)) > u32::from(MAX_BLOCK_LOG) {
            return Err(invalid("sb_dirblklog", u64::from(dir_blk_log)));
        }
        if !(MIN_AG_BLOCKS..=MAX_AG_BLOCKS).contains(&ag_blocks) {
            return Err(invalid("sb_agblocks", ag_blocks));
        }
        // The number of bits in `ag_blocks - 1`: the format's AG block log.
        let want_log = u64::BITS.saturating_sub(ag_blocks.saturating_sub(1).leading_zeros());
        if u32::from(ag_blk_log) != want_log {
            return Err(invalid("sb_agblklog", u64::from(ag_blk_log)));
        }
        if ag_count == 0 || ag_count > MAX_AG_COUNT {
            return Err(invalid("sb_agcount", ag_count));
        }
        // Both products are below 2^55: the limits above hold them down.
        let full = ag_count.saturating_mul(ag_blocks);
        let before_last = ag_count.saturating_sub(1).saturating_mul(ag_blocks);
        // The last group holds at least the smallest group's blocks.
        if data_blocks > full || data_blocks < before_last.saturating_add(MIN_AG_BLOCKS) {
            return Err(invalid("sb_dblocks", data_blocks));
        }
        if data_blocks.checked_mul(block_size).is_none() {
            return Err(invalid("sb_dblocks", data_blocks));
        }

        let geo = Geometry {
            block_size,
            inode_size,
            inop_log,
            ag_blk_log,
            ag_blocks,
            ag_count,
            data_blocks,
            dir_blk_log,
            root_ino,
            v5,
            ftype,
            nrext64,
            ci: version_num & VERSION_BORG != 0,
        };
        geo.inode_offset(root_ino)?;
        Ok(Self {
            geo,
            uuid,
            label,
            icount,
            ifree,
            free_blocks,
            crc_bad,
        })
    }
}
