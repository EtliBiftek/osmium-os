#!/bin/sh
# vm.sh <log> [extra qemu args]: one QEMU run of the test machine; serial output goes to <log>,
# a second serial port (guest ttyS1, nothing else writes there) to <log>.data for binary data.
# Env: DISK (qcow2), VARS (UEFI variables), SB=1 (Secure Boot firmware + swtpm on tpm.sock),
#      REBOOT=1 (let the guest reboot instead of exiting), MINUTES (timeout, default 20).
log=$1; shift
set -- -drive if=none,id=t,file="$DISK",format=qcow2,discard=unmap -device virtio-blk-pci,drive=t,serial=target "$@"
if [ "${SB:-0}" = 1 ]; then
  code=/usr/share/OVMF/OVMF_CODE_4M.secboot.fd
  set -- -machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on \
    -chardev socket,id=chrtpm,path=tpm.sock -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-tis,tpmdev=tpm0 "$@"
else
  code=/usr/share/OVMF/OVMF_CODE_4M.fd
  set -- -machine q35 "$@"
fi
[ "${REBOOT:-0}" = 1 ] || set -- -no-reboot "$@"
exec timeout "${MINUTES:-20}m" qemu-system-x86_64 -enable-kvm -cpu host -m 6G -smp 4 -display none \
  -drive if=pflash,format=raw,readonly=on,file=$code -drive if=pflash,format=raw,file="$VARS" \
  -nic user,model=virtio-net-pci -monitor unix:mon.sock,server,nowait -serial "file:$log" \
  -chardev file,id=data,path="$log.data" -device isa-serial,chardev=data "$@"
