# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

A Windows daemon that turns a Logitech mouse button into a push-to-talk key
**the operating system cannot observe**. Not a remapper — the button is removed
from the HID input stream entirely and read back over a private side channel.

The problem it solves: Windows exposes only 5 mouse buttons and every keyboard
key is globally visible to whatever is focused, so any conventional PTT binding
collides with something (browser back-navigation, game keybinds, text fields).

## The mechanism

Three HID++ 2.0 calls, in this order:

1. `0x8100` ONBOARD PROFILES, fn1 `SetMode(Host=0x02)` — **mandatory**. The
   mapping in step 2 is silently ignored while an onboard profile is active.
2. `0x8110` MOUSE BUTTON SPY, fn4 `SetMouseButtonMapping` — one byte per
   physical button; `0` disables that button *for standard HID reports*.
3. `0x8110` fn1 `StartMouseButtonSpy` — raw physical button state now arrives
   as HID++ notifications regardless of the mapping.

Net effect: no virtual key, no window message, nothing a low-level hook can
see. Only this process knows the button was pressed.

## Verified on hardware — do not re-derive these

Confirmed on a G PRO X Superlight over its Lightspeed receiver (Fedora 44 live,
`cvuchener/hidpp` tools). These were expensive to establish:

- **Spy events fire in Onboard mode.** Host mode is only needed for the
  *mapping* to take effect, not for reading. The docs are ambiguous on this.
- **Disabling works.** With mapping `[1,2,3,0,5]`, pressing back produces
  `Button 3: pressed` from the spy and **no** `POINTER_BUTTON` in
  `libinput debug-events`. This is the core result the design rests on.
- **Spy button numbering is 0-based**: `0`=left `1`=right `2`=middle `3`=back
  `4`=forward.
- **Mapping array values are 1-based codes**, positionally indexed by physical
  button. Back is therefore the *4th byte*. `0` = disabled, `1..16` = passthrough.
- **Default mapping is `01 02 03 04 05`** — all enabled, so switching to Host
  mode will not kill the mouse.
- Feature indices *on this firmware*: `0x8100` → 7, `0x8110` → 8. **Never
  hardcode these** — they are resolved at runtime via the ROOT feature, because
  indices are not stable across firmware revisions.
- `GetMouseButtonCount` returns `05`.

Device IDs: receiver `046d:C547`, mouse WPID `4093` (wireless) / usbid `C094`
(wired). Device index `0x01` behind the receiver, `0xFF` wired.

## Protocol notes

Long report only (20 bytes):

```
[0] 0x11  report id
[1] device index
[2] feature index
[3] (function << 4) | swId
[4..20] params
```

- `swId` is `0x0A` here. Replies echo it; **notifications carry swId 0**, with
  the event index in the high nibble. That is the only way to tell a button
  event from a command reply.
- Errors come back as report `0x10` with feature index `0xFF`, followed by the
  failing request echoed back: `[3]` feature index, `[4]` `(fn << 4) | swId`,
  `[5]` error code. Both echoed bytes must match or you are reading someone
  else's error; the code is at `[5]`, not `[4]`.
- HID++ lives on a **vendor collection, usage page `0xFF00`** — not the mouse
  collection. This is precisely why it is reachable on Windows at all: Windows
  refuses to hand out mouse/keyboard collections but allows vendor ones.
- Receivers expose two vendor collections: usage `0x01` (short reports) and
  `0x02` (long). We must use `0x02`; writing 20 bytes to the short one fails.

## Build and run

```
cargo build --release
target\release\invisible-ptt.exe config.toml
```

**Exit** in the tray menu restores all buttons, stops the spy, and returns to
Onboard mode. Unplugging the receiver does the same — both settings are
volatile.

## The tray app

On Windows this is a GUI-subsystem binary with no window and no console: a
notification-area icon whose menu is the entire user interface (`src/tray.rs`).

- **The tray owns a thread, not the input loop.** `TrackPopupMenu` blocks until
  the user picks something, so it cannot share the loop — same reasoning as
  `discord::RpcHandle`, and the same consequence if ignored. Menu items only
  flip atomics (`running`, `restart`) that the loop reads within its 100ms
  event timeout, or shell out. **Nothing in tray.rs touches the mouse.**
