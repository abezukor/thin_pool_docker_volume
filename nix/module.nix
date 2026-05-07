self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.thin-pool-docker-volume;
  inherit (lib) mkEnableOption mkOption types mkIf;

  envFile = pkgs.writeText "thin_pool_docker_volume" (
    lib.concatStringsSep "\n" (
      lib.mapAttrsToList (k: v: "${k}=${v}") ({
        DOCKER_LVM_THIN_POOL_VG_NAME = cfg.vgName;
        DOCKER_LVM_THIN_POOL_NAME = cfg.thinPoolName;
        DOCKER_LVM_THIN_POOL_IMPORT_EXISTING = lib.boolToString cfg.importExisting;
        RUST_LOG = "info";
      } // cfg.extraEnvironment)
    ) + "\n"
  );
in
{
  options.services.thin-pool-docker-volume = {
    enable = mkEnableOption "Docker LVM thin pool volume plugin";

    vgName = mkOption {
      type = types.str;
      description = "Volume group where the thin pool lives.";
    };

    thinPoolName = mkOption {
      type = types.str;
      description = "Thin pool to allocate docker volumes from.";
    };

    importExisting = mkOption {
      type = types.bool;
      default = true;
      description = "Import existing LVs in the thin pool as docker volumes.";
    };

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.thin-pool-docker-volume;
      defaultText = lib.literalExpression "self.packages.\${system}.thin-pool-docker-volume";
      description = "The thin-pool-docker-volume package to use.";
    };

    lvm2DbusdPackage = mkOption {
      type = types.package;
      default = pkgs.callPackage (self + "/nix/lvm2-with-dbusd.nix") { };
      defaultText = lib.literalExpression "pkgs.callPackage (self + \"/nix/lvm2-with-dbusd.nix\") { }";
      description = "The lvm2-with-dbusd package to use.";
    };

    filesystemTools = mkOption {
      type = types.listOf types.package;
      default = [ pkgs.xfsprogs pkgs.e2fsprogs pkgs.btrfs-progs ];
      defaultText = lib.literalExpression "[ pkgs.xfsprogs pkgs.e2fsprogs pkgs.btrfs-progs ]";
      description = "Packages providing mkfs.* tools available to the daemon.";
    };

    extraEnvironment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      description = "Extra environment variables for the plugin service.";
      example = { RUST_LOG = "debug"; };
    };
  };

  config = mkIf cfg.enable {
    assertions = [{
      assertion = config.virtualisation.docker.enable;
      message = "thin-pool-docker-volume requires Docker to be enabled (virtualisation.docker.enable = true)";
    }];

    boot.kernelModules = [ "dm_thin_pool" ];

    # --- lvm2-lvmdbusd ---
    services.dbus.packages = [ cfg.lvm2DbusdPackage ];

    environment.etc."lvm/profile.d/lvmdbusd.profile".source =
      "${cfg.lvm2DbusdPackage}/etc/profile.d/lvmdbusd.profile";

    systemd.services.lvm2-lvmdbusd = {
      wantedBy = [ "multi-user.target" ];
    };

    # Import upstream service files from both packages.
    systemd.packages = [ cfg.lvm2DbusdPackage cfg.package ];

    # Generate the environment file the service already references.
    environment.etc."default/thin_pool_docker_volume".source = envFile;

    # NixOS-specific overrides via dropin.
    systemd.services.thin_pool_docker_volume = {
      overrideStrategy = "asDropin";

      path = cfg.filesystemTools;

      serviceConfig = {
        ExecStartPre = [ "${pkgs.systemd}/bin/busctl --system --timeout=30 status com.redhat.lvmdbus1" ];
        ExecStart = [ "" "${cfg.package}/bin/thin_pool_docker_volume" ];
      };
    };
  };
}
