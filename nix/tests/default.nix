# nix eval --impure --json --file nix/tests/default.nix --apply 'test: test {}'
# Or --apply 'test: test { homeManager = builtins.getFlake "github:nix-community/home-manager/<revision>"; }'
# for real HM evaluation without making Home Manager a mandatory flake input.
{ homeManager ? null }:
let
  lock = builtins.fromJSON (builtins.readFile ../../flake.lock);
  nixpkgs = builtins.getFlake "github:NixOS/nixpkgs/${lock.nodes.nixpkgs.locked.rev}";
  # Import outputs directly so untracked Nix files can be tested without staging,
  # and the local target directory is never copied as an unfiltered flake source.
  flake = (import ../../flake.nix).outputs { self = flake; inherit nixpkgs; };
in import ./eval.nix { inherit flake nixpkgs homeManager; }
