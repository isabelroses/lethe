let
  lockfile = builtins.fromJSON (builtins.readFile ./flake.lock);
  node = lockfile.nodes.nixpkgs.locked;
  nixpkgs' = fetchTarball {
    inherit (node) url;
    sha256 = node.narHash;
  };
in
{
  nixpkgs ? nixpkgs',
  system ? builtins.currentSystem,
  pkgs ? import nixpkgs { inherit system; },
}:
{
  lethe = pkgs.callPackage ./package.nix { };
}
