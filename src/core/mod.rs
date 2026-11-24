//! The enforcer's brain: a pure, deterministic state machine.
//!
//! This module never calls Windows. It receives an [`Event`], looks at the
//! world through the [`World`] trait, updates its own state and returns the
//! [`Command`]s to execute. Every transition here is a program action of the
//! TLA+ model in `spec/Lockdown.tla`, and `conformance.rs` replays the whole
//! reachable state graph of that model against this code.
//!
//! The lifecycle is enforced by the type system:
//!
//! ```text
//!   Booting --start()--> Running { Enforcing <--Hotkey/timeout--> Paused }
//! ```
//!
//! Events cannot be handled before start-up finished, because only
//! [`Running`] has a `handle` method.
#![forbid(unsafe_code)]

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

/// Identity of a target window (an `HWND` value on Windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WinId(pub isize);

/// What a target window looks like right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinState {
    /// Gone, or hidden.
    Closed,
    Maximized,
    /// Covers its whole monitor without being maximized: video or F11
    /// fullscreen, a slideshow. Left alone.
    Fullscreen,
    Minimized,
    /// Restored to a smaller size.
    Normal,
    /// The user is dragging or resizing it right now (also: a Chrome tab
    /// being torn off). Left alone until the drag ends.
    Moving,
}

impl WinState {
    /// States the enforcer corrects.
    #[must_use]
    pub fn drifted(self) -> bool {
        matches!(self, Self::Minimized | Self::Normal)
    }
}

/// The desktop's icon view compared with the saved "normal".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Ok,
    Drifted,
    /// Explorer is not running (crashed or restarting).
    Unavailable,
}

/// Read-only view of the machine. Implemented over real Win32 calls in
/// `win::world`, and over TLA+ states in the conformance tests.
pub trait World {
    fn window(&self, w: WinId) -> WinState;
    /// Every target window that is currently open.
    fn open_windows(&self) -> Vec<WinId>;
    fn printer_ok(&self) -> bool;
    fn desktop(&self) -> Desktop;
    fn policies_ok(&self) -> bool;
}

/// Something that happened. Notifications may be lost; the periodic
/// [`Event::Tick`] sweep makes correctness independent of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    WindowChanged(WinId),
    PrinterChanged,
    DesktopChanged,
    PoliciesChanged,
    Hotkey,
    Tick,
}

/// Everything the enforcer can ever do. There is deliberately no command to
/// close, hide, minimize or move a window, and nothing that touches input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Command {
    ApplyPolicies,
    RemovePolicies,
    RestorePrinter,
    RestoreDesktop,
    Maximize(WinId),
}

/// The running enforcer's mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Enforcing,
    /// Admin pause. Ends after `ticks_left` timer ticks or on the hotkey.
    Paused {
        ticks_left: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Corrections allowed per window per tick period (fight-loop guard).
    pub max_strikes: u8,
    /// Length of an admin pause, in ticks. Must be at least 1.
    pub pause_ticks: u32,
}

impl Config {
    /// One tick is one minute: 3 corrections per window per minute, and a
    /// 30 minute admin pause.
    pub const DEFAULT: Self = Self {
        max_strikes: 3,
        pause_ticks: 30,
    };
}

/// The process exists but has not looked at the world yet.
#[derive(Debug)]
pub struct Booting {
    cfg: Config,
}

impl Booting {
    #[must_use]
    pub fn new(mut cfg: Config) -> Self {
        // Both bounds are assumptions of the model (ASSUME in the spec).
        cfg.max_strikes = cfg.max_strikes.max(1);
        cfg.pause_ticks = cfg.pause_ticks.max(1);
        Self { cfg }
    }

    /// Transition `HandleStart`: apply policies, sweep everything.
    #[must_use]
    pub fn start(self, world: &dyn World) -> (Running, Vec<Command>) {
        let mut running = Running {
            mode: Mode::Enforcing,
            strikes: BTreeMap::new(),
            cfg: self.cfg,
        };
        let cmds = running.resume(world);
        (running, cmds)
    }
}

