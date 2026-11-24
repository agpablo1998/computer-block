//! Windows runtime. All `unsafe` code of the project is in this module.
//!
//! Command line (for the admin; the user never sees any of this):
//!
//! | command                     | what it does                                         |
//! |-----------------------------|------------------------------------------------------|
//! | `computer-block --setup`    | save the current state as normal, install, start     |
//! | `computer-block --capture`  | save the current state as normal again (after a fix)  |
//! | `computer-block --uninstall`| stop, remove every lock and the auto-start entry     |
//! | `computer-block`            | the hidden enforcer (started at every sign-in)       |
//!
//! Add `--quiet` to skip the confirmation dialogs.

mod config;
mod desktop;
mod printer;
mod registry;
mod runtime;
mod targets;
mod util;

use config::Normal;
use util::{installed_exe, log, wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MessageBoxW, PostMessageW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    MESSAGEBOX_STYLE, WM_CLOSE,
};

const USAGE: &str = "computer-block\n\n\
--setup       Save how this computer looks now as normal, install, start.\n\
--capture     Save the current look as normal again.\n\
--uninstall   Stop and remove all locks.\n\n\
Admin pause: Ctrl+Alt+Shift+F12 (30 minutes, press again to resume).";

pub fn main() -> i32 {
    // SAFETY: once, at start, on the main thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quiet = args.iter().any(|a| a == "--quiet");
    let command = args.iter().map(String::as_str).find(|a| *a != "--quiet");
    let result = match command {
        None => return runtime::run(),
        Some("--setup") => setup(),
        Some("--capture") => capture(),
        Some("--uninstall") => uninstall(),
        Some(_) => {
            message(USAGE, MB_ICONINFORMATION);
            return 2;
        }
    };
    match result {
        Ok(text) => {
            log(&text);
            if !quiet {
                message(&text, MB_ICONINFORMATION);
            }
            0
        }
        Err(text) => {
            log(&text);
            if !quiet {
                message(&text, MB_ICONERROR);
            }
            1
        }
    }
}

fn message(text: &str, icon: MESSAGEBOX_STYLE) {
    let t = wide(text);
    let c = wide("computer-block");
    // SAFETY: strings outlive the call.
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | icon);
    }
}

/// Post a message to the running enforcer, if any. Returns whether one runs.
fn notify_running(msg: u32) -> bool {
    // SAFETY: plain API calls.
    unsafe {
        match FindWindowW(runtime::WINDOW_CLASS, PCWSTR::null()) {
            Ok(h) if !h.is_invalid() => PostMessageW(Some(h), msg, WPARAM(0), LPARAM(0)).is_ok(),
            _ => false,
        }
    }
}

fn stop_running() {
    if notify_running(WM_CLOSE) {
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            // SAFETY: plain API call.
            let still = unsafe { FindWindowW(runtime::WINDOW_CLASS, PCWSTR::null()) }
                .is_ok_and(|h| !h.is_invalid());
            if !still {
                break;
            }
        }
    }
}

fn capture_normal() -> Normal {
    Normal {
        printer: printer::get(),
        desktop: desktop::get().ok(),
    }
}

fn describe(n: &Normal) -> String {
    format!(
        "Default printer: {}\nDesktop icons: {}",
        n.printer.as_deref().unwrap_or("(none, not enforced)"),
        n.desktop.map_or_else(
            || "(not available, not enforced)".to_owned(),
            |d| format!("size {}", d.icon_size)
        ),
    )
}

fn setup() -> Result<String, String> {
    let installed = installed_exe().ok_or("LOCALAPPDATA is not set")?;
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let normal = capture_normal();
    normal
        .save()
        .map_err(|e| format!("cannot save settings: {e}"))?;

    stop_running();
    if current != installed {
        if let Some(dir) = installed.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&current, &installed).map_err(|e| format!("cannot install: {e}"))?;
    }
    registry::apply_all().map_err(|e| format!("cannot write locks: {e}"))?;
    std::process::Command::new(&installed)
        .spawn()
        .map_err(|e| format!("cannot start: {e}"))?;
    Ok(format!(
        "Setup complete.\n\n{}\n\nSign out and back in once so the Windows-key block takes effect.\n\n\
         Admin pause: Ctrl+Alt+Shift+F12.",
        describe(&normal)
    ))
}

fn capture() -> Result<String, String> {
    let normal = capture_normal();
    normal
        .save()
        .map_err(|e| format!("cannot save settings: {e}"))?;
    notify_running(runtime::WM_APP_RELOAD);
    Ok(format!("Saved as normal.\n\n{}", describe(&normal)))
}

fn uninstall() -> Result<String, String> {
    stop_running();
    registry::remove_all().map_err(|e| format!("cannot remove locks: {e}"))?;
    Normal::delete();
    Ok("computer-block is stopped and every lock is removed.\n\
        Sign out and back in to restore Windows-key shortcuts.\n\
        You can delete the folder %LOCALAPPDATA%\\ComputerBlock."
        .to_owned())
}