- **`windows_subsystem = "windows"` means nothing prints anywhere**, so every
  `println!`/`eprintln!` became `log!`/`logerr!` (`src/log.rs`), which tee to
  `invisible-ptt.log` beside the config. That file is now the only account of
  what happened overnight. The attribute is `cfg`-gated off under `test`, or it
  would silence the test harness too.
- **Startup failures get a message box** (`platform::error_box`). Without one a
  bad config just means the app never appears. The *mouse* is the exception:
  `connect_when_available()` waits for it instead of dying, because at sign-in
  the receiver routinely has not enumerated yet and a modal dialog at every
  boot would be the worst of both worlds.
- **Autostart is `HKCU\...\Run`, not a scheduled task.** It needs no elevation
  and gives the interactive session the daemon requires. It cannot restart the
  daemon if it crashes, which a task's `-RestartCount` could; the README used to
  document the task as an alternative and no longer does, because two ways to
  start the same daemon is two ways to end up with two copies of it. The
  installer's tick box writes the same value, so the tray tick reflects it.
- **Reload swaps the config in place; it does not restart the process.** The
  tray reads, parses, and validates (`load_config`), then sends a `Config` down
  a channel; the input loop owns everything derived from it and rebuilds all of
  it together - `default_action`, `rules`, `bit`, the Discord worker if the
  credentials changed, and `dev.apply()`, which is also the only way to give
  back a button the previous mapping had disabled. Two properties this split
  buys: the loop never does file IO, and a rejected config is reported in a
  message box *on the tray thread*, where blocking is allowed - showing one
  from the input loop would freeze button handling until it was dismissed. A
  bad config changes nothing; the running one stays.
- **Reload obeys the held-button gate**, via `pending_reload`: it ends in the
  same `apply()` write as the periodic reassert, so it waits for no buttons
  down plus 500ms quiet. See the gotcha below.
- **`WM_CLOSE` means "stop the daemon", not "destroy the window."** Both it and
  `WM_QUERYENDSESSION` clear `running` and return without destroying anything;
  the input loop notices, restores the mouse, and only then does
  `Tray::shutdown` post the private `WM_TRAY_QUIT` that actually tears the
  window down. Inverting the default was necessary because those are the
  messages an uninstaller's Restart Manager and the shell's sign-out send, and
  under `DefWindowProcW` they would end the tray thread *alone* — leaving an
  invisible input loop still holding a button back from Windows, with the icon
  gone and no way left to ask for it. This is what lets the installer set
  `CloseApplications=yes`. The icon therefore outlives the close request by
  however long the restore takes, which is the point.
- **No single-instance guard.** Nothing stops two copies fighting over the
  mouse. Worth adding if it ever bites - the Restart handover that used to make
  a mutex awkward is gone.
- **Config path: argument, then beside the exe, then
  `%APPDATA%\invisible-ptt\config.toml`**, created from `config.toml.starter`
  (`include_str!`) if nothing exists.
- **The starter is inert on purpose, and is not the example.** Nothing
  disabled, `default_action = "none"`, no rules — installing a push-to-talk
  daemon must not be the moment someone's back button stops working, and the
  example's `rpc` cannot do anything until Discord credentials exist, so
  shipping *that* as the first run would break navigation and give nothing
  back. Two tests hold the line: the starter must parse and validate, and it
  must contain no `0` in `mapping`, no action, and no rules.
  `config.toml.example` stays the fully configured reference the README points
  at, and has its own test that it still parses. Never the working directory, which is what it used to be — launched
  from the Run key the cwd is wherever the shell left it. Exe-adjacent beats
  `%APPDATA%` so that every install predating this keeps working untouched.
- **`log::init` creates its directory.** It runs before the config does, and a
  file opened inside a directory that does not exist just fails — which
  silently disabled logging for the whole first run, starting with the line
  saying where the config had been created. Confirmed by watching it happen.
- **Opening a file from the menu needs two things that are easy to miss**, and
  getting either wrong looks identical to a dead menu item: the editor is
  another process, so `AllowSetForegroundWindow` must hand it the foreground or
  it opens *behind* everything; and neither `.toml` nor `.log` normally has an
  association, so `FindExecutableW` decides between the shell and Notepad
  rather than letting the user meet the "How do you want to open this file?"
  chooser. The tray thread also holds a COM apartment, which `ShellExecuteW`
  wants when it delegates to a shell extension.
- The icon is drawn pixel-by-pixel in `make_icon()` so the repo carries no
  binary asset; the shape is symmetric because `CreateIcon`'s expected row
  order is not worth being sure about.

