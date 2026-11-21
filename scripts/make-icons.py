"""Draws the Glass icon (design/icon/glass-icon.svg, chosen 2026-11-21) and
writes app-win/packaging/glass.ico and app-mac/packaging/Glass.icns.

No image libraries needed: numpy rasterises the few shapes the design uses
(rounded rectangles and quadrilaterals) with supersampling, and the PNG, ICO
and ICNS containers are written by hand. Small sizes (≤ 32 px) use the
simplified version: a white monitor on the blue pane.

Run from the repo root:  python scripts/make-icons.py
"""
import os
import struct
import zlib

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def hexrgb(h):
    h = h.lstrip('#')
    return tuple(int(h[i:i + 2], 16) / 255 for i in (0, 2, 4))


PANE = (112, 112, 800, 800, 180)

FULL = [
    # (kind, geometry, colour, opacity, clipped to the pane)
    ('rrect', PANE, '#173A66', 1.0, False),
    ('rrect', (232, 262, 560, 380, 40), '#0E1726', 1.0, True),
    ('rrect', (262, 292, 500, 320, 16), '#3E7FC8', 1.0, True),
    ('rrect', (290, 320, 200, 130, 10), '#A9D2F7', 1.0, True),
    ('rrect', (510, 320, 224, 60, 10), '#DDEEFF', 1.0, True),
    ('rrect', (510, 396, 224, 188, 10), '#6FA6E0', 1.0, True),
    ('rrect', (472, 642, 80, 62, 0), '#0E1726', 1.0, True),
    ('rrect', (392, 700, 240, 32, 16), '#0E1726', 1.0, True),
    ('quad', [(300, 112), (470, 112), (112, 470), (112, 300)], '#FFFFFF', 0.22, True),
    ('quad', [(560, 112), (612, 112), (112, 612), (112, 560)], '#FFFFFF', 0.16, True),
    ('quad', [(912, 560), (912, 700), (700, 912), (560, 912)], '#FFFFFF', 0.10, True),
    ('ring', ((126, 126, 772, 772, 168), (142, 142, 740, 740, 152)), '#DDEEFF', 0.75, False),
]

SMALL = [
    ('rrect', PANE, '#173A66', 1.0, False),
    ('rrect', (212, 242, 600, 420, 44), '#FFFFFF', 1.0, True),
    ('rrect', (272, 302, 480, 300, 12), '#3E7FC8', 1.0, True),
    ('rrect', (472, 662, 80, 50, 0), '#FFFFFF', 1.0, True),
    ('rrect', (372, 702, 280, 60, 30), '#FFFFFF', 1.0, True),
]


# glass-cli.exe: the same pane with a terminal badge (">_") in the corner, so
# it can't be mistaken for Glass.exe.
CLI_BADGE = [
    ('rrect', (500, 540, 400, 360, 70), '#0E1726', 1.0, False),
    ('ring', ((500, 540, 400, 360, 70), (518, 558, 364, 324, 52)), '#DDEEFF', 0.9, False),
    ('quad', [(585, 640), (640, 640), (725, 730), (670, 730)], '#FFFFFF', 1.0, False),
    ('quad', [(670, 730), (725, 730), (640, 820), (585, 820)], '#FFFFFF', 1.0, False),
    ('rrect', (735, 786, 125, 36, 8), '#FFFFFF', 1.0, False),
]


def rrect_mask(X, Y, g):
    x, y, w, h, r = g
    cx, cy = x + w / 2, y + h / 2
    dx = np.maximum(np.abs(X - cx) - (w / 2 - r), 0)
    dy = np.maximum(np.abs(Y - cy) - (h / 2 - r), 0)
    inside = (np.abs(X - cx) <= w / 2) & (np.abs(Y - cy) <= h / 2)
    if r > 0:
        inside &= dx * dx + dy * dy <= r * r
    return inside


def quad_mask(X, Y, pts):
    m = np.ones_like(X, dtype=bool)
    sign = None
    for i in range(4):
        (x1, y1), (x2, y2) = pts[i], pts[(i + 1) % 4]
        c = (x2 - x1) * (Y - y1) - (y2 - y1) * (X - x1)
        if sign is None:
            # orientation from the polygon's own winding
            area = sum((pts[j][0] * pts[(j + 1) % 4][1] - pts[(j + 1) % 4][0] * pts[j][1]) for j in range(4))
            sign = 1 if area > 0 else -1
        m &= (c * sign) >= 0
    return m


