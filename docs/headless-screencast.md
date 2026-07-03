# Headless screencast authorization

The server captures the screen through the `xdg-desktop-portal`
ScreenCast API. The portal's security model requires a one-time user
approval per grant: a source-picker dialog on the server's display.
On a headless gaming host there is nobody to click it — the picker may
not even appear over VNC, because the portal spawns it with its own
environment inside the compositor session. The server then waits on
`portal source selection` until the timeout:

```
portal source selection timed out after 180s: most likely an approval
dialog is waiting on the server's display. ...
```

A successful approval also mints a **restore token**
(`~/.local/state/stargaze/screencast-restore-token`) that skips the
dialog on later runs — but the token is invalidated by compositor or
portal updates, which silently reintroduces the dialog. For a headless
box, replace the dialog entirely.

## Auto-approval with xdg-desktop-portal-hyprland

`xdg-desktop-portal-hyprland` lets you swap the picker dialog for any
executable: the portal runs it and parses its stdout. A two-line script
approves every request without a GUI.

1. Create the picker script. It resolves the output name at request
   time with `hyprctl`, so it keeps working when the name changes
   between sessions (typical with virtual/headless outputs, e.g.
   `HEADLESS-2` one boot and `HEADLESS-3` the next):

   ```sh
   mkdir -p ~/.local/bin
   cat > ~/.local/bin/stargaze-picker.sh <<'EOF'
   #!/bin/sh
   # Auto-approve screencast of the first active output.
   # Format: [SELECTION]{flags}/{type}:{data} — 'r' allows restore tokens.
   export HYPRLAND_INSTANCE_SIGNATURE="${HYPRLAND_INSTANCE_SIGNATURE:-$(ls -t "$XDG_RUNTIME_DIR/hypr" | head -1)}"
   monitor="$(hyprctl monitors | awk '/^Monitor/ { print $2; exit }')"
   [ -n "$monitor" ] || exit 1
   echo "[SELECTION]r/screen:${monitor}"
   EOF
   chmod +x ~/.local/bin/stargaze-picker.sh
   ```

   With several outputs, replace the `awk` line with a fixed name
   (`monitor=DP-1`) or a `grep` for the one you want. Verify it prints
   a selection before wiring it up:

   ```sh
   ~/.local/bin/stargaze-picker.sh
   # [SELECTION]r/screen:HEADLESS-2
   ```

   If the portal runs with a PATH that lacks `hyprctl` (some distro
   service setups), use the binary's absolute path in the script.

2. Point the portal at it in `~/.config/hypr/xdph.conf` (absolute path,
   no `~`):

   ```ini
   screencopy {
       custom_picker_binary = /home/USER/.local/bin/stargaze-picker.sh
       allow_token_by_default = true
   }
   ```

3. Restart the portal services:

   ```sh
   systemctl --user restart xdg-desktop-portal-hyprland xdg-desktop-portal
   ```

Every grant now auto-approves instantly — first run, expired token, or
re-grant after an update. The restore-token flow keeps working on top;
it just stops being a single point of failure.

**Scope note:** this approves screencast for *any* application in that
session, which is usually fine on a dedicated gaming host. To gate it,
make the script conditional, e.g. only print the selection when a
marker file exists:

```sh
#!/bin/sh
[ -e "$HOME/.config/stargaze/allow-screencast" ] || exit 1
export HYPRLAND_INSTANCE_SIGNATURE="${HYPRLAND_INSTANCE_SIGNATURE:-$(ls -t "$XDG_RUNTIME_DIR/hypr" | head -1)}"
monitor="$(hyprctl monitors | awk '/^Monitor/ { print $2; exit }')"
[ -n "$monitor" ] || exit 1
echo "[SELECTION]r/screen:${monitor}"
```

## Other compositors

GNOME's and KDE's portals have no picker-replacement mechanism; they
need one interactive approval on a real or virtual display, after which
the restore token keeps sessions headless until it is invalidated.
