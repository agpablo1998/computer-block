//! The desktop icon view, read and set live through the shell's own
//! `IFolderView2` (the same object the desktop View menu talks to). Writing
//! the registry instead would only take effect after Explorer restarts.

use windows::core::{Interface, Result};
use windows::Win32::System::Com::{CoCreateInstance, IServiceProvider, CLSCTX_LOCAL_SERVER};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IFolderView2, IShellBrowser, IShellWindows, SID_STopLevelBrowser, ShellWindows, CSIDL_DESKTOP,
    FOLDERVIEWMODE, FWF_AUTOARRANGE, FWF_NOICONS, FWF_SNAPTOGRID, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};

/// Folder flags we keep: icons shown, auto-arrange, align to grid.
const FLAG_MASK: u32 = FWF_NOICONS.0 as u32 | FWF_AUTOARRANGE.0 as u32 | FWF_SNAPTOGRID.0 as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopView {
    pub view_mode: i32,
    pub icon_size: i32,
    pub flags: u32,
}

fn folder_view() -> Result<IFolderView2> {
    // SAFETY: plain COM calls on interfaces we own; COM is initialized
    // (apartment-threaded) on this thread by the runtime.
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
        let loc = VARIANT::from(i32::try_from(CSIDL_DESKTOP).unwrap_or(0));
        let empty = VARIANT::default();
        let mut hwnd = 0i32;
        let disp = windows.FindWindowSW(
            &raw const loc,
            &raw const empty,
            SWC_DESKTOP,
            &raw mut hwnd,
            SWFO_NEEDDISPATCH,
        )?;
        let sp: IServiceProvider = disp.cast()?;
        let browser: IShellBrowser = sp.QueryService(&SID_STopLevelBrowser)?;
        let view = browser.QueryActiveShellView()?;
        view.cast()
    }
}

pub fn get() -> Result<DesktopView> {
    let fv = folder_view()?;
    let mut mode = FOLDERVIEWMODE(0);
    let mut size = 0i32;
    // SAFETY: out-pointers to locals.
    unsafe {
        fv.GetViewModeAndIconSize(&raw mut mode, &raw mut size)?;
        let flags = fv.GetCurrentFolderFlags()?;
        Ok(DesktopView {
            view_mode: mode.0,
            icon_size: size,
            flags: flags & FLAG_MASK,
        })
    }
}

pub fn set(want: &DesktopView) -> Result<()> {
    let fv = folder_view()?;
    // SAFETY: plain COM calls.
    unsafe {
        fv.SetViewModeAndIconSize(FOLDERVIEWMODE(want.view_mode), want.icon_size)?;
        fv.SetCurrentFolderFlags(FLAG_MASK, want.flags & FLAG_MASK)
    }
}
