//! Reads per-process GPU memory from Windows performance counters for every process,
//! and marks the Chromium-family GPU processes (browsers, Electron apps, WebView2):
//! the only ones that can be restarted without closing the app.

use std::collections::HashMap;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, HANDLE, HWND, LPARAM, STILL_ACTIVE, TRUE, UNICODE_STRING, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, GW_OWNER, WM_CLOSE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::*;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32,
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
    /// The process whose windows are this app: the browser for a GPU process, else itself.
    pub owner: u32,
    /// Part of Windows, a graphics driver's tool, or unidentifiable: never closed.
    pub protected: bool,
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
    /// Looked up once per process.
    known: HashMap<u32, Identity>,
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
                let Identity { app, restartable, owner, protected } = self.known.entry(pid).or_insert_with(|| identify(pid)).clone();
                GpuProcess { pid, app, shared, dedicated, restartable, owner, protected }
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

/// Closes a whole app. Checks the pid still belongs to `app` so a recycled pid is
/// never hit, and refuses anything protected.
pub fn close_app(pid: u32, app: &str) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let same = image_path(h).is_some_and(|path| !protected(&path) && friendly_name(stem(&path)) == app);
        let ok = same && TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

/// The graceful way: sends WM_CLOSE to the app's top-level windows, as if you
/// clicked their X, so it can save or ask you to. Returns how many windows were asked.
pub fn ask_to_close(pid: u32) -> usize {
    let windows = windows_of(pid);
    for hwnd in &windows {
        unsafe {
            let _ = PostMessageW(*hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    windows.len()
}

/// The app's visible top-level windows: what a click on their X would close.
pub fn windows_of(pid: u32) -> Vec<HWND> {
    struct Search {
        pid: u32,
        found: Vec<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        let mut owner_pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
        let top_level = GetWindow(hwnd, GW_OWNER).map_or(true, |o| o.is_invalid());
        if owner_pid == search.pid && top_level && IsWindowVisible(hwnd).as_bool() {
            search.found.push(hwnd);
        }
        TRUE
    }
    let mut search = Search { pid, found: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

/// Still running and still the same app (pids get reused).
pub fn is_running(pid: u32, app: &str) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let same = image_path(h).is_some_and(|path| friendly_name(stem(&path)) == app);
        let _ = CloseHandle(h);
        alive && same
    }
}

/// Never closed, by auto-kill or the emergency wipe: everything that runs from the
/// Windows folder (Explorer, the Desktop Window Manager, the touch keyboard...), the
/// graphics drivers' own tools, and this app.
fn protected(path: &str) -> bool {
    const DRIVER_TOOLS: [&str; 14] = [
        "amdrsserv", "amdrssrcext", "radeonsoftware", "amdow", "atieclxx", "atiesrxx", "nvcontainer",
        "nvidia share", "nvidia overlay", "nvdisplay.container", "igfxem", "igfxtray", "igcc", "vram-bloat-killer",
    ];
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into()).to_ascii_lowercase();
    let lower = path.to_ascii_lowercase();
    lower.starts_with(&(windir + "\\")) || DRIVER_TOOLS.contains(&stem(&lower))
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
fn identify(pid: u32) -> Identity {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            let app = snapshot_entry(pid).map_or_else(|| format!("Process {pid}"), |(name, _)| name);
            return Identity { app, restartable: false, owner: pid, protected: true };
        };
        let restartable = is_gpu_process(h);
        let path = image_path(h);
        let _ = CloseHandle(h);
        let app = path.as_deref().map_or_else(|| format!("Process {pid}"), |p| friendly_name(stem(p)));
        // A GPU process's windows belong to the browser that launched it.
        let owner = if restartable { snapshot_entry(pid).map_or(pid, |(_, parent)| parent) } else { pid };
        Identity { app, restartable, owner, protected: path.as_deref().map_or(true, protected) }
    }
}

#[derive(Clone)]
struct Identity {
    app: String,
    restartable: bool,
    owner: u32,
    protected: bool,
}

/// (friendly name, parent pid) from the system process list.
unsafe fn snapshot_entry(pid: u32) -> Option<(String, u32)> {
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
    let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut found = None;
    let mut more = Process32FirstW(snap, &mut entry).is_ok();
    while more {
        if entry.th32ProcessID == pid {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let file = String::from_utf16_lossy(&entry.szExeFile[..len]);
            found = Some((friendly_name(stem(&file)), entry.th32ParentProcessID));
            break;
        }
        more = Process32NextW(snap, &mut entry).is_ok();
    }
    let _ = CloseHandle(snap);
    found
}

/// "C:\...\Opera.exe" or "Opera.exe" -> "Opera"
fn stem(path: &str) -> &str {
    let file = path.rsplit('\\').next().unwrap_or(path);
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

unsafe fn image_path(h: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size).ok()?;
    Some(String::from_utf16_lossy(&buf[..size as usize]))
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

    /// A copy of ping.exe outside the Windows folder: Windows' own copy is protected.
    fn ping_copy() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("vbk-test");
        let _ = std::fs::create_dir_all(&dir);
        let exe = dir.join("pingcopy.exe");
        if !exe.exists() {
            let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
            std::fs::copy(format!("{windir}\\System32\\PING.EXE"), &exe).expect("copy ping");
        }
        exe
    }

    fn long_ping(exe: &std::path::Path, flags: u32) -> std::process::Child {
        use std::os::windows::process::CommandExt;
        std::process::Command::new(exe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .creation_flags(flags)
            .spawn()
            .expect("ping")
    }

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    fn wait() {
        std::thread::sleep(std::time::Duration::from_millis(600));
    }

    /// Live: closes an app it started, and refuses when the name doesn't match.
    /// (Not Notepad: on Windows 11 notepad.exe is a launcher that exits at once.)
    #[test]
    #[ignore]
    fn closes_only_the_named_app() {
        let mut child = long_ping(&ping_copy(), CREATE_NO_WINDOW);
        wait();
        assert!(!close_app(child.id(), "Something Else"));
        assert!(child.try_wait().unwrap().is_none(), "wrong name must not close it");
        assert!(close_app(child.id(), "pingcopy"));
        wait();
        assert!(child.try_wait().unwrap().is_some(), "it should have been closed");
    }

    /// Live: the same program run from the Windows folder is never closed.
    #[test]
    #[ignore]
    fn never_closes_anything_from_the_windows_folder() {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        let mut child = long_ping(std::path::Path::new(&format!("{windir}\\System32\\PING.EXE")), CREATE_NO_WINDOW);
        wait();
        assert!(!close_app(child.id(), "PING"));
        assert!(child.try_wait().unwrap().is_none(), "a Windows program must not be closed");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Live: with no window there is nothing to ask, so graceful close reports zero.
    #[test]
    #[ignore]
    fn graceful_close_needs_a_window() {
        let mut child = long_ping(&ping_copy(), CREATE_NO_WINDOW);
        wait();
        assert!(is_running(child.id(), "pingcopy"));
        assert_eq!(ask_to_close(child.id()), 0);
        let _ = child.kill();
        let _ = child.wait();
        assert!(!is_running(child.id(), "pingcopy"));
    }

    /// Live: an app with a window is asked like clicking its X, and closes itself.
    #[test]
    #[ignore]
    fn graceful_close_asks_a_window_and_it_closes() {
        let mut child = long_ping(&ping_copy(), CREATE_NEW_CONSOLE);
        std::thread::sleep(std::time::Duration::from_millis(900));
        assert!(ask_to_close(child.id()) >= 1, "its console window should be found");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert!(child.try_wait().unwrap().is_some(), "it should have closed when asked");
    }

    #[test]
    fn protects_windows_and_drivers() {
        assert!(protected("C:\\Windows\\System32\\dwm.exe"));
        assert!(protected("C:\\WINDOWS\\explorer.exe"));
        assert!(protected("C:\\Windows\\SystemApps\\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\\TextInputHost.exe"));
        assert!(protected("C:\\Program Files\\AMD\\CNext\\CNext\\RadeonSoftware.exe"));
        assert!(!protected("C:\\Users\\me\\AppData\\Local\\Programs\\Opera GX\\opera.exe"));
        assert!(!protected("D:\\Games\\minecraft\\runtime\\bin\\javaw.exe"));
        assert!(!protected("C:\\Windowsill\\app.exe"), "only the Windows folder itself");
    }
}