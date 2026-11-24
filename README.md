# atlas-xfs

A read-only XFS reader. It implements `atlas_fs_core::fs::FileSystem`, so a mounted volume lists folders, looks up names, reads
file content and reports its allocation through the same trait as the other `atlas-*` readers.

- Versions 4 and 5; version 5 block, inode and superblock checksums are verified.
- Shortform, block, leaf and node directories; extent-list and B-tree data forks; big timestamps.
- The log is never replayed. Read-only: no realtime files, no extended attributes.
- No `unsafe`. Every length, offset and count read from the disk is checked; damage is an error, not a panic.

```rust,no_run
use std::sync::Arc;
use atlas_fs_core::fs::FileSystem;
use atlas_fs_core::io::block::BlockSource;
use atlas_fs_core::io::file::FileSource;

let source: Arc<dyn BlockSource> = Arc::new(FileSource::open("volume.img".as_ref())?);
let volume = atlas_xfs::mount(source)?;
let mut dir = volume.read_dir(volume.root())?;
# Ok::<(), atlas_fs_core::error::Error>(())
```

The `test-util` feature exposes the test image builders and fuzz seed inputs; it is for tests of crates that use
this one and is not part of the stable API.

## Licence

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or MIT license
([LICENSE-MIT](LICENSE-MIT)) at your option.
