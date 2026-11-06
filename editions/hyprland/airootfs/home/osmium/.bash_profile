# Live session: seed config from skel once. Drop the screen lock: the live user has no password.
if [[ ! -d ~/.config/hypr ]]; then
  cp -r /etc/skel/.config ~/
  sed -i '/hypridle/d; /hyprlock/d' ~/.config/hypr/hyprland.conf
fi

if [[ -z $WAYLAND_DISPLAY && $(tty) == /dev/tty1 ]]; then
  command -v start-hyprland >/dev/null && exec start-hyprland
  exec Hyprland
fi
