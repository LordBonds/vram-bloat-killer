"""Paints the icons: a balloon about to meet a pin (bloat, popped).

The balloon is painted, not drawn: gouache value blocks laid down with brush strokes
that follow its curve, warm lights and cool shadows, watercolour pigment pooling at
the edges, granulation and paper grain. Over the paint sits the modern depth: a
glossy clear-coat highlight and rim light, an enamel-and-chrome pin that dimples the
skin and casts a shadow on it, ink keylines, a die-cut sticker border, and layered
soft shadows on the app tile. Every edge comes from a signed distance field, so the
outlines stay crisp at any size.

Writes to icons/:
  {green,amber,red}-{16,20,24,32,48}-{dark,light}.png   tray icons for dark / light taskbars
  app.ico, logo-128.png, logo-256.png                    the app icon on its tile

Run from the repo root: python tools/make_icons.py   (needs Pillow, numpy and scipy)
"""
import math
from dataclasses import dataclass
from pathlib import Path

import numpy as np
from PIL import Image
from scipy import ndimage as ndi

OUT = Path(__file__).resolve().parent.parent / "icons"
TRAY_SIZES = [16, 20, 24, 32, 48]
APP_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
APP_MASTER = 1536  # painted at 1.5x the 1024 app icon
TRAY_MASTER = 768


def hexrgb(h):
    return np.array([int(h[i:i + 2], 16) for i in (1, 3, 5)], np.float32) / 255


# Each status colour as a painter's palette: cool shadow, body, warm light, warmest light, ink.
PALETTES = {
    "green": dict(shadow=hexrgb("#0E4A5E"), mid=hexrgb("#1CA463"), light=hexrgb("#72D957"), warm=hexrgb("#D8F28A"), ink=hexrgb("#06262C")),
    "amber": dict(shadow=hexrgb("#8E3415"), mid=hexrgb("#E7861A"), light=hexrgb("#FFBE3C"), warm=hexrgb("#FFEA9C"), ink=hexrgb("#431B06")),
    "red": dict(shadow=hexrgb("#661036"), mid=hexrgb("#D9323D"), light=hexrgb("#FF6A5A"), warm=hexrgb("#FFC2A6"), ink=hexrgb("#38071A")),
}
PAPER = hexrgb("#F7F1E3")
NAVY_INK = hexrgb("#0D1122")
PIN = dict(shadow=hexrgb("#0E185A"), mid=hexrgb("#2A5FE8"), light=hexrgb("#8AB0FF"))
CHROME_DARK, CHROME_LIGHT = hexrgb("#3A404E"), hexrgb("#F0F3F9")
RIM = hexrgb("#D8F3FF")


def unit(*v):
    v = np.array(v, np.float32)
    return v / np.linalg.norm(v)


LIGHT = unit(-0.55, -0.62, 0.56)  # key light: top left, in front
HALF = unit(*(LIGHT + np.array([0, 0, 1], np.float32)))
BACK = unit(0.82, 0.18, 0.12)  # rim light from the right


# ---------------------------------------------------------------- small helpers

def smoothstep(a, b, x):
    t = np.clip((x - a) / (b - a), 0, 1)
    return t * t * (3 - 2 * t)


def mix(a, b, t):
    a, b = np.asarray(a, np.float32), np.asarray(b, np.float32)
    return a + (b - a) * np.asarray(t, np.float32)[..., None]


def smin(a, b, k):
    """Smooth union of two distance fields."""
    h = np.clip(0.5 + 0.5 * (b - a) / k, 0, 1)
    return b + (a - b) * h - k * h * (1 - h)


def grid(n):
    y, x = np.mgrid[0:n, 0:n].astype(np.float32)
    return x + 0.5, y + 0.5


def coverage(sd):
    """Anti-aliased coverage from a signed distance in pixels (negative inside)."""
    return np.clip(0.5 - sd, 0, 1)


def normalized(a):
    return (a - a.mean()) / (a.std() + 1e-6)


def value_noise(n, cell, rng):
    """A smooth random field with features about `cell` pixels across."""
    g = max(2, int(n / cell) + 3)
    small = rng.standard_normal((g, g)).astype(np.float32)
    big = np.asarray(Image.fromarray(small).resize((int(g * cell), int(g * cell)), Image.BICUBIC))
    return normalized(big[:n, :n])


def fbm(n, cell, rng, octaves=5, gain=0.5):
    total = np.zeros((n, n), np.float32)
    amp = 1.0
    for _ in range(octaves):
        if cell < 1.5:
            break
        total += amp * value_noise(n, cell, rng)
        amp *= gain
        cell /= 2
    return normalized(total)