## Gotchas

- **G HUB must not be running.** It holds the HID++ channel and will fight for
  control. Settings live in onboard memory and survive uninstalling it.
- **The mapping is volatile.** The mouse forgets it on sleep/power-cycle.
  Recovery is driven by the `0x1D4B` wake event; the periodic poll (30s with the
  wake event, 5s without) is only a backstop.
- **The poll reads; it does not blindly rewrite.** `Device::probe()` reads the
  state back with `0x8100` fn2 GetMode and `0x8110` fn3 GetMouseButtonMapping,
  and `apply()` runs only when that disagrees with the config. In steady state
  the daemon writes nothing to the mouse at all. **Do not restore an
  unconditional periodic `apply()`** — see the gate below for why every avoidable
  write was worth removing.
- **Reassert must never fire while a button is held.** Re-sending the mapping
  (`apply()`) glitches a physically-held button into a momentary release —
  confirmed 2026-08-13 as interrupted hold-to-fire in games, and *only* with the
  daemon running. Writes are therefore gated to run solely when no button is
  down and input has been quiet 500ms (`main.rs`, `button_state` /
  `last_button_event`). **Do not remove that gate.** The tray's Reload ends in
  the same `apply()` and waits behind the same condition, via `pending_reload`. It relies on the spy
  reporting every button's raw state, including the passthrough left button
  (confirmed on hardware). Note the gate is checked *before* `apply()` sends its
  round-trips, so a button going down in between is still exposed — narrowing
  that race is exactly why the poll became a read.
- **The spy cannot be read back.** `IMouseButtonSpy` stops at fn4: there is no
  "is the spy armed" getter, so a spy that dropped while mode and mapping
  survived would be invisible, and PTT would be silently dead until restart.
  Accepted deliberately: that state has never been observed (the reset that
  drops the spy takes mode and mapping with it, confirmed 2026-08-14), it is
  loud when it happens, and the periodic write that would have covered it costs
  more than it saves. If it ever does show up, the fix is a periodic
  `StartMouseButtonSpy` *alone* — fn1 does not touch the mapping, so it should
  not glitch a hold — not a return to the full `apply()`.
- **Host mode disables onboard profiles.** The DPI *value* survives the switch
  (confirmed 2026-08-13: 800 DPI before and after). Onboard DPI *switching* via a
  DPI button is still untested — this Superlight has none, so it never came up.
- **Discord RPC is rate-limited.** Conversation is fine; rapid tapping may
  throttle and leave the user stuck muted or open.
- **Discord access tokens expire after 7 days.** The daemon refreshes via the
  OAuth refresh token: at startup and every ~6 days it POSTs
  `grant_type=refresh_token`, then rewrites the rotated
  `access_token`/`refresh_token` into `config.toml` with `toml_edit` (comments
  preserved, temp-file + rename). Needs `client_secret` + `refresh_token` in
  config. Refresh tokens rotate and are single-use, so a failed *persist* after a
  successful refresh is the dangerous case — hence the atomic write.
- **That POST is the program's entire network surface, and it goes through
  WinHTTP** (`discord::http`), from the `windows` crate already in the tree —
  the OS supplies TLS and the root store. It used to shell out to `curl`, which
  worked until the daemon became a GUI-subsystem process: Windows gives a
  console child its own console, so a cmd window flashed on screen at every
  refresh. `CREATE_NO_WINDOW` would have papered over it; spawning nothing
  removes the cause. **Do not reach for an HTTP crate here** — `reqwest` alone
  is an async runtime and ~100 dependencies for one request every six days.
  Linux keeps the curl path, since WinHTTP does not exist there and that build
  is smoke-test only.
- **The refresh has a network test, `#[ignore]`d.** `cargo test -- --ignored`
  posts junk credentials to the real endpoint and asserts Discord answers with
  JSON containing no tokens. Run it after touching `discord::http`: a broken
  refresh is invisible until a token expires a week later and PTT quietly
  stops. CI never depends on the network.
- Any change to `SetMouseButtonMapping` semantics risks bricking mouse input
  until replug. Always test with a second pointing device available.

## Current configuration state

`config.toml` (local, gitignored) currently has `default_action = "rpc"`, a
`chrome.exe → key:V` test rule, and real Discord credentials. The tracked
`config.toml.example` is the template. Discord `rpc` "appears to work" per the
user but is not yet fully verified end-to-end (see the table below).

