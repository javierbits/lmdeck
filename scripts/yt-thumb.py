#!/usr/bin/env python3
"""Draws the 16:9 YouTube thumbnail for the lmdeck demo video.

Same brand system as scripts/x-announce.py (Retro 82 palette, three-card
mark), with the install lines and a mini deck window for the feed-size read:

  python3 scripts/yt-thumb.py docs/brand/lmdeck-youtube-thumb.png
"""

import os
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFont

W, H = 1280, 720
SS = 2  # supersample factor

BG_TOP = (10, 37, 64)
BG_BOTTOM = (2, 12, 23)
AMBER = (250, 169, 104)
ORANGE = (233, 123, 60)
CREAM = (246, 220, 172)
CYAN = (140, 191, 184)
TEAL = (19, 78, 90)
MID_TEAL = (42, 107, 120)
NAVY = (5, 24, 46)


def pick_font(*candidates):
    for path in candidates:
        if os.path.exists(path):
            return path
    raise SystemExit(f"no font found in {candidates}")


SANS_BOLD = pick_font("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf")
MONO_BOLD = pick_font("/home/jvr0x/.fonts/JM-700.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf")
MONO = pick_font("/home/jvr0x/.fonts/JM-500.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")


def font(path, size):
    return ImageFont.truetype(path, int(size * SS))


