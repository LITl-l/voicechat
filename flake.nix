{
  description = "Ultra-low-latency voice chat";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            pkg-config
            cmake
            rustup
          ];

          buildInputs = with pkgs; [
            alsa-lib
            libopus
            openssl
          ];

          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath [
              pkgs.alsa-lib
              pkgs.libopus
              pkgs.openssl
            ]}:$LD_LIBRARY_PATH"
            # Ensure rustup has a default toolchain
            if ! rustup show active-toolchain &>/dev/null 2>&1; then
              rustup default stable
            fi
          '';
        };
      });
}