/// The enforcer after start-up.
#[derive(Debug)]
pub struct Running {
    mode: Mode,
    /// Corrections issued per window since the last sweep. Absent means 0.
    strikes: BTreeMap<WinId, u8>,
    cfg: Config,
}

impl Running {
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[must_use]
    pub fn strikes(&self, w: WinId) -> u8 {
        self.strikes.get(&w).copied().unwrap_or(0)
    }

    /// Transitions `HandleEvent` and `HandleTick`.
    pub fn handle(&mut self, event: Event, world: &dyn World) -> Vec<Command> {
        match (self.mode, event) {
            (Mode::Enforcing, Event::Hotkey) => self.pause(),
            (Mode::Paused { .. }, Event::Hotkey) => self.resume(world),
            (Mode::Paused { ticks_left }, Event::Tick) => self.tick_paused(ticks_left, world),
            (Mode::Paused { .. }, _) => Vec::new(),
            (Mode::Enforcing, Event::Tick) => self.sweep(world),
            (Mode::Enforcing, Event::WindowChanged(w)) => self.check_window(w, world),
            (Mode::Enforcing, Event::PrinterChanged) => {
                only_if(!world.printer_ok(), Command::RestorePrinter)
            }
            (Mode::Enforcing, Event::DesktopChanged) => {
                only_if(world.desktop() == Desktop::Drifted, Command::RestoreDesktop)
            }
            (Mode::Enforcing, Event::PoliciesChanged) => {
                only_if(!world.policies_ok(), Command::ApplyPolicies)
            }
        }
    }

    /// Spec `Pause`.
    fn pause(&mut self) -> Vec<Command> {
        self.mode = Mode::Paused {
            ticks_left: self.cfg.pause_ticks,
        };
        self.strikes.clear();
        vec![Command::RemovePolicies]
    }

    /// Spec `Resume`: also used for start-up.
    fn resume(&mut self, world: &dyn World) -> Vec<Command> {
        self.mode = Mode::Enforcing;
        let mut cmds = self.sweep(world);
        if !cmds.contains(&Command::ApplyPolicies) {
            cmds.insert(0, Command::ApplyPolicies);
        }
        cmds
    }

    fn tick_paused(&mut self, ticks_left: u32, world: &dyn World) -> Vec<Command> {
        if ticks_left > 1 {
            self.mode = Mode::Paused {
                ticks_left: ticks_left - 1,
            };
            Vec::new()
        } else {
            self.resume(world)
        }
    }

    /// Spec `SweepCmds` / `SweepStrikes`: correct everything that drifted and
    /// restart every window's strike count.
    fn sweep(&mut self, world: &dyn World) -> Vec<Command> {
        let mut cmds = Vec::new();
        if !world.policies_ok() {
            cmds.push(Command::ApplyPolicies);
        }
        if !world.printer_ok() {
            cmds.push(Command::RestorePrinter);
        }
        if world.desktop() == Desktop::Drifted {
            cmds.push(Command::RestoreDesktop);
        }
        self.strikes.clear();
        let mut windows = world.open_windows();
        windows.sort_unstable();
        windows.dedup();
        for w in windows {
            if world.window(w).drifted() {
                self.strikes.insert(w, 1);
                cmds.push(Command::Maximize(w));
            }
        }
        cmds
    }

    /// Spec `HandleWindow`.
    fn check_window(&mut self, w: WinId, world: &dyn World) -> Vec<Command> {
        let state = world.window(w);
        let strikes = self.strikes(w);
        if state.drifted() && strikes < self.cfg.max_strikes {
            self.strikes.insert(w, strikes + 1);
            vec![Command::Maximize(w)]
        } else {
            if state == WinState::Closed {
                // Window handles are reused by Windows; forget this one.
                self.strikes.remove(&w);
            }
            Vec::new()
        }
    }
}

fn only_if(cond: bool, cmd: Command) -> Vec<Command> {
    if cond {
        vec![cmd]
    } else {
        Vec::new()
    }
}
