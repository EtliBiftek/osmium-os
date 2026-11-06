# Live session: seed config from skel once. Drop the screen lock: the live user has no password.
if [[ ! -d ~/.config/hypr ]]; then
  cp -r /etc/skel/.config ~/
  sed -i '/hypridle/d; /hyprlock/d' ~/.config/hypr/hyprland.lua
  # Open the installer when the live desktop starts.
  sed -i 's|^\(\s*\)hl.exec_cmd("waybar")|&\n\1hl.exec_cmd("sudo -E calamares")|' ~/.config/hypr/hyprland.lua
fi

if [[ -z $WAYLAND_DISPLAY && $(tty) == /dev/tty1 ]]; then
  command -v start-hyprland >/dev/null && exec start-hyprland
  exec Hyprland
fi
