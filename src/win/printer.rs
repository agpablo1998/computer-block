//! The default printer.

use super::util::{from_wide, pcwstr, wide};
use windows::core::PWSTR;
use windows::Win32::Graphics::Printing::{GetDefaultPrinterW, SetDefaultPrinterW};

pub fn get() -> Option<String> {
    let mut len = 0u32;
    // SAFETY: first call asks for the needed length only.
    unsafe {
        let _ = GetDefaultPrinterW(None, &raw mut len);
    }
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; usize::try_from(len).ok()?];
    // SAFETY: buffer holds `len` UTF-16 units.
    let ok = unsafe { GetDefaultPrinterW(Some(PWSTR(buf.as_mut_ptr())), &raw mut len) };
    ok.as_bool().then(|| from_wide(&buf))
}

pub fn set(name: &str) -> windows::core::Result<()> {
    let w = wide(name);
    // SAFETY: null-terminated string alive for the call.
    let ok = unsafe { SetDefaultPrinterW(pcwstr(&w)) };
    if ok.as_bool() {
        Ok(())
    } else {
        Err(windows::core::Error::from_thread())
    }
}

pub fn is(name: &str) -> bool {
    get().is_some_and(|cur| cur.eq_ignore_ascii_case(name))
}
