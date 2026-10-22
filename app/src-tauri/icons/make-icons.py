"""Generates the application icons.

Checked in alongside the icons it produces so the artwork is reproducible and
reviewable as code rather than as opaque binaries. Pure stdlib: zlib and
struct are enough for PNG, and an ICO is a small header wrapped around PNGs.

    python make-icons.py
"""

import struct
import zlib
from pathlib import Path

HERE = Path(__file__).parent

# Slate background, light glyph: legible on both a light and a dark taskbar.
BACKGROUND = (29, 35, 48, 255)
ARROW = (120, 190, 255, 255)
TRANSPARENT = (0, 0, 0, 0)


def rounded(x, y, size, radius):
    """Whether (x, y) falls inside a rounded square."""
    cx = min(max(x, radius), size - 1 - radius)
    cy = min(max(y, radius), size - 1 - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius * radius


def glyph(x, y, size):
    """A downward arrow over a baseline: the universal download mark."""
    unit = size / 32.0
    fx, fy = x / unit, y / unit

    # Shaft.
    if 14.0 <= fx <= 18.0 and 6.0 <= fy <= 18.0:
        return True
    # Head: a triangle narrowing as it descends.
    if 18.0 <= fy <= 24.0:
        half = 9.0 * (24.0 - fy) / 6.0
        if abs(fx - 16.0) <= half:
            return True
    # Baseline the arrow lands on.
    if 7.0 <= fx <= 25.0 and 26.0 <= fy <= 28.0:
        return True
    return False


def render(size):
    radius = max(2, size // 6)
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            if not rounded(x, y, size, radius):
                row += bytes(TRANSPARENT)
            elif glyph(x, y, size):
                row += bytes(ARROW)
            else:
                row += bytes(BACKGROUND)
        rows.append(bytes(row))
    return rows


def png_bytes(size):
    rows = render(size)
    # Filter byte 0 (None) before each scanline.
    raw = b"".join(b"\x00" + row for row in rows)

    def chunk(kind, payload):
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico_bytes(sizes):
    """An ICO whose entries are PNGs, which Windows has accepted since Vista."""
    images = [(size, png_bytes(size)) for size in sizes]
    offset = 6 + 16 * len(images)

    directory = b""
    for size, data in images:
        directory += struct.pack(
            "<BBBBHHII",
            0 if size >= 256 else size,  # 0 means 256
            0 if size >= 256 else size,
            0,  # palette
            0,  # reserved
            1,  # colour planes
            32,  # bits per pixel
            len(data),
            offset,
        )
        offset += len(data)

    return struct.pack("<HHH", 0, 1, len(images)) + directory + b"".join(d for _, d in images)


def main():
    for size, name in [
        (32, "32x32.png"),
        (128, "128x128.png"),
        (256, "128x128@2x.png"),
        (512, "icon.png"),
    ]:
        (HERE / name).write_bytes(png_bytes(size))
        print("wrote", name)

    (HERE / "icon.ico").write_bytes(ico_bytes([16, 32, 48, 256]))
    print("wrote icon.ico")

    # The tray wants something that reads at 16 px, where the rounded corners
    # and the baseline blur into each other.
    (HERE / "tray.png").write_bytes(png_bytes(32))
    print("wrote tray.png")


if __name__ == "__main__":
    main()
