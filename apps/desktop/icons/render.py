#!/usr/bin/env python3
"""Renders the desktop app icons from the Inkbridge mark (an ink drop whose
highlight is a bridge; the same geometry as apps/android/icon/inkbridge.svg)
without any image library: python3 render.py (writes next to this file)."""
import math, os, struct, zlib

HERE = os.path.dirname(os.path.abspath(__file__))

def cubic(p0, p1, p2, p3, t):
    u = 1 - t
    return tuple(u*u*u*a + 3*u*u*t*b + 3*u*t*t*c + t*t*t*d for a, b, c, d in zip(p0, p1, p2, p3))

# Left edge of the drop's upper half: from the tip (54,25) to (33,63).
_EDGE = [cubic((54, 25), (54, 25), (33, 49), (33, 63), i / 400) for i in range(401)]

def half_width(y):
    """Half-width of the drop's upper part at height y (25..63)."""
    best = min(_EDGE, key=lambda p: abs(p[1] - y))
    return 54 - best[0]

def inside(x, y):
    """The mark in the 108x108 canvas: drop minus the bridge arch."""
    in_drop = (x - 54) ** 2 + (y - 63) ** 2 <= 21 ** 2 or (25 <= y <= 63 and abs(x - 54) <= half_width(y))
    r2 = (x - 54) ** 2 + (y - 70) ** 2
    in_arch = y <= 70 and 7.5 ** 2 <= r2 <= 12.5 ** 2
    return in_drop and not in_arch

def render(size, background=True, mark=(255, 255, 255), bg=(0x10, 0x11, 0x14)):
    # Share of the icon's height the mark fills (its box: x 33..75, y 25..84).
    fill = 0.62 if background else 0.94
    units = 59 / fill  # canvas units across the icon
    """RGBA rows. The mark sits in a rounded square (app icon) or alone."""
    ss = 4
    rows = []
    radius = 0.22 * size
    for py in range(size):
        row = bytearray()
        for px in range(size):
            cover = 0
            for sy in range(ss):
                for sx in range(ss):
                    u = (px + (sx + 0.5) / ss) / size
                    v = (py + (sy + 0.5) / ss) / size
                    x = 54 + (u - 0.5) * units
                    y = 54.5 + (v - 0.5) * units
                    cover += inside(x, y)
            a = cover / (ss * ss)
            if background:
                # Rounded square, anti-aliased by distance to its corner arcs.
                dx = max(radius - px - 0.5, px + 0.5 - (size - radius), 0)
                dy = max(radius - py - 0.5, py + 0.5 - (size - radius), 0)
                inside_sq = max(0.0, min(1.0, radius - math.hypot(dx, dy) + 0.5))
                rgb = [round(bg[i] * (1 - a) + mark[i] * a) for i in range(3)]
                row += bytes(rgb + [round(255 * inside_sq)])
            else:
                row += bytes(list(mark) + [round(255 * a)])
        rows.append(bytes(row))
    return rows

def png(rows):
    size = len(rows)
    raw = b"".join(b"\x00" + r for r in rows)
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))

def ico(images):
    """An .ico holding PNG images (Windows Vista and later)."""
    head = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, data = b"", b""
    for size, blob in images:
        entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    return head + entries + data

def main():
    files = {
        "32x32.png": png(render(32)),
        "128x128.png": png(render(128)),
        "128x128@2x.png": png(render(256)),
        "icon.png": png(render(512)),
        # Tray: the mark alone, white (Windows/Linux trays are dark by default)
        # and black for macOS (a template image the system recolours).
        "tray.png": png(render(64, background=False)),
        "tray-connected.png": png(render(64, background=False, mark=(0x6C, 0x9B, 0xFF))),
        "tray-template.png": png(render(64, background=False, mark=(0, 0, 0))),
    }
    files["icon.ico"] = ico([(s, png(render(s))) for s in (16, 24, 32, 48, 64, 256)])
    for name, blob in files.items():
        with open(os.path.join(HERE, name), "wb") as f:
            f.write(blob)
        print(name, len(blob))

if __name__ == "__main__":
    main()
