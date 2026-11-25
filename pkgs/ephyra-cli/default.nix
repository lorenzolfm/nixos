{
  lib,
  rustPlatform,
  fetchFromGitHub,
}:

# The `ephyra` command from datadog-labs/ephyra. The library itself is a
# per-project dev-dependency; only the CLI is installed. Upstream has no tags,
# and crates.io's 0.0.1 predates main by weeks, so this pins a main commit.
rustPlatform.buildRustPackage {
  pname = "ephyra-cli";
  version = "0.0.1-unstable-2026-11-12";

  src = fetchFromGitHub {
    owner = "datadog-labs";
    repo = "ephyra";
    rev = "3983b2e339ac5f6b843bef62c7fa6a88eba75370";
    hash = "sha256-HexVf/scj4KF7exOOTonK57yp8NyuhRa2wGVMr4YjXE=";
  };

  cargoHash = "sha256-yhrzmPQsUkqzhhTvJqCeITyOjmyuabzbQNtK5D1rTs8=";

  cargoBuildFlags = [
    "-p"
    "ephyra-cli"
  ];
  cargoTestFlags = [
    "-p"
    "ephyra-cli"
  ];

  meta = {
    description = "Runs ephyra property suites, checks their generators and serves a local results page";
    homepage = "https://github.com/datadog-labs/ephyra";
    license = lib.licenses.mit;
    mainProgram = "ephyra";
  };
}
