// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! Seed inputs for the XFS fuzz target in `fuzz/`.

use super::test_image::load;

/// The largest input the fuzz run accepts (`-max_len=1048576`).
const MAX_SEED: usize = 1 << 20;

/// The two small fixtures, cut to the first mebibyte: the superblock, the
/// allocation group headers, the first inode chunk and the first directory
/// blocks sit there. Disk Atlas writes the head of each as a `probe` seed too.
pub fn volumes() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("xfs-v4-tiny", load("v4_tiny").head(MAX_SEED)),
        ("xfs-v5-tiny", load("v5_tiny").head(MAX_SEED)),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use atlas_fs_core::fs::FileSystem;
    use atlas_fs_core::io::block::MemSource;

    use super::{MAX_SEED, volumes};

    /// Writes the seeds into `$ATLAS_FUZZ_SEEDS/xfs_mount`, with an `xfs-`
    /// prefix so other seeds in that folder stay. Run it with
    /// `cargo test --lib write_fuzz_seeds -- --ignored`.
    #[test]
    #[ignore = "writes files; run by the fuzz CI job"]
    fn write_fuzz_seeds() {
        let Some(root) = std::env::var_os("ATLAS_FUZZ_SEEDS") else {
            return;
        };
        let mount_dir = Path::new(&root).join("xfs_mount");
        fs::create_dir_all(&mount_dir).expect("create seed directory");
        for (name, image) in volumes() {
            fs::write(mount_dir.join(name), &image).expect("write mount seed");
        }
    }

    /// Every seed fits the run's input limit, mounts and lists its root, so the
    /// target starts on the valid path.
    #[test]
    fn mount_seeds_are_valid_and_small() {
        for (name, image) in volumes() {
            assert!(image.len() <= MAX_SEED, "{name}: {} bytes", image.len());
            let volume = crate::mount(Arc::new(MemSource::new(image)))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(volume.read_dir(volume.root()).is_ok(), "{name}");
        }
    }
}
