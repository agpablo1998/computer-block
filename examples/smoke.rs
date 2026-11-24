//! End-to-end smoke test on a real Windows desktop (run by CI).
//!
//! `cargo run --release --example smoke -- path\to\computer-block.exe`
//!
//! Installs the enforcer into a throw-away %LOCALAPPDATA%, opens its own
//! test window (registered as an extra target through
//! `COMPUTER_BLOCK_TEST_TARGET`), and checks behaviour from the outside:
//! re-maximizing, fullscreen respect, the fight-loop guard, policy and
//! printer restore, the admin pause, resource use, and clean uninstall.
#![allow(
    unsafe_code,
    clippy::too_many_lines,
    clippy::cast_ptr_alignment,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[cfg(not(windows))]
fn main() {
    eprintln!("the smoke test runs on Windows only");
}

#[cfg(windows)]
fn main() {
    std::process::exit(imp::run());
}

#[cfg(windows)]
mod imp {
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};
    use windows::core::{w, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, FILETIME, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::Graphics::Printing::{
        EnumPrintersW, GetDefaultPrinterW, SetDefaultPrinterW, PRINTER_ENUM_CONNECTIONS,
        PRINTER_ENUM_LOCAL, PRINTER_INFO_4W,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, HKEY_CURRENT_USER, RRF_RT_ANY,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_VM_READ,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY, VK_CONTROL, VK_F12, VK_MENU, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetMessageW,
        GetWindowThreadProcessId, IsIconic, IsZoomed, PostMessageW, RegisterClassW,
        SetWindowPlacement, ShowWindow, TranslateMessage, MSG, SIZE_MAXIMIZED, SW_MINIMIZE,
        SW_RESTORE, SW_SHOWNORMAL, WINDOWPLACEMENT, WINDOW_EX_STYLE, WM_APP, WM_SIZE, WNDCLASSW,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    const TEST_CLASS: PCWSTR = w!("ComputerBlockTestWindow");
    const POLICY_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer");
    const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    const WM_FIGHT_BACK: u32 = WM_APP + 7;

    static FIGHT: AtomicBool = AtomicBool::new(false);
    static MAXIMIZES: AtomicU32 = AtomicU32::new(0);

    struct Report {
        failures: u32,
    }

    impl Report {
        fn check(&mut self, ok: bool, what: &str) {
            println!("{} {what}", if ok { "PASS" } else { "FAIL" });
            if !ok {
                self.failures += 1;
            }
        }
    }

    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        cond()
    }

    fn holds_for(duration: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < duration {
            if !cond() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    unsafe extern "system" fn test_wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_SIZE && wp.0 == SIZE_MAXIMIZED as usize {
            MAXIMIZES.fetch_add(1, Ordering::SeqCst);
            if FIGHT.load(Ordering::SeqCst) {
                let _ = unsafe { PostMessageW(Some(h), WM_FIGHT_BACK, WPARAM(0), LPARAM(0)) };
            }
        }
        if msg == WM_FIGHT_BACK {
            let _ = unsafe { ShowWindow(h, SW_RESTORE) };
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(h, msg, wp, lp) }
    }

    fn spawn_test_window() -> HWND {
        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        std::thread::spawn(move || unsafe {
            let inst = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW {
                lpfnWndProc: Some(test_wndproc),
                hInstance: inst.into(),
                lpszClassName: TEST_CLASS,
                ..Default::default()
            };
            RegisterClassW(&raw const class);
            let h = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                TEST_CLASS,
                w!("computer-block smoke test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                500,
                300,
                None,
                None,
                Some(inst.into()),
                None,
            )
            .unwrap();
            tx.send(h.0 as isize).unwrap();
            let mut msg = MSG::default();
            while GetMessageW(&raw mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        });
        HWND(rx.recv().unwrap() as *mut core::ffi::c_void)
    }

    fn zoomed(h: HWND) -> bool {
        unsafe { IsZoomed(h).as_bool() }
    }

    fn iconic(h: HWND) -> bool {
        unsafe { IsIconic(h).as_bool() }
    }

    fn reg_exists(key: PCWSTR, name: PCWSTR) -> bool {
        unsafe { RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_ANY, None, None, None).is_ok() }
    }

    fn reg_delete(key: PCWSTR, name: PCWSTR) {
        unsafe {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, name);
        }
    }

    fn enforcer() -> Option<HWND> {
        unsafe { FindWindowW(w!("ComputerBlockHidden"), PCWSTR::null()) }
            .ok()
            .filter(|h| !h.is_invalid())
    }

    fn press_hotkey() {
        let key = |vk: VIRTUAL_KEY, up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    ..Default::default()
                },
            },
        };
        let inputs = [
            key(VK_CONTROL, false),
            key(VK_MENU, false),
            key(VK_SHIFT, false),
            key(VK_F12, false),
            key(VK_F12, true),
            key(VK_SHIFT, true),
            key(VK_MENU, true),
            key(VK_CONTROL, true),
        ];
        let sent = unsafe {
            SendInput(
                &inputs,
                i32::try_from(std::mem::size_of::<INPUT>()).unwrap(),
            )
        };
        println!(
            "info: SendInput injected {sent} of {} key events",
            inputs.len()
        );
    }

    fn printers() -> Vec<String> {
        unsafe {
            let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
            let (mut needed, mut count) = (0u32, 0u32);
            let _ = EnumPrintersW(
                flags,
                PCWSTR::null(),
                4,
                None,
                &raw mut needed,
                &raw mut count,
            );
            if needed == 0 {
                return Vec::new();
            }
            // u64 storage keeps the PRINTER_INFO_4W records 8-byte aligned.
            let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
            let buf =
                std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), needed as usize);
            if EnumPrintersW(
                flags,
                PCWSTR::null(),
                4,
                Some(buf),
                &raw mut needed,
                &raw mut count,
            )
            .is_err()
            {
                return Vec::new();
            }
            let infos = std::slice::from_raw_parts(
                storage.as_ptr().cast::<PRINTER_INFO_4W>(),
                count as usize,
            );
            infos
                .iter()
                .filter_map(|i| i.pPrinterName.to_string().ok())
                .collect()
        }
    }

    fn default_printer() -> Option<String> {
        unsafe {
            let mut len = 0u32;
            let _ = GetDefaultPrinterW(None, &raw mut len);
            let mut buf = vec![0u16; len as usize];
            if len == 0
                || !GetDefaultPrinterW(Some(PWSTR(buf.as_mut_ptr())), &raw mut len).as_bool()
            {
                return None;
            }
            Some(String::from_utf16_lossy(
                &buf[..buf.iter().position(|c| *c == 0).unwrap_or(buf.len())],
            ))
        }
    }

    fn set_default_printer(name: &str) {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        unsafe {
            let _ = SetDefaultPrinterW(PCWSTR(wide.as_ptr()));
        }
    }

    fn filetime_ms(t: FILETIME) -> u64 {
        ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) / 10_000
    }

    /// (CPU milliseconds, private bytes, working set bytes) of a process.
    /// Private bytes is what Task Manager's Memory column shows; the
    /// working set also counts pages of shared Windows DLLs.
    fn usage(proc: HANDLE) -> (u64, usize, usize) {
        unsafe {
            let (mut c, mut e, mut k, mut u) = Default::default();
            let _ = GetProcessTimes(proc, &raw mut c, &raw mut e, &raw mut k, &raw mut u);
            let mut mem = PROCESS_MEMORY_COUNTERS_EX::default();
            let _ = GetProcessMemoryInfo(
                proc,
                (&raw mut mem).cast::<PROCESS_MEMORY_COUNTERS>(),
                u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap(),
            );
            (
                filetime_ms(k) + filetime_ms(u),
                mem.PrivateUsage,
                mem.WorkingSetSize,
            )
        }
    }

    pub fn run() -> i32 {
        let exe = std::env::args()
            .nth(1)
            .expect("usage: smoke <computer-block.exe>");
        let tmp = std::env::temp_dir().join(format!("cb-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Inherited by the enforcer.
        std::env::set_var("LOCALAPPDATA", &tmp);
        std::env::set_var("COMPUTER_BLOCK_TEST_TARGET", "ComputerBlockTestWindow");
        std::env::set_var("COMPUTER_BLOCK_TICK_MS", "2000");

        let mut r = Report { failures: 0 };
        let sec = Duration::from_secs;

        let printers_before = printers();
        println!(
            "info: printers: {printers_before:?}, default: {:?}",
            default_printer()
        );

        let win = spawn_test_window();
        r.check(!zoomed(win), "test window starts restored");

        let setup = Command::new(&exe)
            .args(["--setup", "--quiet"])
            .status()
            .unwrap();
        r.check(setup.success(), "--setup exits 0");
        r.check(
            wait_until(sec(5), || enforcer().is_some()),
            "enforcer starts hidden",
        );
        let Some(enf) = enforcer() else {
            println!("FAIL enforcer not running; log follows");
            println!(
                "{}",
                std::fs::read_to_string(tmp.join("ComputerBlock").join("log.txt"))
                    .unwrap_or_default()
            );
            return 1;
        };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(enf, Some(&raw mut pid)) };
        let proc = unsafe {
            OpenProcess(
                PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                false,
                pid,
            )
            .unwrap()
        };

        r.check(
            wait_until(sec(5), || zoomed(win)),
            "start-up sweep maximizes an existing window",
        );

        unsafe { ShowWindow(win, SW_MINIMIZE).ok().ok() };
        r.check(
            wait_until(sec(2), || zoomed(win) && !iconic(win)),
            "minimized window comes back maximized",
        );

        unsafe { ShowWindow(win, SW_RESTORE).ok().ok() };
        r.check(
            wait_until(sec(2), || zoomed(win)),
            "restored (un-maximized) window is maximized again",
        );

        // Fullscreen: restored to exactly the monitor size, in one call.
        unsafe {
            let mut mi = MONITORINFO {
                cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap(),
                ..Default::default()
            };
            let _ = GetMonitorInfoW(
                MonitorFromWindow(win, MONITOR_DEFAULTTONEAREST),
                &raw mut mi,
            );
            let wp = WINDOWPLACEMENT {
                length: u32::try_from(std::mem::size_of::<WINDOWPLACEMENT>()).unwrap(),
                showCmd: SW_SHOWNORMAL.0 as u32,
                rcNormalPosition: RECT { ..mi.rcMonitor },
                ..Default::default()
            };
            let _ = SetWindowPlacement(win, &raw const wp);
        }
        r.check(
            holds_for(sec(3), || !zoomed(win) && !iconic(win)),
            "fullscreen window is left alone",
        );

        // Fight loop: the window un-maximizes itself every time. First give
        // it a small restore size again: after the fullscreen check its
        // restore size is the whole monitor, which counts as fullscreen.
        unsafe {
            let wp = WINDOWPLACEMENT {
                length: u32::try_from(std::mem::size_of::<WINDOWPLACEMENT>()).unwrap(),
                showCmd: SW_SHOWNORMAL.0 as u32,
                rcNormalPosition: RECT {
                    left: 100,
                    top: 100,
                    right: 600,
                    bottom: 400,
                },
                ..Default::default()
            };
            let _ = SetWindowPlacement(win, &raw const wp);
        }
        r.check(
            wait_until(sec(2), || zoomed(win)),
            "small restored window is maximized again",
        );
        std::thread::sleep(Duration::from_millis(300));
        FIGHT.store(true, Ordering::SeqCst);
        MAXIMIZES.store(0, Ordering::SeqCst);
        unsafe { ShowWindow(win, SW_MINIMIZE).ok().ok() };
        std::thread::sleep(Duration::from_millis(1500));
        FIGHT.store(false, Ordering::SeqCst);
        let fights = MAXIMIZES.load(Ordering::SeqCst);
        println!("info: corrections during 1.5 s fight: {fights}");
        r.check(
            (1..=6).contains(&fights),
            "fight-loop guard limits corrections",
        );
        r.check(
            wait_until(sec(4), || zoomed(win)),
            "after the fight, the next sweep maximizes again",
        );

        r.check(
            reg_exists(POLICY_KEY, w!("NoControlPanel")),
            "Settings lock written",
        );
        r.check(
            reg_exists(RUN_KEY, w!("ComputerBlock")),
            "auto-start entry written",
        );
        reg_delete(POLICY_KEY, w!("NoControlPanel"));
        r.check(
            wait_until(sec(2), || reg_exists(POLICY_KEY, w!("NoControlPanel"))),
            "deleted lock is restored",
        );

        match (default_printer(), printers_before.len()) {
            (Some(def), n) if n >= 2 => {
                let other = printers_before.iter().find(|p| **p != def).unwrap().clone();
                set_default_printer(&other);
                r.check(
                    wait_until(sec(3), || {
                        default_printer().as_deref() == Some(def.as_str())
                    }),
                    "default printer change is undone",
                );
            }
            _ => println!("SKIP default printer (needs 2 printers)"),
        }

        // Admin pause through the real hotkey.
        press_hotkey();
        let paused = wait_until(sec(2), || !reg_exists(POLICY_KEY, w!("NoControlPanel")));
        r.check(paused, "admin hotkey pauses and lifts the Settings lock");
        if paused {
            unsafe { ShowWindow(win, SW_MINIMIZE).ok().ok() };
            r.check(
                holds_for(sec(3), || iconic(win)),
                "while paused, a minimized window stays minimized",
            );
            press_hotkey();
            r.check(
                wait_until(sec(2), || reg_exists(POLICY_KEY, w!("NoControlPanel"))),
                "hotkey again resumes, lock back",
            );
            r.check(
                wait_until(sec(2), || zoomed(win)),
                "on resume, the window is maximized",
            );
        }

        // Resource use while idle (only the 2 s test tick wakes it).
        let (cpu0, _, _) = usage(proc);
        std::thread::sleep(sec(6));
        let (cpu1, private, ws) = usage(proc);
        println!(
            "info: idle CPU {} ms over 6 s, total CPU {cpu1} ms, private memory {} KB, working set {} KB (incl. shared DLLs)",
            cpu1 - cpu0,
            private / 1024,
            ws / 1024
        );
        r.check(cpu1 - cpu0 <= 60, "idle CPU under 1%");
        r.check(private < 10 * 1024 * 1024, "private memory under 10 MB");

        let un = Command::new(&exe)
            .args(["--uninstall", "--quiet"])
            .status()
            .unwrap();
        r.check(un.success(), "--uninstall exits 0");
        r.check(
            wait_until(sec(5), || enforcer().is_none()),
            "enforcer stops",
        );
        r.check(
            !reg_exists(POLICY_KEY, w!("NoControlPanel")),
            "locks removed",
        );
        r.check(
            !reg_exists(RUN_KEY, w!("ComputerBlock")),
            "auto-start removed",
        );
        unsafe {
            let _ = CloseHandle(proc);
        }

        println!("--- enforcer log ---");
        println!(
            "{}",
            std::fs::read_to_string(tmp.join("ComputerBlock").join("log.txt")).unwrap_or_default()
        );
        if r.failures == 0 {
            println!("ALL PASSED");
            0
        } else {
            println!("{} FAILED", r.failures);
            1
        }
    }
}
