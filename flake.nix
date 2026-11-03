{
  description = "Show Claude Code agent status on tmux window tabs";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          tmux-agent = pkgs.callPackage ./nix/package.nix { };
        in
        {
          inherit tmux-agent;
          tmux-plugin = pkgs.callPackage ./nix/tmux-plugin.nix { inherit tmux-agent; };
          claude-plugin = pkgs.callPackage ./nix/claude-plugin.nix { inherit tmux-agent; };
          default = tmux-agent;
        }
      );

      overlays.default = final: prev: {
        tmux-agent = final.callPackage ./nix/package.nix { };
        tmuxPlugins = prev.tmuxPlugins // {
          tmux-agent = final.callPackage ./nix/tmux-plugin.nix { };
        };
      };

      homeModules.default = import ./nix/hm-module.nix self;
      homeManagerModules = self.homeModules;

      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) tmux-agent tmux-plugin claude-plugin;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.tmux-agent ];
          packages = [
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
