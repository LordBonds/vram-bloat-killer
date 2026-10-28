//! Reads per-process GPU memory from Windows performance counters for every process,
//! and marks the Chromium-family GPU processes (browsers, Electron apps, WebView2):
//! the only ones that can be restarted without closing the app.

use std::collections::HashMap;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, HANDLE, UNICODE_STRING};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::*;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

pub const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Processes below this are not worth opening every tick.
const MIN_TRACKED: u64 = 32 * 1024 * 1024;
const PROCESS_COMMAND_LINE_INFORMATION: PROCESSINFOCLASS = PROCESSINFOCLASS(60);
const PDH_MORE_DATA: u32 = 0x8000_07D2;

#[derive(Clone)]
pub struct GpuProcess {
    pub pid: u32,
    pub app: String,
    /// System RAM the GPU borrowed for this process. Task Manager's Memory column never shows it.
    pub shared: u64,
    /// Memory on the graphics card itself.
    pub dedicated: u64,
    /// A Chromium/Electron GPU process: its app relaunches it, so killing it is safe.
    pub restartable: bool,
}

impl GpuProcess {
    pub fn total(&self) -> u64 {
        self.shared + self.dedicated
    }

    pub fn total_gb(&self) -> f64 {
        self.total() as f64 / GB
    }
}

pub struct Sampler {
    query: isize,
    shared: isize,
    dedicated: isize,
    /// pid -> (app name, restartable), looked up once per process.
    known: HashMap<u32, (String, bool)>,
}

impl Sampler {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let mut query = 0;
            pdh(PdhOpenQueryW(PCWSTR::null(), 0, &mut query), "open query")?;
            let mut shared = 0;
            pdh(
                PdhAddEnglishCounterW(query, w!("\\GPU Process Memory(*)\\Shared Usage"), 0, &mut shared),
                "add Shared Usage counter",
            )?;
            let mut dedicated = 0;
            pdh(
                PdhAddEnglishCounterW(query, w!("\\GPU Process Memory(*)\\Dedicated Usage"), 0, &mut dedicated),
                "add Dedicated Usage counter",
            )?;
            Ok(Self { query, shared, dedicated, known: HashMap::new() })
        }
    }

    /// Every process holding at least 32 MB of GPU memory, largest first.
    pub fn sample(&mut self) -> Vec<GpuProcess> {
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return Vec::new();
        }
        // An instance is one process on one adapter, so a process on two GPUs appears twice.
        let mut by_pid: HashMap<u32, (u64, u64)> = HashMap::new();
        for (name, bytes) in unsafe { read_counter(self.shared) } {
            if let Some(pid) = instance_pid(&name) {
                by_pid.entry(pid).or_default().0 += bytes;
            }
        }
        for (name, bytes) in unsafe { read_counter(self.dedicated) } {
            if let Some(pid) = instance_pid(&name) {
                by_pid.entry(pid).or_default().1 += bytes;
            }
        }

        self.known.retain(|pid, _| by_pid.contains_key(pid));
        let mut out: Vec<GpuProcess> = by_pid
            .into_iter()
            .filter(|(pid, (s, d))| *pid != 0 && s + d >= MIN_TRACKED)
            .map(|(pid, (shared, dedicated))| {
                let (app, restartable) = self.known.entry(pid).or_insert_with(|| identify(pid)).clone();
                GpuProcess { pid, app, shared, dedicated, restartable }
            })
            .collect();
        out.sort_by(|a, b| b.total().cmp(&a.total()));
        out
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// Kills a GPU process. Chromium-based apps relaunch it within a second and keep
/// every tab and window. Re-checks the command line first so a recycled pid is never hit.
pub fn restart(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let ok = is_gpu_process(h) && TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

/// Closes a whole app. Checks the pid still belongs to `app` so a recycled pid is never hit.
pub fn close_app(pid: u32, app: &str) -> bool {
    if !closable(app) {
        return false;
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let same = image_stem(h).is_some_and(|stem| friendly_name(&stem) == app);
        let ok = same && TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

/// Windows' own processes, and anything that couldn't be identified, are never closed.
pub fn closable(app: &str) -> bool {
    const NEVER: [&str; 12] = [
        "desktop window manager", "explorer", "csrss", "winlogon", "lsass", "services", "smss",
        "svchost", "system", "fontdrvhost", "searchhost", "vram-bloat-killer",
    ];
    !app.starts_with("Process ") && !NEVER.contains(&app.to_ascii_lowercase().as_str())
}

fn pdh(status: u32, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("could not {what} (PDH error 0x{status:08X})"))
    }
}

unsafe fn read_counter(counter: isize) -> Vec<(String, u64)> {
    let mut size = 0u32;
    let mut count = 0u32;
    if PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, None) != PDH_MORE_DATA {
        return Vec::new();
    }
    // u64 backing keeps the items aligned.
    let mut buf = vec![0u64; size as usize / 8 + 1];
    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
    if PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, Some(items)) != 0 {
        return Vec::new();
    }
    (0..count as usize)
        .filter_map(|i| {
            let item = &*items.add(i);
            // 0 = valid data, 1 = new data; anything else is an error status.
            if item.FmtValue.CStatus > 1 {
                return None;
            }
            let name = item.szName.to_string().ok()?;
            Some((name, item.FmtValue.Anonymous.largeValue.max(0) as u64))
        })
        .collect()
}

