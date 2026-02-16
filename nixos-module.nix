{
  config,
  lib,
  ...
}: let
  cfg = config.services.octo-dl;
  stateDir = "/var/lib/octo-dl";
in {
  options.services.octo-dl = {
    enable = lib.mkEnableOption "octo-dl MEGA download service";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The octo-dl package to use.";
    };

    configFile = lib.mkOption {
      type = lib.types.path;
      default = "${stateDir}/config.toml";
      description = ''
        Path to config.toml. Auto-created with defaults on first start; edit to add credentials.

        The API server binds to 127.0.0.1 by default. To expose externally, set
        `api_host = "0.0.0.0"` in the config file and place behind an
        auth-protecting reverse proxy or VPN (e.g., Tailscale).
      '';
    };

    downloadDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/octo-dl/downloads";
      description = "Directory where downloads are stored.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "octo-dl";
      description = "User account under which the service runs.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "media";
      description = "Group under which the service runs.";
    };

    web = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Whether to serve the web UI. When false, runs in headless API-only mode.";
      };

      host = lib.mkOption {
        type = lib.types.str;
        default = "0.0.0.0";
        description = "Bind address for the web/API server.";
      };

      publicHost = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Public hostname for the PWA manifest and share target (e.g. 'octo.example.com'). Defaults to the bind host if unset.";
      };

      port = lib.mkOption {
        type = lib.types.port;
        default = 9723;
        description = "Port for the web/API server.";
      };

      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Whether to open the web UI port in the firewall.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = cfg.downloadDir;
      createHome = true;
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.web.openFirewall [cfg.web.port];

    systemd.services.octo-dl = let
      mode = if cfg.web.enable then "--web" else "--api";
      webHostFlag = lib.optionalString (cfg.web.enable && cfg.web.publicHost != null) " --web-host ${cfg.web.publicHost}";
      apiHostFlag = " --api-host ${cfg.web.host}";
    in {
      description = "octo-dl MEGA download service";
      after = ["network-online.target"];
      wants = ["network-online.target"];
      wantedBy = ["multi-user.target"];

      environment = {
        RUST_LOG = lib.mkDefault "info";
        OCTO_API_PORT = toString cfg.web.port;
      };

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        StateDirectory = "octo-dl";
        WorkingDirectory = cfg.downloadDir;
        ExecStart = "${cfg.package}/bin/octo ${mode}${apiHostFlag}${webHostFlag} --config ${cfg.configFile}";
        Restart = "on-failure";
        RestartSec = 10;
      };
    };
  };
}
