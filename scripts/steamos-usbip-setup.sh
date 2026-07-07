#!/bin/bash
# One-time SteamOS setup for stargaze's built-in-controller handoff
# (USB/IP tunneling of the Deck's own controller to the streaming
# server). Run ON the Steam Deck:
#
#   sudo bash steamos-usbip-setup.sh
#
# What it does (all under /etc, which persists across SteamOS updates):
#  - loads the usbip-host kernel module now and on every boot
#  - makes the usbip sysfs knobs AND the generic usb driver's unbind
#    knob writable by the `wheel` group (the `deck` user is a member),
#    mirroring stargaze's NixOS usb-client module, so the client runs
#    unprivileged
#
# Rerun after a major SteamOS update if the handoff stops working.
set -euo pipefail

if [ "$(id -u)" != 0 ]; then
    echo "Run with sudo." >&2
    exit 1
fi

GROUP=wheel

modprobe usbip-host
echo usbip-host > /etc/modules-load.d/stargaze-usbip.conf

# All static sysfs paths (they exist once usbcore + usbip-host are up)
# get their permissions from this boot-time oneshot. Note the $$
# escaping: systemd expands single-$ variables in ExecStart itself.
cat > /etc/systemd/system/stargaze-usbip-perms.service <<EOF
[Unit]
Description=Group-writable usbip sysfs knobs for stargaze
After=systemd-modules-load.service

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'for p in /sys/bus/usb/drivers/usbip-host/match_busid /sys/bus/usb/drivers/usbip-host/bind /sys/bus/usb/drivers/usbip-host/unbind /sys/bus/usb/drivers/usbip-host/rebind /sys/bus/usb/drivers/usb/unbind /sys/bus/usb/drivers_probe; do [ -e "\$\$p" ] && chgrp ${GROUP} "\$\$p" && chmod g+w "\$\$p"; done; exit 0'

[Install]
WantedBy=multi-user.target
EOF

# The per-device usbip_sockfd attribute only exists after the stub
# binds the device — that IS a udev "bind" event, so a rule can chase
# it. (Static paths deliberately stay out of here: udev only fires on
# real events, and `udevadm trigger` emits "change" events that an
# add|bind rule never sees.)
cat > /etc/udev/rules.d/60-stargaze-usbip.rules <<EOF
# Hand the usbip stub its tunnel socket once a device binds (stargaze).
ACTION=="bind", SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", DRIVER=="usbip-host", RUN+="/bin/sh -c 'f=/sys%p/usbip_sockfd; [ -e \$\$f ] && chgrp ${GROUP} \$\$f && chmod g+w \$\$f; exit 0'"
EOF

systemctl daemon-reload
systemctl enable stargaze-usbip-perms.service
systemctl restart stargaze-usbip-perms.service
udevadm control --reload

echo "Checking permissions:"
ls -l /sys/bus/usb/drivers/usb/unbind /sys/bus/usb/drivers/usbip-host/bind
echo "Done. Enable 'Handoff built-in controller' in the stargaze launcher settings."
