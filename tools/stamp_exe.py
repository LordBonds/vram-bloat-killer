"""Stamps the app icon and version info into a built exe, so Explorer, Task Manager and
the file's Properties show "VRAM Bloat Killer" with the balloon.

The Rust build has no resource compiler (the GNU toolchain ships none), so this writes
the resources afterwards with Windows' own UpdateResource API.

Run from the repo root after `cargo build --release`:
    python tools/stamp_exe.py target/release/vram-bloat-killer.exe
"""
import ctypes
import re
import struct
import sys
from ctypes import wintypes
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RT_ICON, RT_GROUP_ICON, RT_VERSION = 3, 14, 16
LANG = 0x0409  # US English
APP_ICON_ID = 1  # main.rs loads the window icon by this id

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.BeginUpdateResourceW.argtypes = [wintypes.LPCWSTR, wintypes.BOOL]
k32.BeginUpdateResourceW.restype = wintypes.HANDLE
k32.UpdateResourceW.argtypes = [wintypes.HANDLE, ctypes.c_void_p, ctypes.c_void_p, wintypes.WORD, ctypes.c_void_p, wintypes.DWORD]
k32.UpdateResourceW.restype = wintypes.BOOL
k32.EndUpdateResourceW.argtypes = [wintypes.HANDLE, wintypes.BOOL]
k32.EndUpdateResourceW.restype = wintypes.BOOL


def icon_resources(ico):
    """Splits an .ico into RT_ICON images and the RT_GROUP_ICON directory that lists them."""
    _, _, count = struct.unpack_from("<HHH", ico, 0)
    images, group = [], struct.pack("<HHH", 0, 1, count)
    for i in range(count):
        w, h, colours, _, planes, bits, size, offset = struct.unpack_from("<BBBBHHII", ico, 6 + 16 * i)
        images.append(ico[offset:offset + size])
        group += struct.pack("<BBBBHHIH", w, h, colours, 0, planes, bits, size, i + 1)
    return images, group


def node(key, value=b"", children=(), text=True, value_len=None):
    """One VS_VERSIONINFO block: header, key, padding, value, padding, children."""
    def pad(b):
        return b + b"\0" * (-len(b) % 4)

    head = 6 + len((key + "\0").encode("utf-16-le"))
    body = pad(b"\0" * head)[head:] + value
    kids = b"".join(pad(c) for c in children)
    if kids:
        body = pad(b"\0" * head + body)[head:]
    length = head + len(body) + len(kids)
    vlen = len(value) if value_len is None else value_len
    return struct.pack("<HHH", length, vlen, int(text)) + (key + "\0").encode("utf-16-le") + body + kids


def version_info(version):
    parts = [int(p) for p in version.split(".")] + [0] * 4
    ms, ls = (parts[0] << 16) | parts[1], (parts[2] << 16) | parts[3]
    fixed = struct.pack("<13I", 0xFEEF04BD, 0x00010000, ms, ls, ms, ls, 0x3F, 0, 0x00040004, 1, 0, 0, 0)
    strings = {
        "CompanyName": "LordBonds",
        "FileDescription": "VRAM Bloat Killer",
        "FileVersion": version,
        "InternalName": "vram-bloat-killer",
        "LegalCopyright": "© 2026 LordBonds. MIT licence.",
        "OriginalFilename": "vram-bloat-killer.exe",
        "ProductName": "VRAM Bloat Killer",
        "ProductVersion": version,
    }
    table = [node(k, (v + "\0").encode("utf-16-le"), value_len=len(v) + 1) for k, v in strings.items()]
    string_info = node("StringFileInfo", children=[node("040904B0", children=table)])
    var_info = node("VarFileInfo", children=[node("Translation", struct.pack("<HH", LANG, 0x04B0), text=False)])
    return node("VS_VERSION_INFO", fixed, children=[string_info, var_info], text=False)


def update(handle, rtype, name, data):
    buf = ctypes.create_string_buffer(data, len(data))
    if not k32.UpdateResourceW(handle, rtype, name, LANG, buf, len(data)):
        raise OSError(ctypes.get_last_error(), f"UpdateResource {rtype}/{name} failed")


def main():
    exe = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target" / "release" / "vram-bloat-killer.exe").resolve()
    version = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.M).group(1)
    images, group = icon_resources((ROOT / "icons" / "app.ico").read_bytes())

    handle = k32.BeginUpdateResourceW(str(exe), False)
    if not handle:
        raise OSError(ctypes.get_last_error(), f"can't open {exe} for resource updates")
    try:
        for i, image in enumerate(images, start=1):
            update(handle, RT_ICON, i, image)
        update(handle, RT_GROUP_ICON, APP_ICON_ID, group)
        update(handle, RT_VERSION, 1, version_info(version))
    except Exception:
        k32.EndUpdateResourceW(handle, True)
        raise
    if not k32.EndUpdateResourceW(handle, False):
        raise OSError(ctypes.get_last_error(), "writing the resources failed")
    print(f"stamped {exe.name}: {len(images)} icon sizes, version {version}")


if __name__ == "__main__":
    main()
