# invisible-ptt

**A push-to-talk button that nothing else on your computer can see.**

Push-to-talk needs a key you can hold down while doing something else, and on
Windows every candidate is already taken. Bind it to a keyboard key and the
focused application gets it too — your game reloads, your browser scrolls, your
chat box fills up with `v`. Bind it to a mouse button and you discover Windows
exposes only five, all of them spoken for: on a G PRO X Superlight, back and
forward are how you navigate the web all day.

This daemon removes the conflict rather than working around it. It tells the
mouse's own firmware to stop reporting one button to the operating system
entirely, then reads that button over a private channel only this process is
listening on. No keystroke, no window message, nothing a low-level hook can
intercept — as far as Windows is concerned the button was never pressed. Your
browser doesn't navigate back, because it was never told to.

And because the daemon knows which application is focused, one button can do
different things: send the key a game expects for its own voice chat, and
everywhere else drive Discord straight over its local pipe, where no keybind
needs to exist at all.

> [!WARNING]
> **AI-generated code.** This project was written largely by an AI assistant.
> It manipulates mouse firmware state over HID++ and synthesises keystrokes,
> and it has been verified on exactly one mouse and receiver. Review it
> yourself before running it. It is provided "as is", without warranty of any
> kind (see [LICENSE](LICENSE)); the author accepts no responsibility or
> liability for any damage, data loss, or unexpected behaviour resulting from
> its use.

## How it works

Two Logitech HID++ features, on a G PRO X Superlight (and likely other Logitech
gaming mice that advertise `0x8110`):

| Feature | Use |
|---|---|
| `0x8100` Onboard Profiles | `SetMode(Host)` — required, or the mapping below is silently ignored |
| `0x8110` Mouse Button Spy | `SetMouseButtonMapping` with code `0` disables a button *for standard HID reports*, while `StartMouseButtonSpy` keeps delivering its raw state as HID++ notifications |

The second row is the whole trick: the same button is invisible to the
operating system and still legible to us, at the same time.

## Install

Download `invisible-ptt.exe` from the [latest release][latest] and run it:

```
invisible-ptt.exe
```

It runs in the notification area: no window, no console, just an icon. On the
first run it writes a starter config to `%APPDATA%\invisible-ptt\config.toml`
and **does nothing at all** — no button is hidden from Windows until you say
which one. **Open settings file** in the tray menu opens it; the comments in it
say what to change; **Reload settings** picks up the edits.

Two ways to override that location: put a `config.toml` next to the exe (a
portable install), or pass a path as the first argument. Either wins over
`%APPDATA%`.

Or build it yourself:

```
cargo build --release
copy config.toml.example config.toml
target\release\invisible-ptt.exe config.toml
```

[latest]: https://github.com/marhag87/invisible-ptt/releases/latest

**Uninstall G HUB first**, or at least exit it — it holds the HID++ channel
and will fight this program for control. Your DPI and other settings live in
the mouse's onboard memory and survive G HUB's removal.

## Verify it works

With the program running, press the back button:

- Your browser should **not** navigate back.
- Discord (or your game) should transmit.

**Exit** in the tray menu restores the button and hands control back to the
onboard profile. Unplugging the receiver does the same, since both settings are
volatile.

## The tray menu

Right-click (or left-click) the icon:

| | |
|---|---|
| **Open settings file** | `config.toml`, in your usual editor |
| **Open log file** | `invisible-ptt.log`, beside the config — everything the daemon has to say goes there, since there is no console to print to. Rotates at 1 MB |
| **Reload settings** | applies an edited config without restarting: new mapping, new rules, new Discord credentials. A config with a mistake in it is rejected with a message box and nothing changes |
| **Start automatically at sign-in** | ticked when the entry under `HKCU\...\Run` exists. See below |
| **Exit** | stops and restores the mouse |

## Configuration

`%APPDATA%\invisible-ptt\config.toml` unless you overrode it above. It starts
as [`config.toml.starter`](config.toml.starter), which does nothing;
[`config.toml.example`](config.toml.example) is the same file fully configured,
and worth reading next. The important knobs:

- `button` — spy index of the button to hijack (`3` = back on the Superlight)
- `mapping` — one byte per physical button, `0` disables for HID
- `rules` — per-foreground-application behaviour

Two kinds of action:

**`rpc`** drives Discord over its local IPC pipe. No keybind exists anywhere,
so nothing can conflict — this is the right choice for browsers and the
desktop. Requires the one-time setup below.

**`key:X`** synthesises a keystroke while the button is held. Necessary for
in-game voice, which needs a real key. Because the daemon knows which
application is focused, it can send the key *that game* uses and nothing else.

## Discord RPC setup (one time)

1. Create an application at <https://discord.com/developers/applications>.
   Note the **Client ID** and **Client Secret**.
