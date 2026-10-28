<p align="center"><img src="icons/logo-128.png" width="96" alt=""></p>

# VRAM Bloat Killer

A tiny Windows tray app that catches browsers and apps whose **GPU process is hoarding memory**, and restarts that process **without closing a single tab**.

## Why

Chromium-based browsers (Opera GX, Chrome, Edge, Brave, Vivaldi) and Electron apps (Discord, Slack, VS Code, Spotify) run all graphics work in a separate *GPU process*. That process can borrow your **system RAM** as graphics memory, and on some setups it leaks. It can grow to 8–10 GB over a long session.

Task Manager won't show you this. Its Memory column leaves out GPU memory, so the browser looks like it is using 2.8 GB while it is actually holding about 11 GB. All you see is "in use" stuck near your total RAM with no process that explains it.

Restarting only the GPU process frees all of it. The browser relaunches the process within a second, the screen flickers once, and every tab, video and login stays put.

## What it does

- Reads Windows' own `GPU Process Memory` performance counters every 5 seconds. These are the same counters Task Manager uses on its GPU tab.
- Watches every process launched with `--type=gpu-process`, which covers every Chromium browser, Electron app and WebView2 host.
- The tray icon turns **green** when everything is under half your limit, **amber** when something is over half, and **red** when something is over the limit.
- Hover the icon to see the top three apps by GPU memory.
- When an app passes the limit you get a notification. Click it to restart that app's GPU process.
- Right-click the icon to restart any app's GPU process by hand, change the limit (1–8 GB per app), turn on **automatic restarts**, or turn on **Start with Windows**.

Automatic restarts of the same app are at least 10 minutes apart. Chromium turns off hardware acceleration if its GPU process dies too many times in a row, so when an app crosses the limit again inside that window you get a warning instead.

## Install

Download `vram-bloat-killer.exe` from [Releases](../../releases) and run it. There is no installer, and it needs nothing else installed. It is about 400 KB and uses well under 10 MB of RAM.

Settings live in `%APPDATA%\VRAM Bloat Killer\settings.ini`. "Start with Windows" adds a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, and turning it off removes it again.

## Building

```bash
cargo build --release
```

It builds with either Rust Windows toolchain: MSVC (needs the Visual Studio Build Tools) or GNU (`rustup toolchain install stable-x86_64-pc-windows-gnu`, then `cargo +stable-x86_64-pc-windows-gnu build --release`).

To redraw the icons, run `python tools/make_icons.py`. It needs Pillow.

Run the tests with `cargo test -- --include-ignored --nocapture`. The ignored test reads the live counters on your machine and lists every GPU process it finds.

## Limits

- Windows 10 1709 or later. Older versions don't have per-process GPU memory counters.
- It can only restart GPU processes it is allowed to touch. An app running as administrator needs VRAM Bloat Killer to run as administrator too.
- It treats the symptom. If one app balloons over and over, updating your graphics driver is worth trying, and so is turning off that app's animated extras such as live wallpapers and GX mods.

Opera, Opera GX, Chrome, Edge and the other names above are trademarks of their owners. This project is not affiliated with any of them.

## License

MIT © LordBonds
