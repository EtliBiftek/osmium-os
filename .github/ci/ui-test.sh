# Runs in the live system as root via systemd.run=: starts the real live desktop on tty1
# (autologin) and checks that the installer window comes up without QML errors. A screenshot
# goes to the serial log as base64 (OSMIUM-SCREENSHOT), which the workflow turns into a PNG.
exec >/dev/ttyS0 2>&1
uid=$(id -u osmium)
rt=/run/user/$uid
systemctl start getty@tty1.service
for i in $(seq 90); do w=$(ls "$rt" 2>/dev/null | grep -m1 '^wayland-[0-9]*$') && [ -n "$w" ] && break; sleep 2; done
[ -n "$w" ] || { echo OSMIUM-UI-NO-DESKTOP; tail -50 "$rt/hyprland-rust.log"; poweroff -f; }
echo "OSMIUM-UI-DESKTOP $w"
for i in $(seq 60); do pgrep -f osmium-installer.qml >/dev/null && break; sleep 2; done
sleep 20
pgrep -f osmium-installer.qml >/dev/null && echo OSMIUM-UI-RUNNING
as() { runuser -u osmium -- env XDG_RUNTIME_DIR="$rt" WAYLAND_DISPLAY="$w" "$@"; }
as hyprctl clients | head -30
# A second instance with its log in reach: QML errors are printed when the file loads.
as qs -p /usr/share/osmium/dots/ii/home/.config/quickshell/ii/osmium-installer.qml >/tmp/ui.log 2>&1 &
sleep 25
sed 's/^/OSMIUM-UI-LOG /' /tmp/ui.log | head -60
if grep -qiE 'ReferenceError|TypeError|is not a type|is not installed|Cannot assign|failed to load|ERROR' /tmp/ui.log; then
  echo OSMIUM-UI-ERRORS
else
  echo OSMIUM-UI-CLEAN
fi
pkill -f 'qs -p' ; sleep 2
as grim /tmp/s.png && echo "OSMIUM-SCREENSHOT $(base64 -w0 /tmp/s.png)"
# Dot preview: synthwave nested in a window, screenshotted from inside.
as osmium-dots onizle synthwave --png /tmp/p.png --sure 20 2>&1 | sed 's/^/OSMIUM-PREVIEW-LOG /'
[ -s /tmp/p.png ] && echo "OSMIUM-PREVIEW $(base64 -w0 /tmp/p.png)"
tail -20 "$rt/hyprland-rust.log" | sed 's/^/OSMIUM-HR /'
poweroff -f
sleep 60
