//! Unit tests per transition, plus property tests that drive the core with
//! the production configuration (the TLA+ model uses small constants).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::{Booting, Command, Config, Desktop, Event, Mode, Running, WinId, WinState, World};
use proptest::prelude::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
struct FakeWorld {
    windows: BTreeMap<WinId, WinState>,
    printer_ok: bool,
    desktop: Desktop,
    policies_ok: bool,
}

impl FakeWorld {
    fn tidy() -> Self {
        Self {
            windows: BTreeMap::new(),
            printer_ok: true,
            desktop: Desktop::Ok,
            policies_ok: true,
        }
    }
    fn with(mut self, w: isize, s: WinState) -> Self {
        self.windows.insert(WinId(w), s);
        self
    }
    fn apply(&mut self, cmds: &[Command]) {
        for c in cmds {
            match *c {
                Command::Maximize(w) => {
                    if let Some(s) = self.windows.get_mut(&w) {
                        if *s != WinState::Closed {
                            *s = WinState::Maximized;
                        }
                    }
                }
                Command::RestorePrinter => self.printer_ok = true,
                Command::RestoreDesktop => {
                    if self.desktop == Desktop::Drifted {
                        self.desktop = Desktop::Ok;
                    }
                }
                Command::ApplyPolicies => self.policies_ok = true,
                Command::RemovePolicies => self.policies_ok = false,
            }
        }
    }
}

impl World for FakeWorld {
    fn window(&self, w: WinId) -> WinState {
        self.windows.get(&w).copied().unwrap_or(WinState::Closed)
    }
    fn open_windows(&self) -> Vec<WinId> {
        self.windows
            .iter()
            .filter(|(_, s)| **s != WinState::Closed)
            .map(|(w, _)| *w)
            .collect()
    }
    fn printer_ok(&self) -> bool {
        self.printer_ok
    }
    fn desktop(&self) -> Desktop {
        self.desktop
    }
    fn policies_ok(&self) -> bool {
        self.policies_ok
    }
}

const NONE: Vec<Command> = Vec::new();

fn started(world: &FakeWorld) -> Running {
    Booting::new(Config::DEFAULT).start(world).0
}

#[test]
fn start_applies_policies_and_fixes_everything() {
    let mut world = FakeWorld::tidy()
        .with(1, WinState::Minimized)
        .with(2, WinState::Fullscreen);
    world.printer_ok = false;
    world.desktop = Desktop::Drifted;
    let (r, cmds) = Booting::new(Config::DEFAULT).start(&world);
    assert_eq!(r.mode(), Mode::Enforcing);
    assert_eq!(
        cmds,
        vec![
            Command::ApplyPolicies,
            Command::RestorePrinter,
            Command::RestoreDesktop,
            Command::Maximize(WinId(1)),
        ]
    );
}

#[test]
fn minimized_window_is_maximized_until_strikes_run_out() {
    let world = FakeWorld::tidy().with(7, WinState::Minimized);
    let mut r = started(&FakeWorld::tidy());
    for _ in 0..Config::DEFAULT.max_strikes {
        assert_eq!(
            r.handle(Event::WindowChanged(WinId(7)), &world),
            vec![Command::Maximize(WinId(7))]
        );
    }
    // An app that keeps fighting is left alone until the next tick.
    assert_eq!(r.handle(Event::WindowChanged(WinId(7)), &world), NONE);
    assert_eq!(
        r.handle(Event::Tick, &world),
        vec![Command::Maximize(WinId(7))]
    );
}

#[test]
fn fullscreen_and_dragging_are_left_alone() {
    let mut r = started(&FakeWorld::tidy());
    for s in [
        WinState::Fullscreen,
        WinState::Moving,
        WinState::Maximized,
        WinState::Closed,
    ] {
        let world = FakeWorld::tidy().with(1, s);
        assert_eq!(
            r.handle(Event::WindowChanged(WinId(1)), &world),
            NONE,
            "{s:?}"
        );
        assert_eq!(r.handle(Event::Tick, &world), NONE, "{s:?}");
    }
}

#[test]
fn closed_window_forgets_its_strikes() {
    let mut r = started(&FakeWorld::tidy());
    let w = WinId(3);
    r.handle(
        Event::WindowChanged(w),
        &FakeWorld::tidy().with(3, WinState::Normal),
    );
    assert_eq!(r.strikes(w), 1);
    r.handle(Event::WindowChanged(w), &FakeWorld::tidy());
    assert_eq!(r.strikes(w), 0);
}

#[test]
fn pause_lifts_policies_ignores_drift_and_times_out() {
    let mut world = FakeWorld::tidy();
    let mut r = started(&world);
    assert_eq!(
        r.handle(Event::Hotkey, &world),
        vec![Command::RemovePolicies]
    );
    world.policies_ok = false;
    world.printer_ok = false;
    world.windows.insert(WinId(1), WinState::Minimized);
    for left in (1..Config::DEFAULT.pause_ticks).rev() {
        assert_eq!(r.handle(Event::PrinterChanged, &world), NONE);
        assert_eq!(r.handle(Event::WindowChanged(WinId(1)), &world), NONE);
        assert_eq!(r.handle(Event::Tick, &world), NONE);
        assert_eq!(r.mode(), Mode::Paused { ticks_left: left });
    }
    let cmds = r.handle(Event::Tick, &world);
    assert_eq!(r.mode(), Mode::Enforcing);
    assert_eq!(
        cmds,
        vec![
            Command::ApplyPolicies,
            Command::RestorePrinter,
            Command::Maximize(WinId(1))
        ]
    );
}