def render(size, shapes, crop):
    x0, y0, x1, y1 = crop
    ss = 4 if size <= 256 else 2
    n = size * ss
    step = (x1 - x0) / n
    coords = x0 + (np.arange(n) + 0.5) * step
    X, Y = np.meshgrid(coords, y0 + (np.arange(n) + 0.5) * (y1 - y0) / n)
    rgb = np.zeros((size, size, 3), np.float32)  # premultiplied
    a = np.zeros((size, size), np.float32)
    pane = rrect_mask(X, Y, PANE)

    def cover(mask):
        return mask.reshape(size, ss, size, ss).mean(axis=(1, 3)).astype(np.float32)

    for kind, g, col, op, clip in shapes:
        if kind == 'rrect':
            m = rrect_mask(X, Y, g)
        elif kind == 'quad':
            m = quad_mask(X, Y, g)
        else:  # ring between two rounded rects
            m = rrect_mask(X, Y, g[0]) & ~rrect_mask(X, Y, g[1])
        if clip:
            m &= pane
        cov = cover(m) * op
        c = np.array(hexrgb(col), np.float32)
        rgb = rgb * (1 - cov[..., None]) + c * cov[..., None]
        a = a * (1 - cov) + cov
    out = np.zeros((size, size, 4), np.uint8)
    safe = np.where(a > 0, a, 1)
    out[..., :3] = np.clip(rgb / safe[..., None] * 255 + 0.5, 0, 255).astype(np.uint8)
    out[..., 3] = np.clip(a * 255 + 0.5, 0, 255).astype(np.uint8)
    return out


def png(img):
    h, w, _ = img.shape
    raw = b''.join(b'\x00' + img[y].tobytes() for y in range(h))

    def chunk(t, d):
        return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d) & 0xFFFFFFFF)

    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))


def icon_png(size, crop):
    return png(render(size, SMALL if size <= 32 else FULL, crop))


def main():
    # Windows: fill more of the square (no macOS-style margin).
    win_crop = (100, 100, 924, 924)
    sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
    pngs = [icon_png(s, win_crop) for s in sizes]
    ico = struct.pack('<HHH', 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries = b''
    for s, p in zip(sizes, pngs):
        entries += struct.pack('<BBBBHHII', s % 256, s % 256, 0, 0, 1, 32, len(p), offset)
        offset += len(p)
    path = os.path.join(ROOT, 'app-win', 'packaging', 'glass.ico')
    with open(path, 'wb') as f:
        f.write(ico + entries + b''.join(pngs))
    print('wrote', path)

    # glass-cli.exe: the same sizes with the terminal badge.
    cli = [png(render(s, (SMALL if s <= 32 else FULL) + CLI_BADGE, win_crop)) for s in sizes]
    ico = struct.pack('<HHH', 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries = b''
    for s, p in zip(sizes, cli):
        entries += struct.pack('<BBBBHHII', s % 256, s % 256, 0, 0, 1, 32, len(p), offset)
        offset += len(p)
    path = os.path.join(ROOT, 'app-win', 'packaging', 'glass-cli.ico')
    with open(path, 'wb') as f:
        f.write(ico + entries + b''.join(cli))
    print('wrote', path)
    with open(os.path.join(ROOT, 'design', 'icon', 'glass-cli-icon-256.png'), 'wb') as f:
        f.write(png(render(256, FULL + CLI_BADGE, win_crop)))

    # macOS: the full 1024 grid (the design already keeps Apple's margin).
    mac_crop = (0, 0, 1024, 1024)
    types = [('icp4', 16), ('icp5', 32), ('icp6', 64), ('ic07', 128), ('ic08', 256), ('ic09', 512),
             ('ic10', 1024), ('ic11', 32), ('ic12', 64), ('ic13', 256), ('ic14', 512)]
    cache = {}
    body = b''
    for t, s in types:
        if s not in cache:
            cache[s] = icon_png(s, mac_crop)
        body += t.encode() + struct.pack('>I', 8 + len(cache[s])) + cache[s]
    path = os.path.join(ROOT, 'app-mac', 'packaging', 'Glass.icns')
    with open(path, 'wb') as f:
        f.write(b'icns' + struct.pack('>I', 8 + len(body)) + body)
    print('wrote', path)
    # Raw RGBA for the Windows app's window and tray icons (no decoder needed).
    for size in (32, 64):
        img = render(size, SMALL if size <= 32 else FULL, win_crop)
        with open(os.path.join(ROOT, 'app-win', 'packaging', f'glass-{size}.rgba'), 'wb') as f:
            f.write(img.tobytes())
    # A preview for the docs.
    with open(os.path.join(ROOT, 'design', 'icon', 'glass-icon-256.png'), 'wb') as f:
        f.write(icon_png(256, mac_crop))


if __name__ == '__main__':
    main()
