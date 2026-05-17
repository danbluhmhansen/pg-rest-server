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

        # openssl-sys needs headers + pkg-config at build time on Linux (reqwest,
        # tokio-postgres native-tls, etc.).  On Darwin these are harmless.
        ssl-build-deps = with pkgs; [openssl pkg-config];

        # Shared deps-only artifact used by all check derivations.
        cargoArtifacts = crane-lib.buildDepsOnly {
          inherit src;
          nativeBuildInputs = ssl-build-deps;
          buildInputs = ssl-build-deps;
        };
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

        # compat-test uses reqwest which pulls in openssl-sys on Linux.
        rust-project.crates.compat-test.crane.args = {
          nativeBuildInputs = ssl-build-deps;
          buildInputs = ssl-build-deps;
        };

        # Make SSL build deps available to all crate builds.
        rust-project.defaults.perCrate.crane.args = {
          nativeBuildInputs = ssl-build-deps;
        };

        checks = {
          cargo-audit = crane-lib.cargoAudit {inherit src advisory-db;};
          cargo-deny = crane-lib.cargoDeny {inherit src;};

          fmt = crane-lib.cargoFmt {inherit src;};

          unit-tests = crane-lib.cargoTest {
            inherit src cargoArtifacts;
            cargoTestExtraArgs = "--lib --all";
          };

          integration-tests = crane-lib.cargoTest {
            inherit src cargoArtifacts;
            pname = "pg-rest-server-integration-tests";
            nativeBuildInputs = ssl-build-deps;
            buildInputs = with pkgs; [postgresql openssl];
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
