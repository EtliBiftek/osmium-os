# Runs in the live system as root via systemd.run=, read straight from a read-only disk.
# The workflow prepends `enc=0|1`, `win=0|1` and the boot test as /tmp/boot-test.sh.
exec >/dev/ttyS0 2>&1
for i in $(seq 50); do [ -e /dev/disk/by-id/virtio-target ] && break; sleep 0.2; done
disk=$(readlink -f /dev/disk/by-id/virtio-target)
if [ "$win" = 1 ]; then
  # A stand-in for a Windows disk: ESP with Windows Boot Manager, MSR, NTFS, recovery.
  sgdisk --zap-all -n1:0:+100MiB -t1:ef00 -n2:0:+16MiB -t2:0c01 -n3:0:-700MiB -t3:0700 -n4:0:0 -t4:2700 "$disk"
  partprobe "$disk"; udevadm settle
  mkfs.fat -F32 "${disk}1" && mkdir -p /tmp/w && mount -t vfat "${disk}1" /tmp/w \
    && mkdir -p /tmp/w/EFI/Microsoft/Boot && echo fake > /tmp/w/EFI/Microsoft/Boot/bootmgfw.efi && umount /tmp/w
  mkntfs -Q -L Windows "${disk}3" && ntfs-3g "${disk}3" /tmp/w && echo merhaba > /tmp/w/belge.txt && umount /tmp/w
  mkntfs -Q -L Recovery "${disk}4"
  udevadm trigger --action=change "$disk"*; udevadm settle
  printf 'mode=yanina\nsize_gb=24\n' > /tmp/extra
else
  : > /tmp/extra
fi
echo "OSMIUM-DISKS $(osmium-install disks)"
echo "OSMIUM-SETUPMODE $(od -An -tu1 -j4 -N1 /sys/firmware/efi/efivars/SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c | tr -d ' ')"
{ printf 'disk=%s\nencrypt=%s\npassphrase=parola2026sv\nhostname=osmium-test\nusername=tester\nfullname=Test User\npassword=ci-test-pw\ntimezone=Europe/Istanbul\nlocale=tr_TR.UTF-8\nkeymap=trq\nxkblayout=tr\ndots=ii\ndots_mode=config\n' "$disk" "$enc"
  cat /tmp/extra; } | osmium-install run | sed "s/^/OSMIUM-RUN-$enc /"
# Whatever still holds the target after the installer returned (should be nothing).
findmnt -rn -o TARGET,SOURCE | grep -E '^/mnt|cryptroot' | sed 's/^/OSMIUM-LEFT-MOUNT /'
[ -e /dev/mapper/cryptroot ] && echo "OSMIUM-LEFT-OPEN $(dmsetup info -c --noheadings -o name,open cryptroot)"
for p in /proc/[0-9]*; do
  grep -qE "cryptroot|$disk" "$p/mountinfo" 2>/dev/null \
    && echo "OSMIUM-HOLDER ${p#/proc/} $(cat "$p/comm") root=$(readlink "$p/root") ns=$(readlink "$p/ns/mnt")"
done | head -20
mkdir -p /tmp/r /tmp/e
dev="${disk}2"
esp="${disk}1"
if [ "$win" = 1 ]; then
  # Ours are the two partitions after Windows' four: 5 (XBOOTLDR, /boot) and 6 (root).
  dev="${disk}6"
  lsblk -o NAME,SIZE,FSTYPE,PARTTYPENAME,LABEL "$disk"
  # --force: ntfsresize leaves the "check at next Windows boot" flag, which ntfscat respects.
  ntfscat --force "${disk}3" belge.txt | grep -q merhaba && echo OSMIUM-WINDOWS-INTACT
  mount -t vfat "${disk}1" /tmp/e && [ -s /tmp/e/EFI/Microsoft/Boot/bootmgfw.efi ] && [ -s /tmp/e/EFI/systemd/systemd-bootx64.efi ] \
    && grep -q "timeout 3" /tmp/e/loader/loader.conf && echo OSMIUM-ESP-SHARED-OK; umount /tmp/e
  esp="${disk}5"
fi
if [ "$enc" = 1 ]; then
  printf parola2026sv | cryptsetup open --key-file=- "${disk}2" citest && dev=/dev/mapper/citest
  cryptsetup luksDump "${disk}2" | grep -q '^Version:.*2' && echo OSMIUM-LUKS2-OK
fi
# No subvol=: the root is the default subvolume.
mount "$dev" /tmp/r || { echo OSMIUM-INSTALL-FAILED; poweroff -f; sleep 60; }
mount -t vfat "$esp" /tmp/e
ls -R /tmp/e | head -30
cat /tmp/r/etc/fstab /tmp/r/etc/kernel/cmdline /tmp/e/loader/loader.conf 2>/dev/null
if [ "$win" = 1 ]; then
  # /boot is the XBOOTLDR partition: kernel, initramfs and the UKIs live there.
  [ -s /tmp/e/EFI/Linux/osmium.efi ] && [ -s /tmp/e/osmium.efi ] && echo OSMIUM-ESP-OK
  touch /tmp/r/root/sadece-acilis
else
  [ -s /tmp/e/EFI/Linux/osmium.efi ] && [ -s /tmp/e/EFI/systemd/systemd-bootx64.efi ] && [ -s /tmp/r/boot/osmium.efi ] \
    && echo OSMIUM-ESP-OK
fi
[ "$enc" = 1 ] && [ -e /tmp/r/var/lib/osmium/boot/tpm.key ] && echo OSMIUM-LUKS-OK
umount /tmp/e
install -m755 /tmp/boot-test.sh /tmp/r/usr/local/bin/osmium-boot-test
printf '[Unit]\nAfter=systemd-user-sessions.service\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/osmium-boot-test\n' \
  > /tmp/r/etc/systemd/system/osmium-boot-test.service
ln -s ../osmium-boot-test.service /tmp/r/etc/systemd/system/multi-user.target.wants/
grep -q '^tester:' /tmp/r/etc/passwd && echo OSMIUM-INSTALL-OK
umount /tmp/r; sync
poweroff -f
sleep 60
