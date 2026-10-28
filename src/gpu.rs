//! Reads per-process GPU memory from Windows performance counters and finds the
//! Chromium-family GPU processes (browsers, Electron apps, WebView2) among them.

use std::collections::HashMap;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, HANDLE, UNICODE_STRING};
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
    /// pid -> app name if it is a GPU process, None if it is anything else.
    known: HashMap<u32, Option<String>>,
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

    /// Every GPU process holding at least 32 MB, largest first.
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
            .filter(|(_, (s, d))| s + d >= MIN_TRACKED)
            .filter_map(|(pid, (shared, dedicated))| {
                let app = self.known.entry(pid).or_insert_with(|| identify(pid)).clone()?;
                Some(GpuProcess { pid, app, shared, dedicated })
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

fn identify(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let app = if is_gpu_process(h) { image_stem(h) } else { None };
        let _ = CloseHandle(h);
        app.map(|stem| friendly_name(&stem))
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
    let file = path.rsplit('\\').next()?;
    Some(match file.len().checked_sub(4) {
        Some(n) if file[n..].eq_ignore_ascii_case(".exe") => file[..n].to_string(),
        _ => file.to_string(),
    })
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
            println!("{:<12} pid {:<6} shared {:.2} GB  dedicated {:.2} GB", p.app, p.pid, p.shared as f64 / GB, p.dedicated as f64 / GB);
        }
        assert!(!procs.is_empty(), "expected at least one Chromium GPU process running");
    }

    #[test]
    fn names_known_apps() {
        assert_eq!(friendly_name("opera"), "Opera");
        assert_eq!(friendly_name("MSEdge"), "Edge");
        assert_eq!(friendly_name("SomethingElse"), "SomethingElse");
    }
}
