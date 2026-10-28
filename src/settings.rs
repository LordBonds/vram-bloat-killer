//! The two user settings (in %APPDATA%\VRAM Bloat Killer\settings.ini) and the
//! Start with Windows switch (a value under HKCU\...\Run).

use std::path::PathBuf;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

pub const LIMITS_GB: [f64; 6] = [1.0, 2.0, 3.0, 4.0, 6.0, 8.0];
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("VRAM Bloat Killer");

pub struct Settings {
    pub limit_gb: f64,
    pub auto_restart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { limit_gb: 3.0, auto_restart: false }
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Ok(text) = std::fs::read_to_string(file()) else { return s };
        for line in text.lines() {
            match line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
                Some(("limit_gb", v)) => {
                    if let Ok(gb) = v.parse::<f64>() {
                        if gb > 0.0 {
                            s.limit_gb = gb;
                        }
                    }
                }
                Some(("auto_restart", v)) => s.auto_restart = v == "true",
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
        let _ = std::fs::write(path, format!("limit_gb={}\nauto_restart={}\n", self.limit_gb, self.auto_restart));
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
