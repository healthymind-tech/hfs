#!/usr/bin/env python3
"""Regenerates crates/ui/assets/brand/* from the official HealthyMind logos.

Usage: python3 crates/ui/dev/brand-assets.py <logo-dir> <out-dir>

<logo-dir> holds the brand folder's four files (transparent / white
background, horizontal / square). Needs only Pillow. The heart mark is
traced to `favicon.svg` with a small marching-squares + Douglas-Peucker
tracer (no potrace in the toolchain); the PNG icons are cut from the same
mark. Re-run whenever the official logos change; commit the outputs.
"""
import json
import math
import sys
from pathlib import Path

from PIL import Image

SRC = Path(sys.argv[1])
OUT = Path(sys.argv[2])
OUT.mkdir(parents=True, exist_ok=True)

H_TRANSPARENT = SRC / "透明底_橫幅_完整Logo.png"
S_TRANSPARENT = SRC / "透明底_正方形_僅英文Logo.png"
H_WHITE = SRC / "白底_橫幅_完整Logo.jpeg"
S_WHITE = SRC / "白底_正方形_僅英文Logo.png"


def save_png(im, name):
    im.save(OUT / name, optimize=True)


def fit_height(im, h):
    w = round(im.width * h / im.height)
    return im.resize((w, h), Image.LANCZOS)


# Horizontal logo: cropped to its ink, 3x the 28 px topbar height (crisp on
# HiDPI) and 2x the 64 px sign-in/about size.
h = Image.open(H_TRANSPARENT).convert("RGBA")
h = h.crop(h.getbbox())
save_png(fit_height(h, 96), "healthymind-logo-horizontal.png")
save_png(fit_height(h, 192), "healthymind-logo-horizontal@2x.png")
hw = Image.open(H_WHITE).convert("RGB")
hw.thumbnail((1024, 1024), Image.LANCZOS)
hw.save(OUT / "healthymind-logo-horizontal-white.jpg", quality=88, optimize=True)

# Square logo: 240 px (120 px displayed, 2x).
s = Image.open(S_TRANSPARENT).convert("RGBA")
s = s.crop(s.getbbox())
save_png(fit_height(s, 240), "healthymind-logo-square.png")
sw = Image.open(S_WHITE).convert("RGBA")
sw.thumbnail((480, 480), Image.LANCZOS)
save_png(sw, "healthymind-logo-square-white.png")

# Heart mark: the left part of the horizontal logo (everything left of the
# wordmark), padded to a square.
full = Image.open(H_TRANSPARENT).convert("RGBA")
alpha = full.getchannel("A")
# The wordmark starts at the first fully transparent column run after the
# heart; find the first gap column wider than 20 px.
cols = [any(alpha.getpixel((x, y)) > 16 for y in range(0, full.height, 2)) for x in range(full.width)]
start = next(i for i, c in enumerate(cols) if c)
x = start
gap = 0
while x < full.width:
    gap = gap + 1 if not cols[x] else 0
    if gap > 20:
        break
    x += 1
