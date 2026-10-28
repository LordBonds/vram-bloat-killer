"""Draws the emergency wipe panel's art: a hazard-ringed plate, a glossy red button
(normal, hot, pressed) and a glass cover (closed and lifted). Every layer shares one
square canvas so they stack exactly.

Run from the repo root: python tools/make_emergency_art.py
"""
import math
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageFont

OUT = Path(__file__).resolve().parent.parent / "art"
SIZE = 520          # shipped size (2x of the 260 px panel slot)
SS = 4              # drawn at 4x, then downsampled for smooth edges
S = SIZE * SS
C = S // 2


def canvas():
    return Image.new("RGBA", (S, S), (0, 0, 0, 0))


def finish(img, name):
    img.resize((SIZE, SIZE), Image.LANCZOS).save(OUT / f"{name}.png")


def radial(size, inner, outer, centre=(0.5, 0.5), radius=0.5):
    """A square radial gradient from `inner` at `centre` to `outer` at `radius`."""
    small = 256
    img = Image.new("RGBA", (small, small))
    px = img.load()
    cx, cy = centre[0] * small, centre[1] * small
    for y in range(small):
        for x in range(small):
            t = min(1.0, math.hypot(x - cx, y - cy) / (radius * small))
            px[x, y] = tuple(int(inner[i] + (outer[i] - inner[i]) * t) for i in range(4))
    return img.resize((size, size), Image.BICUBIC)


def circle_mask(r, cx=C, cy=C):
    m = Image.new("L", (S, S), 0)
    ImageDraw.Draw(m).ellipse([cx - r, cy - r, cx + r, cy + r], fill=255)
    return m


def plate():
    img = canvas()
    r_out, r_ring = 248 * SS // 2, 212 * SS // 2
    # drop shadow
    shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).ellipse([C - r_out, C - r_out + 10 * SS, C + r_out, C + r_out + 10 * SS], fill=(0, 0, 0, 150))
    img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(12 * SS)))
    # hazard ring: diagonal yellow/black stripes clipped to an annulus
    stripes = Image.new("RGBA", (S, S), (24, 24, 24, 255))
    d = ImageDraw.Draw(stripes)
    w = 26 * SS
    for k in range(-S, 2 * S, 2 * w):
        d.polygon([(k, 0), (k + w, 0), (k + w - S, S), (k - S, S)], fill=(244, 190, 30, 255))
    ring = ImageChops.subtract(circle_mask(r_out), circle_mask(r_ring))
    img.paste(stripes, (0, 0), ring)
    # bevelled rims
    d = ImageDraw.Draw(img)
    d.ellipse([C - r_out, C - r_out, C + r_out, C + r_out], outline=(10, 10, 10, 255), width=3 * SS)
    # inner gunmetal
    metal = radial(S, (70, 74, 82, 255), (28, 30, 34, 255), centre=(0.42, 0.36), radius=0.62)
    img.paste(metal, (0, 0), circle_mask(r_ring))
    d.ellipse([C - r_ring, C - r_ring, C + r_ring, C + r_ring], outline=(12, 12, 14, 255), width=4 * SS)
    d.ellipse([C - r_ring + 5 * SS, C - r_ring + 5 * SS, C + r_ring - 5 * SS, C + r_ring - 5 * SS], outline=(100, 104, 112, 120), width=SS)
    # four screws
    for a in (45, 135, 225, 315):
        x = C + math.cos(math.radians(a)) * (r_ring - 22 * SS)
        y = C + math.sin(math.radians(a)) * (r_ring - 22 * SS)
        sr = 9 * SS
        screw = radial(2 * sr, (190, 194, 200, 255), (70, 72, 78, 255), centre=(0.35, 0.3), radius=0.7)
        mask = Image.new("L", (2 * sr, 2 * sr), 0)
        ImageDraw.Draw(mask).ellipse([0, 0, 2 * sr - 1, 2 * sr - 1], fill=255)
        img.paste(screw, (int(x - sr), int(y - sr)), mask)
        ImageDraw.Draw(img).line([(x - sr * 0.6, y - sr * 0.6), (x + sr * 0.6, y + sr * 0.6)], fill=(30, 30, 34, 255), width=2 * SS)
    finish(img, "plate")


def button(name, top, bottom, glow=0, pressed=False):
    img = canvas()
    r = 132 * SS // 2
    dy = 4 * SS if pressed else 0
    # socket ring and contact shadow
    d = ImageDraw.Draw(img)
    rs = r + 14 * SS
    d.ellipse([C - rs, C - rs, C + rs, C + rs], fill=(18, 18, 20, 255))
    d.ellipse([C - rs, C - rs, C + rs, C + rs], outline=(90, 92, 98, 255), width=2 * SS)
    if glow:
        halo = Image.new("RGBA", (S, S), (0, 0, 0, 0))
        hr = r + glow * SS
        ImageDraw.Draw(halo).ellipse([C - hr, C - hr, C + hr, C + hr], fill=(255, 60, 40, 140))
        img.alpha_composite(halo.filter(ImageFilter.GaussianBlur(14 * SS)))
    shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    off = (3 if pressed else 10) * SS
    ImageDraw.Draw(shadow).ellipse([C - r, C - r + off, C + r, C + r + off], fill=(0, 0, 0, 200))
    img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(8 * SS)))
    # dome
    dome = radial(S, top, bottom, centre=(0.42, 0.34 if not pressed else 0.42), radius=0.7)
    img.paste(dome, (0, dy), circle_mask(r, C, C))
    d = ImageDraw.Draw(img)
    d.ellipse([C - r, C - r + dy, C + r, C + r + dy], outline=(90, 8, 8, 255), width=3 * SS)
    # specular highlight
    hl = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    hw, hh = r * 1.15, r * (0.55 if not pressed else 0.4)
    ImageDraw.Draw(hl).ellipse([C - hw / 2, C - r * 0.86 + dy, C + hw / 2, C - r * 0.86 + hh + dy], fill=(255, 255, 255, 120 if not pressed else 60))
    img.alpha_composite(hl.filter(ImageFilter.GaussianBlur(6 * SS)))
    # label
    font = ImageFont.truetype("C:/Windows/Fonts/segoeuib.ttf", 34 * SS)
    text = "WIPE"
    box = d.textbbox((0, 0), text, font=font)
    tx, ty = C - (box[2] - box[0]) / 2 - box[0], C - (box[3] - box[1]) / 2 - box[1] + dy + 2 * SS
    d.text((tx, ty + 2 * SS), text, font=font, fill=(90, 0, 0, 180))
    d.text((tx, ty), text, font=font, fill=(255, 236, 232, 240))
    finish(img, name)


