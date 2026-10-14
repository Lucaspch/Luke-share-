//! Enumeração de fontes de captura: monitores (HMONITOR) e janelas (HWND).

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind")]
pub enum SourceInfo {
    Monitor {
        id: isize,
        name: String,
        width: u32,
        height: u32,
        primary: bool,
        refresh_rate: u32,
    },
    Window {
        id: isize,
        title: String,
        pid: u32,
        exe: String,
        refresh_rate: u32,
    },
}

impl SourceInfo {
    pub fn id(&self) -> isize {
        match self {
            SourceInfo::Monitor { id, .. } | SourceInfo::Window { id, .. } => *id,
        }
    }

    pub fn refresh_rate(&self) -> u32 {
        match self {
            SourceInfo::Monitor { refresh_rate, .. } | SourceInfo::Window { refresh_rate, .. } => *refresh_rate,
        }
    }
}

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use super::SourceInfo;
    use crate::audio::process_tree::{exe_of, snapshot};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    fn get_monitor_refresh_rate(mi: &MONITORINFOEXW) -> u32 {
        let mut dev_mode = DEVMODEW::default();
        dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        let ok = unsafe {
            EnumDisplaySettingsW(
                windows::core::PCWSTR(mi.szDevice.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            )
        };
        if ok.as_bool() {
            let hz = dev_mode.dmDisplayFrequency;
            if hz >= 30 && hz <= 360 {
                return hz;
            }
        }
        60
    }

    fn get_window_refresh_rate(h: HWND) -> u32 {
        let hmon = unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) };
        if hmon.0.is_null() {
            return 60;
        }
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut MONITORINFO) }.as_bool() {
            return get_monitor_refresh_rate(&mi);
        }
        60
    }

    pub fn list_monitors() -> Vec<SourceInfo> {
        unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
            let v = &mut *(lp.0 as *mut Vec<HMONITOR>);
            v.push(h);
            true.into()
        }
        let mut handles: Vec<HMONITOR> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut handles as *mut _ as isize));
        }
        let mut out = Vec::new();
        for (i, h) in handles.iter().enumerate() {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if unsafe { GetMonitorInfoW(*h, &mut mi as *mut _ as *mut MONITORINFO) }.as_bool() {
                let r = mi.monitorInfo.rcMonitor;
                let (w, hh) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
                let primary = mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
                let hz = get_monitor_refresh_rate(&mi);
                out.push(SourceInfo::Monitor {
                    id: h.0 as isize,
                    name: format!("Monitor {}{} — {}x{} @ {}Hz", i + 1, if primary { " (principal)" } else { "" }, w, hh, hz),
                    width: w,
                    height: hh,
                    primary,
                    refresh_rate: hz,
                });
            }
        }
        out
    }

    pub fn list_windows() -> Vec<SourceInfo> {
        unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
            let v = &mut *(lp.0 as *mut Vec<HWND>);
            v.push(h);
            true.into()
        }
        let mut handles: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&mut handles as *mut _ as isize));
        }
        let table = snapshot();
        let me = std::process::id();
        let mut out = Vec::new();
        for h in handles {
            unsafe {
                if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
                    continue;
                }
                let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
                if ex & WS_EX_TOOLWINDOW.0 != 0 {
                    continue;
                }
                let mut cloaked = 0u32;
                let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
                if cloaked != 0 {
                    continue;
                }
                let len = GetWindowTextLengthW(h);
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len as usize + 1];
                let n = GetWindowTextW(h, &mut buf);
                let title = String::from_utf16_lossy(&buf[..n as usize]);
                if title.trim().is_empty() || title == "Program Manager" {
                    continue;
                }
                let mut r = RECT::default();
                if GetWindowRect(h, &mut r).is_err() || r.right - r.left < 32 || r.bottom - r.top < 32 {
                    continue;
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, Some(&mut pid));
                if pid == me {
                    continue;
                }
                let exe = exe_of(&table, pid).unwrap_or("?").to_string();
                let hz = get_window_refresh_rate(h);
                out.push(SourceInfo::Window { id: h.0 as isize, title, pid, exe, refresh_rate: hz });
            }
        }
        out
    }

    pub fn list_sources() -> Vec<SourceInfo> {
        let mut v = list_monitors();
        v.extend(list_windows());
        v
    }

    /// PID dono da janela.
    pub fn window_pid(hwnd: isize) -> u32 {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)) };
        pid
    }
}
