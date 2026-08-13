# invisible-ptt

A push-to-talk button that Windows cannot see.

> [!WARNING]
> **AI-generated code.** This project was written largely by an AI assistant.
> It manipulates mouse firmware state over HID++ and synthesises keystrokes,
> and parts of it are unverified on real Windows hardware. Review it yourself
> before running it. It is provided "as is", without warranty of any kind (see
> [LICENSE](LICENSE)); the author accepts no responsibility or liability for
> any damage, data loss, or unexpected behaviour resulting from its use.

Uses two Logitech HID++ features on a G PRO X Superlight (and likely other
Logitech gaming mice that advertise `0x8110`):

| Feature | Use |
|---|---|
| `0x8100` Onboard Profiles | `SetMode(Host)` — required, or the mapping below is silently ignored |
| `0x8110` Mouse Button Spy | `SetMouseButtonMapping` with code `0` disables a button *for standard HID reports*, while `StartMouseButtonSpy` keeps delivering its raw state as HID++ notifications |

The consequence: the button produces no window message, no virtual key, and
nothing a low-level hook can observe. Your browser cannot navigate back on it,
and a game cannot bind it, because neither is ever told it was pressed. Only
this process knows.

## Build

```
cargo build --release
copy config.toml.example config.toml
target\release\invisible-ptt.exe config.toml
```

**Uninstall G HUB first**, or at least exit it — it holds the HID++ channel
and will fight this program for control. Your DPI and other settings live in
the mouse's onboard memory and survive G HUB's removal.

## Verify it works

With the program running, press the back button:

- Your browser should **not** navigate back.
- Discord (or your game) should transmit.

Ctrl-C restores the button and hands control back to the onboard profile.
Unplugging the receiver does the same, since both settings are volatile.

## Configuration

See `config.toml.example`. The important knobs:

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

5. Put `client_id` and the returned `access_token` in `config.toml`.

You are allowed to authorize your *own* application without Discord
whitelisting it; the whitelist requirement applies to shipping it to other
people.

**Set Discord's input mode to Voice Activity with sensitivity at minimum.**
There is no hold-to-talk RPC command, so PTT is expressed as self-mute
toggling — with voice activity always-on underneath, the result is true
push-to-talk.

## Known caveats

- **Host mode disables onboard profiles.** Onboard DPI switching stops working
  while this runs. Check whether your DPI survives the switch before
  committing.
- **The mapping is volatile.** The mouse forgets it on sleep or power-cycle.
  The daemon reasserts every 5 seconds; if you catch a stray back-navigation
  right after the mouse wakes, that window is why — shorten the interval.
- **Discord rate-limits RPC.** Normal conversation is fine; rapid tapping may
  get throttled and leave you stuck muted or open.
- **Feature indices are not stable.** They're resolved at runtime via the ROOT
  feature rather than hardcoded, so firmware updates shouldn't break this.
- **This rests on reverse-engineered documentation** — cvuchener's `hidpp`
  library is the only description of `0x8110` in existence. It has been
  confirmed working on a Superlight over the Lightspeed receiver on Linux.
  Windows is the same protocol over a different transport, but is less tested.

## Credit

Protocol details from [cvuchener/hidpp](https://github.com/cvuchener/hidpp),
specifically `IMouseButtonSpy.h` and `IOnboardProfiles.h`.