def grain(n, sigma, rng):
    return normalized(ndi.gaussian_filter(rng.standard_normal((n, n)).astype(np.float32), sigma))


def lic(noise, tx, ty, steps):
    """Line integral convolution: smears noise along the (tx, ty) field into brush strokes."""
    n = noise.shape[0]
    y, x = np.mgrid[0:n, 0:n].astype(np.float32)
    acc = noise.copy()
    count = 1
    for sign in (1.0, -1.0):
        px, py = x.copy(), y.copy()
        for _ in range(steps):
            vx = ndi.map_coordinates(tx, [py, px], order=1, mode="nearest")
            vy = ndi.map_coordinates(ty, [py, px], order=1, mode="nearest")
            px += sign * vx
            py += sign * vy
            acc += ndi.map_coordinates(noise, [py, px], order=1, mode="reflect")
            count += 1
    return normalized(acc / count)


def over(dst_rgb, dst_a, src_rgb, src_a):
    """Straight-alpha 'over'."""
    out_a = src_a + dst_a * (1 - src_a)
    safe = np.maximum(out_a, 1e-6)[..., None]
    out = (src_rgb * src_a[..., None] + dst_rgb * (dst_a * (1 - src_a))[..., None]) / safe
    return out, out_a


# ---------------------------------------------------------------- distance fields

def sd_circle(x, y, c, r):
    return np.hypot(x - c[0], y - c[1]) - r


def sd_taper(x, y, a, b, ra, rb):
    """A segment from a to b whose radius tapers from ra to rb. Also returns the
    position along it (0..1) and the signed distance across it (-1..1)."""
    px, py = x - a[0], y - a[1]
    dx, dy = b[0] - a[0], b[1] - a[1]
    length2 = dx * dx + dy * dy
    t = np.clip((px * dx + py * dy) / length2, 0, 1)
    r = ra + (rb - ra) * t
    across = (px * -dy + py * dx) / math.sqrt(length2) / np.maximum(r, 1e-3)
    return np.hypot(px - dx * t, py - dy * t) - r, t, across


def sd_triangle(x, y, p0, p1, p2):
    """Exact signed distance to a triangle (after Inigo Quilez)."""
    ps = [np.array(p, np.float32) for p in (p0, p1, p2)]
    d2, cross = [], []
    s = np.sign((ps[1][0] - ps[0][0]) * (ps[0][1] - ps[2][1]) - (ps[1][1] - ps[0][1]) * (ps[0][0] - ps[2][0]))
    for i in range(3):
        a, b = ps[i], ps[(i + 1) % 3]
        ex, ey = b[0] - a[0], b[1] - a[1]
        vx, vy = x - a[0], y - a[1]
        h = np.clip((vx * ex + vy * ey) / (ex * ex + ey * ey), 0, 1)
        qx, qy = vx - ex * h, vy - ey * h
        d2.append(qx * qx + qy * qy)
        cross.append(s * (vx * ey - vy * ex))
    return -np.sqrt(np.minimum.reduce(d2)) * np.sign(np.minimum.reduce(cross))


def bezier(points, steps):
    p = np.array(points, np.float32)
    out = []
    for t in np.linspace(0, 1, steps):
        q = p.copy()
        while len(q) > 1:
            q = q[:-1] + (q[1:] - q[:-1]) * t
        out.append(q[0])
    return out


def edt_sdf(inside):
    """Exact Euclidean signed distance (pixels) of a boolean mask."""
    return np.where(inside, -ndi.distance_transform_edt(inside), ndi.distance_transform_edt(~inside)).astype(np.float32)


def die_cut(sd_union, border, rounding):
    """The sticker's outline: `border` px out from the shapes, concave corners rounded."""
    grown = ndi.distance_transform_edt(sd_union >= 0) < border + rounding
    depth = ndi.distance_transform_edt(grown)
    return np.where(grown, rounding - depth, rounding + ndi.distance_transform_edt(~grown)).astype(np.float32)


# ---------------------------------------------------------------- the scene

@dataclass
class Layout:
    cx: float
    cy: float
    rx: float
    ry: float
    tilt: float      # radians
    contact: float   # where the pin touches, as an angle round the balloon
    pin_dir: tuple   # from the contact point out to the pin head
    pin_len: float
    head_r: float
    shaft_r: float
    string: tuple    # Bezier control offsets from the knot
    string_r: float


APP = Layout(cx=0.43, cy=0.46, rx=0.235, ry=0.27, tilt=-0.13, contact=-0.85, pin_dir=(0.86, -0.50),
             pin_len=0.15, head_r=0.066, shaft_r=0.0115,
             string=((-0.06, 0.05), (0.07, 0.085), (-0.025, 0.14)), string_r=0.0085)
