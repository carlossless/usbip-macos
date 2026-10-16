#!/usr/bin/env bash
# Exports a composite Linux USB gadget over USB/IP for the end-to-end tests in
# tests/e2e_real.rs. Needs root, configfs, and the usbip-vudc and libcomposite modules.
#
# Run it in a disposable VM, not on a machine you use: with usbip-vudc and these functions
# loaded, the test host has hung hard enough to need a reboot.
#
#   linux-gadget.sh up [functions...]   # default: loopback storage serial hid audio
#   linux-gadget.sh down
#
# Functions and what serves them on the Linux side:
#   loopback  f_loopback: bulk OUT is echoed on bulk IN
#   storage   f_mass_storage backed by a FAT image holding HELLO.TXT ($STATE/disk.img)
#   serial    f_acm, echoed by socat on /dev/ttyGS*
#   hid       f_hid with 64-byte vendor reports, output reports echoed as input reports
#   audio     f_uac2, playback from the host recorded to $STATE/audio.raw (S16LE, 48 kHz, stereo)
#
# Gadget VID:PID is 1d6b:0104. It is served by a dedicated usbipd on $PORT (default 3250),
# leaving any system usbipd alone.
set -euo pipefail

G=/sys/kernel/config/usb_gadget/usbip-e2e
STATE=${STATE:-/run/usbip-gadget}
PORT=${PORT:-3250}
HELLO="hello from the linux gadget"

down() {
  for pid in "$STATE"/*.pid; do
    # Each helper is a setsid session; kill the group so its children go too.
    [ -e "$pid" ] && kill -- "-$(cat "$pid")" 2>/dev/null || true
    rm -f "$pid"
  done
  if [ -f "$STATE/usbipd.pid" ]; then
    kill "$(cat "$STATE/usbipd.pid")" 2>/dev/null || true
    rm -f "$STATE/usbipd.pid"
  fi
  if [ -d $G ]; then
    echo "" > $G/UDC 2>/dev/null || true
    find $G/configs -maxdepth 2 -type l -delete
    for d in $G/configs/*/strings/* $G/configs/* $G/functions/* $G/strings/*; do
      [ -d "$d" ] && rmdir "$d" 2>/dev/null || true
    done
    rmdir $G 2>/dev/null || true
  fi
  modprobe -r usbip-vudc 2>/dev/null || true
}

hid_report_descriptor() {
  # Vendor page 0xff00, 64-byte input and output reports.
  # One write: configfs keeps only the last write to report_desc.
  printf '\x06\x00\xff\x09\x01\xa1\x01\x15\x00\x26\xff\x00\x75\x08\x95\x40\x09\x01\x81\x02\x09\x01\x91\x02\xc0'
}

daemon() {
  local name=$1
  shift
  setsid "$@" >"$STATE/$name.log" 2>&1 < /dev/null &
  echo $! > "$STATE/$name.pid"
}

if [ "${1:-}" = down ]; then
  down
  exit 0
fi
[ "${1:-}" = up ] || { echo "usage: $0 up [functions...] | down" >&2; exit 2; }
shift
functions=("$@")
[ ${#functions[@]} -gt 0 ] || functions=(loopback storage serial hid audio)

down
mkdir -p "$STATE"
modprobe libcomposite
modprobe usbip-vudc num=1
mountpoint -q /sys/kernel/config || mount -t configfs none /sys/kernel/config

mkdir $G
cd $G
echo 0x1d6b > idVendor
echo 0x0104 > idProduct
echo 0x0100 > bcdDevice
echo 0x0200 > bcdUSB
echo 0xef > bDeviceClass
echo 0x02 > bDeviceSubClass
echo 0x01 > bDeviceProtocol
mkdir strings/0x409
echo usbip-e2e > strings/0x409/serialnumber
echo Linux > strings/0x409/manufacturer
echo "usbip e2e gadget" > strings/0x409/product
mkdir -p configs/c.1/strings/0x409
echo 250 > configs/c.1/MaxPower
echo e2e > configs/c.1/strings/0x409/configuration

for f in "${functions[@]}"; do
  case $f in
    loopback)
      mkdir functions/Loopback.0
      ln -s functions/Loopback.0 configs/c.1/
      ;;
    storage)
      rm -f "$STATE/disk.img"
      mkfs.vfat -n USBIPE2E -C "$STATE/disk.img" 4096 >/dev/null
      printf '%s\n' "$HELLO" > "$STATE/HELLO.TXT"
      mcopy -i "$STATE/disk.img" "$STATE/HELLO.TXT" ::HELLO.TXT
      mkdir functions/mass_storage.0
      # Not removable: a host eject would otherwise clear the backing file.
      echo 0 > functions/mass_storage.0/lun.0/removable
      echo "$STATE/disk.img" > functions/mass_storage.0/lun.0/file
      ln -s functions/mass_storage.0 configs/c.1/
      ;;
    serial)
      mkdir functions/acm.usb0
      ln -s functions/acm.usb0 configs/c.1/
      ;;
    hid)
      mkdir functions/hid.usb0
      echo 0 > functions/hid.usb0/protocol
      echo 0 > functions/hid.usb0/subclass
      echo 64 > functions/hid.usb0/report_length
      hid_report_descriptor > functions/hid.usb0/report_desc
      ln -s functions/hid.usb0 configs/c.1/
      ;;
    audio)
      mkdir functions/uac2.usb0
      # Host playback only (USB OUT): isochronous IN data is dropped by macOS.
      echo 0 > functions/uac2.usb0/p_chmask
      echo 48000 > functions/uac2.usb0/c_srate
      ln -s functions/uac2.usb0 configs/c.1/
      ;;
    *)
      echo "unknown function $f" >&2
      exit 2
      ;;
  esac
done

echo usbip-vudc.0 > UDC

for f in "${functions[@]}"; do
  case $f in
    serial)
      tty=$(ls /dev/ttyGS* | head -1)
      # socat exits when the host detaches; restart it for the next attach.
      daemon serial sh -c "while :; do socat $tty,raw,echo=0 SYSTEM:cat; sleep 0.2; done"
      ;;
    hid)
      daemon hid sh -c 'while :; do head -c 64 /dev/hidg0 > /dev/hidg0 || sleep 0.1; done'
      ;;
    audio)
      card=$(awk -F'[][]' '/UAC2/ {print $2; exit}' /proc/asound/cards | tr -d ' ')
      : > "$STATE/audio.raw"
      # arecord fails while no host streams; keep restarting and append raw samples.
      daemon audio sh -c "while :; do arecord -q -t raw -D hw:CARD=${card:-UAC2Gadget} -f S16_LE -r 48000 -c 2 >> $STATE/audio.raw; sleep 0.5; done"
      ;;
  esac
done

usbipd -e -D --tcp-port "$PORT" --pid="$STATE/usbipd.pid"
sleep 0.5
usbip --tcp-port "$PORT" list -r 127.0.0.1
