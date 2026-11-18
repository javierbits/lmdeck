#!/usr/bin/env python3
"""Draws the 1:1 X announcement card for a lmdeck release.

Uses the Retro 82 palette and the same three-card mark as docs/brand. Pure
Pillow, no SVG rasterizer needed:

  python3 scripts/x-announce.py 0.1.5 docs/brand/lmdeck-0.1.5-square.png
"""

import os
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFont

S = 1000  # canvas size; X shows 1:1 cards up to this
SS = 3  # supersampling for the mark

BG_TOP = (10, 37, 64)
BG_BOTTOM = (2, 12, 23)
AMBER = (250, 169, 104)
ORANGE = (233, 123, 60)
CREAM = (246, 220, 172)
CYAN = (140, 191, 184)
TEAL = (19, 78, 90)
MID_TEAL = (42, 107, 120)
NAVY = (5, 24, 46)
MUTED = (63, 143, 138)


def pick_font(*candidates):
    for path in candidates:
        if os.path.exists(path):
            return path
    raise SystemExit(f"no font found in {candidates}")


SANS_BOLD = pick_font("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf")
MONO_BOLD = pick_font("/home/jvr0x/.fonts/JM-700.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf")
MONO = pick_font("/home/jvr0x/.fonts/JM-500.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")


def font(path, size):
    return ImageFont.truetype(path, size)


def vertical_gradient(w, h, top, bottom):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        t = y / (h - 1)
        d.line([(0, y), (w, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return img


def glow(base, cx, cy, radius, color, strength):
    w, h = base.size
    yy, xx = np.mgrid[0:h, 0:w]
    dist = np.sqrt((xx - cx) ** 2 + (yy - cy) ** 2)
    a = np.clip(1 - dist / radius, 0, 1) ** 2 * strength
    layer = Image.new("RGBA", (w, h), color + (0,))
    layer.putalpha(Image.fromarray(a.astype(np.uint8)))
    return Image.alpha_composite(base.convert("RGBA"), layer)


def grid(base):
    layer = Image.new("RGBA", base.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    for x in range(0, S, 56):
        d.line([(x, 0), (x, S)], fill=(13, 43, 72, 70))
    for y in range(0, S, 56):
        d.line([(0, y), (S, y)], fill=(13, 43, 72, 70))
    return Image.alpha_composite(base, layer)


def rounded(size, radius, fill, outline=None, width=1):
    img = Image.new("RGBA", size, (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([0, 0, size[0] - 1, size[1] - 1], radius=radius, fill=fill, outline=outline, width=width)
    return img


def gradient_rect(size, radius, top, bottom):
    mask = rounded(size, radius, (255, 255, 255, 255)).getchannel("A")
    img = vertical_gradient(size[0], size[1], top, bottom).convert("RGBA")
    img.putalpha(mask)
    return img


def card(w, h, r, fill=None, outline=None, gradient=None, glyph=False):
    cw, ch = int(w * SS), int(h * SS)
    if gradient:
        img = gradient_rect((cw, ch), int(r * SS), gradient[0], gradient[1])
    else:
        img = rounded((cw, ch), int(r * SS), fill, outline, max(1, int(1.4 * SS)))
    if glyph:
        d = ImageDraw.Draw(img)
        lw = int(9 * SS)
        d.line(
            [(cw * 0.34, ch * 0.40), (cw * 0.57, ch * 0.5), (cw * 0.34, ch * 0.60)],
            fill=NAVY,
            width=lw,
            joint="curve",
        )
        d.rounded_rectangle(
            [cw * 0.62, ch * 0.56, cw * 0.80, ch * 0.56 + lw], radius=lw // 2, fill=NAVY
        )
        bar = max(2, int(4 * SS))
        d.rounded_rectangle(
            [cw * 0.22, ch * 0.13, cw * 0.37, ch * 0.13 + bar], radius=bar // 2, fill=(5, 24, 46, 115)
        )
        d.rounded_rectangle(
            [cw * 0.41, ch * 0.13, cw * 0.50, ch * 0.13 + bar], radius=bar // 2, fill=(5, 24, 46, 70)
        )
    return img


def mark(size):
    """The lmdeck mark: three stacked cards, the front one amber with a prompt."""
    canvas = Image.new("RGBA", (size * SS, size * SS), (0, 0, 0, 0))
    s = size / 256
    cx = cy = size * SS / 2
    for dx, dy, angle, fill in [
        (-74, 16, -30, TEAL),
        (-38, 7, -13, MID_TEAL),
    ]:
        c = card(92 * s, 128 * s, 15 * s, fill=fill, outline=(250, 169, 104, 60))
        c = c.rotate(angle, resample=Image.BICUBIC, expand=True)
        canvas.alpha_composite(c, (int(cx + dx * s * SS - c.width / 2), int(cy + dy * s * SS - c.height / 2)))
    front = card(92 * s, 128 * s, 15 * s, gradient=(AMBER, ORANGE), glyph=True)
    canvas.alpha_composite(front, (int(cx - front.width / 2), int(cy - front.height / 2)))
    return canvas.resize((size, size), Image.LANCZOS)


def tracked(draw, xy, text, f, fill, tracking):
    x, y = xy
    for ch in text:
        draw.text((x, y), ch, font=f, fill=fill)
        x += f.getlength(ch) + tracking
    return x


def build(version, out):
    img = vertical_gradient(S, S, BG_TOP, BG_BOTTOM).convert("RGBA")
    img = glow(img, 210, 300, 430, AMBER, 46)
    img = grid(img)
    d = ImageDraw.Draw(img)

    # Small label.
    tracked(d, (74, 66), "LOCAL LLM DECK", font(MONO_BOLD, 20), AMBER, 7)

    # The mark.
    m = mark(360)
    img.alpha_composite(m, (34, 130))
    d = ImageDraw.Draw(img)

    # Wordmark and release row.
    d.text((70, 470), "lmdeck", font=font(SANS_BOLD, 168), fill=CREAM)
    pill = rounded((172, 54), 27, AMBER)
    img.alpha_composite(pill, (72, 668))
    d = ImageDraw.Draw(img)
    d.text((94, 678), f"v{version}", font=font(MONO_BOLD, 28), fill=NAVY)
    d.text((262, 684), "just shipped", font=font(MONO, 25), fill=MUTED)

    # Pitch.
    d.text((72, 758), "Local LLM recipes, one binary.", font=font(MONO_BOLD, 34), fill=CREAM)
    d.text(
        (72, 810),
        "One Spark or four. Fit check, launch, lifecycle.",
        font=font(MONO, 24),
        fill=CYAN,
    )

    # Install block.
    d.line([(72, 868), (928, 868)], fill=TEAL, width=1)
    d.text((74, 890), "\u25b8 brew install javierbits/lmdeck/lmdeck", font=font(MONO, 24), fill=CREAM)
    d.text((74, 926), "\u25b8 cargo install lmdeck", font=font(MONO, 24), fill=CREAM)
    d.text((928, 962), "github.com/javierbits/lmdeck", font=font(MONO, 19), fill=CYAN, anchor="ra")

    img.convert("RGB").save(out, "PNG", optimize=True)
    print(f"wrote {out}")


if __name__ == "__main__":
    build(sys.argv[1] if len(sys.argv) > 1 else "0.1.5", sys.argv[2] if len(sys.argv) > 2 else "card.png")
