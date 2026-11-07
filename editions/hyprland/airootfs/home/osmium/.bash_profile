# Live session: the ii desktop (AI sidebar, settings app) plus the installer window.
if [[ ! -d ~/.config/hypr ]]; then
  osmium-dots apply ii >/dev/null
  # The live user has no password: no idle lock.
  sed -i '/hypridle/d' ~/.config/hypr/hyprland/execs.lua
  cat >>~/.config/hypr/custom/execs.lua <<'LUA'
-- Live ISO: open the installer when the desktop starts.
hl.on("hyprland.start", function ()
    hl.exec_cmd("osmium-installer")
end)
LUA
  # A Lua error drops the whole config (no shell, no binds). If this hyprland-rust cannot load
  # ii yet (docs/hyprland-rust-gaps.md), boot the synthwave desktop instead of a blank screen.
  if ! hyprland-rust --check-config >/dev/null 2>&1; then
    osmium-dots apply synthwave >/dev/null
    sed -i '/hypridle/d; /hyprlock/d' ~/.config/hypr/hyprland.lua
    sed -i 's|^\(\s*\)hl.exec_cmd("ironbar")|&\n\1hl.exec_cmd("osmium-installer")|' ~/.config/hypr/hyprland.lua
  fi
fi

# Start the desktop once per boot. No exec: if it fails (e.g. a VM without 3D), getty's
# autologin would otherwise restart it forever on a black screen.
# hyprland-rust logs to stdout/stderr (tracing) and has no log file, so capture it ourselves.
if [[ -z $WAYLAND_DISPLAY && $(tty) == /dev/tty1 && ! -e /run/user/$UID/osmium-tried ]]; then
  touch /run/user/$UID/osmium-tried
  log=/run/user/$UID/hyprland-rust.log
  osmium-dots session >"$log" 2>&1
  status=$?
  if (( status != 0 )); then
    cat <<MSG

Masaüstü (hyprland-rust) başlatılamadı (çıkış kodu $status).
Sanal makinedeysen: ekran kartını VMSVGA yap ve "3D hızlandırma"yı aç,
ya da QEMU/virt-manager'da virtio-gpu + OpenGL kullan.
Günlük: $log
Tekrar denemek için: osmium-dots session
Kurulum penceresi masaüstüyle açılır (osmium-installer).

MSG
  fi
fi
