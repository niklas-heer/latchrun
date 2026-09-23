{
  lib,
  rustPlatform,
  versionCheckHook,
}:

let
  manifest = (lib.importTOML ../Cargo.toml).package;
in
rustPlatform.buildRustPackage {
  pname = manifest.name;
  inherit (manifest) version;

  src = lib.cleanSource ../.;

  cargoLock.lockFile = ../Cargo.lock;

  # CI runs the full suite; the package build only proves the binary starts.
  doCheck = false;
  doInstallCheck = true;
  nativeInstallCheckInputs = [ versionCheckHook ];

  meta = {
    inherit (manifest) description;
    homepage = manifest.repository;
    license = lib.licenses.mit;
    mainProgram = "latchrun";
    platforms = lib.platforms.unix;
  };
}
