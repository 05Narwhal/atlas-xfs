# Packs an XFS image into the sparse fixture format the tests read:
#   u64 LE total length, then records of (u64 LE offset, u32 LE length, bytes)
# for every run of non-zero 4 KiB chunks. The internal log is zeroed first (the
# reader never looks at it). Compress the result with zstd.
# Usage: pack.py <image> <output>
import struct
import sys

CHUNK = 4096
img = bytearray(open(sys.argv[1], "rb").read())
bs = struct.unpack(">I", img[4:8])[0]
agblocks = struct.unpack(">I", img[84:88])[0]
logstart = struct.unpack(">Q", img[48:56])[0]
logblocks = struct.unpack(">I", img[96:100])[0]
agblklog = img[124]
agno, agbno = logstart >> agblklog, logstart & ((1 << agblklog) - 1)
log_off = (agno * agblocks + agbno) * bs
img[log_off : log_off + logblocks * bs] = bytes(logblocks * bs)

out = bytearray(struct.pack("<Q", len(img)))
run_start = None
for pos in range(0, len(img), CHUNK):
    nonzero = any(img[pos : pos + CHUNK])
    if nonzero and run_start is None:
        run_start = pos
    if not nonzero and run_start is not None:
        out += struct.pack("<QI", run_start, pos - run_start) + img[run_start:pos]
        run_start = None
if run_start is not None:
    out += struct.pack("<QI", run_start, len(img) - run_start) + img[run_start:]
open(sys.argv[2], "wb").write(out)
