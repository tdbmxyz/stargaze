#!/bin/bash
# One-time SteamOS setup for stargaze's built-in-controller handoff
# (USB/IP tunneling of the Deck's own controller to the streaming
# server). Run ON the Steam Deck:
#
#   sudo bash steamos-usbip-setup.sh
#
# What it does (all under /etc, which persists across SteamOS updates):
#  - loads the usbip-host kernel module now and on every boot
#  - makes the usbip sysfs knobs writable by the `wheel` group (the
#    `deck` user is a member), mirroring stargaze's NixOS usb-client
#    module, so the client runs unprivileged
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

cat > /etc/systemd/system/stargaze-usbip-perms.service <<EOF
[Unit]
Description=Group-writable usbip-host sysfs knobs for stargaze
After=systemd-modules-load.service

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'for p in /sys/bus/usb/drivers/usbip-host/match_busid /sys/bus/usb/drivers/usbip-host/bind /sys/bus/usb/drivers/usbip-host/unbind /sys/bus/usb/drivers/usbip-host/rebind /sys/bus/usb/drivers_probe; do [ -e "\$p" ] && chgrp ${GROUP} "\$p" && chmod g+w "\$p"; done'

[Install]
WantedBy=multi-user.target
EOF

cat > /etc/udev/rules.d/60-stargaze-usbip.rules <<EOF
# Let the ${GROUP} group detach a USB device from its regular driver and
# hand the usbip stub its tunnel socket once bound (stargaze handoff).
ACTION=="add|bind", SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", RUN+="/bin/sh -c 'for f in /sys/bus/usb/drivers/usb/unbind /sys%p/usbip_sockfd; do [ -e \$\$f ] && chgrp ${GROUP} \$\$f && chmod g+w \$\$f; done; exit 0'"
EOF

systemctl daemon-reload
systemctl enable --now stargaze-usbip-perms.service
udevadm control --reload
udevadm trigger --subsystem-match=usb

echo "Done. Enable 'Handoff built-in controller' in the stargaze launcher settings."