2. Add `http://localhost/` as an OAuth2 redirect URI.
3. Visit, in a browser, replacing `CLIENT_ID`:

   ```
   https://discord.com/oauth2/authorize?client_id=CLIENT_ID&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%2F&scope=rpc%20rpc.voice.write
   ```

   Approve it. You'll be redirected to a dead `localhost` page — copy the
   `code=` value out of the address bar.

4. Exchange that code for a token (PowerShell):

   ```powershell
   curl.exe -X POST https://discord.com/api/oauth2/token `
     -d "client_id=CLIENT_ID" -d "client_secret=CLIENT_SECRET" `
     -d "grant_type=authorization_code" -d "code=THE_CODE" `
     -d "redirect_uri=http://localhost/"
   ```

5. Put `client_id`, `client_secret`, and the returned `access_token` **and
   `refresh_token`** in `config.toml`.

### Token expiry — handled automatically

Discord access tokens expire after 7 days. Because `client_secret` and
`refresh_token` are in the config, the daemon trades the refresh token for a
fresh access token **at startup and again every ~6 days**, and writes the
rotated values back to `config.toml` (in place, comments preserved). So you do
the browser authorize in steps 3–4 exactly once; after that it self-heals and
you never touch it again. That POST is the only network request this program
ever makes, and it goes through WinHTTP — Windows' own TLS, no bundled HTTP
stack, no child process. If a refresh is ever rejected — e.g. you revoke the
app — the daemon logs `discord authenticate failed: …` instead of silently
going mute.

You are allowed to authorize your *own* application without Discord
whitelisting it; the whitelist requirement applies to shipping it to other
people.

**Set Discord's input mode to Voice Activity with sensitivity at minimum.**
There is no hold-to-talk RPC command, so PTT is expressed as self-mute
toggling — with voice activity always-on underneath, the result is true
push-to-talk.

## Run at startup

Tick **Start automatically at sign-in** in the tray menu. That writes an entry
under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` pointing at the exe
and the config it was started with, which runs the daemon **in your interactive
session as you** — required, because it synthesises keystrokes and reads the
foreground window, neither of which works from a service or a Session 0 task.
No elevation needed: HID++ vendor-collection access doesn't require admin.

Untick it to remove the entry. Move the exe and you must re-tick it, since the
path is baked in.

Discord does **not** need to be running when the daemon starts. The RPC
connection is made lazily on first use and retried on every press, so it picks
Discord up whenever it launches — order at sign-in doesn't matter.

**Stop G HUB from auto-starting** too, or it'll grab the HID++ channel and
fight the daemon at sign-in.

The receiver often hasn't enumerated yet when the Run key fires. That's fine —
the daemon waits for the mouse rather than giving up, and the icon is there the
whole time.

### Or: Task Scheduler

The one thing the Run key can't do is start the daemon again if it crashes.
Run once in PowerShell (point the paths at wherever you keep the exe and
config):

```powershell
$exe = "C:\path\to\invisible-ptt.exe"
$cfg = "C:\path\to\config.toml"
$action  = New-ScheduledTaskAction -Execute $exe -Argument "`"$cfg`""
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit 0 -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName "invisible-ptt" -Action $action -Trigger $trigger -Settings $settings
```

Why these settings:

- `-ExecutionTimeLimit 0` — the daemon runs indefinitely; without this the task
  is killed after three days.
- `-RestartCount 3 -RestartInterval 1min` — brings it back if it dies.
- Runs as you, in your session — required for `SendInput`, foreground
  detection, and writing rotated Discord tokens back to `config.toml`.

Untick the tray toggle if you use this, or both will start a copy.

## Known caveats

- **Host mode disables onboard profiles.** Onboard DPI switching stops working
  while this runs. Check whether your DPI survives the switch before
  committing.
- **The mapping is volatile.** The mouse forgets it on sleep or power-cycle,
  but the daemon re-applies it the moment the mouse reconnects (via the `0x1D4B`
  wake event), so recovery is effectively instant. A slow periodic reassert runs
  only as a backstop — and it holds off whenever a mouse button is down, so it
  can never interrupt a hold such as hold-to-fire in a game.
- **Discord rate-limits RPC.** Normal conversation is fine; rapid tapping may
  get throttled and leave you stuck muted or open.
- **Feature indices are not stable.** They're resolved at runtime via the ROOT
  feature rather than hardcoded, so firmware updates shouldn't break this.
- **This rests on reverse-engineered documentation** — cvuchener's `hidpp`
  library is the only description of `0x8110` in existence. It has been
  confirmed working on a G PRO X Superlight over its Lightspeed receiver, on
  both Linux and Windows.

## Credit

Protocol details from [cvuchener/hidpp](https://github.com/cvuchener/hidpp),
specifically `IMouseButtonSpy.h` and `IOnboardProfiles.h`.