def glass_streaks(img, box, alpha):
    """Two soft diagonal light bands across `box`."""
    x0, y0, x1, y1 = box
    band = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(band)
    w = x1 - x0
    for off, width in ((0.18, 0.10), (0.36, 0.04)):
        a = x0 + w * off
        d.polygon([(a, y0), (a + w * width, y0), (a + w * width - w * 0.35, y1), (a - w * 0.35, y1)], fill=(255, 255, 255, alpha))
    return band.filter(ImageFilter.GaussianBlur(5 * SS))


def cover_closed():
    img = canvas()
    half = 172 * SS // 2
    box = [C - half, C - half, C + half, C + half]
    rad = 40 * SS
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle(box, rad, fill=255)
    glass = Image.new("RGBA", (S, S), (200, 225, 255, 46))
    img.paste(glass, (0, 0), mask)
    streaks = glass_streaks(img, box, 80)
    img.alpha_composite(Image.composite(streaks, Image.new("RGBA", (S, S)), mask))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle(box, rad, outline=(230, 240, 255, 170), width=3 * SS)
    d.rounded_rectangle([b + (6 * SS if i < 2 else -6 * SS) for i, b in enumerate(box)], rad - 6 * SS, outline=(255, 255, 255, 50), width=SS)
    # hinge across the top
    hinge = [C - half * 0.7, C - half - 10 * SS, C + half * 0.7, C - half + 6 * SS]
    d.rounded_rectangle(hinge, 6 * SS, fill=(150, 154, 162, 255), outline=(60, 62, 68, 255), width=2 * SS)
    for k in range(5):
        x = hinge[0] + (hinge[2] - hinge[0]) * (k + 0.5) / 5
        d.line([(x, hinge[1] + 3 * SS), (x, hinge[3] - 3 * SS)], fill=(90, 94, 100, 255), width=2 * SS)
    # pull tab at the bottom
    tab = [C - 34 * SS, C + half - 6 * SS, C + 34 * SS, C + half + 20 * SS]
    d.rounded_rectangle(tab, 8 * SS, fill=(230, 60, 48, 255), outline=(120, 20, 16, 255), width=2 * SS)
    font = ImageFont.truetype("C:/Windows/Fonts/segoeuib.ttf", 13 * SS)
    box2 = d.textbbox((0, 0), "LIFT", font=font)
    d.text((C - (box2[2] - box2[0]) / 2 - box2[0], (tab[1] + tab[3]) / 2 - (box2[3] - box2[1]) / 2 - box2[1]), "LIFT", font=font, fill=(255, 245, 240, 255))
    finish(img, "cover_closed")


def cover_open():
    """The cover swung up on its hinge: a foreshortened slab above the plate."""
    img = canvas()
    half = 172 * SS // 2
    hinge_y = C - half
    top_y = hinge_y - 150 * SS
    near, far = half, half * 0.82
    poly = [(C - far, top_y), (C + far, top_y), (C + near, hinge_y), (C - near, hinge_y)]
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).polygon(poly, fill=255)
    mask = mask.filter(ImageFilter.GaussianBlur(SS / 2))
    img.paste(Image.new("RGBA", (S, S), (200, 225, 255, 52)), (0, 0), mask)
    streaks = glass_streaks(img, [C - near, top_y, C + near, hinge_y], 90)
    img.alpha_composite(Image.composite(streaks, Image.new("RGBA", (S, S)), mask))
    d = ImageDraw.Draw(img)
    d.polygon(poly, outline=(230, 240, 255, 170), width=3 * SS)
    # its tab, now up at the far edge
    tab = [C - 30 * SS, top_y - 16 * SS, C + 30 * SS, top_y + 4 * SS]
    d.rounded_rectangle(tab, 7 * SS, fill=(230, 60, 48, 255), outline=(120, 20, 16, 255), width=2 * SS)
    hinge = [C - half * 0.7, hinge_y - 10 * SS, C + half * 0.7, hinge_y + 6 * SS]
    d.rounded_rectangle(hinge, 6 * SS, fill=(150, 154, 162, 255), outline=(60, 62, 68, 255), width=2 * SS)
    for k in range(5):
        x = hinge[0] + (hinge[2] - hinge[0]) * (k + 0.5) / 5
        d.line([(x, hinge[1] + 3 * SS), (x, hinge[3] - 3 * SS)], fill=(90, 94, 100, 255), width=2 * SS)
    finish(img, "cover_open")


def main():
    OUT.mkdir(exist_ok=True)
    plate()
    button("button", (255, 96, 80, 255), (150, 14, 10, 255))
    button("button_hot", (255, 130, 110, 255), (190, 24, 16, 255), glow=26)
    button("button_down", (200, 40, 30, 255), (110, 8, 6, 255), pressed=True)
    cover_closed()
    cover_open()


if __name__ == "__main__":
    main()