def vertical_gradient(w, h, top, bottom):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        t = y / (h - 1)
        d.line([(0, y), (w, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return img


def glow(base, cx, cy, radius, color, strength):
    w, h = base.size
    yy, xx = np_mgrid(w, h)
    dist = ((xx - cx) ** 2 + (yy - cy) ** 2) ** 0.5
    a = (1 - dist / radius).clip(0, 1) ** 2 * strength
    layer = Image.new("RGBA", (w, h), color + (0,))
    layer.putalpha(Image.fromarray(a.astype("uint8")))
    return Image.alpha_composite(base.convert("RGBA"), layer)


def np_mgrid(w, h):
    import numpy as np

    yy, xx = np.mgrid[0:h, 0:w]
    return xx.astype(float), yy.astype(float)


def grid(base):
    layer = Image.new("RGBA", base.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    step = 56 * SS
    for x in range(0, base.width, step):
        d.line([(x, 0), (x, base.height)], fill=(13, 43, 72, 70))
    for y in range(0, base.height, step):
        d.line([(0, y), (base.width, y)], fill=(13, 43, 72, 70))
    return Image.alpha_composite(base, layer)


def card(w, h, r, fill=None, outline=None, gradient=None, glyph=False):
    cw, ch = int(w), int(h)
    if gradient:
        mask = Image.new("L", (cw, ch), 0)
        ImageDraw.Draw(mask).rounded_rectangle([0, 0, cw - 1, ch - 1], radius=int(r), fill=255)
        img = vertical_gradient(cw, ch, gradient[0], gradient[1]).convert("RGBA")
        img.putalpha(mask)
    else:
        img = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
        d = ImageDraw.Draw(img)
        d.rounded_rectangle([0, 0, cw - 1, ch - 1], radius=int(r), fill=fill, outline=outline, width=max(1, SS))
    if glyph:
        d = ImageDraw.Draw(img)
        lw = 9 * SS
        d.line(
            [(cw * 0.34, ch * 0.40), (cw * 0.57, ch * 0.5), (cw * 0.34, ch * 0.60)],
            fill=NAVY,
            width=lw,
            joint="curve",
        )
        d.rounded_rectangle([cw * 0.62, ch * 0.56, cw * 0.80, ch * 0.56 + lw], radius=lw // 2, fill=NAVY)
        bar = 4 * SS
        d.rounded_rectangle([cw * 0.22, ch * 0.13, cw * 0.37, ch * 0.13 + bar], radius=bar // 2, fill=(5, 24, 46, 115))
        d.rounded_rectangle([cw * 0.41, ch * 0.13, cw * 0.50, ch * 0.13 + bar], radius=bar // 2, fill=(5, 24, 46, 70))
    return img


def mark(size):
    canvas = Image.new("RGBA", (size * SS, size * SS), (0, 0, 0, 0))
    s = size / 256
    cx = cy = size * SS / 2
    for dx, dy, angle, fill in [(-74, 16, -30, TEAL), (-38, 7, -13, MID_TEAL)]:
        c = card(92 * s * SS, 128 * s * SS, 15 * s * SS, fill=fill, outline=(250, 169, 104, 60))
        c = c.rotate(angle, resample=Image.BICUBIC, expand=True)
        canvas.alpha_composite(c, (int(cx + dx * s * SS - c.width / 2), int(cy + dy * s * SS - c.height / 2)))
    front = card(92 * s * SS, 128 * s * SS, 15 * s * SS, gradient=(AMBER, ORANGE), glyph=True)
    canvas.alpha_composite(front, (int(cx - front.width / 2), int(cy - front.height / 2)))
    return canvas.resize((size, size), Image.LANCZOS)


def build(out):
    img = vertical_gradient(W * SS, H * SS, BG_TOP, BG_BOTTOM).convert("RGBA")
    img = glow(img, 210 * SS, 190 * SS, 430 * SS, AMBER, 50)
    img = grid(img)
    d = ImageDraw.Draw(img)

    # The mark, left side.
    m = mark(330)
    img.alpha_composite(m, (52 * SS, 130 * SS))

    # Wordmark.
    d.text((64 * SS, 160 * SS), "lmdeck", font=font(SANS_BOLD, 150), fill=CREAM)
    pill_w, pill_h = 150 * SS, 46 * SS
    d.rounded_rectangle([66 * SS, 328 * SS, 66 * SS + pill_w, 328 * SS + pill_h], radius=pill_h // 2, fill=AMBER)
    d.text((92 * SS, 336 * SS), "v0.1.5", font=font(MONO_BOLD, 24), fill=NAVY)
    d.text((232 * SS, 342 * SS), "public · MIT", font=font(MONO, 20), fill=CYAN)

    # The hook.
    d.text((66 * SS, 430 * SS), "Local LLM recipes,", font=font(SANS_BOLD, 64), fill=CREAM)
    d.text((66 * SS, 500 * SS), "one DGX Spark or four.", font=font(SANS_BOLD, 64), fill=CREAM)

    # Install line.
    d.line([(66 * SS, 596 * SS), (1214 * SS, 596 * SS)], fill=TEAL, width=SS)
    d.text((68 * SS, 620 * SS), "\u25b8 brew install javierbits/lmdeck/lmdeck", font=font(MONO, 22), fill=CREAM)
    d.text((68 * SS, 660 * SS), "\u25b8 cargo install lmdeck", font=font(MONO, 22), fill=CREAM)

    # Demo strip on the right: a mini TUI window.
    win = card(330 * SS, 210 * SS, 10 * SS, fill=(3, 18, 34, 235), outline=(25, 60, 90, 255))
    img.alpha_composite(win, (878 * SS, 56 * SS))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([886 * SS, 64 * SS, 902 * SS, 80 * SS], radius=4 * SS, fill=(233, 123, 60))
    d.rounded_rectangle([910 * SS, 64 * SS, 926 * SS, 80 * SS], radius=4 * SS, fill=(42, 107, 120))
    d.text((938 * SS, 62 * SS), "lmdeck", font=font(MONO, 17), fill=CYAN)
    rows = [
        ("\u25cf", "dsv41-1017-tf", "loaded"),
        ("\u25cf", "glm-5.3-flash-2x", "loaded"),
        ("\u25cb", "qwen3.6-27b", "stopped"),
        ("\u25cb", "lfm3-1.7b", "stopped"),
    ]
    for i, (glyph, name, status) in enumerate(rows):
        y = (100 + i * 26) * SS
        color = AMBER if status == "loaded" else (63, 143, 138)
        d.text((892 * SS, y), glyph, font=font(MONO, 16), fill=color)
        d.text((918 * SS, y), name, font=font(MONO, 16), fill=CREAM if status == "loaded" else CYAN)

    img = img.resize((W, H), Image.LANCZOS).convert("RGB")
    img.save(out, "PNG", optimize=True)
    print(f"wrote {out} {W}x{H}")


if __name__ == "__main__":
    build(sys.argv[1] if len(sys.argv) > 1 else "thumb.png")
