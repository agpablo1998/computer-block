# computer-block

A tiny hidden Windows program that keeps one user account looking exactly the
way it was set up: Word, Excel, PowerPoint and Chrome stay maximized, the
default printer stays put, the desktop icons keep their size, and Windows
settings can't be changed by accident.

* One `.exe`, no installer, no admin rights, no window, no tray icon.
* Event-driven: it sleeps until Windows tells it something changed.
* Never touches the keyboard or mouse, so it can never freeze the screen.

## For the admin

1. Set the computer up the way it should stay (default printer, desktop icon
   size, icons visible).
2. Download `computer-block.exe` from Releases and check it (optional):
   `gh attestation verify computer-block.exe --repo agpablo1998/computer-block`
3. Run `computer-block.exe --setup`. It saves the current state as "normal",
   copies itself to `%LOCALAPPDATA%\ComputerBlock`, registers itself to start
   at sign-in and starts. Sign out and back in once.

When you visit:

| Want to | Do |
|---|---|
| Make changes | Press **Ctrl+Alt+Shift+F12** (two-tone beep). Locks lift for 30 minutes; press again to resume early. |
| Keep a new printer or icon size as normal | While paused, run `%LOCALAPPDATA%\ComputerBlock\computer-block.exe --capture` (Start menu search: `cmd`). |
| Remove everything | `%LOCALAPPDATA%\ComputerBlock\computer-block.exe --uninstall`, then sign out and in. |
| See what it did | `%LOCALAPPDATA%\ComputerBlock\log.txt` |

## How it works

Three independent layers. Each one still works if the one above it fails.

**1. Windows' own locks (per-user policies, no admin needed).** Written once,
enforced by Windows itself, and still active even if the program is not
running:

| Policy | Effect | Lifted during admin pause |
|---|---|---|
| `NoControlPanel` | Settings app and Control Panel blocked | yes |
| `NoSetTaskbar` | Taskbar settings blocked | yes |
| `NoViewContextMenu` | No right-click menu on the desktop (the View menu) or in folders | yes |
| `DisableTaskMgr` | Task Manager blocked | yes |
| `NoWinKeys` | Win+D, Win+M, Win+arrows, Win+I and every other Win shortcut off | no (read at sign-in) |
| `LegacyDefaultPrinterMode` | "Let Windows manage my default printer" off | no |

**2. The enforcer (this program).** Listens for window, registry and Explorer
events and puts things back:

* A target window that is minimized or un-maximized (title-bar double-click,
  drag, Snap, taskbar click, touchpad gesture, Aero Shake, "Show desktop" ...)
  is maximized again. It leaves alone windows that are fullscreen (video, F11,
  slideshow) or being dragged (a Chrome tab being torn off).
* The default printer, desktop icon view and the locks above are restored
  the moment they change.
* An app that fights back gets 3 corrections a minute, then is left alone
  until the next sweep. No flicker storms, no CPU spikes.

**3. The one-minute sweep.** Every change notification can be lost (Windows
gives none at all for desktop icon size). A sweep once a minute corrects
anything that slipped through. The model below proves that correction
depends only on this sweep, not on any notification arriving.

## Why it is built this way

The original plan put a low-level keyboard and mouse hook at the center:
every click and key press would pass through this program, which would drop
clicks on minimize buttons by computing where those buttons are. That design
was dropped, because:

* **It is the only design that can freeze her computer.** If a low-level
  hook stalls, every keystroke waits for it. Windows then silently removes the
  hook, which turns the protection off without telling anyone.
* **It misses most of the ways windows get un-maximized**: clicking the active
  app's taskbar button, the "Show desktop" corner, touchpad three-finger
  swipe, Alt+Space menu, Snap layouts, Aero Shake. Reacting to the window's
  *state* catches all of them, including ones not invented yet.
* **Pixel positions for Chrome's buttons break** with display scaling, themes
  and Chrome redesigns.
* **It looks like a keylogger.** A hidden, auto-starting, unsigned program
  with a global keyboard hook that disables Task Manager matches antivirus
  heuristics almost exactly. Windows Defender would likely quarantine it.
* Windows already has policies that block most of the keys at the source
  (`NoWinKeys`, `DisableTaskMgr`, `NoViewContextMenu`).

Also dropped: **self-update from GitHub.** A program that downloads and runs
code at every sign-in turns any compromise of the GitHub account into code
running on her computer. Updates are installed by the admin, after checking
the attestation.

## Verification

| Evidence | What it shows | Where |
|---|---|---|
| TLC model checker, about 2M states | Safety and liveness properties hold in every reachable state, assuming any notification can be lost, the program can crash at any moment, Explorer can restart and window commands land late | `spec/Lockdown.tla`, `spec/MC.cfg` |
| Model conformance test | The Rust core makes the same decision as the model in every program transition of the model's full state graph (9,540 transitions with one window, all of them with two windows in CI). Three planted bugs (off-by-one strike limit, late pause timeout, stale strikes) were each caught. | `src/core/conformance.rs`, `scripts/check-model.sh` |
| Property tests | Same safety rules with production numbers (3 strikes, 30-minute pause, 5 windows), 2,000 random runs | `src/core/tests.rs` |
| End-to-end smoke test on a real Windows desktop | Re-maximize, fullscreen respect, fight-loop guard, lock and printer restore, real admin hotkey, idle CPU and memory, clean uninstall | `examples/smoke.rs`, CI job `windows` |
| `clippy` (pedantic, no `unwrap`/`panic`/unchecked indexing), `cargo audit` | Code hygiene and known-vulnerable dependencies | CI |

Proven properties (`spec/Lockdown.tla`):

* **PauseBounded**: a pause always has a deadline. **PauseEnds**: it ends.
* **QuietWhilePaused**: while paused, the program does nothing but lift locks.
* **OnlyMaximizeDrifted**: it only ever maximizes a window seen minimized or
  restored: never a closed, fullscreen or dragged window.
* **StrikesBounded**: no fight loop.
* **WindowsCorrected / PrinterCorrected / DesktopCorrected /
  PoliciesCorrected**: any drift is eventually corrected.
* The program has no command that closes, hides or minimizes anything, or
  that touches input. That is a property of its type, not of a test.

Safety is no longer about "never block all input": no input is filtered, so
that whole class of failure cannot happen.

## Code map

```
spec/Lockdown.tla      formal model (the source of truth for behavior)
src/core/              pure state machine, #![forbid(unsafe_code)], no Windows calls
src/win/               Windows runtime: the only place with unsafe code
  runtime.rs           message loop, WinEvent hooks, registry watches, timer, hotkey
  targets.rs           which windows count, and their state
  registry.rs          the locks and the Run entry
  printer.rs desktop.rs
examples/smoke.rs      end-to-end test on a real desktop
```

## Known limits

* Windows has no system-wide "default scanner". Scanning apps (HP Smart,
  Windows Scan) each remember their own device. Not handled yet.
* A UAC prompt runs on a secure desktop that no normal program can touch.
* The desktop-icon check runs when she arrives on or leaves the desktop, and
  once a minute.
* Per-user policies can be removed by anyone with a command prompt in her
  account. The program puts them back within a second. It protects against
  accidents, not against a determined person.
* Unsigned binaries trigger a SmartScreen prompt on first run. Code signing
  (e.g. Azure Trusted Signing) removes it.
