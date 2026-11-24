#!/bin/sh
# Rebuilds the XFS fixtures. Needs root, xfsprogs, python3, zstd and a kernel that
# mounts XFS (WSL2 does): wsl.exe -d kali-linux -u root -- sh make_fixtures.sh <out dir>
# mkfs.xfs refuses images under 300 MiB, so the images are 300 MiB and mostly
# zeros; pack.py stores only the non-zero chunks.
set -e
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here}
work=$(mktemp -d)
make() { # name, mode, mkfs options...
    name=$1; mode=$2; shift 2
    truncate -s 300m "$work/$name.img"
    mkdir -p "$work/mnt"
    if mkfs.xfs -f -L atlas "$@" "$work/$name.img" > /dev/null 2>&1 \
        && mount -o loop "$work/$name.img" "$work/mnt" 2>/dev/null; then
        python3 "$here/populate.py" "$work/mnt" "$mode"
        sync
        umount "$work/mnt"
    else
        # Recent kernels no longer mount v4: build the tree on the host and let
        # mkfs.xfs copy it in.
        rm -rf "$work/tree"; mkdir "$work/tree"
        python3 "$here/populate.py" "$work/tree" "$mode"
        mkfs.xfs -f -L atlas "$@" -p "$work/tree" "$work/$name.img" > /dev/null
    fi
    python3 "$here/pack.py" "$work/$name.img" "$work/$name.pack"
    zstd -19 -q -f "$work/$name.pack" -o "$out/$name.pack.zst"
}
make v5_main full
make v4_main full -m crc=0 -n ftype=0,size=8192 -i size=256 -d agcount=2
make v5_tiny tiny -d agcount=2
make v4_tiny tiny -m crc=0 -n ftype=0 -i size=256 -d agcount=2
rm -rf "$work"
