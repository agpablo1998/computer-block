//! The hidden, event-driven enforcer process.
//!
//! One thread, one message loop, no polling. Windows wakes the thread for:
//! * `WinEvents` (out-of-context, so nothing is injected into other apps):
//!   foreground changes, minimize, drag start/end; and window location
//!   changes, subscribed per process for the four target apps only;
//! * registry change notifications (printer, policies, Run entry);
//! * a one-minute timer that sweeps everything (the safety net that makes
//!   correctness independent of any single notification);
//! * the admin hotkey, Explorer restarts, session unlock, resume from sleep.
//!
//! Callbacks never act. They only append to a small queue, which the loop
//! drains between messages. This mirrors the queue in the TLA+ model and
//! makes re-entrancy impossible (cross-process COM calls pump messages).
//!
//! Nothing here hooks the keyboard or mouse. If this process hangs or
//! crashes, the user's input is unaffected; only corrections stop until the
//! next sign-in, and the registry locks keep working on their own.

use super::config::Normal;
use super::registry::{self, Watch, POLICIES_ROOT, PRINTER_KEY, RUN_KEY};
use super::targets::{self, Targets};
use super::util::log;
use super::{desktop, printer};
use crate::core::{
    Booting, Command, Config, Desktop, Event, Mode, Running, WinId, WinState, World,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0,
    WPARAM,
};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, INFINITE};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, VK_F12,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetAncestor, GetClassNameW,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetTimer, TranslateMessage, CHILDID_SELF, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GA_ROOT, MB_ICONASTERISK, MB_OK, MSG,
    MWMO_INPUTAVAILABLE, OBJID_WINDOW, PBT_APMRESUMEAUTOMATIC, PM_REMOVE, QS_ALLINPUT,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_CLOSE, WM_DESTROY, WM_HOTKEY,
    WM_POWERBROADCAST, WM_QUIT, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};

pub const WINDOW_CLASS: PCWSTR = w!("ComputerBlockHidden");
/// Sent by `--capture` to the running instance: reload the saved normal.
pub const WM_APP_RELOAD: u32 = WM_APP + 1;
const TICK_MS: u32 = 60_000;

