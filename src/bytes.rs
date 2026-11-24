// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! Big-endian field reads from a buffer. Every read is bounds-checked and
//! fails with an error, never a panic.

use super::other;
use atlas_fs_core::error::Result;

const SHORT: &str = "a structure is shorter than its fields";

fn take<const N: usize>(buf: &[u8], at: usize) -> Result<[u8; N]> {
    at.checked_add(N)
        .and_then(|end| buf.get(at..end))
        .and_then(|s| <[u8; N]>::try_from(s).ok())
        .ok_or_else(|| other(SHORT))
}

pub(super) fn u8_at(buf: &[u8], at: usize) -> Result<u8> {
    buf.get(at).copied().ok_or_else(|| other(SHORT))
}

pub(super) fn be16(buf: &[u8], at: usize) -> Result<u16> {
    take::<2>(buf, at).map(u16::from_be_bytes)
}

pub(super) fn be32(buf: &[u8], at: usize) -> Result<u32> {
    take::<4>(buf, at).map(u32::from_be_bytes)
}

pub(super) fn be64(buf: &[u8], at: usize) -> Result<u64> {
    take::<8>(buf, at).map(u64::from_be_bytes)
}

/// `len` bytes at `at`.
pub(super) fn slice(buf: &[u8], at: usize, len: usize) -> Result<&[u8]> {
    at.checked_add(len)
        .and_then(|end| buf.get(at..end))
        .ok_or_else(|| other(SHORT))
}

/// The one little-endian field: a version 5 checksum.
pub(super) fn le32(buf: &[u8], at: usize) -> Result<u32> {
    take::<4>(buf, at).map(u32::from_le_bytes)
}
