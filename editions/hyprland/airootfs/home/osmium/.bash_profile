# Start Hyprland on tty1 autologin.
[[ -z $WAYLAND_DISPLAY && $(tty) == /dev/tty1 ]] && exec Hyprland
