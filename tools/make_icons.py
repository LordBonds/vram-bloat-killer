"""Draws the tray icons: a GPU chip in the status colour, at every tray size.

Run from the repo root: python tools/make_icons.py
"""
from pathlib import Path

from PIL import Image, ImageDraw

COLOURS = {
    "green": (63, 185, 80),
    "amber": (230, 162, 40),
    "red": (248, 81, 73),
}
SIZES = [16, 20, 24, 32, 48]
MASTER = 256
OUT = Path(__file__).resolve().parent.parent / "icons"


def master(colour):
    img = Image.new("RGBA", (MASTER, MASTER), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    u = MASTER / 16
    dark = (24, 26, 30, 255)
    # pins on all four sides
    for i in range(3):
        o = (4.5 + i * 3) * u
        d.rectangle([o, 0.5 * u, o + 1.4 * u, 15.5 * u], fill=dark)
        d.rectangle([0.5 * u, o, 15.5 * u, o + 1.4 * u], fill=dark)
    # chip body: dark rim, coloured die, dark core
    d.rounded_rectangle([2 * u, 2 * u, 14 * u, 14 * u], radius=2 * u, fill=dark)
    d.rounded_rectangle([3 * u, 3 * u, 13 * u, 13 * u], radius=1.4 * u, fill=colour + (255,))
    d.rounded_rectangle([6 * u, 6 * u, 10 * u, 10 * u], radius=0.6 * u, fill=dark)
    return img


def main():
    OUT.mkdir(exist_ok=True)
    for name, colour in COLOURS.items():
        big = master(colour)
        for s in SIZES:
            big.resize((s, s), Image.LANCZOS).save(OUT / f"{name}-{s}.png")
        if name == "green":
            big.save(OUT / "app.ico", sizes=[(s, s) for s in SIZES + [64, 128, 256]])
            big.resize((128, 128), Image.LANCZOS).save(OUT / "logo-128.png")


if __name__ == "__main__":
    main()
