# atlas-xfs

[![crates.io](https://img.shields.io/crates/v/atlas-xfs.svg)](https://crates.io/crates/atlas-xfs) [![docs.rs](https://img.shields.io/docsrs/atlas-xfs)](https://docs.rs/atlas-xfs) [![CI](https://github.com/05Narwhal/atlas-xfs/actions/workflows/ci.yml/badge.svg)](https://github.com/05Narwhal/atlas-xfs/actions/workflows/ci.yml) ![License](https://img.shields.io/crates/l/atlas-xfs.svg)

**A read-only XFS reader for versions 4 and 5.**

`atlas-xfs` mounts an XFS volume from any `BlockSource` and implements `atlas_fs_core::fs::FileSystem`, so folders, names, file content and allocation come through the same trait as every other `atlas-*` reader.

## Features

- XFS **versions 4 and 5**; version 5 **superblock, block and inode checksums are verified**, along with the owner and number each block records.
- **Shortform, block, leaf and node** directories.
- **Extent-list and B-tree** data forks.
- Big timestamps.

## Quick start

```rust,no_run
use std::path::Path;
use std::sync::Arc;

use atlas_fs_core::fs::{DirBatch, DirProgress, FileSystem};
use atlas_fs_core::io::file::FileSource;

let source = Arc::new(FileSource::open(Path::new("volume.img"))?);
let volume = atlas_xfs::mount(source)?;
println!("{}", volume.info().name);

let mut stream = volume.read_dir(volume.root())?;
let mut batch = DirBatch::new();
while stream.next_batch(&mut batch, 256)? == DirProgress::More {}
for entry in batch.iter() {
    println!("{}", entry.name);
}
```

## How it works

A node id is the inode number. Metadata read from the disk is checked before use: every length, offset and count, the checksum of version 5 blocks and inodes, and the owner and number each one records.

## Limits

Read-only. The log is never replayed, so a volume that was not unmounted cleanly may show the state before its last changes. Not supported: realtime files, extended attributes, the free-space map, and the metadata directory feature of the newest kernels. Lookups scan the directory rather than using the hash index.

## Safety and robustness

- **No `unsafe`.** The crate is `#![forbid(unsafe_code)]`.
- **On-disk bytes are hostile.** Every length, offset and count read from a disk is validated before use. Damage is returned as an error, never a panic, an unchecked index or an unbounded loop.
- **Lints enforce it.** Clippy's `indexing_slicing`, `arithmetic_side_effects`, `unwrap_used`, `expect_used` and `panic` lints are on for the library code.
- **Read-only.** Nothing in this crate writes to a disk or image.
- **Fuzzed.** A `cargo-fuzz` target in `fuzz/` runs the parser on arbitrary bytes.

## Feature flags

- `test-util`: exposes the synthetic test image builders and fuzz seed inputs this crate's own tests use, for tests of crates built on it. It is not part of the stable API.

## Install

```toml
[dependencies]
atlas-xfs = "26.0.1-a"
```

Versions follow `26.MAJOR.MINOR-STAGE`: the year, the major and minor version, and a stage letter (`a` alpha, `b` beta, `s` stable). Cargo treats a stage suffix as a pre-release, so name the version exactly as above.

Minimum supported Rust version: 1.88 (edition 2024).

## Licence

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or MIT license
([LICENSE-MIT](LICENSE-MIT)) at your option.

<!-- atlas-family -->
## The atlas family

`atlas-xfs` is one of the `atlas-*` crates: read-only, safe-Rust readers for disk formats, built for [Disk Atlas](https://github.com/05Narwhal/DiskAtlas), a fast cross-platform file explorer that reads disks the host OS cannot. They share the traits in [`atlas-fs-core`](https://github.com/05Narwhal/atlas-fs-core).

| Crate | What it reads |
|---|---|
| [`atlas-fs-core`](https://github.com/05Narwhal/atlas-fs-core) | Core traits and types for reading filesystems and disk images: BlockSource, FileSystem, DirStream and one error type |
| [`atlas-partition`](https://github.com/05Narwhal/atlas-partition) | Read GPT, MBR and Apple Partition Map partition tables, with damage reported instead of hidden |
| [`atlas-fat`](https://github.com/05Narwhal/atlas-fat) | Read FAT12, FAT16 and FAT32 volumes: boot sector, cluster chains, 8.3 and long names |
| [`atlas-exfat`](https://github.com/05Narwhal/atlas-exfat) | Read exFAT volumes: boot sector, allocation bitmap, entry sets, contiguous and chained files |
| [`atlas-ntfs`](https://github.com/05Narwhal/atlas-ntfs) | Read NTFS volumes: MFT records, attribute lists, $I30 indexes, LZNT1 compression, alternate data streams and reparse points |
| [`atlas-hfsplus`](https://github.com/05Narwhal/atlas-hfsplus) | Read HFS+ and HFSX volumes: catalog and extents B-trees, hard links, decmpfs compression, extended attributes and journal replay |
| [`atlas-hfs`](https://github.com/05Narwhal/atlas-hfs) | Read classic HFS (Mac OS Standard) volumes: master directory block, catalog and extents B-trees, forks, Mac text encodings |
| [`atlas-apfs`](https://github.com/05Narwhal/atlas-apfs) | Read APFS containers: checkpoints, object maps, volumes, snapshots, sealed volumes, extended attributes and decmpfs compression |
| [`atlas-btrfs`](https://github.com/05Narwhal/atlas-btrfs) | Read Btrfs volumes: chunk map, subvolumes, inline, regular and sparse extents, zlib, LZO and zstd compression, crc32c checksums |
| [`atlas-xfs`](https://github.com/05Narwhal/atlas-xfs) | Read XFS volumes, versions 4 and 5: shortform, block, leaf and node directories, extent and B-tree data forks |
| [`atlas-iso9660`](https://github.com/05Narwhal/atlas-iso9660) | Read ISO 9660 (ECMA-119) discs with Joliet and Rock Ridge extensions |
| [`atlas-udf`](https://github.com/05Narwhal/atlas-udf) | Read UDF (ECMA-167, OSTA UDF 1.02 to 2.60) discs and images: metadata partitions, extended entries, sparse files |
| [`atlas-dmg`](https://github.com/05Narwhal/atlas-dmg) | Read Apple disk images (UDIF .dmg): zlib, bzip2, LZFSE, LZMA and ADC chunks, plus a bounded property list parser |
| [`atlas-sparsebundle`](https://github.com/05Narwhal/atlas-sparsebundle) | Read Apple sparse bundle and sparse image disk images |
| [`atlas-dc42`](https://github.com/05Narwhal/atlas-dc42) | Read DiskCopy 4.2 floppy disk images |
| [`atlas-vhd`](https://github.com/05Narwhal/atlas-vhd) | Read Microsoft VHD and VHDX virtual disks: fixed, dynamic and differencing images |
| [`atlas-vmdk`](https://github.com/05Narwhal/atlas-vmdk) | Read VMware VMDK virtual disks: monolithic sparse, stream-optimized and descriptor-based images |
| [`atlas-vdi`](https://github.com/05Narwhal/atlas-vdi) | Read VirtualBox VDI virtual disks: dynamic and fixed images |
| [`atlas-luks`](https://github.com/05Narwhal/atlas-luks) | Read LUKS1 and LUKS2 encrypted volumes: keyslot unlock with PBKDF2 or Argon2, AES-XTS and AES-CBC sector decryption |
| [`atlas-bitlocker`](https://github.com/05Narwhal/atlas-bitlocker) | Read BitLocker encrypted volumes: FVE metadata, password and recovery-password unlock, AES-XTS and AES-CBC sector decryption |

Made by Julian A. Loy · [julianloy.com](https://julianloy.com)