TRAY = Layout(cx=0.42, cy=0.49, rx=0.318, ry=0.358, tilt=-0.10, contact=-0.72, pin_dir=(0.84, -0.54),
              pin_len=0.155, head_r=0.10, shaft_r=0.022,
              string=((-0.035, 0.025), (0.04, 0.05), (-0.005, 0.085)), string_r=0.016)


def scene(n, lay):
    """Every shape as a distance field, plus what the painter needs to light the balloon."""
    x, y = grid(n)
    c, s = math.cos(lay.tilt), math.sin(lay.tilt)
    cx, cy, rx, ry = lay.cx * n, lay.cy * n, lay.rx * n, lay.ry * n

    def world(u, v):
        px, py = u * rx, v * ry
        return (cx + px * c - py * s, cy + px * s + py * c)

    def taper(v):
        # Narrower toward the knot. Clamped: unclamped it crosses zero far below the
        # balloon and leaves a phantom edge there.
        return np.maximum(1 - 0.10 * v - 0.22 * np.clip(v, 0, None) ** 3, 0.35)

    dx, dy = x - cx, y - cy
    u = (dx * c + dy * s) / rx
    v = (-dx * s + dy * c) / ry
    F = (u / taper(v)) ** 2 + v ** 2

    vc = math.sin(lay.contact)
    contact = world(math.cos(lay.contact) * float(taper(np.float32(vc))), vc)
    d = unit(*lay.pin_dir)[:2]
    # The pin presses a dimple into the skin.
    near = np.exp(-((x - contact[0]) ** 2 + (y - contact[1]) ** 2) / (2 * (0.028 * n) ** 2))
    F = F + 0.11 * near
    g = np.sqrt(F)
    gy, gx = np.gradient(g)
    gnorm = np.maximum(np.hypot(gx, gy), 1e-6)
    sd_balloon = (g - 1) / gnorm

    knot = sd_triangle(x, y, world(0, 0.95), world(-0.085, 1.10), world(0.085, 1.10)) - 0.011 * n
    sd_body = smin(sd_balloon, knot, 0.022 * n)

    foot = world(0, 1.10)
    ctrl = [foot] + [(foot[0] + a * n, foot[1] + b * n) for a, b in lay.string]
    pts = bezier(ctrl, 40)
    sd_string = np.full((n, n), 1e9, np.float32)
    for a, b in zip(pts[:-1], pts[1:]):
        sd_string = np.minimum(sd_string, sd_taper(x, y, a, b, 1, 1)[0] + 1)
    sd_string -= lay.string_r * n

    tip = (contact[0] - d[0] * 0.012 * n, contact[1] - d[1] * 0.012 * n)
    head = (contact[0] + d[0] * lay.pin_len * n, contact[1] + d[1] * lay.pin_len * n)
    sd_shaft, along, across = sd_taper(x, y, head, tip, lay.shaft_r * n, 0.0018 * n)

    return dict(n=n, x=x, y=y, F=F, g=g, u=u, v=v, gx=gx / gnorm, gy=gy / gnorm, sd_balloon=sd_balloon, sd_body=sd_body,
                sd_string=sd_string, sd_shaft=sd_shaft, shaft_along=along, shaft_across=across,
                head=head, head_r=lay.head_r * n, tip=tip, contact=contact, rx=rx, pin_dir=d)


# ---------------------------------------------------------------- painting

