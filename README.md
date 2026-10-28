<p align="center"><img src="icons/logo-128.png" width="96" alt=""></p>

# VRAM Bloat Killer

A tiny Windows tray app that watches the **GPU memory of every app**. It tells you when one is bloating, and it can kill one that passes a hard limit you set. For browsers and Electron apps, "kill" means restarting only the GPU process, **so every tab stays open**.

## Why

Apps can borrow your **system RAM** as graphics memory, and Task Manager's Memory column doesn't count it. A browser that leaks this way looks like it is using 2.8 GB while it is actually holding about 11 GB. All you see is "in use" stuck near your total RAM, with no process that explains it.

Chromium-based browsers (Opera GX, Chrome, Edge, Brave, Vivaldi) and Electron apps (Discord, Slack, VS Code, Spotify) do all graphics work in a separate *GPU process*, which is where these leaks usually live. Restarting only that process frees all of it. The app relaunches it within a second, the screen flickers once, and every tab, video and login stays put.

## What counts as bloat

Games and 3D apps are supposed to use lots of VRAM, so size alone only counts against browsers. You get a notification when:

| | Browsers and Electron apps | Every other app |
|---|---|---|
| **Over the warn limit** | total GPU memory | system RAM borrowed as graphics memory (only on PCs with a dedicated card, since on integrated graphics all GPU memory is borrowed) |
| **Leak alert** | climbed by your leak amount over 30 minutes, rising in most 5-minute steps and never dropping back. One big jump, like a level loading, doesn't count. | same |
| **Over the auto-kill limit** | total GPU memory | total GPU memory |

## What it does about it

- **Browsers and Electron apps:** click the notification, or the app's row in the tray menu, to restart its GPU process. Tabs stay open.
- **Auto-kill (off by default):** set a hard limit. Anything that passes it is dealt with automatically:
  - A browser or Electron app has its GPU process restarted. Tabs stay open.
  - Any other app only gets a warning, unless you turn on **"Auto-kill closes other apps too"**. Then it is closed completely and **any unsaved work in it is lost**.
  - Windows' own processes (Desktop Window Manager, Explorer and the like) are never closed, whatever the setting.
- **Cooldown:** auto-kill hits the same app at most once every 5, 10, 30 or 60 minutes. You choose. If the app passes the limit again inside that window, you get a warning instead. Chromium turns off hardware acceleration if its GPU process dies too often, so don't go below 5 minutes.

The tray icon is **green** when everything is calm, **amber** when something is over half the warn limit, and **red** when there is an active alert. Hover it to see the top three apps.

## Settings (right-click the tray icon)

| Setting | Default | Choices |
|---|---|---|
| Warn above | 3 GB | 1, 2, 3, 4, 6, 8 GB |
| Leak alert | +2 GB in 30 min | +1 to +4 GB |
| Auto-kill above | off | 2, 3, 4, 6, 8, 10, 12, 16 GB |
| Auto-kill at most every | 10 min | 5, 10, 30, 60 min |
| Auto-kill closes other apps too | off | |
| Start with Windows | off | |

Settings are saved in `%APPDATA%\VRAM Bloat Killer\settings.ini`. "Start with Windows" adds a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, and turning it off removes it again.

## Install

Download `vram-bloat-killer.exe` from [Releases](../../releases) and run it. There is no installer, and it needs nothing else installed. It is about 400 KB and uses well under 10 MB of RAM.

## Building

```bash
cargo build --release
```

It builds with either Rust Windows toolchain: MSVC (needs the Visual Studio Build Tools) or GNU (`rustup toolchain install stable-x86_64-pc-windows-gnu`, then `cargo +stable-x86_64-pc-windows-gnu build --release`).

To redraw the icons, run `python tools/make_icons.py`. It needs Pillow.

Run the tests with `cargo test -- --include-ignored --nocapture`. The ignored tests run on your real machine: one lists every process using the GPU, and one checks that closing an app only works when the name matches.

## Limits

- Windows 10 1709 or later. Older versions don't have per-process GPU memory counters.
- It can only stop processes it is allowed to touch. An app running as administrator needs VRAM Bloat Killer to run as administrator too.
- Usage history lives in memory, so leak alerts need 30 minutes of watching after each launch.
- It treats the symptom. If one app balloons over and over, updating your graphics driver is worth trying, and so is turning off that app's animated extras such as live wallpapers and GX mods.

Opera, Opera GX, Chrome, Edge and the other names above are trademarks of their owners. This project is not affiliated with any of them.

## License

MIT © LordBonds
