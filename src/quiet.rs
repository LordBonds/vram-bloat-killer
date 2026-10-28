//! Whether now is a moment the user won't notice: no fullscreen game or video,
//! and the app in question isn't the one being looked at.

use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::gpu::GpuProcess;

/// Away this long, the user isn't watching anything.
const AWAY_SECS: u64 = 10 * 60;

/// A fullscreen game, fullscreen video or presentation is running.
pub fn fullscreen() -> bool {
    matches!(
        unsafe { SHQueryUserNotificationState() },
        Ok(QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE)
    )
}

pub fn idle_secs() -> u64 {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if !GetLastInputInfo(&mut info).as_bool() {
            return 0;
        }
        (GetTickCount().wrapping_sub(info.dwTime) / 1000) as u64
    }
}

fn foreground_pid() -> u32 {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
    }
    pid
}

/// Restarting this process now would go unnoticed: nothing is fullscreen and its
/// app isn't in front, or the user has been away for ten minutes.
pub fn unnoticed(p: &GpuProcess) -> bool {
    if fullscreen() {
        return false;
    }
    if idle_secs() >= AWAY_SECS {
        return true;
    }
    // WebView2 draws inside other apps' windows, so "in front" can't be told apart.
    if p.app == "WebView2" {
        return false;
    }
    let front = foreground_pid();
    front != p.owner && front != p.pid
}