## Project state — what is and isn't verified

| Thing | Status |
|---|---|
| `0x8110` mapping suppresses HID reports | **Confirmed on hardware** (Linux/Lightspeed, and Windows 2026-08-13 — back button does not navigate) |
| Spy still reports the disabled button | **Confirmed on hardware** (Linux, and Windows 2026-08-13 — F13 fires from the disabled button) |
| Builds clean on Windows | **Confirmed** — `windows` 0.58 as pinned, no source changes needed |
| Daemon works end-to-end on Windows | **Confirmed 2026-08-13** — `key:F13` global: button read via spy, synthesised F13, native back suppressed |
| HID++ reachable through Windows' HID stack | **Confirmed 2026-08-13** — vendor collection opened, ROOT probe answered, spy notifications arrive |
| DPI survives Host mode | **Confirmed 2026-08-13** — 800 DPI before and after. NB: this Superlight has no DPI-shift button, so onboard DPI *switching* is untested (and moot here); only the DPI *value* is verified to survive |
| Recovery after mouse sleep/power-cycle | **Confirmed 2026-08-13** — mouse forgets mapping *and* spy; daemon reasserts both within ≤5s and F13 returns. Required fixing `apply()` to re-arm the spy, not just the mapping (see its doc comment) |
| Wake-driven reassert via `0x1D4B` WirelessDeviceStatus | **Confirmed 2026-08-13** — feature present over the Lightspeed receiver on Windows; broadcasts on wake from **both** a power-cycle and a ~6min idle-sleep, and recovery (mapping + F13) is instant. Fires ~twice per wake; `apply()` is idempotent so the second is a harmless no-op. Reachability logging is edge-triggered (`lost the mouse; waiting for it to come back...` once on the falling edge), so a long sleep no longer spams the log |
| Per-app `key:` rule + foreground detection | **Confirmed 2026-08-13** — `chrome.exe`→`key:V` sends V only while Chrome is focused, F13 default elsewhere. Confirms `platform::foreground_process()` returns the real exe name on Windows (was previously only known not to crash) and the case-insensitive exact-match rule logic |
| Discord `rpc` action (mute-toggle PTT over local pipe) | **Confirmed 2026-08-13** — user runs `default_action = "rpc"`; mute-toggle PTT works over the local IPC pipe |
| Token refresh over WinHTTP (replacing `curl`) | **Confirmed 2026-08-14** — twice over. `cargo test -- --ignored` posted junk credentials to the live endpoint and Discord answered `{"client_id": ["Value \"x\" is not snowflake."]}`, proving DNS, TLS, the form body, and the response read; then the user ran the daemon with real credentials and the startup refresh minted a token as before. No cmd window either way: nothing is spawned to have one |
| Discord token auto-refresh (7-day expiry) | **Confirmed 2026-08-13** — startup refresh mints a fresh access_token, rotates the refresh_token, and rewrites `config.toml` in place with comments intact. The ~6-day periodic refresh and the auth-error log path share this same code but were not independently triggered |
| State readback via `GetMode` + `GetMouseButtonMapping` | **Confirmed 2026-08-14** — both getters answer in *either* mode. Steady state reads `mode=02 (host), mapping=[01 02 03 00 05]`; after an idle-sleep the wake probe read `mode=01 (onboard), mapping=[01 02 03 04 05]`, i.e. the mouse falls all the way back to the factory profile, which is what makes the mapping a sound stand-in for the unreadable spy state. A failed read also precedes `lost the mouse`, so it doubles as the liveness check. Firmware returns exactly the configured span, no padding |
| Read-gated reassert (poll writes only on divergence) | **Confirmed 2026-08-14** — a session covering a power-cycle toggle and a 5min idle-sleep produced zero `mouse forgot its configuration` lines, i.e. the poll read "as configured" every time and never wrote; startup's post-apply verification stayed silent; the wake path still recovers twice per wake and PTT still fires afterwards. The divergence branch itself (probe disagrees → `apply()`) has **not** been triggered on hardware: the wake event beat the poll to every reset in this run, which is the fallback working as intended but leaves that branch exercised only by unit tests |
| Tray app starts, logs, and waits for an absent mouse | **Confirmed 2026-08-14** — run with the receiver unplugged and an inert config. No console window, no dialog; the log appears beside the config with local timestamps and a `---` session marker, `could not set up the mouse` is logged **once** and the process then sits waiting instead of exiting. Neither `tray: could not create its window` nor `tray: the shell refused the icon` appeared, so `CreateWindowExW` and `Shell_NotifyIconW(NIM_ADD)` both succeeded. `platform::error_box` was confirmed separately, by accident: an earlier build of the same run (before `connect_when_available`) blocked in `MessageBoxW` on the same failure |
| Tray menu: opens, dispatches, autostart toggle, Exit | **Confirmed 2026-08-14** — by the user, and the log is the evidence: `will no longer start at sign-in` (so the Run key value had been written by an earlier click and was then deleted, i.e. both directions and the tick that follows the registry), then `exit requested from the tray` → `restoring mouse...`. The icon is findable and clickable |
| First run writes an inert config and logs where | **Confirmed 2026-08-14** — with `%APPDATA%\invisible-ptt` absent, a no-argument launch created the directory, wrote the starter, logged `first run: wrote a starter config to ...`, and connected to the mouse having disabled nothing: the applied mapping read `[01 02 03 04 05]` and the log said `button 3 still reaches Windows normally`. An earlier build that wrote the *example* instead was watched doing the opposite - it disabled the back button on a config with no Discord credentials, which is what the starter exists to prevent |
| Open settings file / Open log file | **Fixed, not re-verified** — reported as "does nothing". It was not: Notepad opened the right file, *behind* everything, because nothing granted it the foreground. Now `AllowSetForegroundWindow` + `FindExecutableW` + a logged failure line. That fix has not been clicked yet |
| Reload settings applies an edited config in place | **Confirmed 2026-08-14** — by the user, on hardware: edited config, Reload, new mapping in effect with no restart |
| Installer builds and installs | **Confirmed 2026-08-14** — CI compiles the script (Inno Setup 6.7.1 on the runner; it is deliberately not installed on the dev machine, so `iscc` only ever runs there) and the resulting setup exe installs and runs. `MB_DEFBUTTON2` and the extensionless `LICENSE` as `LicenseFile` were both fine. Two failures on the way, both worth not repeating: ISPP reads *any* line whose first non-blank character is `#` as a directive, so a wrapped Pascal string continued with `#13#10` aborts the compile; and `choco install` is a no-op against an already-installed older version, which is the one case that step exists for, so it must be `choco upgrade` |
| Uninstaller stops a running daemon | **Confirmed 2026-08-14** — uninstalled with the daemon running and a button disabled; it closed and the button navigated again afterwards. This is also the only confirmation of the `WM_CLOSE` handling, since `InitializeUninstall` posting `WM_CLOSE` to the tray window is what exercises it: the button coming back proves the message reached the input loop and not just the tray thread. The first attempt did nothing, because `CloseApplications` is a **Setup** directive that the uninstaller never consults — enabling it is precisely what argued the uninstall-side check away. `WM_QUERYENDSESSION` shares the handler but was not independently triggered, so sign-out restore is inferred, not seen |
| Tray: the icon's appearance | **Not verified** — whether `CreateIcon` drew a dot in a ring or quietly fell back to the generic app icon (32bpp DDB, expected row order is an assumption) |
| Reassert does not interrupt held buttons | **Confirmed 2026-08-13** — the periodic `apply()` was glitching a held left button into a brief release (interrupted hold-to-fire, daemon-only). Fixed by gating the reassert on no-button-held + 500ms quiet; verified with a 2s-interval build holding left through ~10 reasserts with zero interruptions. Also confirms the spy reports the passthrough left button |

