//! Per-user registry locks (no admin rights needed) and change watching.

use super::util::{installed_exe, wide};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, LPARAM, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegNotifyChangeKeyValue,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_NOTIFY, REG_DWORD,
    REG_NOTIFY_CHANGE_LAST_SET, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

const EXPLORER: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer");
const SYSTEM: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Policies\System");
pub const POLICIES_ROOT: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Policies");
pub const PRINTER_KEY: PCWSTR = w!(r"Software\Microsoft\Windows NT\CurrentVersion\Windows");
pub const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("ComputerBlock");

struct Lock {
    key: PCWSTR,
    name: PCWSTR,
    value: u32,
    /// Lifted while the admin pause is active.
    pausable: bool,
}

/// Every DWORD this program writes. Nothing else in the registry is touched,
/// apart from the Run entry.
const LOCKS: &[Lock] = &[
    // Settings app and Control Panel.
    Lock {
        key: EXPLORER,
        name: w!("NoControlPanel"),
        value: 1,
        pausable: true,
    },
    // Taskbar settings (size, position, auto-hide, pinned items).
    Lock {
        key: EXPLORER,
        name: w!("NoSetTaskbar"),
        value: 1,
        pausable: true,
    },
    // Right-click menu on the desktop and in folders (desktop View menu).
    Lock {
        key: EXPLORER,
        name: w!("NoViewContextMenu"),
        value: 1,
        pausable: true,
    },
    // Task Manager, so the program can't be ended by accident.
    Lock {
        key: SYSTEM,
        name: w!("DisableTaskMgr"),
        value: 1,
        pausable: true,
    },
    // Windows-key shortcuts (Win+D, Win+M, Win+arrows, Win+I, ...). Read by
    // Explorer at sign-in, so it stays on during a pause.
    Lock {
        key: EXPLORER,
        name: w!("NoWinKeys"),
        value: 1,
        pausable: false,
    },
    // "Let Windows manage my default printer" off: the usual reason the
    // default printer switches by itself.
    Lock {
        key: PRINTER_KEY,
        name: w!("LegacyDefaultPrinterMode"),
        value: 1,
        pausable: false,
    },
];

struct Key(HKEY);

impl Key {
    fn open(
        path: PCWSTR,
        access: windows::Win32::System::Registry::REG_SAM_FLAGS,
    ) -> windows::core::Result<Self> {
        let mut hkey = HKEY::default();
        // SAFETY: valid null-terminated path, out-pointer to a local.
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path,
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                access,
                None,
                &raw mut hkey,
                None,
            )
            .ok()?;
        }
        Ok(Self(hkey))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: we own this handle.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn read_dword(key: PCWSTR, name: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: data/size point to locals of the declared size.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&raw mut size),
        )
    };
    (r == ERROR_SUCCESS).then_some(data)
}

fn write_dword(key: PCWSTR, name: PCWSTR, value: u32) -> windows::core::Result<()> {
    let k = Key::open(key, KEY_ALL_ACCESS)?;
    // SAFETY: valid open key, 4-byte payload.
    unsafe { RegSetValueExW(k.0, name, None, REG_DWORD, Some(&value.to_le_bytes())).ok() }
}

fn delete_value(key: PCWSTR, name: PCWSTR) -> windows::core::Result<()> {
    let k = Key::open(key, KEY_ALL_ACCESS)?;
    // SAFETY: valid open key.
    let r = unsafe { RegDeleteValueW(k.0, name) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        r.ok()
    }
}

fn read_string(key: PCWSTR, name: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = u32::try_from(std::mem::size_of_val(&buf)).ok()?;
    // SAFETY: buffer and size describe the same local array.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    (r == ERROR_SUCCESS).then(|| super::util::from_wide(&buf))
}

fn run_command() -> Option<String> {
    installed_exe().map(|p| format!("\"{}\"", p.display()))
}

/// Tell Explorer and the shell to re-read policies now.
fn broadcast_policy_change() {
    let policy = wide("Policy");
    // SAFETY: lParam points to a null-terminated string that outlives the call.
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(policy.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            500,
            None,
        );
    }
}

/// Write every lock and the Run entry.
pub fn apply_all() -> windows::core::Result<()> {
    let mut result = Ok(());
    for l in LOCKS {
        if read_dword(l.key, l.name) != Some(l.value) {
            if let Err(e) = write_dword(l.key, l.name, l.value) {
                result = Err(e);
            }
        }
    }
    if let Some(cmd) = run_command() {
        if read_string(RUN_KEY, RUN_VALUE).as_deref() != Some(cmd.as_str()) {
            if let Err(e) = write_run(&cmd) {
                result = Err(e);
            }
        }
    }
    broadcast_policy_change();
    result
}

fn write_run(cmd: &str) -> windows::core::Result<()> {
    let k = Key::open(RUN_KEY, KEY_ALL_ACCESS)?;
    let data: Vec<u8> = wide(cmd).iter().flat_map(|c| c.to_le_bytes()).collect();
    // SAFETY: valid open key, REG_SZ payload with terminating null.
    unsafe { RegSetValueExW(k.0, RUN_VALUE, None, REG_SZ, Some(&data)).ok() }
}

/// Lift the pausable locks (admin pause).
pub fn remove_pausable() -> windows::core::Result<()> {
    let mut result = Ok(());
    for l in LOCKS.iter().filter(|l| l.pausable) {
        if let Err(e) = delete_value(l.key, l.name) {
            result = Err(e);
        }
    }
    broadcast_policy_change();
    result
}

/// Uninstall: remove every lock and the Run entry.
/// `LegacyDefaultPrinterMode` is left on; it is what any admin would want.
pub fn remove_all() -> windows::core::Result<()> {
    let mut result = Ok(());
    for l in LOCKS.iter().filter(|l| l.key != PRINTER_KEY) {
        if let Err(e) = delete_value(l.key, l.name) {
            result = Err(e);
        }
    }
    if let Err(e) = delete_value(RUN_KEY, RUN_VALUE) {
        result = Err(e);
    }
    broadcast_policy_change();
    result
}

pub fn all_applied() -> bool {
    LOCKS
        .iter()
        .all(|l| read_dword(l.key, l.name) == Some(l.value))
        && run_command()
            .is_none_or(|cmd| read_string(RUN_KEY, RUN_VALUE).as_deref() == Some(cmd.as_str()))
}

/// One armed `RegNotifyChangeKeyValue`. The notification fires once; call
/// [`Watch::arm`] again after every signal (done by the runtime loop
/// before it handles the event, so no change can slip between).
pub struct Watch {
    key: Key,
    subtree: bool,
    pub event: HANDLE,
}

impl Watch {
    pub fn new(path: PCWSTR, subtree: bool, event: HANDLE) -> windows::core::Result<Self> {
        let w = Self {
            key: Key::open(path, KEY_NOTIFY)?,
            subtree,
            event,
        };
        w.arm()?;
        Ok(w)
    }

    pub fn arm(&self) -> windows::core::Result<()> {
        // SAFETY: key and event are valid for the lifetime of self.
        let r: WIN32_ERROR = unsafe {
            RegNotifyChangeKeyValue(
                self.key.0,
                self.subtree,
                REG_NOTIFY_CHANGE_LAST_SET,
                Some(self.event),
                true,
            )
        };
        r.ok()
    }
}
