#!/bin/bash
# test.sh <luks|plain|win|ui>: one leg of the install-test job. The legs share nothing but the
# ISO in the working directory, so the workflow runs them on separate runners at once.
#   luks   plain install, LUKS over it (Secure Boot firmware in Setup Mode, software TPM), then
#          two boots: passphrase, then TPM2 only
#   plain  install, then boot through a good update and a broken one that must roll back
#   win    install next to a stand-in Windows disk, then boot
#   ui     the live desktop and the installer window
set -eu
ci=$(dirname "$0")
leg=$1
iso=$(ls ./*.iso)
bsdtar -xf "$iso" arch/boot/x86_64 loader boot/grub 2>/dev/null || true
params=$(grep -rhom1 'archisobasedir=[^ ]* archisosearchuuid=[^ ]*' loader boot | head -1)
[ -n "$params" ] || { echo "no archiso boot parameters found in the ISO"; exit 1; }

has() { sudo grep -aq -- "$1" "$2"; }
# Without the terminal escapes of the firmware and the boot menu, which hide what follows them.
show() { sudo sed 's/\x1b\[[0-9;=?]*[A-Za-z]//g' "$1" | cut -c1-400 || true; }
# need <log> <marker> <message>
need() { has "OSMIUM-$2" "$1" || { echo "::error::$3"; sudo tail -60 "$1" | cut -c1-400; exit 1; }; }
vars() { cp /usr/share/OVMF/OVMF_VARS_4M.fd "$1"; }
tpm() { sudo pkill swtpm || true; sleep 1; mkdir -p tpm; sudo swtpm socket --tpmstate dir=tpm --ctrl type=unixio,path=tpm.sock --tpm2 --daemon; }
# live <script> <log> [extra qemu args]: boots the ISO's kernel and runs the script as root.
live() {
  local script=$1 log=$2; shift 2
  sudo env DISK="$DISK" VARS="$VARS" SB="${SB:-0}" MINUTES="${MINUTES:-20}" "$ci/vm.sh" "$log" -cdrom "$iso" "$@" \
    -drive if=none,id=s,file="$script",format=raw,readonly=on -device virtio-blk-pci,drive=s,serial=osmtest \
    -kernel arch/boot/x86_64/vmlinuz-linux-zen -initrd arch/boot/x86_64/initramfs-linux-zen.img \
    -append "$params console=ttyS0 systemd.run=\"/bin/sh /dev/disk/by-id/virtio-osmtest\"" || true
}
# inst <enc> <disk> <vars> <secure boot> <log name> [windows]
inst() {
  { echo "enc=$1"; echo "win=${6:-0}"; echo "cat > /tmp/boot-test.sh <<'BOOTTEST'"; cat "$ci/boot-test.sh"; echo BOOTTEST
    cat "$ci/install-body.sh"; } > test.img
  truncate -s 1M test.img
  vars "$3"
  [ "$4" = 0 ] || tpm
  DISK=$2 VARS=$3 SB=$4 live test.img "install-$5.log"
  sudo grep -a 'OSMIUM-' "install-$5.log" | grep -av '"step":"copy"' | cut -c1-400 || true
  if has 'OSMIUM-LEFT-' "install-$5.log"; then echo "::warning::$5: the installer left the target disk mounted or open"; fi
}
# installed <log name>: the checks every finished install has to pass.
installed() {
  need "install-$1.log" INSTALL-OK "$1: install failed"
  need "install-$1.log" ESP-OK "$1: no signed UKI / systemd-boot on the ESP"
  echo "Install ($1, 4 cores): $(sudo grep -ao 'OSMIUM-RUN-. {"done":true,"seconds":[0-9]*' "install-$1.log" | grep -o '[0-9]*$') s" >> "$GITHUB_STEP_SUMMARY"
}
# booted <log>: what every first boot of an installed system has to show.
booted() {
  need "$1" BOOT-OK "$1: the installed system did not boot to multi-user"
  # Serial log lines end in CR, so require a unit name, not just any character.
  if has "OSMIUM-FAILED-UNITS [[:alnum:]@_.-]\+" "$1"; then echo "::error::$1: units failed on the installed system"; exit 1; fi
  if has OSMIUM-DEVICE-TIMEOUT "$1"; then echo "::error::$1: the boot waited for a device that never came"; exit 1; fi
  has 'OSMIUM-BOOT-TITLES.*title: OsmiumOS' "$1" || echo "::warning::$1: the boot menu entry is not titled OsmiumOS"
}

case $leg in
luks)
  qemu-img create -q -f qcow2 disk-luks.qcow2 24G
  inst 0 disk-luks.qcow2 vars-wipe.fd 0 wipe
  need install-wipe.log INSTALL-OK "first plain install failed"
  inst 1 disk-luks.qcow2 vars-luks.fd 1 luks
  need install-luks.log LUKS-OK "encrypted install failed"
  need install-luks.log LUKS2-OK "the disk is not LUKS2"
  installed luks
  tpm
  # First boot: the passphrase comes as a systemd credential; typed keys are the fallback
  # (only letters and digits: the initramfs keymap is Turkish Q).
  ( for r in 1 2 3; do sleep 45; for k in p a r o l a 2 0 2 6 s v ret; do echo "sendkey $k" | sudo socat - unix-connect:mon.sock >/dev/null 2>&1 || true; done; done ) &
  typer=$!
  sudo env DISK=disk-luks.qcow2 VARS=vars-luks.fd SB=1 MINUTES=10 "$ci/vm.sh" boot-luks-1.log \
    -smbios type=11,value=io.systemd.credential:cryptsetup.passphrase=parola2026sv || true
  # Not `wait` alone: the typist sleeps on long after the guest is gone.
  kill $typer 2>/dev/null || true; wait $typer 2>/dev/null || true
  show boot-luks-1.log
  tpm
  # Second boot: nothing types the passphrase; only the TPM can open the disk.
  sudo env DISK=disk-luks.qcow2 VARS=vars-luks.fd SB=1 MINUTES=6 "$ci/vm.sh" boot-luks-2.log || true
  show boot-luks-2.log
  booted boot-luks-1.log
  need boot-luks-1.log "SECUREBOOT 1 SETUPMODE 0" "Secure Boot is not on after the install enrolled the keys"
  need boot-luks-1.log TPM-ENROLLED "no TPM2 token after the first boot"
  has OSMIUM-TPM-KEY-GONE boot-luks-1.log || echo "::warning::the one-time key file is still there"
  need boot-luks-2.log TPM-UNLOCK-OK "the disk did not open with the TPM"
  ;;
plain)
  qemu-img create -q -f qcow2 disk-plain.qcow2 24G
  inst 0 disk-plain.qcow2 vars-plain.fd 0 plain
  installed plain
  # Reboots are allowed: the test reboots twice itself, and the broken kernel three times.
  sudo env DISK=disk-plain.qcow2 VARS=vars-plain.fd REBOOT=1 MINUTES=15 "$ci/vm.sh" boot-plain.log || true
  show boot-plain.log
  booted boot-plain.log
  has OSMIUM-DOTS-OK boot-plain.log || echo "::warning::ii dots were not applied to the installed user"
  has OSMIUM-PKG-FILES-OK boot-plain.log || echo "::warning::installed packages are missing files (pacman -Qk)"
  has OSMIUM-SNAPSHOT-OK boot-plain.log || echo "::warning::no 'Kurulum' snapper snapshot after install"
  need boot-plain.log UPDATE-ARMED "osmium-boot kur did not install the update"
  need boot-plain.log BLESS-OK "the updated entry was not blessed after a good boot"
  need boot-plain.log BROKEN-BUILT "could not build the broken test kernel"
  need boot-plain.log ROLLBACK-OK "a kernel that does not boot was not rolled back"
  need boot-plain.log MIX-OK "osmium-dots karistir/apply of a mixed dot failed"
  need boot-plain.log MIX2-OK "mixing a bar and a launcher into synthwave failed"
  need boot-plain.log INITRD-RESCUE-OK "the initramfs has no rescue menu (osmium-kurtar hook)"
  ;;
win)
  qemu-img create -q -f qcow2 disk-win.qcow2 64G
  inst 0 disk-win.qcow2 vars-win.fd 0 win 1
  need install-win.log WINDOWS-INTACT "the NTFS partition did not survive the install next to it"
  need install-win.log ESP-SHARED-OK "systemd-boot is not on the Windows ESP next to Windows Boot Manager"
  installed win
  sudo env DISK=disk-win.qcow2 VARS=vars-win.fd MINUTES=8 "$ci/vm.sh" boot-win.log || true
  show boot-win.log
  booted boot-win.log
  need boot-win.log XBOOTLDR-OK "/boot is not the XBOOTLDR partition"
  ;;
ui)
  cp "$ci/ui-test.sh" ui.img; truncate -s 1M ui.img
  qemu-img create -q -f qcow2 disk-ui.qcow2 1G; vars vars-ui.fd
  DISK=disk-ui.qcow2 VARS=vars-ui.fd MINUTES=10 live ui.img ui.log -vga virtio
  sudo grep -a 'OSMIUM-' ui.log | grep -av SCREENSHOT | cut -c1-400 || true
  sudo grep -ao 'OSMIUM-SCREENSHOT [A-Za-z0-9+/=]*' ui.log.data | head -1 | cut -d' ' -f2 | base64 -d > installer.png || true
  sudo grep -ao 'OSMIUM-PREVIEW [A-Za-z0-9+/=]*' ui.log.data | head -1 | cut -d' ' -f2 | base64 -d > preview-synthwave.png || true
  [ -s preview-synthwave.png ] || echo "::warning::osmium-dots onizle made no screenshot in the live desktop"
  need ui.log UI-RUNNING "the installer window did not start in the live desktop"
  need ui.log UI-CLEAN "the installer QML logged errors (see OSMIUM-UI-LOG lines)"
  need ui.log UI-SHELL-ALIVE "the ii shell was not running next to the installer"
  need ui.log UI-SELFTEST-OK "the installer's page walk failed (see OSMIUM-UI-OSMIUM-SELFTEST lines)"
  ;;
*)
  echo "usage: test.sh luks|plain|win|ui"; exit 2 ;;
esac