def paint_balloon(sc, pal, rng, stages=None):
    """The balloon in gouache and watercolour, finished with modern gloss. With a
    `stages` dict, it also keeps a picture of every step (for --breakdown)."""
    n, x, y, F = sc["n"], sc["x"], sc["y"], sc["F"]
    k = n / 1024  # detail scale

    def snap(name, img):
        if stages is not None:
            stages[name] = np.clip(img if img.ndim == 3 else np.repeat(img[..., None], 3, 2), 0, 1).copy()

    # Form: a dome over the outline, dented where the pin presses.
    z = np.sqrt(np.clip(1 - F, 0, 1))
    zy, zx = np.gradient(z)
    lim = 3.0
    nx, ny = np.clip(-zx * sc["rx"] * 0.95, -lim, lim), np.clip(-zy * sc["rx"] * 0.95, -lim, lim)
    norm = np.sqrt(nx * nx + ny * ny + 1)
    nx, ny, nz = nx / norm, ny / norm, 1 / norm

    ndl = nx * LIGHT[0] + ny * LIGHT[1] + nz * LIGHT[2]
    diffuse = np.clip((ndl + 0.25) / 1.25, 0, 1)
    occlusion = 0.62 + 0.38 * np.sqrt(z)
    lum = np.clip(0.04 + 0.96 * diffuse * occlusion, 0, 1)
    snap("form", lum)

    # Brush strokes: they wrap round the form near the rim and follow the painter's hand
    # (a loose diagonal) across the middle, with a wandering angle so no two runs match.
    tx, ty = -sc["gy"], sc["gx"]
    hand = unit(0.86, -0.5, 0)
    flip = np.sign(tx * hand[0] + ty * hand[1] + 1e-6)
    tx, ty = tx * flip, ty * flip
    wrap = smoothstep(0.45, 0.95, sc["g"])
    sx, sy = hand[0] + (tx - hand[0]) * wrap, hand[1] + (ty - hand[1]) * wrap
    turn = 0.35 * fbm(n, 120 * k, rng, octaves=3)
    sx, sy = sx * np.cos(turn) - sy * np.sin(turn), sx * np.sin(turn) + sy * np.cos(turn)
    length = np.maximum(np.hypot(sx, sy), 1e-6)
    marks = 0.25 * grain(n, 1.6 * k, rng) + 0.8 * grain(n, 6 * k, rng) + 0.9 * grain(n, 13 * k, rng)
    strokes = lic(marks, sx / length, sy / length, int(34 * k))
    wobble = fbm(n, 90 * k, rng, octaves=4)
    snap("strokes", 0.5 + 0.18 * strokes)

    # Gouache: opaque value blocks with brushy borders, a little continuous form under them.
    lp = lum + 0.065 * strokes + 0.035 * wobble
    w = 0.028
    b1, b2, b3 = smoothstep(0.27 - w, 0.27 + w, lp), smoothstep(0.53 - w, 0.53 + w, lp), smoothstep(0.81 - w, 0.81 + w, lp)
    blocks = mix(mix(mix(pal["shadow"], pal["mid"], b1), pal["light"], b2), pal["warm"], b3)
    ramp = mix(mix(pal["shadow"], pal["mid"], smoothstep(0.1, 0.5, lum)), pal["light"], smoothstep(0.5, 0.9, lum))
    snap("blocks", blocks)
    col = 0.72 * blocks + 0.28 * ramp
    col *= (1 + 0.035 * strokes)[..., None]
    snap("gouache", col)

    # A cool transparent glaze over the shadow side.
    glaze = (1 - smoothstep(0.05, 0.5, lum)) ** 1.4 * 0.38
    col = mix(col, col * np.array([0.80, 0.88, 1.06], np.float32), glaze)
    snap("glaze", col)

    # Watercolour: pigment pools at the wet edge, granulates in the darks, and blooms once.
    depth = np.clip(-sc["sd_body"], 0, None)
    pooling = np.exp(-depth / (11 * k)) * (0.78 + 0.22 * fbm(n, 40 * k, rng, octaves=3))
    col *= (1 - 0.30 * pooling - 0.16 * np.exp(-depth / (2.6 * k)))[..., None]
    gran = np.clip(grain(n, 0.9 * k, rng), 0, None)
    col *= (1 - 0.075 * gran * (1 - lum))[..., None]
    blob = ndi.gaussian_filter((fbm(n, 150 * k, rng, octaves=3) > 0.95).astype(np.float32), 3 * k)
    bloom = blob * (1 - smoothstep(0.25, 0.5, lum))
    rim_of_bloom = np.clip(np.hypot(*np.gradient(blob)) * 9 * k, 0, 1) * (1 - smoothstep(0.25, 0.5, lum))
    col *= (1 + 0.07 * bloom - 0.22 * rim_of_bloom)[..., None]
    snap("watercolour", col)

    # Modern depth: the pin's shadow and the dimple's occlusion...
    off = (0.028 * n, 0.04 * n)
    shadow_sd = np.minimum(
        sd_taper(x - off[0], y - off[1], sc["head"], sc["tip"], 0.011 * n, 0.002 * n)[0],
        sd_circle(x - off[0], y - off[1], sc["head"], sc["head_r"]),
    )
    pin_shadow = 1 - smoothstep(-0.012 * n, 0.03 * n, shadow_sd)
    dimple = np.exp(-((x - sc["contact"][0]) ** 2 + (y - sc["contact"][1]) ** 2) / (2 * (0.02 * n) ** 2))
    col *= (1 - 0.34 * pin_shadow - 0.28 * dimple)[..., None]
    snap("cast", col)

    # ...light shining through the skin on the far side, as it does through a real balloon...
    inside_band = np.exp(-depth / (55 * k)) * (1 - np.exp(-depth / (7 * k)))
    through = np.clip(-ndl + 0.15, 0, 1) ** 1.3 * inside_band
    col = col + pal["light"] * (0.42 * through)[..., None]
    snap("through", col)

    # ...a cool rim light from behind, and a crisp clear-coat highlight over the paint.
    rim = (1 - nz) ** 2.2 * np.clip(nx * BACK[0] + ny * BACK[1] + nz * BACK[2], 0, 1)
    col = col + RIM * (0.5 * rim)[..., None]
    ndh = nx * HALF[0] + ny * HALF[1] + nz * HALF[2]
    col = col + (np.clip(ndh, 0, 1) ** 22 * 0.22)[..., None]
    col = mix(col, np.array([1, 0.995, 0.95], np.float32), highlight(sc, k, rng))

    paper = 1 - 0.028 * grain(n, 0.7 * k, rng)
    col *= paper[..., None]
    snap("gloss", col)
    return np.clip(col, 0, 1)


