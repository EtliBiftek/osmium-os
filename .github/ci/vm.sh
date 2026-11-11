#!/bin/sh
# vm.sh <log> [extra qemu args]: one QEMU run of the test machine; serial output goes to <log>,
# a second serial port (guest ttyS1, nothing else writes there) to <log>.data for binary data.
# Env: DISK (qcow2), VARS (UEFI variables), SB=1 (Secure Boot firmware + swtpm on tpm.sock),
#      REBOOT=1 (let the guest reboot instead of exiting), MINUTES (timeout, default 20).
# A guest still running at the timeout is hung: its screen goes to <log>.ppm before it is
# killed, and the exit status is 124.
log=$1; shift
# cache=unsafe: the guest's flushes stop at the host page cache. The disk only has to survive
# QEMU exiting, not the runner losing power, and installs are mostly write + fsync.
set -- -drive if=none,id=t,file="$DISK",format=qcow2,discard=unmap,cache=unsafe -device virtio-blk-pci,drive=t,serial=target "$@"
if [ "${SB:-0}" = 1 ]; then
  code=/usr/share/OVMF/OVMF_CODE_4M.secboot.fd
  set -- -machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on \
    -chardev socket,id=chrtpm,path=tpm.sock -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-tis,tpmdev=tpm0 "$@"
else
  code=/usr/share/OVMF/OVMF_CODE_4M.fd
  set -- -machine q35 "$@"
fi
[ "${REBOOT:-0}" = 1 ] || set -- -no-reboot "$@"
rm -f mon.sock
qemu-system-x86_64 -enable-kvm -cpu host -m 6G -smp 4 -display none \
  -drive if=pflash,format=raw,readonly=on,file=$code -drive if=pflash,format=raw,file="$VARS" \
  -nic user,model=virtio-net-pci -monitor unix:mon.sock,server,nowait -serial "file:$log" \
  -chardev file,id=data,path="$log.data" -device isa-serial,chardev=data "$@" &
qemu=$!
# Watchdog, in one-second steps so it is gone right after a guest that finished in time.
(
  i=$(( ${MINUTES:-20} * 60 ))
  while [ $i -gt 0 ]; do sleep 1; i=$((i - 1)); done
  echo "screendump $log.ppm" | socat - unix-connect:mon.sock
  sleep 1
  kill $qemu
) >/dev/null 2>&1 &
dog=$!
wait $qemu
rc=$?
kill $dog 2>/dev/null
if [ $rc = 143 ]; then
  echo "vm.sh: $log: still running after ${MINUTES:-20} minutes, killed" >&2
  rc=124
fi
exit $rc
