{
  description = "Faster Overpass API database import: reference builds, database comparator and test corpus";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    # The reference implementation: minor_issues at the commit released as 0.7.62.11.
    overpass-src = {
      url = "github:drolbr/Overpass-API/87bfad187673d891327f8bb68de7002a9e0e401d";
      flake = false;
    };
  };

  outputs =
    { nixpkgs, overpass-src, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      packagesFor =
        pkgs:
        let
          rust = pkgs.callPackage ./nix/rust.nix { };
          scripts = pkgs.callPackage ./nix/scripts.nix { inherit overpass overpass-import; };
          # Upstream, whole: what the server runs, what makes the areas, and
          # the reference the importer is checked against.
          overpass = pkgs.callPackage ./nix/overpass.nix { src = overpass-src; };
          overpass-import = pkgs.callPackage ./nix/importer.nix { };
        in
        {
          inherit overpass overpass-import;
          # Upstream plus $OVERPASS_FOREACH_SHARD, which lets one foreach run
          # as several processes. The areas pass is a foreach over about two
          # million relations on one core -- two thirds of a planet build --
          # and its iterations are independent, so this is the whole of what
          # makes them parallel.
          #
          # A separate package rather than a patch on `overpass`: the server
          # must keep running stock upstream, and nothing but the area build
          # has any use for this. Also $OVERPASS_AREA_COMMIT_BLOCKS, for
          # tests only: see nix/shard-check.nix.
          overpass-sharded = overpass.override {
            patches = [
              ./nix/patches/foreach-shard.patch
              ./nix/patches/area-commit-blocks.patch
            ];
            variant = "sharded";
          };
          # Upstream plus the fix that zeroes block padding, which makes
          # builds byte-reproducible; for the comparator's checks only.
          overpass-patched = overpass.override {
            patches = [ ./nix/patches/zero-block-padding.patch ];
            variant = "patched";
            programs = [
              "update_database"
              "osm3s_query"
            ];
          };
          inherit (rust) overpass-cmp osm-gen;
          inherit (scripts) overpass-load overpass-areas overpass-load-with-areas;
          default = scripts.overpass-load-with-areas;
        };

      mkDbFor = pkgs: pkgs.callPackage ./nix/mk-db.nix { };
      realFor = pkgs: import ./nix/inputs.nix { inherit (pkgs) fetchurl; };

      corpusFor =
        pkgs:
        let
          packages = packagesFor pkgs;
        in
        import ./nix/corpus.nix {
          inherit pkgs;
          inherit (pkgs) lib;
          inherit (packages)
            overpass
            overpass-cmp
            osm-gen
            overpass-import
            overpass-load
            overpass-areas
            overpass-load-with-areas
            ;
          mkDb = mkDbFor pkgs;
          real = realFor pkgs;
        };
    in
    {
      packages = forAllSystems packagesFor;

      # The corpus inputs (PBF) and their reference databases, by name:
      # corpus.references.<name>.lz4 and, for most, .none; corpus.invalid,
      # inputs an importer must refuse; corpus.heavy.<name>.{reference,check},
      # cases too big for the default checks.
      legacyPackages = forAllSystems (pkgs: {
        corpus = removeAttrs (corpusFor pkgs) [ "checks" ];
      });

      lib = forAllSystems (pkgs: {
        mkDb = mkDbFor pkgs;
      });

      checks = forAllSystems (
        pkgs:
        import ./nix/checks.nix {
          inherit pkgs;
          packages = packagesFor pkgs;
          mkDb = mkDbFor pkgs;
          inputs = realFor pkgs;
        }
        // (corpusFor pkgs).checks
        // {
          foreach-shard = pkgs.callPackage ./nix/shard-check.nix {
            inherit (packagesFor pkgs)
              overpass
              overpass-sharded
              overpass-import
              overpass-cmp
              ;
            # Base data only: the check runs the areas pass itself, both ways.
            base = (mkDbFor pkgs) {
              name = "liechtenstein";
              pbf = (realFor pkgs).liechtenstein;
              overpass = (packagesFor pkgs).overpass;
            };
          };
        }
      );

      apps = forAllSystems (pkgs: {
        fuzz = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.callPackage ./nix/fuzz.nix { inherit (packagesFor pkgs) overpass osm-gen; }
          );
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.osmctools
            pkgs.osmium-tool
            (packagesFor pkgs).overpass
          ];
        };
      });
    };
}
