{ slug, port, self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.${slug};
  prefix = lib.toUpper slug;
  env = {
    "${prefix}_PORT" = toString cfg.port;
    "${prefix}_BIND" = cfg.bind;
    "${prefix}_DATA_DIR" = cfg.dataDir;
    "${prefix}_BONJOUR" = lib.boolToString cfg.bonjour;
    "${prefix}_OIDC_GROUPS_CLAIM" = cfg.oidc.groupsClaim;
    "${prefix}_OIDC_TOKEN_TYPE" = cfg.oidc.tokenType;
  }
  // lib.optionalAttrs (cfg.orgName != null) { "${prefix}_ORG_NAME" = cfg.orgName; }
  // lib.optionalAttrs (cfg.orgTimezone != null) { "${prefix}_ORG_TIMEZONE" = cfg.orgTimezone; }
  // lib.optionalAttrs (cfg.listen != null) { "${prefix}_LISTEN" = cfg.listen; }
  // lib.optionalAttrs (cfg.publicUrl != null) { "${prefix}_PUBLIC_URL" = cfg.publicUrl; }
  // lib.optionalAttrs (cfg.oidc.issuer != null) { "${prefix}_OIDC_ISSUER" = cfg.oidc.issuer; }
  // lib.optionalAttrs (cfg.oidc.clientId != null) { "${prefix}_OIDC_CLIENT_ID" = cfg.oidc.clientId; }
  // lib.optionalAttrs (cfg.oidc.audience != null) { "${prefix}_OIDC_AUDIENCE" = cfg.oidc.audience; }
  // lib.optionalAttrs (cfg.oidc.adminGroups != [ ]) {
    "${prefix}_OIDC_ADMIN_GROUPS" = lib.concatStringsSep "," cfg.oidc.adminGroups;
  }
  // lib.optionalAttrs (cfg.bootstrapTokenFile != null) {
    "${prefix}_BOOTSTRAP_TOKEN_FILE" = "%d/bootstrap-token";
  }
  // cfg.environment;
in
{
  options.services.${slug} = {
    enable = lib.mkEnableOption "the ${slug} server";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.server;
      defaultText = lib.literalExpression "self.packages.\${pkgs.system}.server";
    };
    port = lib.mkOption { type = lib.types.port; default = port; };
    bind = lib.mkOption { type = lib.types.str; default = "0.0.0.0"; };
    dataDir = lib.mkOption { type = lib.types.str; default = "/var/lib/${slug}"; };
    openFirewall = lib.mkOption { type = lib.types.bool; default = false; };
    bonjour = lib.mkOption { type = lib.types.bool; default = true; };
    orgName = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "Grace Church";
      description = "Organization name shown in the apps and on paired TVs. null leaves it to Studio's first-run setup; set, it is locked.";
    };
    orgTimezone = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "America/Chicago";
      description = "IANA zone for every schedule. null uses the host zone (else UTC) and lets Studio change it; set, it is locked.";
    };
    listen = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "unix:/run/${slug}/${slug}.sock";
      description = "unix:/path listens on a unix socket instead of bind:port (Bonjour is not advertised).";
    };
    publicUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "https://screens.example.org";
      description = "Externally reachable base URL when behind a reverse proxy.";
    };
    bootstrapTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
    };
    oidc = {
      issuer = lib.mkOption { type = lib.types.nullOr lib.types.str; default = null; };
      clientId = lib.mkOption { type = lib.types.nullOr lib.types.str; default = null; };
      audience = lib.mkOption { type = lib.types.nullOr lib.types.str; default = null; };
      groupsClaim = lib.mkOption { type = lib.types.str; default = "groups"; };
      adminGroups = lib.mkOption { type = lib.types.listOf lib.types.str; default = [ ]; };
      tokenType = lib.mkOption {
        type = lib.types.enum [ "access_token" "id_token" ];
        default = "access_token";
      };
    };
    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
    };
  };

  config = lib.mkIf cfg.enable {
    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = lib.optional (cfg.listen == null) cfg.port;
      allowedUDPPorts = lib.optional cfg.bonjour 5353;
    };

    systemd.services.${slug} = {
      description = "${slug} server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      environment = env;
      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        DynamicUser = true;
        StateDirectory = lib.mkIf (cfg.dataDir == "/var/lib/${slug}") slug;
        RuntimeDirectory = slug;
        ReadWritePaths = lib.mkIf (cfg.dataDir != "/var/lib/${slug}") [ cfg.dataDir ];
        LoadCredential = lib.mkIf (cfg.bootstrapTokenFile != null)
          [ "bootstrap-token:${toString cfg.bootstrapTokenFile}" ];
        Restart = "on-failure";
        RestartSec = 2;
        LimitNOFILE = 1048576;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
        RestrictNamespaces = true;
        LockPersonality = true;
        SystemCallArchitectures = "native";
      };
    };
  };
}
