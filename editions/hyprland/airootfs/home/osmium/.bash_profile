# Live session: seed config from skel once. Drop the screen lock: the live user has no password.
if [[ ! -d ~/.config/hypr ]]; then
  cp -r /etc/skel/.config ~/
  sed -i '/hypridle/d; /hyprlock/d' ~/.config/hypr/hyprland.lua
  # Open the installer when the live desktop starts.
  sed -i 's|^\(\s*\)hl.exec_cmd("ironbar")|&\n\1hl.exec_cmd("sudo -E calamares")|' ~/.config/hypr/hyprland.lua
fi

# Start Hyprland once per boot. No exec: if it fails (e.g. a VM without 3D), getty's
# autologin would otherwise restart it forever on a black screen.
if [[ -z $WAYLAND_DISPLAY && $(tty) == /dev/tty1 && ! -e /run/user/$UID/osmium-tried ]]; then
  touch /run/user/$UID/osmium-tried
  if command -v start-hyprland >/dev/null; then start-hyprland; else Hyprland; fi
  status=$?
  if (( status != 0 )); then
    log=$(ls -t /run/user/$UID/hypr/*/hyprland.log 2>/dev/null | head -n1)
    cat <<EOF

Masaüstü (Hyprland) başlatılamadı (çıkış kodu $status).
Sanal makinedeysen: ekran kartını VMSVGA yap ve "3D hızlandırma"yı aç,
ya da QEMU/virt-manager'da virtio-gpu + OpenGL kullan.
Günlük: ${log:-yok}
Tekrar denemek için: Hyprland    Kurulum için: sudo calamares (Wayland/X gerekir)

EOF
  fi
fi
