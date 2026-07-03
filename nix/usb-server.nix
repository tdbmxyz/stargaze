# Server-side permissions for stargaze's built-in USB forwarding.
#
# The stargaze server attaches tunneled devices to the `vhci-hcd`
# virtual host controller, making them genuine USB devices on the host
# (Steam requires that for Valve controllers — it checks USB interface
# numbers). The vhci sysfs attributes are root-only by default; this
# module makes them writable by a dedicated group so the server runs
# unprivileged.
{
  config,
  lib,
  ...
}: let
  cfg = config.services.stargaze.usbServer;
  group = "stargaze-usb";
in {
  options.services.stargaze.usbServer = {
    enable = lib.mkEnableOption ''
      permissions for stargaze's server-side USB forwarding (attaching
      tunneled devices to vhci-hcd without root)
    '';
    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = "Users allowed to attach tunneled USB devices.";
    };
  };

  config = lib.mkIf cfg.enable {
    boot.kernelModules = ["vhci-hcd"];
    users.groups.${group}.members = cfg.users;

    # sysfs permissions don't survive reboots; grant them after
    # modules-load on every boot.
    systemd.services.stargaze-usb-server-perms = {
      description = "Group-writable vhci-hcd sysfs knobs for stargaze";
      after = ["systemd-modules-load.service"];
      wantedBy = ["multi-user.target"];
      serviceConfig.Type = "oneshot";
      script = ''
        for c in /sys/devices/platform/vhci_hcd.*; do
          for f in attach detach; do
            [ -e "$c/$f" ] && { chgrp ${group} "$c/$f"; chmod g+w "$c/$f"; }
          done
        done
      '';
    };
  };
}
