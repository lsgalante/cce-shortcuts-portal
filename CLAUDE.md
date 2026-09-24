# CLAUDE.md

Read `../cce-compositor/WORKSPACE.md` first: this crate is one member of the
cce workspace and follows its multi-repo, `ccebuild` and concurrent-session
rules.

## What this is

`cce-shortcuts-portal` is the **`org.freedesktop.impl.portal.GlobalShortcuts`
backend** for the cce compositor — the piece that lets a native Wayland app
own a global hotkey. A Wayland client cannot grab keys; what it can do is ask
xdg-desktop-portal to bind a trigger on its behalf (1Password's Quick Access,
Ctrl+Shift+Space, is the motivating case: its binary calls this portal and
nothing else). The portal *frontend* forwards that request to whichever
backend the desktop's portals.conf names for the interface, and until this
crate existed nothing on the machine declared it, so the frontend did not even
publish `GlobalShortcuts` on the bus and every app's request failed silently.

The backend is deliberately thin: it does no key handling. Both directions go
through the compositor's existing sockets, and `src/server/global_shortcuts.rs`
in cce-compositor is the other half of the contract:

- **Binding** — `BindShortcuts` turns each shortcut's `preferred_trigger`
  (shortcuts-spec syntax, `CTRL+SHIFT+space`) into `shortcut bind <session>
  <id> <trigger>` on the control socket. The compositor parses the trigger,
  refuses a chord its own keybinds already use (the user's config wins, and
  the app is told it did not get the shortcut rather than having it silently
  shadowed), and answers with the `trigger_description` the app renders
  (`Ctrl+Shift+Space`). A shortcut without a `preferred_trigger` is not bound:
  there is no configuration dialog (interface version 1, no
  `ConfigureShortcuts`).
- **Firing** — the backend subscribes to the status socket's `shortcuts`
  topic and re-emits each `activated|deactivated <session> <id> <time_msec>`
  line as the portal's `Activated` / `Deactivated` signal. Both edges are
  reported; neither reaches the focused client.
- **Sessions** — one `org.freedesktop.impl.portal.Session` object per
  session at the path the frontend chose. `Close` unbinds that session's
  chords. Request objects are not exported: every call answers at once, so
  there is never a pending request to cancel.

Shortcut ids are app-chosen and may contain spaces; the control socket
splits on whitespace, so ids are percent-encoded on the wire and the
compositor stores them encoded (`ccectl shortcut list` shows
`Quick%20Access`). Only this side decodes. `+` is left alone so triggers
pass through as written.

## Lifecycle

Bus-activated: `dbus/org.freedesktop.impl.portal.desktop.cce-shortcuts.service`
starts it on the first request. It begins with `shortcut clear`, which both
drops chords a dead predecessor left bound (nobody would ever hear them
fire) and proves the compositor speaks the command — an older compositor
answers `error: unknown command`, and the backend exits rather than claim
to serve an interface it cannot. It then lives as long as the status socket
does and exits when the compositor closes it; the next request starts a
fresh one. Sessions do not survive that, which is right: they were bound in
a compositor that is gone.

Reads `WAYLAND_DISPLAY` for both socket paths exactly as `ccectl` does, so
it needs the bus activation environment to carry it (startcce imports it).
Pointed at a `cce-shadow` instance's environment it drives that shadow
instead — which is how it is verified.

## Wiring (three files, one of them not installed from here)

- `portals/cce-shortcuts.portal` — declares the bus name and interface;
  `ccebuild install` puts it in `$XDG_DATA_HOME/xdg-desktop-portal/portals/`
  (the `portal_files()` step, added for this crate).
- `dbus/…cce-shortcuts.service` — bus activation, installed to
  `~/.local/share/dbus-1/services/`. `Exec` is absolute because D-Bus
  expands nothing, the same as the file chooser's service file.
- **`~/.config/xdg-desktop-portal/cce-portals.conf`** must name it:
  `org.freedesktop.impl.portal.GlobalShortcuts=cce-shortcuts`. That file is
  user config, not versioned anywhere; without the line the frontend's
  `default=gtk` applies and gtk does not implement the interface.

xdg-desktop-portal reads portal files and the conf at startup:
`systemctl --user restart xdg-desktop-portal` after installing, then
`busctl --user introspect org.freedesktop.portal.Desktop
/org/freedesktop/portal/desktop | grep GlobalShortcuts` shows whether the
frontend now publishes it. An app that registered its shortcut before that
(1Password at login) has to be restarted to ask again.

## Verifying without the live session

The frontend is one per session bus, so the full path (app → frontend →
backend) can only be exercised live. Everything below the frontend can be
driven in a shadow, and that is where the two bugs so far were found
(`+` being percent-encoded out of the trigger; a lone modifier accepted as
a key):

```sh
cce-shadow start --new                                   # prints agent-N
cce-shadow --instance agent-N run target/release/cce-shortcuts-portal &
busctl --user call org.freedesktop.impl.portal.desktop.cce-shortcuts \
  /org/freedesktop/portal/desktop org.freedesktop.impl.portal.GlobalShortcuts \
  CreateSession 'oosa{sv}' /r/1 /s/1 com.example 0
busctl --user call … BindShortcuts 'ooa(sa{sv})sa{sv}' /r/1 /s/1 \
  1 quick 2 description s "Quick access" preferred_trigger s CTRL+SHIFT+space "" 0
dbus-monitor --session "type='signal',interface='org.freedesktop.impl.portal.GlobalShortcuts'" &
cce-shadow --instance agent-N run ../cce-compositor/verify/clients/target/release/vkey mod:5 57 mod:0
```

`vkey` (see `../cce-compositor/CLAUDE.md`, `verify/`) injects through the
virtual-keyboard protocol, which is the same `handle_group_key` path hardware
keys take; `ccectl keypress` goes straight to the focused client and never
reaches the chord matcher, so it cannot be used for this.
