#!/usr/bin/env bash
# teardown-gadget.sh - unbind and fully remove the beaglewing USB gadget.
#
# Safe on partial state: removes whatever exists, in the required order
# (unbind first, then config symlinks, then functions/configs/strings,
# then the gadget directory). Leaves the UDC and dwc2 untouched.

set -euo pipefail

G=/sys/kernel/config/usb_gadget/beaglewing

[[ $EUID -eq 0 ]] || { echo "ERROR: must run as root" >&2; exit 1; }

if [[ ! -d $G ]]; then
    echo "No gadget directory at $G; nothing to tear down."
    exit 0
fi

bound=$(cat "$G/UDC" 2>/dev/null || true)
if [[ -n $bound ]]; then
    echo "Unbinding from UDC '$bound'..."
    echo "" > "$G/UDC"
fi

[[ -L $G/configs/c.1/hid.kbd ]] && rm "$G/configs/c.1/hid.kbd"
[[ -L $G/configs/c.1/hid.mouse ]] && rm "$G/configs/c.1/hid.mouse"
[[ -L $G/configs/c.1/hid.data ]] && rm "$G/configs/c.1/hid.data"
[[ -d $G/configs/c.1/strings/0x409 ]] && rmdir "$G/configs/c.1/strings/0x409"
[[ -d $G/configs/c.1 ]] && rmdir "$G/configs/c.1"
[[ -d $G/functions/hid.kbd ]] && rmdir "$G/functions/hid.kbd"
[[ -d $G/functions/hid.mouse ]] && rmdir "$G/functions/hid.mouse"
[[ -d $G/functions/hid.data ]] && rmdir "$G/functions/hid.data"
[[ -d $G/strings/0x409 ]] && rmdir "$G/strings/0x409"
rmdir "$G"

echo "Gadget removed."