/// "pid_16148_luid_0x00000000_0x0000DE16_phys_0" -> 16148
fn instance_pid(name: &str) -> Option<u32> {
    name.strip_prefix("pid_")?.split('_').next()?.parse().ok()
}

/// Protected and elevated processes can't be opened; they are named from the
/// process list instead and are never restartable.
fn identify(pid: u32) -> (String, bool) {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return (snapshot_name(pid).unwrap_or_else(|| format!("Process {pid}")), false);
        };
        let restartable = is_gpu_process(h);
        let app = image_stem(h).map(|stem| friendly_name(&stem)).unwrap_or_else(|| format!("Process {pid}"));
        let _ = CloseHandle(h);
        (app, restartable)
    }
}

unsafe fn snapshot_name(pid: u32) -> Option<String> {
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
    let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut found = None;
    let mut more = Process32FirstW(snap, &mut entry).is_ok();
    while more {
        if entry.th32ProcessID == pid {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let file = String::from_utf16_lossy(&entry.szExeFile[..len]);
            found = Some(friendly_name(strip_exe(&file)));
            break;
        }
        more = Process32NextW(snap, &mut entry).is_ok();
    }
    let _ = CloseHandle(snap);
    found
}

fn strip_exe(file: &str) -> &str {
    match file.len().checked_sub(4) {
        Some(n) if file.is_char_boundary(n) && file[n..].eq_ignore_ascii_case(".exe") => &file[..n],
        _ => file,
    }
}

unsafe fn is_gpu_process(h: HANDLE) -> bool {
    command_line(h).is_some_and(|c| c.contains("--type=gpu-process"))
}

unsafe fn command_line(h: HANDLE) -> Option<String> {
    let mut len = 0u32;
    let _ = NtQueryInformationProcess(h, PROCESS_COMMAND_LINE_INFORMATION, std::ptr::null_mut(), 0, &mut len);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u64; len as usize / 8 + 1];
    NtQueryInformationProcess(h, PROCESS_COMMAND_LINE_INFORMATION, buf.as_mut_ptr().cast(), len, &mut len)
        .ok()
        .ok()?;
    let us = &*(buf.as_ptr() as *const UNICODE_STRING);
    if us.Buffer.is_null() {
        return None;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(us.Buffer.0, us.Length as usize / 2)))
}

unsafe fn image_stem(h: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size).ok()?;
    let path = String::from_utf16_lossy(&buf[..size as usize]);
    Some(strip_exe(path.rsplit('\\').next()?).to_string())
}

fn friendly_name(stem: &str) -> String {
    match stem.to_ascii_lowercase().as_str() {
        "opera" => "Opera",
        "chrome" => "Chrome",
        "msedge" => "Edge",
        "brave" => "Brave",
        "vivaldi" => "Vivaldi",
        "arc" => "Arc",
        "discord" => "Discord",
        "spotify" => "Spotify",
        "slack" => "Slack",
        "teams" | "ms-teams" => "Teams",
        "code" => "VS Code",
        "msedgewebview2" => "WebView2",
        "steamwebhelper" => "Steam",
        "claude" => "Claude",
        "java" | "javaw" => "Java",
        "dwm" => "Desktop Window Manager",
        _ => return stem.to_string(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_instance_pid() {
        assert_eq!(instance_pid("pid_16148_luid_0x00000000_0x0000DE16_phys_0"), Some(16148));
        assert_eq!(instance_pid("luid_0x00000000_0x0000DE16_phys_0"), None);
        assert_eq!(instance_pid("pid_x_luid"), None);
    }

    /// Live check against this machine: cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn samples_live_gpu_processes() {
        let mut s = Sampler::new().expect("GPU counters");
        let procs = s.sample();
        for p in &procs {
            let kind = if p.restartable { "restartable" } else { "watch only" };
            println!("{:<24} pid {:<6} shared {:.2} GB  dedicated {:.2} GB  {kind}", p.app, p.pid, p.shared as f64 / GB, p.dedicated as f64 / GB);
        }
        assert!(procs.iter().any(|p| p.restartable), "expected at least one Chromium GPU process running");
        assert!(procs.iter().any(|p| !p.restartable), "expected at least one other process using the GPU");
    }

    #[test]
    fn names_known_apps() {
        assert_eq!(friendly_name("opera"), "Opera");
        assert_eq!(friendly_name("MSEdge"), "Edge");
        assert_eq!(friendly_name("SomethingElse"), "SomethingElse");
    }

    /// Live: closes a long ping it started, and refuses when the name doesn't match.
    /// (Not Notepad: on Windows 11 notepad.exe is a launcher that exits at once.)
    #[test]
    #[ignore]
    fn closes_only_the_named_app() {
        let mut child = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ping");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!close_app(child.id(), "Something Else"));
        assert!(child.try_wait().unwrap().is_none(), "wrong name must not close it");
        assert!(close_app(child.id(), "PING"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(child.try_wait().unwrap().is_some(), "ping should have been closed");
    }

    #[test]
    fn never_closes_windows_itself() {
        assert!(!closable("Desktop Window Manager"));
        assert!(!closable("explorer"));
        assert!(!closable("Process 1234"));
        assert!(closable("Java"));
        assert!(closable("Blender"));
    }
}
