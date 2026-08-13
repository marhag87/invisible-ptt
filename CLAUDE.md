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
- Errors come back as report `0x10` with feature index `0xFF`.
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
- **The mapping is volatile.** The mouse forgets it on sleep/power-cycle. The
  daemon reasserts every 5s (`main.rs`, `last_reassert`). A stray
  back-navigation right after wake means that window needs shortening.
- **Host mode disables onboard profiles.** The DPI *value* survives the switch
  (confirmed 2026-08-13: 800 DPI before and after). Onboard DPI *switching* via a
  DPI button is still untested — this Superlight has none, so it never came up.
- **Discord RPC is rate-limited.** Conversation is fine; rapid tapping may
  throttle and leave the user stuck muted or open.
- Any change to `SetMouseButtonMapping` semantics risks bricking mouse input
  until replug. Always test with a second pointing device available.

## Current configuration state

`config.toml` currently uses `default_action = "key:F13"` globally with all
per-app rules commented out and no Discord credentials. So today the button
synthesises F13 everywhere — which still leaks to the focused application. The
per-app `rpc` rules are the intended endgame; they need the one-time OAuth
setup in README.md.

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

The Windows path is now verified end-to-end. Every row above was established on
hardware — do not regress any of them to "unverified". The one soft edge left:
there is a ≤5s window after the mouse wakes where the button navigates and the
action is dead, before the reassert lands. Shortening `last_reassert`'s 5s
interval in `main.rs` is the lever if that window ever needs to be tighter.

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
  this — it makes hardware testing on a Linux live image possible.

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
