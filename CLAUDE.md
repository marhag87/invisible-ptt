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

Ctrl-C restores all buttons, stops the spy, returns to Onboard mode. Unplugging
the receiver does the same — both settings are volatile.

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
  `last_button_event`). **Do not remove that gate.** It relies on the spy
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
  `grant_type=refresh_token` (shelling out to `curl`), then rewrites the rotated
  `access_token`/`refresh_token` into `config.toml` with `toml_edit` (comments
  preserved, temp-file + rename). Needs `client_secret` + `refresh_token` in
  config. Refresh tokens rotate and are single-use, so a failed *persist* after a
  successful refresh is the dangerous case — hence the atomic write.
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
| Discord token auto-refresh (7-day expiry) | **Confirmed 2026-08-13** — startup refresh mints a fresh access_token, rotates the refresh_token, and rewrites `config.toml` in place with comments intact. The ~6-day periodic refresh and the auth-error log path share this same code but were not independently triggered |
| State readback via `GetMode` + `GetMouseButtonMapping` | **Confirmed 2026-08-14** — both getters answer in *either* mode. Steady state reads `mode=02 (host), mapping=[01 02 03 00 05]`; after an idle-sleep the wake probe read `mode=01 (onboard), mapping=[01 02 03 04 05]`, i.e. the mouse falls all the way back to the factory profile, which is what makes the mapping a sound stand-in for the unreadable spy state. A failed read also precedes `lost the mouse`, so it doubles as the liveness check. Firmware returns exactly the configured span, no padding |
| Read-gated reassert (poll writes only on divergence) | **Confirmed 2026-08-14** — a session covering a power-cycle toggle and a 5min idle-sleep produced zero `mouse forgot its configuration` lines, i.e. the poll read "as configured" every time and never wrote; startup's post-apply verification stayed silent; the wake path still recovers twice per wake and PTT still fires afterwards. The divergence branch itself (probe disagrees → `apply()`) has **not** been triggered on hardware: the wake event beat the poll to every reset in this run, which is the fallback working as intended but leaves that branch exercised only by unit tests |
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