### Review changes — what has been re-verified

A code review on 2026-08-13 (branch `review-fixes`) landed nine commits *after*
the table above was filled in: HID++ error-page offsets, a device-index filter
on notifications, releasing a held action when the mouse drops or wakes, config
validation, `restore()` spanning the configured mapping, and Discord RPC moved
onto a worker thread.

Re-confirmed on hardware the same day, **after** those changes:

- **`rpc` action through the worker thread** — press/release still mutes and
  unmutes; the channel hop costs nothing perceptible.
- **Shutdown drain** — Ctrl-C *while the button is held* leaves Discord muted.
  That release is queued rather than sent inline, so this is precisely the
  property proving `shutdown()` drains the channel before the process exits.
- **Ctrl-C restore after the `restore()` change** — back button navigates again.
- **Lazy connect and retry on the worker thread** — with Discord quit, a press
  logs `discord rpc unavailable` exactly once (edge-triggered, not per press),
  and PTT resumes once Discord starts, with no daemon restart.
- **Wake path with the added `release()`** — the wake event still arrives and
  PTT fires again afterwards.

Every row in the table above therefore still holds against the current code. The
two changes that remain unexercised cannot be reached from this setup: the
device-index filter is unobservable on a single-device Lightspeed receiver, and
the error-page fix needs a device that actually returns an error — G HUB would
provoke one, but it is not installed here.

