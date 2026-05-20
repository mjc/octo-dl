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
      default = "octo-dl";
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
        description = ''
          Whether to publish the loopback remote TUI attach stream with the API.

          When false, octo-dl still runs in headless API mode using the
          configured `[api]` host and port, but does not expose the remote TUI
          attach stream.
        '';
      };

      host = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
        description = "Loopback bind address for the remote-TUI/API server.";
      };

      publicHost = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Public hostname used when rendering the bookmarklet helper page. Defaults to the bind host if unset.";
      };

      port = lib.mkOption {
        type = lib.types.port;
        default = 9723;
        description = "Port for the remote-TUI/API server.";
      };

      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Whether to open the remote-TUI/API port in the firewall.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    users.groups.${cfg.group} = {};

    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = cfg.downloadDir;
      createHome = true;
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.web.openFirewall [cfg.web.port];

    assertions = [
      {
        assertion = !cfg.web.enable || lib.hasPrefix "127." cfg.web.host || cfg.web.host == "::1";
        message = "services.octo-dl.web.host must be loopback when services.octo-dl.web.enable is true.";
      }
    ];

    systemd.services.octo-dl = let
      listenHost =
        if lib.hasInfix ":" cfg.web.host
        then "[${cfg.web.host}]"
        else cfg.web.host;
      manageConfigScript = ''
        toml_quote() {
          local value="$1"
          value=''${value//\\/\\\\}
          value=''${value//\"/\\\"}
          printf '"%s"' "$value"
        }

        read_toml_value() {
          local section="$1"
          local key="$2"
          local file="$3"

          [ -f "$file" ] || return 1

          ${pkgs.gawk}/bin/awk -v section="$section" -v key="$key" '
            /^\[/ {
              in_section = ($0 == "[" section "]")
              next
            }
            in_section && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
              sub(/^[[:space:]]*[^=]+=[[:space:]]*/, "", $0)
              print
              exit
            }
          ' "$file"
        }

        umask 077
        mkdir -p "$(dirname "$OCTO_CONFIG_PATH")"
        had_config=false
        [ -f "$OCTO_CONFIG_PATH" ] && had_config=true

        encrypted="$(read_toml_value credentials encrypted "$OCTO_CONFIG_PATH" || true)"
        email="$(read_toml_value credentials email "$OCTO_CONFIG_PATH" || true)"
        password="$(read_toml_value credentials password "$OCTO_CONFIG_PATH" || true)"
        mfa="$(read_toml_value credentials mfa "$OCTO_CONFIG_PATH" || true)"
        api_key="$(read_toml_value api api_key "$OCTO_CONFIG_PATH" || true)"

        encrypted=''${encrypted:-false}
        if { [ -z "$email" ] || [ "$email" = '""' ]; } && [ -n "''${MEGA_EMAIL:-}" ]; then
          email="$(toml_quote "$MEGA_EMAIL")"
          encrypted=false
        fi
        if { [ -z "$password" ] || [ "$password" = '""' ]; } && [ -n "''${MEGA_PASSWORD:-}" ]; then
          password="$(toml_quote "$MEGA_PASSWORD")"
          encrypted=false
        fi
        mfa=''${mfa:-\"\"}
        if { [ -z "$mfa" ] || [ "$mfa" = '""' ]; } && [ -n "''${MEGA_MFA:-}" ]; then
          mfa="$(toml_quote "$MEGA_MFA")"
        fi

        email=''${email:-\"\"}
        password=''${password:-\"\"}

        if $had_config && { [ "$email" = '""' ] || [ "$password" = '""' ]; } \
          && [ -z "''${MEGA_EMAIL:-}" ] && [ -z "''${MEGA_PASSWORD:-}" ]; then
          echo "Refusing to rewrite $OCTO_CONFIG_PATH with empty credentials." >&2
          echo "Set MEGA_EMAIL/MEGA_PASSWORD, or restore the existing [credentials] block first." >&2
          exit 1
        fi

        if [ -n "$OCTO_API_KEY_FILE" ] && [ -f "$OCTO_API_KEY_FILE" ]; then
          api_key="$(toml_quote "$(tr -d '\n' < "$OCTO_API_KEY_FILE")")"
        fi

        force_overwrite=false
        cleanup_on_error=false
        [ "$OCTO_FORCE_OVERWRITE" = "true" ] && force_overwrite=true
        [ "$OCTO_CLEANUP_ON_ERROR" = "true" ] && cleanup_on_error=true

        {
          printf '%s\n' '[credentials]'
          printf 'encrypted = %s\n' "$encrypted"
          printf 'email = %s\n' "$email"
          printf 'password = %s\n' "$password"
          printf 'mfa = %s\n' "$mfa"
          printf '\n'
          printf '%s\n' '[api]'
          printf 'host = %s\n' "$(toml_quote "$OCTO_API_HOST")"
          printf 'port = %s\n' "$OCTO_API_PORT"
          if [ -n "$api_key" ]; then
            printf 'api_key = %s\n' "$api_key"
          fi
          printf '\n'
          printf '%s\n' '[download]'
          printf 'path = %s\n' "$(toml_quote "$OCTO_DOWNLOAD_DIR")"
          printf 'chunks_per_file = %s\n' "$OCTO_CHUNKS_PER_FILE"
          printf 'concurrent_files = %s\n' "$OCTO_CONCURRENT_FILES"
          printf 'force_overwrite = %s\n' "$force_overwrite"
          printf 'cleanup_on_error = %s\n' "$cleanup_on_error"
          printf '\n'
        } > "$OCTO_CONFIG_PATH"

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
        ExecStart = lib.escapeShellArgs (
          [(lib.getExe cfg.package) "--headless" "--config" configPath]
          ++ lib.optional cfg.web.enable "--tui-listen"
          ++ lib.optional cfg.web.enable "${listenHost}:${toString cfg.web.port}"
        );
        EnvironmentFile = environmentFile;
        Restart = "on-failure";
        RestartSec = 10;
        UMask = "0077";
      };
    };
  };
}
