{
  description = "Screen server, NixOS module, and docker image";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      branding = builtins.fromTOML (builtins.readFile ./branding.toml);
      slug = branding.product.slug;
      port = branding.network.default_port;
      version = (builtins.fromTOML (builtins.readFile ./server/Cargo.toml)).package.version;
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
      mkServer = pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "${slug}-server";
          inherit version;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [ ./server ./branding.toml ./api/openapi.yaml ];
          };
          buildInputs = [ pkgs.sqlite ];
          cargoRoot = "server";
          buildAndTestSubdir = "server";
          cargoLock.lockFile = ./server/Cargo.lock;
          postInstall = ''
            mv $out/bin/screen-server $out/bin/${slug}-server
          '';
          meta.mainProgram = "${slug}-server";
        };
      mkDocker = pkgs: server:
        let
          envPrefix = pkgs.lib.toUpper slug;
          nss = pkgs.dockerTools.fakeNss.override {
            extraPasswdLines = [ "${slug}:x:1000:1000::/data:" ];
            extraGroupLines = [ "${slug}:x:1000:" ];
          };
        in
        pkgs.dockerTools.buildLayeredImage {
          name = slug;
          tag = version;
          contents = [ server pkgs.cacert nss ];
          fakeRootCommands = ''
            mkdir -p data tmp
            chown 1000:1000 data
            chmod 1777 tmp
          '';
          enableFakechroot = true;
          config = {
            Entrypoint = [ "${server}/bin/${slug}-server" ];
            User = "1000:1000";
            WorkingDir = "/data";
            ExposedPorts."${toString port}/tcp" = { };
            Env = [
              "${envPrefix}_DATA_DIR=/data"
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
            ];
            Volumes."/data" = { };
            Labels = {
              "org.opencontainers.image.source" = branding.product.repository;
              "org.opencontainers.image.version" = version;
              "org.opencontainers.image.licenses" = "AGPL-3.0-only";
            };
          };
        };
    in
    {
      packages = forAllSystems (system: pkgs:
        let server = mkServer pkgs; in
        {
          inherit server;
          default = server;
          "${slug}-server" = server;
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          docker = mkDocker pkgs server;
        });

      nixosModules.default = import ./nix/module.nix { inherit slug port self; };

      devShells = forAllSystems (system: pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ rustc cargo clippy rustfmt rust-analyzer sqlite jq yq-go ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ xcodegen libiconv ];
        };
      });

      checks = forAllSystems (system: pkgs:
        { server = self.packages.${system}.server; }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          vm = pkgs.testers.runNixOSTest {
            name = "${slug}-vm";
            nodes.machine = {
              imports = [ self.nixosModules.default ];
              services.${slug} = {
                enable = true;
                package = self.packages.${system}.server;
                bonjour = false;
                orgName = "Grace Church";
              };
            };
            testScript = ''
              machine.wait_for_unit("${slug}.service")
              machine.wait_for_open_port(${toString port})
              machine.succeed("curl -sf http://localhost:${toString port}/v1/info | grep ${slug}")
              machine.succeed(
                "curl -sf -H \"Authorization: Bearer $(cat /var/lib/${slug}/bootstrap-token)\" "
                "http://localhost:${toString port}/v1/me/organizations | grep 'Grace Church'"
              )
            '';
          };
          docker = pkgs.testers.runNixOSTest {
            name = "${slug}-docker";
            nodes.machine = {
              virtualisation.docker.enable = true;
              virtualisation.diskSize = 4096;
            };
            testScript = ''
              machine.wait_for_unit("docker.service")
              machine.succeed("docker load < ${self.packages.${system}.docker}")
              machine.succeed("docker run -d --name ${slug} -p ${toString port}:${toString port} -e ${pkgs.lib.toUpper slug}_BONJOUR=false -v data:/data ${slug}:${version}")
              machine.wait_until_succeeds("curl -sf http://localhost:${toString port}/v1/info | grep ${slug}", timeout=60)
              machine.succeed("docker cp ${slug}:/data/bootstrap-token /tmp/token")
              machine.succeed("test -s /tmp/token")
              machine.succeed("docker restart ${slug}")
              machine.wait_until_succeeds("curl -sf http://localhost:${toString port}/v1/info", timeout=60)
            '';
          };
        });
    };
}
