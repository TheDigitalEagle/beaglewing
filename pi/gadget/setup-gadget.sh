#!/usr/bin/env bash
# setup-gadget.sh - create and bind the beaglewing USB HID gadget.
#
# Composite: HID keyboard, HID absolute pointer, vendor raw HID data channel.
# Idempotent: exits successfully if the gadget is already bound; refuses
# to run on top of a half-built gadget (run teardown-gadget.sh first).
# Enabled at boot via beaglewing-gadget.service now that it has proven stable.

set -euo pipefail

CONFIGFS=/sys/kernel/config/usb_gadget
GADGET_NAME=beaglewing
G="$CONFIGFS/$GADGET_NAME"

die() { echo "ERROR: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "must run as root"

modprobe libcomposite
[[ -d $CONFIGFS ]] || die "configfs usb_gadget not available; is libcomposite loaded?"

# Idempotency / inconsistent-state guard
if [[ -d $G ]]; then
    bound=$(cat "$G/UDC" 2>/dev/null || true)
    if [[ -n $bound ]]; then
        echo "Gadget already set up and bound to UDC '$bound'; nothing to do."
        exit 0
    fi
    die "gadget directory exists but is not bound (partial state); run teardown-gadget.sh first"
fi

# Detect the UDC before building anything, so we fail loudly up front.
shopt -s nullglob
udcs=(/sys/class/udc/*)
shopt -u nullglob
[[ ${#udcs[@]} -gt 0 ]] || die "no UDC in /sys/class/udc; is dtoverlay=dwc2,dr_mode=peripheral active?"
[[ ${#udcs[@]} -eq 1 ]] || die "multiple UDCs found (${udcs[*]}); refusing to guess"
UDC_NAME=$(basename "${udcs[0]}")

mkdir "$G"
cd "$G"

# Linux Foundation VID with the multifunction-composite PID; standard,
# honest identifiers; we are not disguising this device as anything.
echo 0x1d6b > idVendor
echo 0x0104 > idProduct
# Bumped on every interface change (0x0100 keyboard only, 0x0101 +pointer,
# 0x0102 +data channel, 0x0103 data channel 1024-byte reports) so Windows
# discards cached descriptors and re-enumerates fully.
echo 0x0103 > bcdDevice
echo 0x0200 > bcdUSB

mkdir -p strings/0x409
serial=$(awk '/^Serial/{print $3}' /proc/cpuinfo)
echo "${serial:-0000000000000000}" > strings/0x409/serialnumber
echo "Beaglewing Project"          > strings/0x409/manufacturer
echo "Beaglewing Input Bridge"     > strings/0x409/product

mkdir -p configs/c.1/strings/0x409
echo "HID keyboard + absolute pointer + data channel" > configs/c.1/strings/0x409/configuration
echo 250 > configs/c.1/MaxPower   # 500 mA, honest for a bus-powered Pi

mkdir -p functions/hid.kbd
echo 1 > functions/hid.kbd/protocol      # keyboard
echo 1 > functions/hid.kbd/subclass     # boot interface subclass
echo 8 > functions/hid.kbd/report_length
# Standard 8-byte boot keyboard report descriptor:
# byte 0 modifiers, byte 1 reserved, bytes 2-7 keycodes.
printf '\x05\x01\x09\x06\xa1\x01\x05\x07\x19\xe0\x29\xe7\x15\x00\x25\x01\x75\x01\x95\x08\x81\x02\x95\x01\x75\x08\x81\x03\x95\x05\x75\x01\x05\x08\x19\x01\x29\x05\x91\x02\x95\x01\x75\x03\x91\x03\x95\x06\x75\x08\x15\x00\x25\x65\x05\x07\x19\x00\x29\x65\x81\x00\xc0' \
    > functions/hid.kbd/report_desc

# Absolute pointer. Created AFTER hid.kbd so device nodes are stable:
# keyboard = /dev/hidg0, pointer = /dev/hidg1.
# Report (6 bytes): buttons(1) | X lo,hi | Y lo,hi | wheel(1).
# X/Y are absolute, logical 0..32767 (proven range for Windows absolute
# pointers); the bridge scales the project's canonical 0..65535 down by 1 bit.
mkdir -p functions/hid.mouse
echo 0 > functions/hid.mouse/protocol    # no boot protocol (abs needs report mode)
echo 0 > functions/hid.mouse/subclass
echo 6 > functions/hid.mouse/report_length
# bInterval 1: 125us endpoint polling at high speed (default 4 = 1ms) so
# rapid absolute updates drain instead of queueing ("ice skating" lag).
echo 1 > functions/hid.mouse/interval
printf '\x05\x01\x09\x02\xa1\x01\x09\x01\xa1\x00\x05\x09\x19\x01\x29\x03\x15\x00\x25\x01\x95\x03\x75\x01\x81\x02\x95\x01\x75\x05\x81\x03\x05\x01\x09\x30\x09\x31\x16\x00\x00\x26\xff\x7f\x75\x10\x95\x02\x81\x02\x09\x38\x15\x81\x25\x7f\x75\x08\x95\x01\x81\x06\xc0\xc0' \
    > functions/hid.mouse/report_desc

# Vendor raw HID data channel for clipboard sync (docs/clipboard.md).
# Created third so /dev/hidg2; keyboard and pointer keep their nodes.
# Usage page 0xFF60/usage 0x61 (QMK-style raw HID): one 1024-byte input
# report and one 1024-byte output report (the high-speed interrupt
# maximum, 16x the throughput of 64-byte reports), no report IDs. Windows binds the
# generic HID driver; userspace opens it without admin rights.
mkdir -p functions/hid.data
echo 0 > functions/hid.data/protocol
echo 0 > functions/hid.data/subclass
echo 1024 > functions/hid.data/report_length
# bInterval 1 (125us at high speed); with 1024-byte reports the channel
# tops out around 8MB/s.
echo 1 > functions/hid.data/interval
printf '\x06\x60\xff\x09\x61\xa1\x01\x09\x62\x15\x00\x26\xff\x00\x75\x08\x96\x00\x04\x81\x02\x09\x63\x75\x08\x96\x00\x04\x91\x02\xc0' \
    > functions/hid.data/report_desc

ln -s functions/hid.kbd   configs/c.1/
ln -s functions/hid.mouse configs/c.1/
ln -s functions/hid.data  configs/c.1/

# Bind last, only after every descriptor/function/config is complete.
echo "$UDC_NAME" > UDC

echo "Gadget '$GADGET_NAME' bound to UDC '$UDC_NAME'."
echo "keyboard: /dev/hidg$(cut -d: -f2 functions/hid.kbd/dev)  pointer: /dev/hidg$(cut -d: -f2 functions/hid.mouse/dev)  data: /dev/hidg$(cut -d: -f2 functions/hid.data/dev)"
ls -l /dev/hidg* 2>/dev/null || echo "WARNING: no /dev/hidg* node appeared"
