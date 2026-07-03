# Client-side permissions for stargaze's built-in USB forwarding.
#
# The stargaze client binds forwarded devices (Steam Controller
# hardware) to the kernel's `usbip-host` stub and hands the kernel one
# end of a socket pair; the USB/IP byte flow then rides the session's
# QUIC connection. The sysfs attributes involved are root-only by
# default — this module makes them writable by a dedicated group and
# adds the configured users to it, so the client runs unprivileged
# (same spirit as Sunshine's udev rule for /dev/uinput).
{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.stargaze.usbClient;
  group = "stargaze-usb";
in {
  options.services.stargaze.usbClient = {
    enable = lib.mkEnableOption ''
      permissions for stargaze's client-side USB forwarding (binding
      devices to the usbip-host stub without root)
    '';
    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = "Users allowed to export USB devices via stargaze.";
    };
  };

  config = lib.mkIf cfg.enable {
    boot.kernelModules = ["usbip_host"];
    users.groups.${group}.members = cfg.users;

    # Driver-level knobs exist once the module is loaded; sysfs
    # permissions don't survive reboots, so grant them after
    # modules-load on every boot.
    systemd.services.stargaze-usb-client-perms = {
      description = "Group-writable usbip-host sysfs knobs for stargaze";
      after = ["systemd-modules-load.service"];
      wantedBy = ["multi-user.target"];
      serviceConfig.Type = "oneshot";
      script = ''
        for p in /sys/bus/usb/drivers/usbip-host/{match_busid,bind,unbind,rebind} \
                 /sys/bus/usb/drivers_probe; do
          if [ -e "$p" ]; then
            chgrp ${group} "$p"
            chmod g+w "$p"
          else
            echo "missing $p (usbip_host module not loaded?)" >&2
          fi
        done
      '';
    };

    services.udev.extraRules = ''
      # Let the stargaze group detach a device from its regular driver
      # (the unbind knob of whichever driver holds it) and hand the
      # stub its tunnel socket once bound.
      ACTION=="add|bind", SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", \
        RUN+="${pkgs.runtimeShell} -c 'for f in /sys/bus/usb/drivers/usb/unbind /sys%p/usbip_sockfd; do [ -e $$f ] && ${pkgs.coreutils}/bin/chgrp ${group} $$f && ${pkgs.coreutils}/bin/chmod g+w $$f; done; exit 0'"
    '';
  };
}
