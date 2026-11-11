# Runs in the live system as root via systemd.run=: starts the real live desktop on tty1
# (autologin) and checks that the installer window comes up without QML errors. A screenshot
# goes to the serial log as base64 (OSMIUM-SCREENSHOT), which the workflow turns into a PNG.
exec >/dev/ttyS0 2>&1
uid=$(id -u osmium)
rt=/run/user/$uid
# systemd.run boots only this unit: bring up the normal system (logind, getty autologin on tty1).
# A login prompt on ttyS0 would hang the port up (vhangup), and this script's output with it.
systemctl mask --runtime serial-getty@ttyS0.service
systemctl start --no-block multi-user.target
sleep 5
systemctl start getty@tty1.service
for i in $(seq 90); do w=$(ls "$rt" 2>/dev/null | grep -m1 '^wayland-[0-9]*$') && [ -n "$w" ] && break; sleep 2; done
if [ -z "$w" ]; then
  echo OSMIUM-UI-NO-DESKTOP
  loginctl list-sessions --no-legend | sed 's/^/OSMIUM-UI-DIAG /'
  systemctl status getty@tty1.service systemd-logind.service --no-pager 2>&1 | head -30 | sed 's/^/OSMIUM-UI-DIAG /'
  tail -50 "$rt/hyprland-rust.log" 2>&1 | sed 's/^/OSMIUM-UI-DIAG /'
  poweroff -f
fi
echo "OSMIUM-UI-DESKTOP $w"
for i in $(seq 60); do pgrep -f osmium-installer.qml >/dev/null && break; sleep 2; done
sleep 20
pgrep -f osmium-installer.qml >/dev/null && echo OSMIUM-UI-RUNNING
as() { runuser -u osmium -- env XDG_RUNTIME_DIR="$rt" WAYLAND_DISPLAY="$w" "$@"; }
as hyprctl clients | head -30
# Our own instance with its log in reach (QML errors are printed when the file loads). The
# auto-started one goes first: two would register the same global shortcuts.
pkill -f osmium-installer.qml; sleep 2
as qs -p /usr/share/osmium/dots/ii/home/.config/quickshell/ii/osmium-installer.qml >/tmp/ui.log 2>&1 &
sleep 25
sed 's/^/OSMIUM-UI-LOG /' /tmp/ui.log | head -60
# QML and Wayland failures only: Mesa reports its software-rendering fallback as "error" too.
if grep -v '^MESA' /tmp/ui.log | grep -qE 'ReferenceError|TypeError|is not a type|is not installed|Cannot assign|Cannot override|Failed to load|ERROR|Protocol error'; then
  echo OSMIUM-UI-ERRORS
else
  echo OSMIUM-UI-CLEAN
fi
# Click-through: the installer's self-test walks all pages and prints the config it would send.
pkill -f 'qs -p' ; sleep 2
as env OSMIUM_INSTALLER_SELFTEST=1 timeout 90 qs -p /usr/share/osmium/dots/ii/home/.config/quickshell/ii/osmium-installer.qml >/tmp/st.log 2>&1
grep -o 'OSMIUM-SELFTEST.*' /tmp/st.log | sed 's/^/OSMIUM-UI-/'
if [ "$(grep -c 'OSMIUM-SELFTEST page [0-9] .* ok$' /tmp/st.log)" = 6 ] && grep -q 'OSMIUM-SELFTEST problem ""' /tmp/st.log; then
  echo OSMIUM-UI-SELFTEST-OK
fi
# Starting the installer must not take the ii shell down (shared global shortcuts once did).
pgrep -f 'qs -c ii' >/dev/null && echo OSMIUM-UI-SHELL-ALIVE
pkill -f 'qs -p' ; sleep 2
# Images go to ttyS1: the console on ttyS0 interleaves systemd's status lines with long lines.
as grim /tmp/s.png && { echo "OSMIUM-SCREENSHOT $(base64 -w0 /tmp/s.png)"; } >/dev/ttyS1
# Dot preview: synthwave nested in a window, screenshotted from inside.
as osmium-dots onizle synthwave --png /tmp/p.png --sure 20 2>&1 | sed 's/^/OSMIUM-PREVIEW-LOG /'
[ -s /tmp/p.png ] && { echo "OSMIUM-PREVIEW $(base64 -w0 /tmp/p.png)"; } >/dev/ttyS1
tail -20 "$rt/hyprland-rust.log" | sed 's/^/OSMIUM-HR /'
poweroff -f
sleep 60