#[test]
fn hotkey_toggles_pause() {
    let world = FakeWorld::tidy();
    let mut r = started(&world);
    r.handle(Event::Hotkey, &world);
    assert!(matches!(r.mode(), Mode::Paused { .. }));
    assert_eq!(
        r.handle(Event::Hotkey, &world),
        vec![Command::ApplyPolicies]
    );
    assert_eq!(r.mode(), Mode::Enforcing);
}

#[test]
fn single_notifications_fix_only_their_subject() {
    let mut world = FakeWorld::tidy();
    world.printer_ok = false;
    world.desktop = Desktop::Drifted;
    world.policies_ok = false;
    let mut r = started(&FakeWorld::tidy());
    assert_eq!(
        r.handle(Event::PrinterChanged, &world),
        vec![Command::RestorePrinter]
    );
    assert_eq!(
        r.handle(Event::DesktopChanged, &world),
        vec![Command::RestoreDesktop]
    );
    assert_eq!(
        r.handle(Event::PoliciesChanged, &world),
        vec![Command::ApplyPolicies]
    );
    world.desktop = Desktop::Unavailable;
    assert_eq!(r.handle(Event::DesktopChanged, &world), NONE);
}

// ---------------------------------------------------------------------------
// Property tests with the production configuration.

#[derive(Debug, Clone)]
enum Step {
    SetWindow(isize, WinState),
    SetPrinter(bool),
    SetDesktop(Desktop),
    SetPolicies(bool),
    Deliver(Event),
}

fn win_state() -> impl Strategy<Value = WinState> {
    prop_oneof![
        Just(WinState::Closed),
        Just(WinState::Maximized),
        Just(WinState::Fullscreen),
        Just(WinState::Minimized),
        Just(WinState::Normal),
        Just(WinState::Moving),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0..5isize, win_state()).prop_map(|(w, s)| Step::SetWindow(w, s)),
        1 => any::<bool>().prop_map(Step::SetPrinter),
        1 => prop_oneof![Just(Desktop::Ok), Just(Desktop::Drifted), Just(Desktop::Unavailable)].prop_map(Step::SetDesktop),
        1 => any::<bool>().prop_map(Step::SetPolicies),
        6 => prop_oneof![
            (0..5isize).prop_map(|w| Event::WindowChanged(WinId(w))),
            Just(Event::PrinterChanged),
            Just(Event::DesktopChanged),
            Just(Event::PoliciesChanged),
            Just(Event::Hotkey),
            Just(Event::Tick),
            Just(Event::Tick),
        ].prop_map(Step::Deliver),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn safety_holds_with_production_config(steps in proptest::collection::vec(step(), 0..400)) {
        let cfg = Config::DEFAULT;
        let mut world = FakeWorld::tidy();
        let (mut r, cmds) = Booting::new(cfg).start(&world);
        world.apply(&cmds);
        let mut paused_ticks = 0u32;
        for s in steps {
            match s {
                Step::SetWindow(w, st) => { world.windows.insert(WinId(w), st); }
                Step::SetPrinter(ok) => world.printer_ok = ok,
                Step::SetDesktop(d) => world.desktop = d,
                Step::SetPolicies(ok) => world.policies_ok = ok,
                Step::Deliver(ev) => {
                    let before = world.clone();
                    let was_paused = matches!(r.mode(), Mode::Paused { .. });
                    let cmds = r.handle(ev, &before);
                    // Commands are unique and never contradictory.
                    let mut sorted = cmds.clone();
                    sorted.sort_unstable();
                    sorted.dedup();
                    prop_assert_eq!(sorted.len(), cmds.len());
                    prop_assert!(!(cmds.contains(&Command::ApplyPolicies) && cmds.contains(&Command::RemovePolicies)));
                    // Only drifted windows are maximized.
                    for c in &cmds {
                        if let Command::Maximize(w) = c {
                            prop_assert!(before.window(*w).drifted());
                        }
                    }
                    match r.mode() {
                        Mode::Paused { ticks_left } => {
                            prop_assert!(cmds.iter().all(|c| *c == Command::RemovePolicies));
                            prop_assert!(ticks_left >= 1 && ticks_left <= cfg.pause_ticks);
                            if was_paused && ev == Event::Tick { paused_ticks += 1; } else if !was_paused { paused_ticks = 0; }
                            prop_assert!(paused_ticks < cfg.pause_ticks);
                        }
                        Mode::Enforcing => {
                            paused_ticks = 0;
                            if ev == Event::Tick {
                                // A sweep leaves nothing drifted.
                                let mut after = before.clone();
                                after.apply(&cmds);
                                prop_assert!(after.printer_ok && after.policies_ok);
                                prop_assert!(after.desktop != Desktop::Drifted);
                                prop_assert!(after.open_windows().iter().all(|w| !after.window(*w).drifted()));
                            }
                        }
                    }
                    for w in 0..5 {
                        prop_assert!(r.strikes(WinId(w)) <= cfg.max_strikes);
                    }
                    world.apply(&cmds);
                }
            }
        }
    }
}
