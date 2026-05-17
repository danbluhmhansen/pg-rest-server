{
  description = "Dev shell and CI pipeline for the `pg-rest-server` project.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust.url = "github:juspay/rust-flake";
    rust.inputs.nixpkgs.follows = "nixpkgs";
    advisory-db.url = "github:rustsec/advisory-db";
    advisory-db.flake = false;
  };

  outputs = inputs @ {
    flake-parts,
    advisory-db,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      debug = true;
      imports = with inputs; [
        rust.flakeModules.default
        rust.flakeModules.nixpkgs
      ];
      systems = inputs.nixpkgs.lib.systems.flakeExposed;
      perSystem = {
        config,
        pkgs,
        ...
      }: let
        inherit (config.rust-project) crane-lib src;
      in {
        # Disable tests in the main build -- they need a running PostgreSQL.
        rust-project.crates.pg-rest-server-resolute.crane.args = {
          doCheck = false;
          meta.mainProgram = "pg-rest-server-resolute";
        };
        rust-project.crates.pg-rest-server-tokio-postgres-pg-wired.crane.args = {
          doCheck = false;
          meta.mainProgram = "pg-rest-server-tokio-postgres-pg-wired";
        };
        rust-project.crates.pg-rest-server-tokio-postgres-deadpool.crane.args = {
          doCheck = false;
          meta.mainProgram = "pg-rest-server-tokio-postgres-deadpool";
        };

        checks = {
          cargo-audit = crane-lib.cargoAudit {inherit src advisory-db;};
          cargo-deny = crane-lib.cargoDeny {inherit src;};

          integration-tests = crane-lib.cargoTest {
            inherit src;
            cargoArtifacts = crane-lib.buildDepsOnly {inherit src;};
            pname = "pg-rest-server-integration-tests";
            buildInputs = with pkgs; [postgresql];
            preCheck = ''
              export PGDATA=$PWD/pgdata
              export PGHOST=$PWD
              export PGPORT=54322
              initdb --no-locale --encoding=UTF8 --auth=trust -U postgres
              pg_ctl -o "-k $PWD -p $PGPORT" start -U postgres -w
              createdb -U postgres postgrest_test
              psql -U postgres -d postgrest_test -f ${./test/fixtures/setup.sql}
              psql -U postgres -d postgrest_test -f ${./test/fixtures/setup_extended.sql}
            '';
            postCheck = ''
              pg_ctl stop -U postgres || true
            '';
            checkPhaseCargoCommand = ''
              cargo test -p pg-rest-server-resolute --test integration
              cargo test -p pg-rest-server-tokio-postgres-pg-wired --test integration
              cargo test -p pg-rest-server-tokio-postgres-deadpool --test integration
            '';
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [config.devShells.rust];
          packages = with pkgs; [nixd tombi cargo-audit cargo-deny cargo-nextest];
        };
      };
    };
}
