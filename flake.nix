{
  description = "Screen server, NixOS module, and docker image";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      branding = builtins.fromTOML (builtins.readFile ./branding.toml);
      slug = branding.product.slug;
      port = branding.network.default_port;
      version = (builtins.fromTOML (builtins.readFile ./server/Cargo.toml)).package.version;
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
      mkServer = pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "${slug}-server";
          inherit version;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [ ./server ./branding.toml ];
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
          docker = pkgs.dockerTools.buildLayeredImage {
            name = slug;
            tag = version;
            contents = [ server pkgs.cacert ];
            config = {
              Cmd = [ "${server}/bin/${slug}-server" ];
              ExposedPorts."${toString port}/tcp" = { };
              Env = [
                "${pkgs.lib.toUpper slug}_DATA_DIR=/data"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              Volumes."/data" = { };
            };
          };
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
              };
            };
            testScript = ''
              machine.wait_for_unit("${slug}.service")
              machine.wait_for_open_port(${toString port})
              machine.succeed("curl -sf http://localhost:${toString port}/v1/info | grep ${slug}")
            '';
          };
        });
    };
}