def highlight(sc, k, rng):
    """The clear-coat glare, laid on like one confident stroke of white gouache: a streak
    that follows the rim and tapers at both ends, then a separate glint further round."""
    u, v, g, rx = sc["u"], sc["v"], sc["g"], sc["rx"]
    edge = 1.6 * k * (1 + 0.9 * np.clip(fbm(sc["n"], 5 * k, rng, octaves=2), -1, 1))
    angle = np.arctan2(v, u)

    a0, a1, r0 = -2.80, -1.98, 0.70
    t = np.clip((angle - a0) / (a1 - a0), 0, 1)
    half_width = 0.085 * np.clip(np.sin(np.pi * t), 0, 1) ** 0.7
    in_range = (angle > a0) & (angle < a1)
    streak = np.where(in_range, (np.abs(g - r0) - half_width) * rx, 1e4)
    glint_u, glint_v = r0 * math.cos(-1.70), r0 * math.sin(-1.70) * 1.0
    glint = (np.hypot(u - glint_u, v - glint_v) - 0.042) * rx

    strength = 0.97 - 0.22 * np.abs(t - 0.45) / 0.55  # brightest just past the middle
    main = np.clip(0.5 - streak / edge, 0, 1) * strength + np.clip(0.5 - glint / edge, 0, 1) * 0.92
    halo = ndi.gaussian_filter(main, 16 * k) * 0.32
    return np.clip(main + halo, 0, 1)

def paint_string(sc, rng):
    n, k = sc["n"], sc["n"] / 1024
    across = np.clip(-sc["sd_string"] / (0.009 * n), 0, 1)
    col = mix(PAPER * np.array([0.72, 0.70, 0.70], np.float32), PAPER, across ** 0.6)
    return np.clip(col * (1 + 0.04 * grain(n, 1.5 * k, rng))[..., None], 0, 1)


def paint_pin(sc, pal):
    """The pin is rendered, not painted: an enamel head and a chrome needle."""
    x, y, (hx, hy), r = sc["x"], sc["y"], sc["head"], sc["head_r"]
    nx, ny = (x - hx) / r, (y - hy) / r
    nz = np.sqrt(np.clip(1 - nx * nx - ny * ny, 0, 1))
    ndl = np.clip(nx * LIGHT[0] + ny * LIGHT[1] + nz * LIGHT[2], 0, 1)
    head = mix(PIN["shadow"], PIN["mid"], smoothstep(0.0, 0.55, ndl))
    head = mix(head, PIN["light"], smoothstep(0.62, 0.95, ndl))
    bounce = np.clip(-nx * 0.7 + ny * 0.7, 0, 1) ** 2 * (1 - ndl)  # the balloon's colour, reflected
    head = head + pal["mid"] * (0.4 * bounce)[..., None]
    head = head + RIM * (0.5 * (1 - nz) ** 3)[..., None]
    ndh = nx * HALF[0] + ny * HALF[1] + nz * HALF[2]
    head = mix(head, np.ones(3, np.float32), np.clip(smoothstep(0.975, 0.984, ndh) * 0.95 + ndh ** 32 * 0.3, 0, 1))

    w = sc["shaft_across"]
    lit = -0.35 if (-sc["pin_dir"][1] * LIGHT[0] + sc["pin_dir"][0] * LIGHT[1]) > 0 else 0.35
    shaft = mix(CHROME_DARK, CHROME_LIGHT, np.exp(-((w - lit) / 0.5) ** 2))
    shaft = mix(shaft, np.ones(3, np.float32), np.exp(-((w - lit * 1.35) / 0.13) ** 2) * 0.9)
    return np.clip(head, 0, 1), np.clip(shaft, 0, 1)


def layers(n, lay, pal, seed, stages=None):
    """Everything painted for one composition and colour."""
    rng = np.random.default_rng(seed)
    sc = scene(n, lay)
    head_rgb, shaft_rgb = paint_pin(sc, pal)
    return sc, dict(balloon=paint_balloon(sc, pal, rng, stages), string=paint_string(sc, rng), head=head_rgb, shaft=shaft_rgb)


