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
            # eframe/egui dependencies (Phase 4: system tray UI)
            libxkbcommon
            wayland
            libx11
            libxcursor
            libxrandr
            libxi
            libGL
            vulkan-loader
          ];

          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath [
              pkgs.alsa-lib
              pkgs.libopus
              pkgs.openssl
              pkgs.libxkbcommon
              pkgs.wayland
              pkgs.libx11
              pkgs.libxcursor
              pkgs.libxrandr
              pkgs.libxi
              pkgs.libGL
              pkgs.vulkan-loader
            ]}:$LD_LIBRARY_PATH"
            # Ensure rustup has a default toolchain
            if ! rustup show active-toolchain &>/dev/null 2>&1; then
              rustup default stable
            fi
          '';
        };
      });
}
