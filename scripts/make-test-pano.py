#!/usr/bin/env python3
"""Writes a synthetic 2:1 equirectangular test panorama as PNG (standard library only).

Used by the CI smoke tests:  python3 scripts/make-test-pano.py out.png [width]
"""
import math
import struct
import sys
import zlib


def main() -> None:
    path = sys.argv[1]
    w = int(sys.argv[2]) if len(sys.argv) > 2 else 2048
    h = w // 2
    rows = bytearray()
    for y in range(h):
        rows.append(0)  # PNG filter: none
        v = y / h
        for x in range(w):
            u = x / w
            if (u * 36) % 1 < 0.04 or (v * 18) % 1 < 0.04:
                rows += b"\xf0\xf0\xf0"  # lat/long grid
            else:
                rows += bytes((int(255 * u), int(255 * v), int(127 + 120 * math.sin(u * math.tau * 3))))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(rows), 6)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


if __name__ == "__main__":
    main()