def stack(sc, paint, pal, key, dst_rgb, dst_a):
    """String, balloon and pin, each with its ink keyline `key` px wide."""
    sd_head = sd_circle(sc["x"], sc["y"], sc["head"], sc["head_r"])
    sd_pin = np.minimum(sc["sd_shaft"], sd_head)
    for sd, ink, fills in (
        (sc["sd_string"], NAVY_INK, [(sc["sd_string"], paint["string"])]),
        (sc["sd_body"], pal["ink"], [(sc["sd_body"], paint["balloon"])]),
        (sd_pin, NAVY_INK, [(sc["sd_shaft"], paint["shaft"]), (sd_head, paint["head"])]),
    ):
        if key > 0:
            dst_rgb, dst_a = over(dst_rgb, dst_a, np.broadcast_to(ink, dst_rgb.shape), coverage(sd - key))
        for fsd, rgb in fills:
            dst_rgb, dst_a = over(dst_rgb, dst_a, rgb, coverage(fsd))
    return dst_rgb, dst_a


def union(sc):
    sd_head = sd_circle(sc["x"], sc["y"], sc["head"], sc["head_r"])
    return np.minimum.reduce([sc["sd_body"], sc["sd_string"], sc["sd_shaft"], sd_head])


def sticker_border(sc, key, border, rounding, rng):
    """The die-cut border in paper white, bevelled toward the light."""
    n, k = sc["n"], sc["n"] / 1024
    sd = die_cut(union(sc) - key, border, rounding)
    gy, gx = np.gradient(ndi.gaussian_filter(sd, 1.5 * k))
    facing = -(gx * LIGHT[0] + gy * LIGHT[1]) / np.maximum(np.hypot(gx, gy), 1e-6)
    edge = np.clip(1 + sd / (7 * k), 0, 1)
    col = PAPER * (1 + 0.10 * edge * facing)[..., None]
    col *= (1 - 0.02 * grain(n, 0.8 * k, rng))[..., None]
    return np.clip(col, 0, 1), coverage(sd)


# ---------------------------------------------------------------- compositions

def to_image(rgb, a, size=None):
    arr = np.dstack([np.clip(rgb, 0, 1), np.clip(a, 0, 1)[..., None]])
    img = Image.fromarray((arr * 255 + 0.5).astype(np.uint8), "RGBA")
    if size and size != img.width:
        img = img.convert("RGBa").resize((size, size), Image.LANCZOS).convert("RGBA")
    return img


def app_icon(n=APP_MASTER, stages=None):
    k = n / 1024
    pal = PALETTES["green"]
    sc, paint = layers(n, APP, pal, seed=11, stages=stages)
    rng = np.random.default_rng(29)
    x, y = sc["x"], sc["y"]

    # The tile: a squircle with a deep watercolour wash, a glow behind the balloon and a vignette.
    half, e = 0.462 * n, 5.0
    gt = (np.abs((x - n / 2) / half) ** e + np.abs((y - n / 2) / half) ** e) ** (1 / e)
    gty, gtx = np.gradient(gt)
    sd_tile = (gt - 1) / np.maximum(np.hypot(gtx, gty), 1e-6)
    t = np.clip(y / n, 0, 1)
    bg = mix(hexrgb("#2B3360"), hexrgb("#12142A"), t)
    bg = mix(bg, hexrgb("#1B5A6E"), np.clip(0.5 + 0.5 * fbm(n, 300 * k, rng, octaves=4), 0, 1) * 0.16)
    bg = mix(bg, hexrgb("#4A2C78"), np.clip(fbm(n, 220 * k, rng, octaves=4), 0, 1) * 0.12)
    glow = np.exp(-(((x - APP.cx * n) ** 2 + (y - APP.cy * n) ** 2) / (0.38 * n) ** 2))
    bg = bg + pal["light"] * (0.26 * glow)[..., None]
    blooms = ndi.gaussian_filter((fbm(n, 260 * k, rng, octaves=3) > 1.1).astype(np.float32), 6 * k)
    bloom_rim = np.clip(np.hypot(*np.gradient(blooms)) * 14 * k, 0, 1)
    bg *= (1 + 0.05 * blooms - 0.10 * bloom_rim)[..., None]
    vignette = smoothstep(0.30 * n, 0.75 * n, np.hypot(x - 0.5 * n, y - 0.45 * n))
    bg *= (1 - 0.28 * vignette)[..., None]
    bg *= (1 + 0.025 * grain(n, 0.8 * k, rng))[..., None]
    depth_in = np.clip(-sd_tile, 0, None)
    bg = bg + (0.20 * np.exp(-depth_in / (5 * k)) * smoothstep(0.55 * n, 0.15 * n, y))[..., None]
    bg *= (1 - 0.30 * np.exp(-depth_in / (9 * k)) * smoothstep(0.55 * n, 0.95 * n, y))[..., None]
    rgb, a = np.clip(bg, 0, 1), coverage(sd_tile)
    if stages is not None:
        stages.update(scene=sc, paint=paint, pal=pal, tile=(rgb.copy(), a.copy()))

    # The sticker's soft shadows, then the sticker.
    key, border, rounding = 6.5 * k, 26 * k, 34 * k
    border_rgb, border_a = sticker_border(sc, key, border, rounding, rng)
    for sigma, (ox, oy), strength in ((24 * k, (10 * k, 30 * k), 0.55), (5 * k, (2 * k, 7 * k), 0.35)):
        soft = ndi.shift(ndi.gaussian_filter(border_a, sigma), (oy, ox), order=1)
        rgb, a = over(rgb, a, np.broadcast_to(hexrgb("#05060F"), rgb.shape), soft * strength * a)
    if stages is not None:
        stages.update(shadows=(rgb.copy(), a.copy()), border=(border_rgb, border_a), key=key)
    rgb, a = over(rgb, a, border_rgb, border_a)
    rgb, a = stack(sc, paint, pal, key, rgb, a)
    return to_image(rgb, a * coverage(sd_tile))


