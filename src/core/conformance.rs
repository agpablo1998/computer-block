//! Model-based conformance: the Rust core against the TLA+ model.
//!
//! TLC dumps every reachable state of `spec/Lockdown.tla` and every
//! transition between them (`tlc -dump dot,actionlabels`). For every
//! transition taken by the program (`HandleStart`, `HandleTick`,
//! `HandleEvent`) this test rebuilds the Rust core from the source state,
//! feeds it the same event and world, and requires the exact same result:
//! same mode, same pause deadline, same strike counts, same set of commands.
//! It also requires that every state in which the model's program can act
//! has such a transition, so no case is skipped silently.
//!
//! Run `scripts/check-model.sh` (or see CI) to produce the dump, then:
//! `LOCKDOWN_STATE_GRAPH=target/state-graph.dot cargo test conformance`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::{Booting, Command, Config, Desktop, Event, Mode, Running, WinId, WinState, World};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader};

/// Must match `MaxStrikes` / `PauseTicks` in spec/Conformance.cfg and MC.cfg.
const MODEL_CFG: Config = Config {
    max_strikes: 2,
    pause_ticks: 2,
};

// ---------------------------------------------------------------------------
// A small parser for the TLA+ values TLC prints.

#[derive(Debug, Clone, PartialEq)]
enum V {
    Str(String),
    Int(i64),
    Bool(bool),
    Model(String),
    Set(Vec<V>),
    Seq(Vec<V>),
    Rec(Vec<(String, V)>),
    Fun(Vec<(V, V)>),
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn eat(&mut self, t: &str) -> bool {
        self.ws();
        if self.s[self.i..].starts_with(t.as_bytes()) {
            self.i += t.len();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &str) {
        assert!(self.eat(t), "expected {t:?} at {}", self.rest());
    }
    fn rest(&self) -> String {
        String::from_utf8_lossy(&self.s[self.i..]).into_owned()
    }
    fn ident(&mut self) -> String {
        self.ws();
        let start = self.i;
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_')
        {
            self.i += 1;
        }
        assert!(self.i > start, "expected identifier at {}", self.rest());
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }
    fn list(&mut self, close: &str) -> Vec<V> {
        let mut out = Vec::new();
        if self.eat(close) {
            return out;
        }
        loop {
            out.push(self.value());
            if self.eat(close) {
                return out;
            }
            self.expect(",");
        }
    }
    fn value(&mut self) -> V {
        self.ws();
        if self.eat("\"") {
            let start = self.i;
            while self.s[self.i] != b'"' {
                self.i += 1;
            }
            let v = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
            self.i += 1;
            return V::Str(v);
        }
        if self.eat("<<") {
            return V::Seq(self.list(">>"));
        }
        if self.eat("{") {
            return V::Set(self.list("}"));
        }
        if self.eat("[") {
            let mut fields = Vec::new();
            loop {
                let k = self.ident();
                self.expect("|->");
                fields.push((k, self.value()));
                if self.eat("]") {
                    return V::Rec(fields);
                }
                self.expect(",");
            }
        }
        if self.eat("(") {
            let mut pairs = Vec::new();
            loop {
                let k = self.value();
                self.expect(":>");
                pairs.push((k, self.value()));
                if self.eat(")") {
                    return V::Fun(pairs);
                }
                self.expect("@@");
            }
        }
        if self.s[self.i].is_ascii_digit() || self.s[self.i] == b'-' {
            let start = self.i;
            self.i += 1;
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
            return V::Int(
                std::str::from_utf8(&self.s[start..self.i])
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
        }
        match self.ident().as_str() {
            "TRUE" => V::Bool(true),
            "FALSE" => V::Bool(false),
            other => V::Model(other.to_owned()),
        }
    }
}

fn parse_value(s: &str) -> V {
    let mut p = P {
        s: s.as_bytes(),
        i: 0,
    };
    let v = p.value();
    p.ws();
    assert_eq!(p.i, s.len(), "trailing input in {s:?}");
    v
}

impl V {
    fn str(&self) -> &str {
        match self {
            V::Str(s) | V::Model(s) => s,
            other => panic!("not a string: {other:?}"),
        }
    }
    fn int(&self) -> i64 {
        match self {
            V::Int(i) => *i,
            other => panic!("not an int: {other:?}"),
        }
    }
    fn field(&self, name: &str) -> &V {
        match self {
            V::Rec(f) => &f.iter().find(|(k, _)| k == name).unwrap().1,
            other => panic!("not a record: {other:?}"),
        }
    }
    fn items(&self) -> &[V] {
        match self {
            V::Set(v) | V::Seq(v) => v,
            other => panic!("not a collection: {other:?}"),
        }
    }
    fn pairs(&self) -> &[(V, V)] {
        match self {
            V::Fun(p) => p,
            other => panic!("not a function: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Model states.

fn win_id(name: &str) -> WinId {
    // w1 -> 1, w2 -> 2, ...
    WinId(name.trim_start_matches('w').parse().unwrap())
}

#[derive(Debug, Clone)]
struct ModelState {
    wstate: BTreeMap<WinId, WinState>,
    printer_ok: bool,
    desktop: Desktop,
    policies_ok: bool,
    mode: String,
    pause_left: u32,
    strikes: BTreeMap<WinId, u8>,
    queue: Vec<Event>,
    tick_pending: bool,
    issued: BTreeSet<Command>,
}

fn win_state(s: &str) -> WinState {
    match s {
        "Closed" => WinState::Closed,
        "Max" => WinState::Maximized,
        "Full" => WinState::Fullscreen,
        "Min" => WinState::Minimized,
        "Normal" => WinState::Normal,
        "Moving" => WinState::Moving,
        other => panic!("unknown window state {other}"),
    }
}

fn event(v: &V) -> Event {
    match v.field("k").str() {
        "Win" => Event::WindowChanged(win_id(v.field("w").str())),
        "Printer" => Event::PrinterChanged,
        "Desktop" => Event::DesktopChanged,
        "Policy" => Event::PoliciesChanged,
        "Hotkey" => Event::Hotkey,
        other => panic!("unknown event {other}"),
    }
}

fn command(v: &V) -> Command {
    match v.field("k").str() {
        "Maximize" => Command::Maximize(win_id(v.field("w").str())),
        "SetPrinter" => Command::RestorePrinter,
        "SetDesktop" => Command::RestoreDesktop,
        "ApplyPolicies" => Command::ApplyPolicies,
        "RemovePolicies" => Command::RemovePolicies,
        other => panic!("unknown command {other}"),
    }
}

fn parse_state(label: &str) -> ModelState {
    // TLC wraps long values over several lines; a variable starts with "/\ ".
    let mut conjuncts: Vec<String> = Vec::new();
    for line in label.split('\n') {
        match line.strip_prefix("/\\ ") {
            Some(start) => conjuncts.push(start.to_owned()),
            None => conjuncts.last_mut().unwrap().push_str(line),
        }
    }
    let mut vars: HashMap<String, V> = HashMap::new();
    for c in &conjuncts {
        let (name, value) = c.split_once(" = ").unwrap();
        vars.insert(name.trim().to_owned(), parse_value(value.trim()));
    }
    let var = |n: &str| {
        vars.get(n)
            .unwrap_or_else(|| panic!("missing variable {n}"))
    };
    ModelState {
        wstate: var("wstate")
            .pairs()
            .iter()
            .map(|(k, v)| (win_id(k.str()), win_state(v.str())))
            .collect(),
        printer_ok: var("printer").str() == "Ok",
        desktop: match var("desktop").str() {
            "Ok" => Desktop::Ok,
            "Drift" => Desktop::Drifted,
            "Down" => Desktop::Unavailable,
            other => panic!("unknown desktop {other}"),
        },
        policies_ok: var("policies").str() == "On",
        mode: var("mode").str().to_owned(),
        pause_left: u32::try_from(var("pauseLeft").int()).unwrap(),
        strikes: var("strikes")
            .pairs()
            .iter()
            .filter(|(_, v)| v.int() != 0)
            .map(|(k, v)| (win_id(k.str()), u8::try_from(v.int()).unwrap()))
            .collect(),
        queue: var("queue").items().iter().map(event).collect(),
        tick_pending: *var("tickPending") == V::Bool(true),
        issued: var("issued").items().iter().map(command).collect(),
    }
}

impl World for ModelState {
    fn window(&self, w: WinId) -> WinState {
        self.wstate.get(&w).copied().unwrap_or(WinState::Closed)
    }
    fn open_windows(&self) -> Vec<WinId> {
        self.wstate
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

fn running_from(m: &ModelState) -> Running {
    Running {
        mode: match m.mode.as_str() {
            "Enforcing" => Mode::Enforcing,
            "Paused" => Mode::Paused {
                ticks_left: m.pause_left,
            },
            other => panic!("not a running mode: {other}"),
        },
        strikes: m.strikes.clone(),
        cfg: MODEL_CFG,
    }
}

fn check_result(edge: &str, dst: &ModelState, running: &Running, cmds: &[Command]) {
    let (mode, pause_left) = match running.mode {
        Mode::Enforcing => ("Enforcing", 0),
        Mode::Paused { ticks_left } => ("Paused", ticks_left),
    };
    let issued: BTreeSet<Command> = cmds.iter().copied().collect();
    assert_eq!(
        issued.len(),
        cmds.len(),
        "{edge}: duplicate commands {cmds:?}"
    );
    assert_eq!(mode, dst.mode, "{edge}: mode");
    assert_eq!(pause_left, dst.pause_left, "{edge}: pause deadline");
    let strikes: BTreeMap<WinId, u8> = running
        .strikes
        .iter()
        .filter(|(_, n)| **n != 0)
        .map(|(w, n)| (*w, *n))
        .collect();
    assert_eq!(strikes, dst.strikes, "{edge}: strikes");
    assert_eq!(issued, dst.issued, "{edge}: commands");
}

// ---------------------------------------------------------------------------
// Reading the dot dump.

/// Returns the content of `label="..."`, unescaped.
fn label_of(line: &str) -> Option<String> {
    let start = line.find("label=\"")? + 7;
    let bytes = line.as_bytes();
    let mut out = String::new();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                match bytes[i + 1] {
                    b'n' => out.push('\n'),
                    c => out.push(char::from(c)),
                }
                i += 2;
            }
            b'"' => return Some(out),
            c => {
                out.push(char::from(c));
                i += 1;
            }
        }
    }
    None
}

#[test]
fn rust_core_matches_tla_model() {
    let Ok(path) = std::env::var("LOCKDOWN_STATE_GRAPH") else {
        assert!(
            std::env::var_os("LOCKDOWN_REQUIRE_GRAPH").is_none(),
            "LOCKDOWN_REQUIRE_GRAPH is set but LOCKDOWN_STATE_GRAPH is not"
        );
        eprintln!("skipping: set LOCKDOWN_STATE_GRAPH to a `tlc -dump dot,actionlabels` file");
        return;
    };
    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));

    let mut nodes: HashMap<i64, ModelState> = HashMap::new();
    let mut edges: Vec<(i64, i64, String)> = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.unwrap();
        let mut parts = line.splitn(3, ' ');
        let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(src) = a.parse::<i64>() else { continue };
        if b == "->" {
            let Some(rest) = parts.next() else { continue };
            let dst: i64 = rest.split(' ').next().unwrap().parse().unwrap();
            let label = label_of(rest).unwrap_or_default();
            let action = label.split('(').next().unwrap_or("").to_owned();
            if action.starts_with("Handle") {
                edges.push((src, dst, action));
            }
        } else if let Some(label) = label_of(&line) {
            nodes.insert(src, parse_state(&label));
        }
    }
    assert!(!nodes.is_empty(), "no states in {path}");

    let mut checked: BTreeMap<String, usize> = BTreeMap::new();
    let mut has_edge: BTreeSet<(i64, &str)> = BTreeSet::new();
    for (s, t, action) in &edges {
        let src = &nodes[s];
        let dst = &nodes[t];
        let edge = format!("{action} {s} -> {t}\nfrom {src:#?}\nto {dst:#?}");
        let (running, cmds) = match action.as_str() {
            "HandleStart" => Booting::new(MODEL_CFG).start(src),
            "HandleTick" => {
                let mut r = running_from(src);
                let c = r.handle(Event::Tick, src);
                (r, c)
            }
            "HandleEvent" => {
                let mut r = running_from(src);
                let c = r.handle(src.queue[0], src);
                (r, c)
            }
            other => panic!("unexpected program action {other}"),
        };
        check_result(&edge, dst, &running, &cmds);
        *checked.entry(action.clone()).or_default() += 1;
        let name: &str = match action.as_str() {
            "HandleStart" => "start",
            "HandleTick" => "tick",
            _ => "event",
        };
        has_edge.insert((*s, name));
    }

    // Completeness: wherever the model's program is enabled, we checked it.
    for (id, m) in &nodes {
        let running = m.mode == "Enforcing" || m.mode == "Paused";
        if m.mode == "Starting" {
            assert!(
                has_edge.contains(&(*id, "start")),
                "HandleStart not checked in {m:#?}"
            );
        }
        if running && m.tick_pending {
            assert!(
                has_edge.contains(&(*id, "tick")),
                "HandleTick not checked in {m:#?}"
            );
        }
        if running && !m.queue.is_empty() {
            assert!(
                has_edge.contains(&(*id, "event")),
                "HandleEvent not checked in {m:#?}"
            );
        }
    }

    for a in ["HandleStart", "HandleTick", "HandleEvent"] {
        assert!(
            checked.get(a).copied().unwrap_or(0) > 0,
            "no {a} transitions in {path}"
        );
    }
    eprintln!("conformance: {} states, {checked:?}", nodes.len());
}

#[test]
fn value_parser_handles_tlc_output() {
    let v = parse_value(r#"(w1 :> "Max" @@ w2 :> "Min")"#);
    assert_eq!(v.pairs().len(), 2);
    let v = parse_value(r#"{[k |-> "Maximize", w |-> w1], [k |-> "SetPrinter", w |-> None]}"#);
    assert_eq!(v.items().len(), 2);
    assert_eq!(parse_value("<<>>"), V::Seq(vec![]));
    assert_eq!(parse_value("FALSE"), V::Bool(false));
}
