{ config, lib, pkgs, ... }:

with lib;

let
  cfg = config.services.octo;
in {
  options.services.octo = {
    enable = mkEnableOption "octo MEGA download API server";

    package = mkOption {
      type = types.package;
      default = pkgs.octo;
      description = "The octo package to use";
    };

    downloadDir = mkOption {
      type = types.path;
      default = "/var/lib/octo-dl/downloads";
      description = "Directory where downloaded files are stored";
    };

    stateDir = mkOption {
      type = types.path;
      default = "/var/lib/octo-dl/sessions";
      description = "Directory where session state is stored";
    };

    apiHost = mkOption {
      type = types.str;
      default = "0.0.0.0";
      description = "API server bind address";
    };

    apiPort = mkOption {
      type = types.port;
      default = 9723;
      description = "API server port";
    };

    user = mkOption {
      type = types.str;
      default = "octo";
      description = "User to run the service as";
    };

    group = mkOption {
      type = types.str;
      default = "octo";
      description = "Group to run the service as";
    };
  };

  config = mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      description = "octo service user";
      home = "/var/lib/octo-dl";
      createHome = true;
    };

    users.groups.${cfg.group} = {};

    systemd.services.octo = {
      description = "octo MEGA download API server";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = "${cfg.package}/bin/octo --api --api-host ${cfg.apiHost} --api-port ${toString cfg.apiPort}";
        Restart = "on-failure";
        RestartSec = "10s";

        # Environment
        Environment = [
          "OCTO_DOWNLOAD_DIR=${cfg.downloadDir}"
          "OCTO_STATE_DIR=${cfg.stateDir}"
        ];

        # Security hardening
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [ cfg.downloadDir cfg.stateDir ];

        # Working directory
        WorkingDirectory = cfg.downloadDir;
      };
    };

    # Ensure directories exist with correct permissions
    systemd.tmpfiles.rules = [
      "d ${cfg.downloadDir} 0750 ${cfg.user} ${cfg.group} -"
      "d ${cfg.stateDir} 0700 ${cfg.user} ${cfg.group} -"
    ];
  };
}
