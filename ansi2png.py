#!/usr/bin/env python3
"""Render a tmux pane capture (with ANSI colours) into a PNG."""
import re, sys
from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
FONT_B = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
BASE_BG = (13, 17, 23)
FG = (220, 223, 228)

STD = {30:"#000000",31:"#cd3131",32:"#0dbc79",33:"#e5e510",34:"#2472c8",35:"#bc3fbc",
       36:"#11a8cd",37:"#e5e5e5",90:"#666666",91:"#f14c4c",92:"#23d18b",93:"#f5f543",
       94:"#3b8eea",95:"#d670d6",96:"#29b8db",97:"#ffffff"}

def hex2rgb(h):
    h = h.lstrip("#")
    return tuple(int(h[i:i+2],16) for i in (0,2,4))

def xterm256(n):
    n = int(n)
    if n < 16:
        base = ["#000000","#800000","#008000","#808000","#000080","#800080","#008080","#c0c0c0",
                "#808080","#ff0000","#00ff00","#ffff00","#0000ff","#ff00ff","#00ffff","#ffffff"]
        return hex2rgb(base[n])
    if n < 232:
        n -= 16
        r,g,b = n//36, (n%36)//6, n%6
        conv = lambda v: 0 if v==0 else 55+40*v
        return (conv(r),conv(g),conv(b))
    v = 8 + (n-232)*10
    return (v,v,v)

def parse(text):
    """Yield per-line lists of (char, fg, bg, bold)."""
    fg, bg, bold = FG, None, False
    lines, cur = [], []
    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "\x1b":
            m = re.match(r"\x1b\[([0-9;]*)m", text[i:])
            if m:
                codes = [c for c in m.group(1).split(";") if c != ""] or ["0"]
                j = 0
                while j < len(codes):
                    c = codes[j]
                    if c == "0":
                        fg, bg, bold = FG, None, False
                    elif c == "1":
                        bold = True
                    elif c == "22":
                        bold = False
                    elif c == "39":
                        fg = FG
                    elif c == "49":
                        bg = None
                    elif c in STD:
                        fg = hex2rgb(STD[int(c)])
                    elif str(int(c)-10) in STD and 40 <= int(c) <= 47:
                        bg = hex2rgb(STD[int(c)+10])
                    elif 100 <= int(c) <= 107:
                        bg = hex2rgb(STD[int(c)])
                    elif c == "38" and j+2 < len(codes) and codes[j+1] == "5":
                        fg = xterm256(codes[j+2]); j += 2
                    elif c == "48" and j+2 < len(codes) and codes[j+1] == "5":
                        bg = xterm256(codes[j+2]); j += 2
                    elif c == "38" and j+4 < len(codes) and codes[j+1] == "2":
                        fg = tuple(int(codes[j+k]) for k in (2,3,4)); j += 4
                    elif c == "48" and j+4 < len(codes) and codes[j+1] == "2":
                        bg = tuple(int(codes[j+k]) for k in (2,3,4)); j += 4
                    j += 1
                i += m.end()
                continue
            i += 1
            continue
        if ch == "\n":
            lines.append(cur); cur = []
        elif ch == "\r":
            pass
        else:
            cur.append((ch, fg, bg, bold))
        i += 1
    if cur:
        lines.append(cur)
    return lines

def render(path, out):
    text = open(path, encoding="utf-8", errors="replace").read()
    lines = parse(text)
    size, bold = 15, False
    f = ImageFont.truetype(FONT, size)
    fb = ImageFont.truetype(FONT_B, size)
    cw = f.getlength("M")
    lh = 19
    cols = max((len(l) for l in lines), default=100)
    img = Image.new("RGB", (int(cw*cols)+24, lh*len(lines)+24), BASE_BG)
    d = ImageDraw.Draw(img)
    for y, line in enumerate(lines):
        x = 12
        for ch, fg, bg, bold_ in line:
            if bg:
                d.rectangle([x, 12+y*lh, x+cw, 12+y*lh+lh], fill=bg)
            if ch.strip():
                d.text((x, 10+y*lh), ch, font=(fb if bold_ else f), fill=fg)
            x += cw
    img.save(out)
    return img.size

if __name__ == "__main__":
    print(render(sys.argv[1], sys.argv[2]))
