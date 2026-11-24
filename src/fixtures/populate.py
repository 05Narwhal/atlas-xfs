# Fills a mounted XFS with the tree the xfs tests expect. Run by make_fixtures.sh.
# Usage: populate.py <mount point> <full|tiny>
import os
import stat
import sys

root, mode = sys.argv[1], sys.argv[2]
T = 1_700_000_000


def pat(n, seed):
    return bytes(((i * 31 + seed) & 0xFF) for i in range(n))


def put(path, data, ns=None):
    full = os.path.join(root, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "wb") as f:
        f.write(data)
    t = T * 1_000_000_000 + (ns if ns is not None else 0)
    os.utime(full, ns=(t, t))


put("hello.txt", b"hello xfs\n", 123_456_789)
put("empty", b"")
put("big.bin", pat(100_000, 3))
put("unicode/héllo wörld.txt", b"unicode")
put("sf/a.txt", b"alpha")
put("sf/b.txt", b"bravo")
put("sf/c/d.txt", b"delta")
put("deep/1/2/3/4/5/6/7/8/9/file.txt", b"deep")
put("longname/" + "L" * 255, b"long name")
os.link(os.path.join(root, "hello.txt"), os.path.join(root, "hello-link.txt"))

os.makedirs(os.path.join(root, "links"))
os.symlink("../hello.txt", os.path.join(root, "links/short"))
os.symlink("../sf", os.path.join(root, "links/dirlink"))
os.symlink("nowhere/at/all", os.path.join(root, "links/dangling"))
LONG = "/".join(["abcdefgh"] * 44)
os.symlink(LONG, os.path.join(root, "links/long"))
os.mkfifo(os.path.join(root, "fifo"))
os.mknod(os.path.join(root, "null"), 0o666 | stat.S_IFCHR, os.makedev(1, 3))

# Block form: one directory block. The rest are hard links.
os.makedirs(os.path.join(root, "blk"))
for i in range(40):
    os.link(os.path.join(root, "hello.txt"), os.path.join(root, "blk", "blk-%04d" % i))

if mode == "full":
    # Leaf form: a few data blocks and a leaf block.
    os.makedirs(os.path.join(root, "leaf"))
    for i in range(300):
        os.link(
            os.path.join(root, "hello.txt"),
            os.path.join(root, "leaf", "leaf-entry-with-a-long-name-%04d" % i),
        )
    # Node form: more entries than one leaf block holds.
    os.makedirs(os.path.join(root, "node"))
    for i in range(1300):
        os.link(os.path.join(root, "hello.txt"), os.path.join(root, "node", "n%05d" % i))

    # A fork in the btree format: 600 extents with a hole between each.
    with open(os.path.join(root, "frag.bin"), "wb") as f:
        for i in range(0, 1200, 2):
            f.seek(i * 4096)
            f.write(pat(4096, i & 0xFF))
    # Unwritten extents: preallocated, then the first block written.
    fd = os.open(os.path.join(root, "unwritten.bin"), os.O_CREAT | os.O_RDWR)
    os.posix_fallocate(fd, 0, 65536)
    os.pwrite(fd, pat(4096, 9), 0)
    os.close(fd)
    # Bigger than 4 GiB, sparse.
    fd = os.open(os.path.join(root, "huge-sparse.bin"), os.O_CREAT | os.O_RDWR)
    os.ftruncate(fd, 5 * 2**30)
    os.pwrite(fd, pat(4096, 5), 5 * 2**30 - 4096)
    os.close(fd)
