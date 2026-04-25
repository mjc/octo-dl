{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.octo-dl;
  stateDir = "/var/lib/octo-dl";
  configPath = toString cfg.configFile;
  managedConfig = cfg.manageConfig;
  apiKeyFile = if cfg.apiKeyFile == null then "" else toString cfg.apiKeyFile;
  environmentFile =
    if cfg.environmentFile == null
    then []
    else [cfg.environmentFile];
in {
  options.services.octo-dl = {
    enable = lib.mkEnableOption "octo-dl MEGA download service";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The octo-dl package to use.";
    };

    manageConfig = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Whether the NixOS module should keep `${configPath}` aligned with the
        NixOS options for API host/port and download settings.

        When enabled, the module preserves any existing `[credentials]` block
        and existing `api.api_key`, while rewriting the managed `[api]` and
        `[download]` fields on each start.
      '';
    };

    configFile = lib.mkOption {
      type = lib.types.path;
      default = "${stateDir}/config.toml";
      description = ''
        Path to config.toml.

        By default the module manages this file so that `[api]` and `[download]`
        match the NixOS options below. Existing credentials and API key are
        preserved across restarts. Set `manageConfig = false` to manage the file
        yourself.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Optional environment file loaded by systemd. This is the recommended
        way to provide `MEGA_EMAIL`, `MEGA_PASSWORD`, and optional `MEGA_MFA`
        without storing secrets in the Nix store.
      '';
    };

    apiKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Optional file containing the API key to write into `${configPath}`.
        When unset, the module preserves any existing key in the config file,
        and octo-dl will auto-generate one on first start if none exists.
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

    chunksPerFile = lib.mkOption {
      type = lib.types.ints.positive;
      default = 2;
      description = "Number of parallel chunks per file.";
    };

    concurrentFiles = lib.mkOption {
      type = lib.types.ints.positive;
      default = 4;
      description = "Number of concurrent file downloads.";
    };

    forceOverwrite = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Whether to overwrite existing files.";
    };

    cleanupOnError = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Whether to delete resume artifacts after recoverable errors.";
    };

    web = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Whether to serve the web UI. When false, runs in headless API-only mode.";
      };

      host = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
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
      apiHostFlag = " --host ${cfg.web.host}";
      manageConfigScript = ''
        umask 077
        mkdir -p "$(dirname "$OCTO_CONFIG_PATH")"
        ${pkgs.python3}/bin/python3 <<'PY'
import json
import os
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib

config_path = Path(os.environ["OCTO_CONFIG_PATH"])
api_key_file = os.environ.get("OCTO_API_KEY_FILE", "")

existing = {}
if config_path.exists():
    try:
        with config_path.open("rb") as fh:
            existing = tomllib.load(fh)
    except Exception:
        existing = {}

credentials = existing.get("credentials") or {}
api = existing.get("api") or {}

if api_key_file:
    api_key = Path(api_key_file).read_text(encoding="utf-8").strip()
else:
    api_key = str(api.get("api_key", "")).strip()

def toml_string(value: str) -> str:
    return json.dumps(value)

def toml_bool(value: bool) -> str:
    return "true" if value else "false"

lines = [
    "[credentials]",
    f"encrypted = {toml_bool(bool(credentials.get(\"encrypted\", False)))}",
    f"email = {toml_string(str(credentials.get(\"email\", \"\")))}",
    f"password = {toml_string(str(credentials.get(\"password\", \"\")))}",
    f"mfa = {toml_string(str(credentials.get(\"mfa\", \"\")))}",
    "",
    "[api]",
    f"host = {toml_string(os.environ[\"OCTO_API_HOST\"])}",
    f"port = {os.environ[\"OCTO_API_PORT\"]}",
]

if api_key:
    lines.append(f"api_key = {toml_string(api_key)}")

lines += [
    "",
    "[download]",
    f"path = {toml_string(os.environ[\"OCTO_DOWNLOAD_DIR\"])}",
    f"chunks_per_file = {os.environ[\"OCTO_CHUNKS_PER_FILE\"]}",
    f"concurrent_files = {os.environ[\"OCTO_CONCURRENT_FILES\"]}",
    f"force_overwrite = {toml_bool(os.environ[\"OCTO_FORCE_OVERWRITE\"] == \"true\")}",
    f"cleanup_on_error = {toml_bool(os.environ[\"OCTO_CLEANUP_ON_ERROR\"] == \"true\")}",
    "",
]

config_path.write_text("\\n".join(lines), encoding="utf-8")
PY
        chown ${cfg.user}:${cfg.group} "$OCTO_CONFIG_PATH"
        chmod 600 "$OCTO_CONFIG_PATH"
      '';
    in {
      description = "octo-dl MEGA download service";
      after = ["network-online.target"];
      wants = ["network-online.target"];
      wantedBy = ["multi-user.target"];

      environment = {
        RUST_LOG = lib.mkDefault "info";
        OCTO_API_PORT = toString cfg.web.port;
        OCTO_API_HOST = cfg.web.host;
        OCTO_DOWNLOAD_DIR = toString cfg.downloadDir;
        OCTO_CHUNKS_PER_FILE = toString cfg.chunksPerFile;
        OCTO_CONCURRENT_FILES = toString cfg.concurrentFiles;
        OCTO_FORCE_OVERWRITE = lib.boolToString cfg.forceOverwrite;
        OCTO_CLEANUP_ON_ERROR = lib.boolToString cfg.cleanupOnError;
        OCTO_CONFIG_PATH = configPath;
        OCTO_API_KEY_FILE = apiKeyFile;
      };

      preStart = lib.optionalString managedConfig manageConfigScript;

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        StateDirectory = "octo-dl";
        PermissionsStartOnly = managedConfig;
        WorkingDirectory = cfg.downloadDir;
        ExecStart = "${cfg.package}/bin/octo ${mode}${apiHostFlag}${webHostFlag} --config ${cfg.configFile}";
        EnvironmentFile = environmentFile;
        Restart = "on-failure";
        RestartSec = 10;
        UMask = "0077";
      };
    };
  };
}