def breakdown():
    """art/breakdown.png: the icon taken apart, one panel per stage, in painting order."""
    from PIL import ImageDraw, ImageFont

    stages = {}
    final = app_icon(1024, stages)
    sc, paint, pal, key = stages["scene"], stages["paint"], stages["pal"], stages["key"]
    n = sc["n"]
    body = coverage(sc["sd_body"])
    paper = np.broadcast_to(PAPER * 0.94, (n, n, 3)).astype(np.float32)

    # Rows 1-3 are cropped to the sticker; row 4 shows the whole tile.
    near = np.argwhere(union(sc) < 46)
    (y0, x0), (y1, x1) = near.min(0), near.max(0)
    side = max(y1 - y0, x1 - x0)
    cy, cx = (y0 + y1) // 2, (x0 + x1) // 2
    box = (cx - side // 2, cy - side // 2, cx + side // 2, cy + side // 2)

    def on_paper(rgb, a, bg=paper):
        out, _ = over(bg.copy(), np.ones((n, n), np.float32), rgb, a)
        return Image.fromarray((np.clip(out, 0, 1) * 255).astype(np.uint8)).crop(box)

    def balloon(name):
        return on_paper(stages[name], body)

    # The geometry: the union's signed distance field as iso-bands, edge in white. (Exact, from a
    # distance transform: the painter's own fields are only trusted near the outline.)
    d = edt_sdf(union(sc) < 0)
    tint = np.where((d < 0)[..., None], np.array([0.60, 0.80, 1.0], np.float32), np.array([0.95, 0.66, 0.36], np.float32))
    bands = tint * (0.80 + 0.20 * np.cos(d * 0.35))[..., None] * (1 - np.exp(-np.abs(d) / 60))[..., None] ** 0.25
    bands = mix(bands, np.ones(3, np.float32), np.clip(1.5 - np.abs(d), 0, 1))
    sdf = Image.fromarray((np.clip(bands, 0, 1) * 255).astype(np.uint8)).crop(box)

    sd_head = sd_circle(sc["x"], sc["y"], sc["head"], sc["head_r"])
    pin_rgb, pin_a = over(paper.copy(), np.ones((n, n), np.float32), paint["shaft"], coverage(sc["sd_shaft"]))
    pin_rgb, _ = over(pin_rgb, pin_a, paint["head"], coverage(sd_head))
    pin = Image.fromarray((np.clip(pin_rgb, 0, 1) * 255).astype(np.uint8)).crop(box)

    inked, inked_a = stack(sc, paint, pal, key, paper.copy(), np.ones((n, n), np.float32))
    keylines = Image.fromarray((np.clip(inked, 0, 1) * 255).astype(np.uint8)).crop(box)
    grey = np.broadcast_to(np.float32(0.36), (n, n, 3)).astype(np.float32)
    cut, cut_a = over(grey.copy(), np.ones((n, n), np.float32), *stages["border"])
    cut, _ = stack(sc, paint, pal, key, cut, cut_a)
    die = Image.fromarray((np.clip(cut, 0, 1) * 255).astype(np.uint8)).crop(box)

    def whole(rgb_a):
        rgb, a = rgb_a
        bg = np.broadcast_to(np.float32(0.95), (n, n, 3)).astype(np.float32)
        out, _ = over(bg.copy(), np.ones((n, n), np.float32), rgb, a)
        return Image.fromarray((np.clip(out, 0, 1) * 255).astype(np.uint8))

    tile_floating = stages["shadows"]
    tile_floating = over(*tile_floating, *stages["border"])
    panels = [
        ("1  Silhouette: one signed distance field", sdf),
        ("2  Form: dome normals under a key light", balloon("form")),
        ("3  Brush field: strokes follow the form", balloon("strokes")),
        ("4  Gouache: three flat value blocks", balloon("blocks")),
        ("5  Blocks over a soft form, brushed", balloon("gouache")),
        ("6  Cool transparent glaze in the shadows", balloon("glaze")),
        ("7  Watercolour: pooling, grain, a bloom", balloon("watercolour")),
        ("8  The pin's cast shadow and dimple", balloon("cast")),
        ("9  Light through the skin", balloon("through")),
        ("10 Rim light and clear-coat glare", balloon("gloss")),
        ("11 Rendered pin: enamel and chrome", pin),
        ("12 Ink keylines", keylines),
        ("13 Die-cut sticker border", die),
        ("14 Tile: wash, glow, bevel", whole(stages["tile"])),
        ("15 Sticker lifted on two shadows", whole(tile_floating)),
        ("16 Finished", Image.alpha_composite(Image.new("RGBA", final.size, (242, 242, 242, 255)), final).convert("RGB")),
    ]
    tile, gap, caption = 340, 18, 38
    sheet = Image.new("RGB", (4 * tile + 5 * gap, 4 * (tile + caption) + 5 * gap - 6), (246, 246, 248))
    draw = ImageDraw.Draw(sheet)
    font = ImageFont.truetype("C:/Windows/Fonts/seguisb.ttf", 15)
    for i, (label, img) in enumerate(panels):
        col, row = i % 4, i // 4
        x, y = gap + col * (tile + gap), gap + row * (tile + caption + gap)
        sheet.paste(img.resize((tile, tile), Image.LANCZOS), (x, y))
        draw.text((x + 2, y + tile + 8), label, font=font, fill=(40, 42, 50))
    sheet.save(OUT.parent / "art" / "breakdown.png")


def tray_icons():
    n, k = TRAY_MASTER, TRAY_MASTER / 1024
    out = {}
    for i, (name, pal) in enumerate(PALETTES.items()):
        sc, paint = layers(n, TRAY, pal, seed=40 + i)
        rng = np.random.default_rng(90 + i)
        for size in TRAY_SIZES:
            px = n / size  # one target pixel, in master pixels
            for theme in ("dark", "light"):
                rgb = np.zeros((n, n, 3), np.float32)
                a = np.zeros((n, n), np.float32)
                if theme == "dark":
                    key = 0.75 * px
                    border_rgb, border_a = sticker_border(sc, key, 1.0 * px, 1.2 * px, rng)
                    rgb, a = over(rgb, a, border_rgb, border_a)
                else:
                    key = 1.1 * px
                rgb, a = stack(sc, paint, pal, key, rgb, a)
                out[(name, size, theme)] = to_image(rgb, a, size)
    return out


def save_ico(path, frames):
    """An .ico with one PNG-compressed image per size (Pillow can't mix sources per size)."""
    import io
    import struct

    blobs = []
    for img in frames:
        buf = io.BytesIO()
        img.save(buf, "PNG")
        blobs.append(buf.getvalue())
    header = struct.pack("<HHH", 0, 1, len(frames))
    offset = 6 + 16 * len(frames)
    entries = b""
    for img, blob in zip(frames, blobs):
        w = img.width if img.width < 256 else 0
        entries += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    path.write_bytes(header + entries + b"".join(blobs))


def main():
    OUT.mkdir(exist_ok=True)
    for old in OUT.glob("*.png"):
        old.unlink()
    tray = tray_icons()
    for (name, size, theme), img in tray.items():
        img.save(OUT / f"{name}-{size}-{theme}.png")
    app = app_icon()
    big = app.convert("RGBa").resize((1024, 1024), Image.LANCZOS).convert("RGBA")
    # At 32 px and under the tile would swallow the balloon, so those sizes are the
    # bare sticker balloon (it reads on light and dark backgrounds alike).
    frames = [tray[("green", s, "dark")] if s <= 32 else big.convert("RGBa").resize((s, s), Image.LANCZOS).convert("RGBA") for s in APP_SIZES]
    save_ico(OUT / "app.ico", frames)
    for s in (128, 256):
        big.convert("RGBa").resize((s, s), Image.LANCZOS).convert("RGBA").save(OUT / f"logo-{s}.png")
    big.save(OUT.parent / "art" / "icon-1024.png")


if __name__ == "__main__":
    import sys

    breakdown() if "--breakdown" in sys.argv else main()
