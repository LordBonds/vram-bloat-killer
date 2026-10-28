<p align="center"><img src="icons/logo-256.png" width="160" alt="VRAM Bloat Killer: a painted balloon about to meet a pin"></p>

# VRAM Bloat Killer

A small Windows tray app that watches the **GPU memory of every app**. It tells you quietly when one is bloating, frees browser bloat at moments you won't notice, can close anything that passes a hard limit (asking it first), and has a big red **emergency wipe** button under a glass cover.

## Why

Apps can borrow your **system RAM** as graphics memory, and Task Manager's Memory column doesn't count it. A browser that leaks this way looks like it is using 2.8 GB while it is actually holding about 11 GB. All you see is "in use" stuck near your total RAM, with no process that explains it.

Chromium-based browsers (Opera GX, Chrome, Edge, Brave, Vivaldi) and Electron apps (Discord, Slack, VS Code, Spotify) do all graphics work in a separate *GPU process*, which is where these leaks usually live. Restarting only that process frees all of it. The app relaunches it within a second, and every tab, video and login stays put.

## What counts as bloat

Games and 3D apps are supposed to use lots of VRAM, so size alone only counts against browsers.

| | Browsers and Electron apps | Every other app |
|---|---|---|
| **Over the warn limit** | total GPU memory | system RAM borrowed as graphics memory (only on PCs with a dedicated card, since on integrated graphics all GPU memory is borrowed) |
| **Leak alert** | climbed by your leak amount over 30 minutes, rising in most 5-minute steps and never dropping back. One big jump, like a level loading, doesn't count. | same |
| **Over the auto-kill limit** | total GPU memory | total GPU memory |

## What it does about it

- **Quiet restarts (on by default).** When a browser's GPU process passes the warn limit, it is restarted at a moment you won't notice: when that app isn't in front, or when you've been away for ten minutes. It never happens while something is fullscreen. Your tabs stay open, and the restart is logged without a pop-up.
- **Auto-kill (off by default).** A hard limit for anything:
  - A browser or Electron app has its GPU process restarted. If no quiet moment comes within 15 minutes, it happens anyway.
  - Any other app only gets a warning, unless you turn on **"Auto-kill closes other apps too"**. It is then **asked** to close the way the X button does: a card counts down 30 seconds that you can cancel, and then the app can still ask you to save. **"Force-close if they don't"** (off by default) finishes the job when an app ignores the request, and any unsaved work in it is lost.
  - **Cooldown.** Auto-kill hits the same app at most once every 5, 10, 30 or 60 minutes. You choose.
- **Game mode (on by default).** While a game or video is fullscreen, every automatic action waits, and no pop-ups appear.
- **Emergency wipe:** `Ctrl+Alt+Shift+W`, the tray menu, or the button in the Activity window. Lift the glass cover to arm it. The button stays dead for a moment after the cover opens, so a fast double-click can't fire it. Before you press, the panel lists exactly whose GPU processes it will restart and which apps it will ask to close. Then BAM. Limits and cooldowns don't apply, and the result names every app it touched.

**Never touched, whatever the setting:** anything that runs from the Windows folder (Explorer, the Desktop Window Manager, the touch keyboard and so on), your graphics driver's own tools, and VRAM Bloat Killer itself.

## Notifications that don't interrupt

VRAM Bloat Killer has its own small dark cards instead of Windows' notifications. They never take focus and never make a sound. They fade out on their own, wait while your mouse is on them, and stay hidden while something is fullscreen.

Everything also goes to the **Activity** window, which you open by left-clicking the tray icon. It keeps a scrollable history that survives restarts, and its header shows how much system RAM your GPUs are borrowing right now.

## The icon

A balloon about to meet a pin, painted in gouache and watercolour and finished with modern depth: a gloss highlight, a rim light, an enamel-and-chrome pin and a die-cut sticker border. The tray version changes colour with the state (green, amber, red) and switches between an ink outline for light taskbars and a paper sticker border for dark ones, following your Windows theme live. How it is painted is described in [art/STYLE.md](art/STYLE.md).

## Settings (right-click the tray icon)

| Setting | Default | Choices |
|---|---|---|
| Warn above | 3 GB | 1, 2, 3, 4, 6, 8 GB |
| Leak alert | +2 GB in 30 min | +1 to +4 GB |
| Auto-kill above | off | 2, 3, 4, 6, 8, 10, 12, 16 GB |
| Auto-kill at most every | 10 min | 5, 10, 30, 60 min |
| Options: quietly restart browsers at the warn limit | on | |
| Options: game mode | on | |
| Options: auto-kill closes other apps too | off | |
| Options: ask them to close first (30 s warning) | on | |
| Options: force-close if they don't | off | |
| Options: pop-up notifications | on | |
| Options: start with Windows | off | |

Settings are saved in `%APPDATA%\VRAM Bloat Killer\settings.ini` and the history in `activity.log` next to it. "Start with Windows" adds a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, and turning it off removes it again.

## Install

Download `vram-bloat-killer.exe` from [Releases](../../releases) and run it. There is no installer, and it needs nothing else installed. It is under 1 MB, most of that the painted artwork.

## Building

```bash
cargo build --release
```

```bash
python tools/stamp_exe.py target/release/vram-bloat-killer.exe
```

The second step writes the app icon and version info into the exe. The GNU Rust toolchain has no resource compiler, so this is done afterwards with Windows' own resource API. Run it after every build you mean to ship.

It builds with either Rust Windows toolchain: MSVC (needs the Visual Studio Build Tools) or GNU (`rustup toolchain install stable-x86_64-pc-windows-gnu`, then add `+stable-x86_64-pc-windows-gnu` after `cargo`).

To repaint the art, run `python tools/make_icons.py` for the icons (about a minute; needs Pillow, numpy and scipy) and `python tools/make_emergency_art.py` for the emergency panel.

To test, run `cargo test -- --include-ignored --test-threads=1`. The ignored tests run on your real machine: they list every process using the GPU, close a copy of `ping` but never Windows' own, and check that graceful close finds a window and that the app really closes.

## Limits

- Windows 10 1709 or later. Older versions don't have per-process GPU memory counters.
- It can only touch processes it is allowed to. An app running as administrator needs VRAM Bloat Killer to run as administrator too.
- Usage history lives in memory, so leak alerts need 30 minutes of watching after each launch.
- It treats the symptom. If one app balloons over and over, updating your graphics driver is worth trying, and so is turning off that app's animated extras such as live wallpapers and GX mods.

Opera, Opera GX, Chrome, Edge and the other names above are trademarks of their owners. This project is not affiliated with any of them.

## License

MIT © LordBonds
