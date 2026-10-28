//! User settings (in %APPDATA%\VRAM Bloat Killer\settings.ini) and the
//! Start with Windows switch (a value under HKCU\...\Run).

use std::path::PathBuf;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

pub const WARN_GB: [f64; 6] = [1.0, 2.0, 3.0, 4.0, 6.0, 8.0];
pub const GROWTH_GB: [f64; 4] = [1.0, 2.0, 3.0, 4.0];
pub const KILL_GB: [f64; 8] = [2.0, 3.0, 4.0, 6.0, 8.0, 10.0, 12.0, 16.0];
/// Under five minutes, Chromium may give up on hardware acceleration.
pub const COOLDOWN_MIN: [u64; 4] = [5, 10, 30, 60];

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("VRAM Bloat Killer");

pub struct Settings {
    /// Notify above this (browsers: total GPU memory; other apps: borrowed RAM).
    pub warn_gb: f64,
    /// Notify when an app climbs this much in 30 minutes without dropping.
    pub growth_gb: f64,
    /// Kill above this much total GPU memory; None is off.
    pub kill_gb: Option<f64>,
    /// Let auto-kill close non-browser apps entirely.
    pub kill_apps: bool,
    /// Minimum minutes between auto-kills of the same app.
    pub cooldown_min: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { warn_gb: 3.0, growth_gb: 2.0, kill_gb: None, kill_apps: false, cooldown_min: 10 }
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Ok(text) = std::fs::read_to_string(file()) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) else { continue };
            let gb = v.parse::<f64>().ok().filter(|g| *g >= 0.0);
            match k {
                "warn_gb" => s.warn_gb = gb.filter(|g| *g > 0.0).unwrap_or(s.warn_gb),
                "growth_gb" => s.growth_gb = gb.filter(|g| *g > 0.0).unwrap_or(s.growth_gb),
                "kill_gb" => s.kill_gb = gb.filter(|g| *g > 0.0),
                "kill_apps" => s.kill_apps = v == "true",
                "cooldown_min" => s.cooldown_min = v.parse().unwrap_or(s.cooldown_min).max(1),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let path = file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "warn_gb={}\ngrowth_gb={}\nkill_gb={}\nkill_apps={}\ncooldown_min={}\n",
            self.warn_gb,
            self.growth_gb,
            self.kill_gb.unwrap_or(0.0),
            self.kill_apps,
            self.cooldown_min
        );
        let _ = std::fs::write(path, text);
    }
}

fn file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    base.join("VRAM Bloat Killer").join("settings.ini")
}

pub fn starts_with_windows() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

/// Points the Run entry at this exe, or removes it.
pub fn set_starts_with_windows(on: bool) {
    unsafe {
        if !on {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let cmd: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(Some(0)).collect();
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ.0,
            Some(cmd.as_ptr().cast()),
            (cmd.len() * 2) as u32,
        );
    }
}
