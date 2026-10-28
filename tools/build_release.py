"""Builds the exe to ship: a release build with no trace of whoever built it, then
stamped with the icon and version info.

Rust bakes the full paths of its source files into the binary for panic messages,
which would put the builder's home folder (and so their name) into every download.
This remaps those paths to neutral ones, then refuses to finish if the user name or
home folder is still anywhere in the exe.

Run from the repo root:
    python tools/build_release.py                 # default toolchain
    python tools/build_release.py +stable-x86_64-pc-windows-gnu
"""
import getpass
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXE = ROOT / "target" / "release" / "vram-bloat-killer.exe"


def main():
    home = Path.home()
    cargo_home = Path(os.environ.get("CARGO_HOME", home / ".cargo"))
    rustup_home = Path(os.environ.get("RUSTUP_HOME", home / ".rustup"))
    remaps = {cargo_home: "/cargo", rustup_home: "/rustup", ROOT: "/vram-bloat-killer", home: "/home"}
    flags = " ".join(f"--remap-path-prefix={src}={dst}" for src, dst in remaps.items())
    env = dict(os.environ, RUSTFLAGS=(os.environ.get("RUSTFLAGS", "") + " " + flags).strip())

    toolchain = [a for a in sys.argv[1:] if a.startswith("+")]
    subprocess.run(["cargo", *toolchain, "build", "--release"], cwd=ROOT, env=env, check=True)

    # Refuse to ship anything that still names the builder.
    data = EXE.read_bytes()
    needles = {str(home), home.name, getpass.getuser()}
    for needle in needles:
        for encoded in (needle.encode(), needle.encode("utf-16-le")):
            if re.search(re.escape(encoded), data, re.IGNORECASE):
                sys.exit(f"refusing to ship: {EXE.name} still contains {needle!r}")

    subprocess.run([sys.executable, str(ROOT / "tools" / "stamp_exe.py"), str(EXE)], check=True)
    print(f"clean: no trace of {len(needles)} personal strings in {EXE.name}")


if __name__ == "__main__":
    main()
