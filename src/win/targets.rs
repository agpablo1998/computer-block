//! Which windows are enforced, and what state they are in.

use crate::core::{WinId, WinState};
use std::collections::HashMap;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetGUIThreadInfo, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, ShowWindowAsync,
    GUITHREADINFO, GUI_INMOVESIZE, GWL_EXSTYLE, GWL_STYLE, GW_OWNER, SW_MAXIMIZE, WS_EX_TOOLWINDOW,
    WS_MAXIMIZEBOX, WS_THICKFRAME,
};

/// (window class, process image name). Both must match.
const TARGETS: &[(&str, &str)] = &[
    ("OpusApp", "winword.exe"),
    ("XLMAIN", "excel.exe"),
    ("PPTFrameClass", "powerpnt.exe"),
    ("Chrome_WidgetWin_1", "chrome.exe"),
];

pub fn hwnd(w: WinId) -> HWND {
    HWND(w.0 as *mut core::ffi::c_void)
}

pub fn id(h: HWND) -> WinId {
    WinId(h.0 as isize)
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: buffer is a local array.
    let n = unsafe { GetClassNameW(h, &mut buf) };
    let n = usize::try_from(n).unwrap_or(0);
    String::from_utf16_lossy(buf.get(..n).unwrap_or_default())
}

fn process_image(pid: u32) -> Option<String> {
    // SAFETY: handle is closed below; buffer and length describe one array.
    unsafe {
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = u32::try_from(buf.len()).unwrap_or(0);
        let r = QueryFullProcessImageNameW(
            proc,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        );
        let _ = CloseHandle(proc);
        r.ok()?;
        let path = String::from_utf16_lossy(buf.get(..usize::try_from(len).ok()?)?);
        path.rsplit('\\').next().map(str::to_ascii_lowercase)
    }
}

/// Classifies windows. Process names are cached per pid; the cache is
/// cleared on every sweep because pids are reused.
#[derive(Debug, Default)]
pub struct Targets {
    image_by_pid: HashMap<u32, Option<String>>,
    /// Extra window class accepted from any process (CI tests only).
    test_class: Option<String>,
}

impl Targets {
    pub fn new() -> Self {
        Self {
            image_by_pid: HashMap::new(),
            test_class: std::env::var("COMPUTER_BLOCK_TEST_TARGET")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }

    pub fn clear_cache(&mut self) {
        self.image_by_pid.clear();
    }

    pub fn pid_of(h: HWND) -> u32 {
        let mut pid = 0u32;
        // SAFETY: out-pointer to a local.
        unsafe {
            GetWindowThreadProcessId(h, Some(&raw mut pid));
        }
        pid
    }

    /// A visible, resizable, maximizable, unowned top-level window of one of
    /// the four apps. Excludes dialogs, menus, tooltips, Chrome's hidden
    /// helper windows and fixed-size popups.
    pub fn is_target(&mut self, h: HWND) -> bool {
        // SAFETY: all calls tolerate stale or invalid handles.
        unsafe {
            if !IsWindow(Some(h)).as_bool() || !IsWindowVisible(h).as_bool() {
                return false;
            }
            if GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                return false;
            }
            let style = low32(GetWindowLongPtrW(h, GWL_STYLE));
            let ex = low32(GetWindowLongPtrW(h, GWL_EXSTYLE));
            if style & WS_MAXIMIZEBOX.0 == 0
                || style & WS_THICKFRAME.0 == 0
                || ex & WS_EX_TOOLWINDOW.0 != 0
            {
                return false;
            }
        }
        let class = class_name(h);
        if self.test_class.as_deref() == Some(class.as_str()) {
            return true;
        }
        let Some(image) = TARGETS.iter().find(|(c, _)| *c == class).map(|(_, i)| *i) else {
            return false;
        };
        let pid = Self::pid_of(h);
        let actual = self
            .image_by_pid
            .entry(pid)
            .or_insert_with(|| process_image(pid));
        actual.as_deref() == Some(image)
    }

    pub fn enumerate(&mut self) -> Vec<WinId> {
        let mut all: Vec<HWND> = Vec::new();
        // SAFETY: the callback only pushes into the Vec passed via lparam,
        // which outlives the synchronous EnumWindows call.
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(std::ptr::from_mut(&mut all) as isize));
        }
        all.into_iter()
            .filter(|h| self.is_target(*h))
            .map(id)
            .collect()
    }
}

/// Window styles live in the low 32 bits.
fn low32(v: isize) -> u32 {
    u32::try_from(v & 0xFFFF_FFFF).unwrap_or(0)
}

unsafe extern "system" fn collect(h: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the &mut Vec<HWND> from `enumerate`.
    let all = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    all.push(h);
    BOOL(1)
}

fn covers_monitor(h: HWND) -> bool {
    // SAFETY: out-pointers to locals; MONITORINFO size is set.
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(h, &raw mut r).is_err() {
            return false;
        }
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0),
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &raw mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

/// Whether the user is dragging or resizing `h` right now. Asked of Windows
/// directly instead of tracked from start/end events: a lost end event must
/// not leave a window marked as moving forever.
fn in_move_size(h: HWND) -> bool {
    let mut info = GUITHREADINFO {
        cbSize: u32::try_from(std::mem::size_of::<GUITHREADINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: out-pointer to a local with cbSize set.
    unsafe {
        let thread = GetWindowThreadProcessId(h, None);
        GetGUIThreadInfo(thread, &raw mut info).is_ok()
            && info.flags.contains(GUI_INMOVESIZE)
            && info.hwndMoveSize == h
    }
}

/// Current state of a target window.
pub fn state(w: WinId) -> WinState {
    let h = hwnd(w);
    // SAFETY: all calls tolerate stale handles.
    unsafe {
        if !IsWindow(Some(h)).as_bool() || !IsWindowVisible(h).as_bool() {
            WinState::Closed
        } else if in_move_size(h) {
            WinState::Moving
        } else if IsIconic(h).as_bool() {
            WinState::Minimized
        } else if IsZoomed(h).as_bool() {
            WinState::Maximized
        } else if covers_monitor(h) {
            WinState::Fullscreen
        } else {
            WinState::Normal
        }
    }
}

/// Posted to the app's own thread: never blocks on a hung app.
pub fn maximize(w: WinId) -> bool {
    // SAFETY: tolerates stale handles.
    unsafe { ShowWindowAsync(hwnd(w), SW_MAXIMIZE).as_bool() }
}
