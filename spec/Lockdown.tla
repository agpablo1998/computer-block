------------------------------ MODULE Lockdown ------------------------------
(***************************************************************************)
(* Formal model of the computer-block enforcer.                             *)
(*                                                                         *)
(* The model splits the universe into three parts:                         *)
(*   world     what Windows really looks like right now                    *)
(*   program   what lives in the enforcer's memory                         *)
(*   plumbing  the message queue, the timer flag and window commands that   *)
(*             were issued but not yet processed by the target app          *)
(*                                                                         *)
(* Pessimistic assumptions, on purpose:                                    *)
(*   - Any change notification may be LOST (registry watch not re-armed,    *)
(*     queue full, desktop icon size has no notification at all).           *)
(*   - Window commands are ASYNC (ShowWindowAsync): the user can act again  *)
(*     before the app processes them.                                       *)
(*   - The enforcer can CRASH at any moment and is restarted at next login. *)
(*   - Explorer can crash and come back with a different desktop view.      *)
(*                                                                         *)
(* The program actions (HandleStart, HandleTick, HandleEvent) are exactly   *)
(* the decisions made by src/core.rs. The Rust test suite checks every      *)
(* program transition in the reachable state graph against the Rust code.   *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Win,          \* the target windows (Word, Excel, PowerPoint, Chrome)
    None,         \* placeholder for events/commands without a window
    MaxStrikes,   \* corrections allowed per window per tick period
    PauseTicks,   \* length of an admin pause, in timer ticks
    QueueMax      \* capacity of the message queue

ASSUME MaxStrikes \in Nat \ {0}
ASSUME PauseTicks \in Nat \ {0}
ASSUME QueueMax \in Nat \ {0}
ASSUME None \notin Win

WinStates == {"Closed", "Max", "Full", "Min", "Normal", "Moving"}
Drifted   == {"Min", "Normal"}          \* states the enforcer must correct
Modes     == {"Down", "Starting", "Enforcing", "Paused"}
Running   == {"Enforcing", "Paused"}    \* hooks installed, timer armed

WinEv(w)  == [k |-> "Win", w |-> w]
Ev(kind)  == [k |-> kind, w |-> None]
Events    == {WinEv(w) : w \in Win}
             \cup {Ev(k) : k \in {"Printer", "Desktop", "Policy", "Hotkey"}}

MaxCmd(w)     == [k |-> "Maximize", w |-> w]
SetPrinterCmd == [k |-> "SetPrinter", w |-> None]
SetDesktopCmd == [k |-> "SetDesktop", w |-> None]
ApplyPolCmd   == [k |-> "ApplyPolicies", w |-> None]
RemovePolCmd  == [k |-> "RemovePolicies", w |-> None]
\* The complete list of things the enforcer can ever do. There is no command
\* that closes, hides, minimizes or moves a window, or that touches input.
Commands == {MaxCmd(w) : w \in Win}
            \cup {SetPrinterCmd, SetDesktopCmd, ApplyPolCmd, RemovePolCmd}

VARIABLES
    wstate, printer, desktop, policies,   \* world
    mode, pauseLeft, strikes,             \* program memory
    queue, tickPending, pending,          \* plumbing
    issued                                \* commands issued by the last step

world   == <<wstate, printer, desktop, policies>>
program == <<mode, pauseLeft, strikes>>
vars    == <<wstate, printer, desktop, policies, mode, pauseLeft, strikes,
             queue, tickPending, pending, issued>>

TypeOK ==
    /\ wstate \in [Win -> WinStates]
    /\ printer \in {"Ok", "Drift"}
    /\ desktop \in {"Ok", "Drift", "Down"}
    /\ policies \in {"On", "Off"}
    /\ mode \in Modes
    /\ pauseLeft \in 0..PauseTicks
    /\ strikes \in [Win -> 0..MaxStrikes]
    /\ queue \in Seq(Events) /\ Len(queue) <= QueueMax
    /\ tickPending \in BOOLEAN
    /\ pending \subseteq Win
    /\ issued \subseteq Commands

Init ==
    \* Windows boots in any state at all; nothing is assumed tidy.
    /\ wstate \in [Win -> WinStates \ {"Moving"}]
    /\ printer \in {"Ok", "Drift"}
    /\ desktop \in {"Ok", "Drift"}
    /\ policies \in {"On", "Off"}
    /\ mode = "Down"
    /\ pauseLeft = 0
    /\ strikes = [w \in Win |-> 0]
    /\ queue = <<>>
    /\ tickPending = FALSE
    /\ pending = {}
    /\ issued = {}

-----------------------------------------------------------------------------
(* Program: the decisions implemented in src/core.rs *)

\* Effects of issued commands. Registry, printer and desktop writes are
\* synchronous; Maximize is posted to the app and lands later (ApplyMax).
Exec(cmds) ==
    /\ issued'   = cmds
    /\ printer'  = IF SetPrinterCmd \in cmds THEN "Ok" ELSE printer
    /\ desktop'  = IF SetDesktopCmd \in cmds /\ desktop = "Drift"
                   THEN "Ok" ELSE desktop
    /\ policies' = IF ApplyPolCmd \in cmds THEN "On"
                   ELSE IF RemovePolCmd \in cmds THEN "Off" ELSE policies
    /\ pending'  = pending \cup {c.w : c \in {d \in cmds : d.k = "Maximize"}}

\* A full sweep: correct everything that has drifted. Strikes restart.
SweepCmds ==
    {MaxCmd(w) : w \in {v \in Win : wstate[v] \in Drifted}}
    \cup (IF printer = "Drift" THEN {SetPrinterCmd} ELSE {})
    \cup (IF desktop = "Drift" THEN {SetDesktopCmd} ELSE {})
    \cup (IF policies = "Off" THEN {ApplyPolCmd} ELSE {})
SweepStrikes == [w \in Win |-> IF wstate[w] \in Drifted THEN 1 ELSE 0]

Sweep ==
    /\ strikes' = SweepStrikes
    /\ Exec(SweepCmds)
    /\ UNCHANGED <<mode, pauseLeft>>

\* Leave a pause (by timeout or admin hotkey) or finish starting up.
Resume ==
    /\ mode' = "Enforcing"
    /\ pauseLeft' = 0
    /\ strikes' = SweepStrikes
    /\ Exec(SweepCmds \cup {ApplyPolCmd})

Pause ==
    /\ mode' = "Paused"
    /\ pauseLeft' = PauseTicks
    /\ strikes' = [w \in Win |-> 0]
    /\ Exec({RemovePolCmd})

Idle ==
    /\ Exec({})
    /\ UNCHANGED program

HandleStart ==
    /\ mode = "Starting"
    /\ Resume
    /\ UNCHANGED <<wstate, queue, tickPending>>

HandleTick ==
    /\ tickPending
    /\ mode \in Running
    /\ tickPending' = FALSE
    /\ CASE mode = "Enforcing"                 -> Sweep
         [] mode = "Paused" /\ pauseLeft > 1   ->
                /\ pauseLeft' = pauseLeft - 1
                /\ Exec({})
                /\ UNCHANGED <<mode, strikes>>
         [] mode = "Paused" /\ pauseLeft = 1   -> Resume
    /\ UNCHANGED <<wstate, queue>>

HandleWindow(w) ==
    IF wstate[w] \in Drifted /\ strikes[w] < MaxStrikes
    THEN /\ strikes' = [strikes EXCEPT ![w] = @ + 1]
         /\ Exec({MaxCmd(w)})
         /\ UNCHANGED <<mode, pauseLeft>>
    ELSE IF wstate[w] = "Closed"
    THEN /\ strikes' = [strikes EXCEPT ![w] = 0]
         /\ Exec({})
         /\ UNCHANGED <<mode, pauseLeft>>
    ELSE Idle   \* Max, Full, Moving, or out of strikes: leave it alone

HandleEvent ==
    /\ queue # <<>>
    /\ mode \in Running
    /\ queue' = Tail(queue)
    /\ LET e == Head(queue) IN
       CASE e.k = "Hotkey" /\ mode = "Enforcing" -> Pause
         [] e.k = "Hotkey" /\ mode = "Paused"    -> Resume
         [] mode = "Paused"                      -> Idle
         [] e.k = "Win"     -> HandleWindow(e.w)
         [] e.k = "Printer" ->
                /\ Exec(IF printer = "Drift" THEN {SetPrinterCmd} ELSE {})
                /\ UNCHANGED program
         [] e.k = "Desktop" ->
                /\ Exec(IF desktop = "Drift" THEN {SetDesktopCmd} ELSE {})
                /\ UNCHANGED program
         [] e.k = "Policy"  ->
                /\ Exec(IF policies = "Off" THEN {ApplyPolCmd} ELSE {})
                /\ UNCHANGED program
    /\ UNCHANGED <<wstate, tickPending>>

-----------------------------------------------------------------------------
(* Environment: the user, the apps, Windows, Explorer, the OS scheduler *)

\* A change notification. It is only delivered while hooks are installed,
\* and even then it may be lost.
Notify(e) ==
    IF mode \in Running /\ Len(queue) < QueueMax
    THEN queue' \in {queue, Append(queue, e)}
    ELSE queue' = queue

EnvStep == issued' = {} /\ UNCHANGED <<program, tickPending>>

WinChange(w, from, to) ==
    /\ wstate[w] \in from
    /\ wstate' = [wstate EXCEPT ![w] = to]
    /\ Notify(WinEv(w))
    /\ EnvStep
    /\ UNCHANGED <<printer, desktop, policies, pending>>

AppOpen(w)     == \E to \in {"Max", "Normal"} : WinChange(w, {"Closed"}, to)
AppClose(w)    == WinChange(w, WinStates \ {"Closed"}, "Closed")
Minimize(w)    == WinChange(w, {"Max", "Normal", "Full"}, "Min")
Unminimize(w)  == \E to \in {"Max", "Normal"} : WinChange(w, {"Min"}, to)
Unmaximize(w)  == WinChange(w, {"Max"}, "Normal")   \* dbl-click, Win+Down, snap
EnterFull(w)   == WinChange(w, {"Max", "Normal"}, "Full")
ExitFull(w)    == \E to \in {"Max", "Normal"} : WinChange(w, {"Full"}, to)
DragStart(w)   == WinChange(w, {"Max", "Normal"}, "Moving")
DragEnd(w)     == \E to \in {"Max", "Normal"} : WinChange(w, {"Moving"}, to)

\* The app finally processes ShowWindowAsync(SW_MAXIMIZE).
ApplyMax(w) ==
    /\ w \in pending
    /\ pending' = pending \ {w}
    /\ wstate' = [wstate EXCEPT ![w] = IF @ = "Closed" THEN "Closed" ELSE "Max"]
    /\ Notify(WinEv(w))
    /\ EnvStep
    /\ UNCHANGED <<printer, desktop, policies>>

PrinterDrift ==
    /\ printer = "Ok" /\ printer' = "Drift"
    /\ Notify(Ev("Printer"))
    /\ EnvStep /\ UNCHANGED <<wstate, desktop, policies, pending>>

DesktopDrift ==
    /\ desktop = "Ok" /\ desktop' = "Drift"
    /\ Notify(Ev("Desktop"))
    /\ EnvStep /\ UNCHANGED <<wstate, printer, policies, pending>>

ExplorerCrash ==
    /\ desktop \in {"Ok", "Drift"} /\ desktop' = "Down"
    /\ queue' = queue
    /\ EnvStep /\ UNCHANGED <<wstate, printer, policies, pending>>

ExplorerBack ==
    /\ desktop = "Down" /\ desktop' \in {"Ok", "Drift"}
    /\ Notify(Ev("Desktop"))
    /\ EnvStep /\ UNCHANGED <<wstate, printer, policies, pending>>

PolicyReset ==   \* e.g. a Windows feature update rewrites the policy keys
    /\ policies = "On" /\ policies' = "Off"
    /\ Notify(Ev("Policy"))
    /\ EnvStep /\ UNCHANGED <<wstate, printer, desktop, pending>>

Hotkey ==
    /\ mode \in Running /\ Len(queue) < QueueMax
    /\ queue' = Append(queue, Ev("Hotkey"))
    /\ EnvStep /\ UNCHANGED <<world, pending>>

Timer ==   \* WM_TIMER is a flag, never queued and never lost
    /\ mode \in Running /\ ~tickPending
    /\ tickPending' = TRUE
    /\ issued' = {}
    /\ UNCHANGED <<world, program, queue, pending>>

Crash ==
    /\ mode # "Down"
    /\ mode' = "Down"
    /\ pauseLeft' = 0
    /\ strikes' = [w \in Win |-> 0]
    /\ queue' = <<>>
    /\ tickPending' = FALSE
    /\ issued' = {}
    /\ UNCHANGED <<world, pending>>

Login ==
    /\ mode = "Down"
    /\ mode' = "Starting"
    /\ issued' = {}
    /\ UNCHANGED <<world, pauseLeft, strikes, queue, tickPending, pending>>

Program == HandleStart \/ HandleTick \/ HandleEvent

Environment ==
    \/ \E w \in Win :
          \/ AppOpen(w) \/ AppClose(w) \/ Minimize(w) \/ Unminimize(w)
          \/ Unmaximize(w) \/ EnterFull(w) \/ ExitFull(w)
          \/ DragStart(w) \/ DragEnd(w) \/ ApplyMax(w)
    \/ PrinterDrift \/ DesktopDrift \/ ExplorerCrash \/ ExplorerBack
    \/ PolicyReset \/ Hotkey \/ Timer \/ Crash \/ Login

Next == Program \/ Environment

\* Only the enforcer's own steps, the OS timer, login and the apps processing
\* posted messages are assumed to eventually happen. The user, Explorer,
\* Windows Update and crashes get no fairness at all: they may do anything,
\* forever, or nothing.
Fairness ==
    /\ WF_vars(HandleStart)
    /\ WF_vars(HandleTick)
    /\ WF_vars(HandleEvent)
    /\ WF_vars(Timer)
    /\ WF_vars(Login)
    /\ \A w \in Win : WF_vars(ApplyMax(w))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* Safety: things that must never happen *)

\* A pause always has a deadline, and no deadline exists outside a pause.
PauseBounded ==
    /\ mode = "Paused" => pauseLeft \in 1..PauseTicks
    /\ mode # "Paused" => pauseLeft = 0

\* While paused, the enforcer does nothing except lift the policies.
QuietWhilePaused == mode = "Paused" => issued \subseteq {RemovePolCmd}

\* A Maximize is only ever issued for a window seen minimized or restored:
\* never for a closed window, a fullscreen video or slideshow, or a window
\* the user is in the middle of dragging (e.g. a Chrome tab being torn off).
OnlyMaximizeDrifted ==
    \A c \in issued : c.k = "Maximize" => wstate[c.w] \in Drifted

\* No fight loop: an app that keeps un-maximizing itself gets at most
\* MaxStrikes corrections per tick period, then is left alone until the
\* next tick. Bounded CPU, no flicker storm.
StrikesBounded == \A w \in Win : strikes[w] <= MaxStrikes

\* The enforcer is never in two minds about policies in one step.
NoContradictoryCommands == ~(ApplyPolCmd \in issued /\ RemovePolCmd \in issued)

-----------------------------------------------------------------------------
(* Liveness: things that must eventually happen *)

\* Every pause ends (unless the program dies, after which login restarts it).
PauseEnds == (mode = "Paused") ~> (mode \in {"Enforcing", "Down"})

StartupCompletes == (mode = "Starting") ~> (mode \in {"Enforcing", "Down"})

\* Even with every notification lost, drift is corrected.
WindowsCorrected ==
    \A w \in Win :
        (mode = "Enforcing" /\ wstate[w] \in Drifted)
            ~> (mode # "Enforcing" \/ wstate[w] \notin Drifted
                \/ strikes[w] = MaxStrikes)

PrinterCorrected ==
    (mode = "Enforcing" /\ printer = "Drift")
        ~> (mode # "Enforcing" \/ printer = "Ok")

DesktopCorrected ==
    (mode = "Enforcing" /\ desktop = "Drift")
        ~> (mode # "Enforcing" \/ desktop # "Drift")

PoliciesCorrected ==
    (mode = "Enforcing" /\ policies = "Off")
        ~> (mode # "Enforcing" \/ policies = "On")

=============================================================================
