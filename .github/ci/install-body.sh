# Runs in the live system as root via systemd.run=, read straight from a read-only disk.
# The workflow prepends `enc=0|1` and the boot test as /tmp/boot-test.sh.
exec >/dev/ttyS0 2>&1
for i in $(seq 50); do [ -e /dev/disk/by-id/virtio-target ] && break; sleep 0.2; done
disk=$(readlink -f /dev/disk/by-id/virtio-target)
echo "OSMIUM-DISKS $(osmium-install disks)"
echo "OSMIUM-SETUPMODE $(od -An -tu1 -j4 -N1 /sys/firmware/efi/efivars/SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c | tr -d ' ')"
printf 'disk=%s\nencrypt=%s\npassphrase=parola2026sv\nhostname=osmium-test\nusername=tester\nfullname=Test User\npassword=ci-test-pw\ntimezone=Europe/Istanbul\nlocale=tr_TR.UTF-8\nkeymap=trq\nxkblayout=tr\ndots=ii\ndots_mode=config\n' "$disk" "$enc" \
  | osmium-install run | sed "s/^/OSMIUM-RUN-$enc /"
# Whatever still holds the target after the installer returned (should be nothing).
findmnt -rn -o TARGET,SOURCE | grep -E '^/mnt|cryptroot' | sed 's/^/OSMIUM-LEFT-MOUNT /'
[ -e /dev/mapper/cryptroot ] && echo "OSMIUM-LEFT-OPEN $(dmsetup info -c --noheadings -o name,open cryptroot)"
for p in /proc/[0-9]*; do
  grep -qE "cryptroot|$disk" "$p/mountinfo" 2>/dev/null \
    && echo "OSMIUM-HOLDER ${p#/proc/} $(cat "$p/comm") root=$(readlink "$p/root") ns=$(readlink "$p/ns/mnt")"
done | head -20
mkdir -p /tmp/r /tmp/e
dev="${disk}2"
if [ "$enc" = 1 ]; then
  printf parola2026sv | cryptsetup open --key-file=- "${disk}2" citest && dev=/dev/mapper/citest
  cryptsetup luksDump "${disk}2" | grep -q '^Version:.*2' && echo OSMIUM-LUKS2-OK
fi
# No subvol=: the root is the default subvolume.
mount "$dev" /tmp/r || { echo OSMIUM-INSTALL-FAILED; poweroff -f; sleep 60; }
mount "${disk}1" /tmp/e
ls -R /tmp/e | head -30
cat /tmp/r/etc/fstab /tmp/r/etc/kernel/cmdline /tmp/e/loader/loader.conf
[ -s /tmp/e/EFI/Linux/osmium.efi ] && [ -s /tmp/e/EFI/systemd/systemd-bootx64.efi ] && [ -s /tmp/r/boot/osmium.efi ] \
  && echo OSMIUM-ESP-OK
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