The whole path is verified end-to-end on hardware — HID++ core, the `key:`
and `rpc` actions, and Discord token auto-refresh. Do not regress any row to
"unverified". With the wake event in place, recovery after sleep is effectively
instant; the fallback poll is now only a backstop for states that never
broadcast (e.g. a receiver replug). The only paths not independently triggered
are the ~6-day periodic refresh and the auth-error log branch, which reuse the
confirmed startup-refresh code.

## Rejected alternatives — do not re-propose these

Six rounds of investigation eliminated every simpler option. Re-suggesting them
wastes the user's time.

- **Keyboard keys (V, F13–F24, Right Ctrl)** — globally visible to the focused
  application. F13+ additionally collides with Japanese-layout scancodes and
  produces stray `~`. This is the original problem, not a solution.
- **A different mouse button** — the Superlight has exactly 5, all in use:
  back is PTT, forward is a game bind, and **both are actively used for browser
  navigation**. Windows has no 6th mouse button; MMO mice send *keystrokes* for
  buttons 6+, which reintroduces the problem.
- **Disabling back/forward in the browser** — rejected, the user navigates with
  them constantly.
- **Foot pedals / any fixed-position hardware** — rejected on ergonomic
  grounds; the user wants to talk while lounging, not seated at the desk.
- **Bluetooth clickers** — already owned and tried. Cheap BLE HID drops presses
  and reports phantom holds.
- **Custom mouse firmware** — Logitech images are signed, bootloader locked, no
  recovery path. Protocol-level work only.
- **G HUB per-app profiles** — rejected; profile switching lags 1–2 seconds.

Plan B, if the HID++ approach fails on Windows: an AutoHotkey v2 tap/hold
script scoped to browsers — tap the back button to navigate, hold it to talk.
Works, but trades a ~200 ms disambiguation gesture for the conflict.

## Conventions

- No external HID++ crate — `src/hidpp.rs` is a deliberately minimal hand-rolled
  client. Keep it that way; the surface needed is tiny.
- Dependencies are intentionally few. Do not add a HID abstraction layer.
- `platform.rs` is cfg-gated so the HID++ half builds and smoke-tests on Linux,
  where `foreground_process()` returns `None` and `key()` just logs. Preserve
  this — it makes hardware testing on a Linux live image possible. CI builds
  both targets so it cannot rot unnoticed.
- **Nothing may block the input loop.** Discord RPC therefore lives on its own
  thread behind `discord::RpcHandle`; every call into Discord is a blocking pipe
  round-trip with no read timeout, and a stall on the loop would freeze button
  handling *and* leave Ctrl-C unable to restore the mouse. `shutdown()` is the
  single deliberate exception, and it runs after `dev.restore()` for that reason.

## Testing on Linux

`cvuchener/hidpp` is the reference implementation and the only public
description of `0x8110` in existence. To re-verify behaviour:

```bash
BIN=<build>/src/tools
D="-d 1 /dev/hidraw3"          # receiver node + paired device index
sudo $BIN/hidpp-list-features $D
sudo $BIN/hidpp20-mouse-event-test $D
sudo $BIN/hidpp20-call-function $D 7 1 02          # Host mode
sudo $BIN/hidpp20-call-function $D 8 4 01 02 03 00 05
sudo libinput debug-events | grep -i button        # should stay silent
```

Note `hidpp20-call-function` takes a feature **index**, not a feature ID.

## Source of truth

Protocol semantics come from
[`cvuchener/hidpp`](https://github.com/cvuchener/hidpp), specifically
`IMouseButtonSpy.h` and `IOnboardProfiles.h`. The load-bearing sentence:

> Button can be remapped or disabled for standard HID reports but it does not
> affect how they are reported in MouseButtonSpy event.

libratbag and Solaar define the `0x8110` constant but implement nothing.
Treat cvuchener's headers as authoritative and hardware tests as final.
