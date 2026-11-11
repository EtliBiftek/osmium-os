#!/bin/sh
# Runs on the installed system (osmium-boot-test.service, multi-user) and reports on the serial
# port. Several boots: markers in /root say which phase this is. A snapper rollback brings back
# the markers of the snapshot, which is what the rollback phase checks.
exec >/dev/ttyS0 2>&1
say() { echo "OSMIUM-$*"; }
esp() { ls /efi/EFI/Linux | tr '\n' ' ' | sed "s/^/OSMIUM-ESP-$1 /"; echo; }
checks() {
  # Let the boot settle, so units that fail late are seen. Only here: the phases that just
  # count boots do not wait.
  sleep 20
  # No unit may hold the boot for a device this machine does not have (a pulled-in tpm2.target
  # once cost 90 s on every boot without a TPM).
  journalctl -b --no-pager | grep -m3 'Timed out waiting for device' | sed 's/^/OSMIUM-DEVICE-TIMEOUT /'
  say "FAILED-UNITS $(systemctl --failed --no-legend --plain | tr '\n' ';')"
  say "GREETD $(systemctl is-active greetd)"
  journalctl -b -p err --no-pager | tail -40
  [ -d /home/tester/.config/quickshell/ii ] && say DOTS-OK
  # Smoke-run the read-only tools on a real installed system (output only, never fatal).
  for t in "osmium-boot" "osmium-doctor" "osmium-hw" "osmium-changes list" "osmium-auto check" "osmium-battery" "osmium-guest status" "osmium-apply plan /dev/null"; do
    echo "OSMIUM-TOOL $t -> exit $(timeout 60 $t </dev/null >/tmp/t.out 2>&1; echo $?)"
    head -c 1500 /tmp/t.out | sed 's/^/  | /'
  done
  pacman -Qkq >/tmp/qk 2>&1 && say PKG-FILES-OK || sed 's/^/OSMIUM-MISSING /' /tmp/qk | head -20
  snapper --no-dbus -c root list | grep -q Kurulum && say SNAPSHOT-OK
  # The initramfs carries the rescue menu (mkinitcpio hook osmium-kurtar).
  lsinitcpio /boot/initramfs-linux-zen.img | grep -q 'usr/bin/osmium-boot$' \
    && lsinitcpio /boot/initramfs-linux-zen.img | grep -q 'emergency.service.d/osmium-rescue.conf$' && say INITRD-RESCUE-OK
  [ "$(cat /etc/hostname)" = osmium-test ] && id tester && ! id osmium 2>/dev/null && say BOOT-OK
  # Mixing: ii with synthwave's accent color and wallpaper; synthwave cannot take ii's bar.
  runuser -u tester -- osmium-dots parcalar synthwave | sed 's/^/OSMIUM-PARTS /'
  runuser -u tester -- osmium-dots karistir neon ii renk=synthwave duvar=synthwave kilit=synthwave \
    && grep -q 'karisim.renk=#ff2bd6' /home/tester/.local/share/osmium/dots/neon/info \
    && grep -q 'karisim.kilit=hyprlock' /home/tester/.local/share/osmium/dots/neon/info \
    && [ -f /home/tester/.local/share/osmium/dots/neon/home/.config/hypr/hyprlock.conf ] \
    && runuser -u tester -- env HOME=/tmp/mixhome XDG_STATE_HOME=/tmp/mixhome/.local/state XDG_DATA_HOME=/home/tester/.local/share osmium-dots apply neon >/dev/null \
    && [ -d /tmp/mixhome/.config/quickshell/ii ] && say MIX-OK
  # synthwave as the base takes m3ww's EWW bar and launcher.
  runuser -u tester -- osmium-dots karistir neonbar synthwave cubuk=m3ww baslatici=m3ww \
    && [ -d /home/tester/.local/share/osmium/dots/neonbar/home/.config/eww ] \
    && grep -q 'karisim.baslatici=fuzzel' /home/tester/.local/share/osmium/dots/neonbar/info && say MIX2-OK
}
efibyte() { od -An -tu1 -j4 -N1 "/sys/firmware/efi/efivars/$1-8be4df61-93ca-11d2-aa0d-00e098032b8c" 2>/dev/null | tr -d ' '; }

if [ -e /root/sadece-acilis ]; then
  # Next to Windows: one boot, /boot is our XBOOTLDR partition.
  checks
  [ "$(findmnt -n -o FSTYPE /boot)" = vfat ] && [ -e /boot/EFI/Linux/osmium.efi ] && say XBOOTLDR-OK
  systemctl poweroff
  exit
fi

if grep -q rd.luks /etc/kernel/cmdline; then
  dev=/dev/disk/by-uuid/$(sed -n 's/.*rd.luks.name=\([^=]*\)=.*/\1/p' /etc/kernel/cmdline)
  if [ ! -e /root/tpm-faz1 ]; then
    # First boot, unlocked with the passphrase: Secure Boot on, TPM2 enrolled by osmium-boot ilk.
    touch /root/tpm-faz1
    checks
    say "SECUREBOOT $(efibyte SecureBoot) SETUPMODE $(efibyte SetupMode)"
    for i in $(seq 30); do [ -e /var/lib/osmium/boot/tpm.key ] || break; sleep 2; done
    systemctl status osmium-boot-first.service --no-pager | tail -8
    cryptsetup luksDump "$dev" | grep -q systemd-tpm2 && say TPM-ENROLLED
    [ -e /var/lib/osmium/boot/tpm.key ] || say TPM-KEY-GONE
    sync
  else
    say TPM-UNLOCK-OK
  fi
  systemctl poweroff
  exit
fi

if [ ! -e /root/fazA ]; then
  # Phase A: a good update, installed as a counted entry.
  checks
  touch /root/fazA
  touch /boot/osmium.efi
  osmium-boot kur
  esp A
  say UPDATE-ARMED
  sync; systemctl reboot
elif [ ! -e /root/fazB ]; then
  # Phase B: the update booted, so systemd-bless-boot made it the main entry. Then a broken one.
  for i in $(seq 60); do systemctl is-active -q systemd-bless-boot.service && break; sleep 2; done
  esp B
  [ -e /efi/EFI/Linux/osmium.efi ] && [ -e /efi/EFI/Linux/osmium-kurtarma.efi ] \
    && ! ls /efi/EFI/Linux/osmium+* >/dev/null 2>&1 && say BLESS-OK
  touch /root/fazB
  n=$(snapper --no-dbus -c root create -t pre -p -c number -d test-guncelleme)
  touch /root/guncelleme-sonrasi
  # Panics before the root is mounted; panic=1 reboots, systemd-boot counts the try.
  ukify build --linux=/boot/vmlinuz-linux-zen --initrd=/boot/initramfs-linux-zen.img \
    --cmdline="panic=1 rdinit=/yok root=/dev/yok" --output=/boot/osmium.efi && say BROKEN-BUILT
  osmium-boot kur
  esp C
  say "ROLLBACK-ARMED $n $(cat /var/lib/osmium/boot/geri-don)"
  sync; systemctl reboot
else
  # Phase C: three failed tries, the rescue copy booted, osmium-boot kontrol rolled back.
  esp D
  cat /var/lib/osmium/boot/son-geri-donus
  [ ! -e /root/guncelleme-sonrasi ] && [ -e /var/lib/osmium/boot/son-geri-donus ] \
    && [ -e /efi/EFI/Linux/osmium.efi ] && say ROLLBACK-OK
  systemctl poweroff
fi