/// One minute, or `COMPUTER_BLOCK_TICK_MS` (1 s .. 10 min) for the smoke test.
fn tick_ms() -> u32 {
    std::env::var("COMPUTER_BLOCK_TICK_MS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map_or(TICK_MS, |v| v.clamp(1_000, 600_000))
}
const HOTKEY_ID: i32 = 1;
const WTS_SESSION_UNLOCK: usize = 0x8;
/// Bounded like the model's queue. Duplicate events are coalesced; anything
/// dropped is picked up by the next sweep.
const QUEUE_MAX: usize = 64;

thread_local! {
    static QUEUE: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    static LAST_FG_WAS_DESKTOP: Cell<bool> = const { Cell::new(false) };
    static RELOAD: Cell<bool> = const { Cell::new(false) };
    static RESYNC: Cell<bool> = const { Cell::new(false) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

fn push(ev: Event) {
    QUEUE.with(|q| {
        if let Ok(mut q) = q.try_borrow_mut() {
            if q.len() < QUEUE_MAX && !q.contains(&ev) {
                q.push_back(ev);
            }
        }
    });
}

fn pop() -> Option<Event> {
    QUEUE.with(|q| q.try_borrow_mut().ok().and_then(|mut q| q.pop_front()))
}

fn is_desktop_window(h: HWND) -> bool {
    let mut buf = [0u16; 16];
    // SAFETY: local buffer.
    let n = unsafe { GetClassNameW(h, &mut buf) };
    let class = String::from_utf16_lossy(
        buf.get(..usize::try_from(n).unwrap_or(0))
            .unwrap_or_default(),
    );
    class == "Progman" || class == "WorkerW"
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    h: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0
        || id_child != i32::try_from(CHILDID_SELF).unwrap_or(0)
        || h.is_invalid()
    {
        return;
    }
    if event == EVENT_SYSTEM_FOREGROUND {
        // Icon size has no change notification: check the desktop when the
        // user arrives on it or leaves it.
        let now = is_desktop_window(h);
        if now || LAST_FG_WAS_DESKTOP.get() {
            push(Event::DesktopChanged);
        }
        LAST_FG_WAS_DESKTOP.set(now);
    }
    // Only top-level windows can be targets; skip child-window noise.
    // SAFETY: tolerates stale handles.
    if unsafe { GetAncestor(h, GA_ROOT) } == h {
        push(Event::WindowChanged(targets::id(h)));
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => push(Event::Hotkey),
        WM_TIMER => push(Event::Tick),
        WM_APP_RELOAD => RELOAD.set(true),
        WM_WTSSESSION_CHANGE if wp.0 == WTS_SESSION_UNLOCK => RESYNC.set(true),
        WM_POWERBROADCAST if wp.0 == PBT_APMRESUMEAUTOMATIC as usize => RESYNC.set(true),
        WM_CLOSE => {
            // SAFETY: our own window.
            let _ = unsafe { DestroyWindow(h) };
        }
        // SAFETY: plain API call.
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        m if m != 0 && m == TASKBAR_CREATED.get() => {
            // Explorer restarted: desktop view and policies may be reset.
            push(Event::DesktopChanged);
            RESYNC.set(true);
        }
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
    LRESULT(0)
}

/// The live machine, as seen by the core.
struct LiveWorld<'a> {
    normal: &'a Normal,
    targets: &'a RefCell<Targets>,
}

impl World for LiveWorld<'_> {
    fn window(&self, w: WinId) -> WinState {
        // Anything that is not one of the four apps' main windows counts as
        // closed: the core can never act on it.
        if !self.targets.borrow_mut().is_target(targets::hwnd(w)) {
            return WinState::Closed;
        }
        targets::state(w)
    }
    fn open_windows(&self) -> Vec<WinId> {
        self.targets.borrow_mut().enumerate()
    }
    fn printer_ok(&self) -> bool {
        self.normal.printer.as_deref().is_none_or(printer::is)
    }
    fn desktop(&self) -> Desktop {
        let Some(want) = &self.normal.desktop else {
            return Desktop::Ok;
        };
        match desktop::get() {
            Ok(v) if v == *want => Desktop::Ok,
            Ok(_) => Desktop::Drifted,
            Err(_) => Desktop::Unavailable,
        }
    }
    fn policies_ok(&self) -> bool {
        registry::all_applied()
    }
}

struct App {
    running: Running,
    normal: Normal,
    targets: RefCell<Targets>,
    location_hooks: HashMap<u32, HWINEVENTHOOK>,
}

impl App {
    fn handle(&mut self, ev: Event) {
        let before = self.running.mode();
        let cmds = {
            let world = LiveWorld {
                normal: &self.normal,
                targets: &self.targets,
            };
            self.running.handle(ev, &world)
        };
        self.execute(&cmds);
        let after = self.running.mode();
        if before != after {
            beep(after);
            log(&format!("mode {before:?} -> {after:?}"));
        }
        match ev {
            Event::WindowChanged(w) => self.watch_process_of(w),
            Event::Tick => self.refresh_location_hooks(),
            _ => {}
        }
    }

    fn execute(&self, cmds: &[Command]) {
        for c in cmds {
            let result = match c {
                Command::ApplyPolicies => registry::apply_all(),
                Command::RemovePolicies => registry::remove_pausable(),
                Command::RestorePrinter => {
                    self.normal.printer.as_deref().map_or(Ok(()), printer::set)
                }
                Command::RestoreDesktop => {
                    self.normal.desktop.as_ref().map_or(Ok(()), desktop::set)
                }
                Command::Maximize(w) => {
                    targets::maximize(*w);
                    Ok(())
                }
            };
            match result {
                Ok(()) => log(&format!("{c:?}")),
                Err(e) => log(&format!("{c:?} failed: {e}")),
            }
        }
    }

    /// Subscribe to location changes of a target app's process (needed to
    /// see un-maximize by double-click, Win+Down, snap or Aero Shake).
    fn watch_process_of(&mut self, w: WinId) {
        let h = targets::hwnd(w);
        if !self.targets.borrow_mut().is_target(h) {
            return;
        }
        let pid = Targets::pid_of(h);
        if pid == 0 || self.location_hooks.contains_key(&pid) {
            return;
        }
        // SAFETY: callback is a valid extern "system" fn for the process life.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_LOCATIONCHANGE,
                None,
                Some(on_win_event),
                pid,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if !hook.is_invalid() {
            self.location_hooks.insert(pid, hook);
        }
    }

    /// On every sweep: hook new target processes, unhook exited ones.
    fn refresh_location_hooks(&mut self) {
        self.targets.borrow_mut().clear_cache();
        let open = self.targets.borrow_mut().enumerate();
        let live: HashSet<u32> = open
            .iter()
            .map(|w| Targets::pid_of(targets::hwnd(*w)))
            .collect();
        self.location_hooks.retain(|pid, hook| {
            let keep = live.contains(pid);
            if !keep {
                // SAFETY: hook was returned by SetWinEventHook.
                unsafe {
                    let _ = UnhookWinEvent(*hook);
                }
            }
            keep
        });
        for w in open {
            self.watch_process_of(w);
        }
    }

    fn resync(&mut self) {
        for ev in [
            Event::PoliciesChanged,
            Event::PrinterChanged,
            Event::DesktopChanged,
        ] {
            push(ev);
        }
        for w in self.targets.borrow_mut().enumerate() {
            push(Event::WindowChanged(w));
        }
    }
}

fn beep(mode: Mode) {
    let kind = match mode {
        Mode::Paused { .. } => MB_ICONASTERISK,
        Mode::Enforcing => MB_OK,
    };
    // SAFETY: plain API call.
    unsafe {
        let _ = MessageBeep(kind);
    }
}

fn create_window() -> windows::core::Result<HWND> {
    // SAFETY: class and window are created once, on this thread.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        if RegisterClassW(&raw const class) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        // Hidden top-level window (not message-only: it must receive the
        // TaskbarCreated broadcast). Never shown.
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            w!("computer-block"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn install_global_hooks() -> usize {
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
    ];
    let mut ok = 0;
    for (lo, hi) in ranges {
        // SAFETY: valid callback; hooks live as long as the process.
        let h = unsafe {
            SetWinEventHook(
                lo,
                hi,
                None,
                Some(on_win_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if h.is_invalid() {
            log(&format!("SetWinEventHook {lo:#x} failed"));
        } else {
            ok += 1;
        }
    }
    ok
}

/// Runs the enforcer until `WM_QUIT`. Returns the process exit code.
pub fn run() -> i32 {
    // Single instance per user session.
    // SAFETY: the handle is intentionally kept open for the process life.
    let mutex = unsafe { CreateMutexW(None, false, w!(r"Local\ComputerBlock.Running")) };
    // SAFETY: plain API call.
    if mutex.is_err() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return 0;
    }

    let Some(normal) = Normal::load() else {
        log("no saved normal state; run --setup first");
        return 1;
    };

    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            log(&format!("cannot create window: {e}"));
            return 1;
        }
    };
    // SAFETY: plain API calls with our own window.
    unsafe {
        TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        if SetTimer(Some(hwnd), 1, tick_ms(), None) == 0 {
            log("SetTimer failed");
        }
        if RegisterHotKey(
            Some(hwnd),
            HOTKEY_ID,
            MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT,
            u32::from(VK_F12.0),
        )
        .is_err()
        {
            log("admin hotkey unavailable (already registered by another program)");
        }
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
    }
    log(&format!("started; {} global hooks", install_global_hooks()));

    // Registry watches. Any that fail are covered by the one-minute sweep.
    let mut watches: Vec<(Watch, &[Event])> = Vec::new();
    let specs: [(PCWSTR, bool, &[Event]); 3] = [
        (
            PRINTER_KEY,
            false,
            &[Event::PrinterChanged, Event::PoliciesChanged],
        ),
        (POLICIES_ROOT, true, &[Event::PoliciesChanged]),
        (RUN_KEY, false, &[Event::PoliciesChanged]),
    ];
    for (key, subtree, events) in specs {
        // SAFETY: auto-reset event, closed at exit.
        match unsafe { CreateEventW(None, false, false, None) }
            .and_then(|ev| Watch::new(key, subtree, ev))
        {
            Ok(w) => watches.push((w, events)),
            Err(e) => log(&format!("registry watch failed: {e}")),
        }
    }
    let handles: Vec<HANDLE> = watches.iter().map(|(w, _)| w.event).collect();

    let targets = RefCell::new(Targets::new());
    let (running, cmds) = Booting::new(Config::DEFAULT).start(&LiveWorld {
        normal: &normal,
        targets: &targets,
    });
    let mut app = App {
        running,
        normal,
        targets,
        location_hooks: HashMap::new(),
    };
    app.execute(&cmds);
    app.refresh_location_hooks();

    let code = event_loop(&mut app, &watches, &handles);
    for h in handles {
        // SAFETY: events we created.
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    code
}

fn event_loop(app: &mut App, watches: &[(Watch, &[Event])], handles: &[HANDLE]) -> i32 {
    let n = u32::try_from(handles.len()).unwrap_or(0);
    loop {
        let wait_handles = if handles.is_empty() {
            None
        } else {
            Some(handles)
        };
        // SAFETY: handles are valid auto-reset events.
        let r = unsafe {
            MsgWaitForMultipleObjectsEx(wait_handles, INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
        let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0);
        match idx.cmp(&n) {
            std::cmp::Ordering::Less => {
                if let Some((watch, events)) = watches.get(idx as usize) {
                    // Re-arm first, so a change made while we handle this one
                    // still signals.
                    if let Err(e) = watch.arm() {
                        log(&format!("re-arm failed: {e}"));
                    }
                    for ev in *events {
                        push(*ev);
                    }
                }
            }
            std::cmp::Ordering::Equal => {
                let mut msg = MSG::default();
                // SAFETY: standard message pump on our own thread.
                while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    if msg.message == WM_QUIT {
                        log("exit");
                        return 0;
                    }
                    // SAFETY: msg was filled in by PeekMessageW.
                    unsafe {
                        let _ = TranslateMessage(&raw const msg);
                        DispatchMessageW(&raw const msg);
                    }
                }
            }
            std::cmp::Ordering::Greater => {
                log("wait failed");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }

        if RELOAD.replace(false) {
            if let Some(n) = Normal::load() {
                app.normal = n;
                log("reloaded saved normal state");
            }
        }
        if RESYNC.replace(false) {
            app.resync();
        }
        while let Some(ev) = pop() {
            app.handle(ev);
        }
    }
}