mark = full.crop((0, 0, x - gap, full.height))
mark = mark.crop(mark.getbbox())
side = max(mark.size)
pad = round(side * 0.06)
sq = Image.new("RGBA", (side + 2 * pad, side + 2 * pad), (0, 0, 0, 0))
sq.paste(mark, ((sq.width - mark.width) // 2, (sq.height - mark.height) // 2), mark)
for size, name in [(32, "favicon-32.png"), (180, "apple-touch-icon-180.png"),
                   (192, "icon-192.png"), (512, "icon-512.png"), (64, "mark-64.png")]:
    save_png(sq.resize((size, size), Image.LANCZOS), name)
# Apple touch / maskable icons want an opaque background.
for size, name in [(180, "apple-touch-icon-180.png"), (512, "icon-maskable-512.png")]:
    bg = Image.new("RGBA", (size, size), (255, 255, 255, 255))
    inner = round(size * 0.78)
    m = sq.resize((inner, inner), Image.LANCZOS)
    bg.paste(m, ((size - inner) // 2, (size - inner) // 2), m)
    save_png(bg.convert("RGB"), name)

# ---- trace the mark to SVG -------------------------------------------------
N = 256
small = sq.resize((N, N), Image.LANCZOS).getchannel("A")
inside = [[small.getpixel((x, y)) >= 128 for x in range(N)] for y in range(N)]


def cell(x, y):
    return 0 <= x < N and 0 <= y < N and inside[y][x]


# Edges between inside and outside pixels, oriented so the inside is on the
# left; then chain them into loops.
edges = {}
for y in range(N + 1):
    for x in range(N + 1):
        pass
for y in range(N):
    for x in range(N):
        if not inside[y][x]:
            continue
        if not cell(x, y - 1):
            edges[(x + 1, y)] = (x, y)          # top edge, right->left
        if not cell(x, y + 1):
            edges[(x, y + 1)] = (x + 1, y + 1)  # bottom edge, left->right
        if not cell(x - 1, y):
            edges[(x, y)] = (x, y + 1)          # left edge, top->bottom
        if not cell(x + 1, y):
            edges[(x + 1, y + 1)] = (x + 1, y)  # right edge, bottom->top
# Edges may share start points at diagonal pinches; keep a multimap.
multi = {}
for y in range(N):
    for x in range(N):
        if not inside[y][x]:
            continue
        if not cell(x, y - 1):
            multi.setdefault((x + 1, y), []).append((x, y))
        if not cell(x, y + 1):
            multi.setdefault((x, y + 1), []).append((x + 1, y + 1))
        if not cell(x - 1, y):
            multi.setdefault((x, y), []).append((x, y + 1))
        if not cell(x + 1, y):
            multi.setdefault((x + 1, y + 1), []).append((x + 1, y))
loops = []
while multi:
    startp = next(iter(multi))
    loop = [startp]
    p = startp
    while True:
        nxt = multi[p].pop()
        if not multi[p]:
            del multi[p]
        if nxt == startp:
            break
        loop.append(nxt)
        p = nxt
        if p not in multi:
            break
    if len(loop) > 8:
        loops.append(loop)


def smooth(pts, k=2):
    n = len(pts)
    out = []
    for i in range(n):
        sx = sy = 0
        for j in range(-k, k + 1):
            px, py = pts[(i + j) % n]
            sx += px
            sy += py
        out.append((sx / (2 * k + 1), sy / (2 * k + 1)))
    return out


def rdp(pts, eps):
    if len(pts) < 3:
        return pts
    a, b = pts[0], pts[-1]
    dx, dy = b[0] - a[0], b[1] - a[1]
    norm = math.hypot(dx, dy) or 1e-9
    best, idx = -1, 0
    for i in range(1, len(pts) - 1):
        d = abs(dy * pts[i][0] - dx * pts[i][1] + b[0] * a[1] - b[1] * a[0]) / norm
        if d > best:
            best, idx = d, i
    if best > eps:
        return rdp(pts[: idx + 1], eps)[:-1] + rdp(pts[idx:], eps)
    return [a, b]


paths = []
for loop in loops:
    pts = smooth(loop, 2)
    half = len(pts) // 2
    simp = rdp(pts[: half + 1], 0.45)[:-1] + rdp(pts[half:] + [pts[0]], 0.45)[:-1]
    scale = 64 / N
    d = "M" + " L".join(f"{px * scale:.2f} {py * scale:.2f}" for px, py in simp) + "Z"
    paths.append(d)
svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img" aria-label="HealthyMind">
<defs><linearGradient id="hm" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="#0AEAFF"/><stop offset=".5" stop-color="#3EADFF"/><stop offset="1" stop-color="#756DFF"/>
</linearGradient></defs>
<path fill="url(#hm)" fill-rule="evenodd" d="{' '.join(paths)}"/>
</svg>
"""
(OUT / "favicon.svg").write_text(svg)
(OUT / "manifest.webmanifest").write_text(json.dumps({
    "name": "HealthyMind FHIR Server Manager",
    "short_name": "HealthyMind FHIR",
    "start_url": "/ui",
    "scope": "/ui",
    "display": "standalone",
    "background_color": "#F6F8FC",
    "theme_color": "#2F5BE0",
    "icons": [
        {"src": "/ui/assets/brand/icon-192.png", "sizes": "192x192", "type": "image/png"},
        {"src": "/ui/assets/brand/icon-512.png", "sizes": "512x512", "type": "image/png"},
        {"src": "/ui/assets/brand/icon-maskable-512.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable"},
        {"src": "/ui/assets/brand/favicon.svg", "sizes": "any", "type": "image/svg+xml"},
    ],
}, indent=2) + "\n")
print("loops", len(loops), "svg bytes", len(svg))
