{
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.zst";
  };

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems =
        function:
        nixpkgs.lib.genAttrs nixpkgs.lib.systems.flakeExposed (
          system: function nixpkgs.legacyPackages.${system}
        );
    in
    {
      packages = forAllSystems (pkgs: {
        lethe = pkgs.callPackage ./package.nix { };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.lethe;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.callPackage ./shell.nix { };
      });

      nixosModules.default = import ./nix/module.nix self;

      # evaluates the module on a minimal system, so a broken option or unit
      # fails `nix flake check` instead of a deploy
      checks.x86_64-linux.module =
        (nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            self.nixosModules.default
            {
              services.lethe-server = {
                enable = true;
                tokenFile = "/run/secrets/lethe-token";
              };
              fileSystems."/" = {
                device = "nodev";
                fsType = "tmpfs";
              };
              boot.loader.grub.enable = false;
              system.stateVersion = "26.05";
            }
          ];
        }).config.systemd.units."lethe-server.service".unit;
    };
}
