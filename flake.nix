{
  description = "Wayward Griffin";

  inputs = {
    nixpkgs = {
      url = "github:nixos/nixpkgs/nixos-unstable";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs = {
    nixpkgs,
    rust-overlay,
    crane,
    ...
  }: let
    system = "x86_64-linux";
    pkgs = import nixpkgs {
      inherit system;
      config.allowUnfree = true;
      overlays = [rust-overlay.overlays.default];
    };
    rustToolchain = pkgs.rust-bin.stable.latest.default.override {
      extensions = ["llvm-tools-preview" "rust-src"];
      targets = ["x86_64-unknown-linux-gnu"];
    };
    craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
    dynamicInputs = with pkgs; [
      libglvnd
      libxkbcommon
      pipewire
      vulkan-loader
      wayland
    ];
    rpath = "/run/opengl-driver/lib:${pkgs.lib.makeLibraryPath dynamicInputs}";
    commonInputs = with pkgs;
      [
        cudaPackages.cudatoolkit
        llvmPackages.llvm
        xorg.libxcb
      ]
      ++ dynamicInputs;
    commonEnv = with pkgs; {
      LIBCLANG_PATH = "${libclang.lib}/lib";
      SHADERC_LIB_DIR = "${shaderc.lib}/lib/";
    };
    craneArgs = {
      pname = "waygriff";
      version = "0.1.0";
      src = pkgs.lib.cleanSourceWith {
        src = craneLib.path ./.;
        filter = path: type:
          (craneLib.filterCargoSources path type)
          || (pkgs.lib.hasSuffix ".c" path)
          || (pkgs.lib.hasSuffix ".h" path);
      };
      env = commonEnv;
      stdenv = p: p.clangStdenv;
      buildInputs = commonInputs;
      nativeBuildInputs = with pkgs; [libxkbcommon pkg-config];
    };
    cargoArtifacts = craneLib.buildDepsOnly craneArgs;
    package = craneLib.buildPackage (craneArgs
      // {
        inherit cargoArtifacts;
        postFixup = ''
          patchelf \
          --set-rpath "${rpath}" \
          $out/bin/${craneArgs.pname}
        '';
      });
  in {
    packages.${system}.default = package;

    checks.${system}.default = package;

    devShells.${system}.default = pkgs.mkShell {
      buildInputs = commonInputs;
      packages =
        (with pkgs; [
          cargo-flamegraph
          cmake
          fontconfig
          cgdb
          gdb
          pkg-config
          xorg.libX11
          xorg.libXpresent
          xorg.libXrandr
          xorg.libXext
          xorg.xmodmap
          xorg.xprop
          xorg.xvfb
          xorg.xwininfo
        ])
        ++ [
          rustToolchain
        ];
      shellHook = with pkgs;
        ''
          export LD_LIBRARY_PATH="${rpath}"
          # export BINDGEN_EXTRA_CLANG_ARGS="-isystem ${llvmPackages.libclang.lib}/lib/clang/${lib.versions.major (lib.getVersion clang)}/include -I${glibc.dev}/include"
        ''
        + (lib.concatStringsSep "\n" (
          lib.mapAttrsToList (name: value: "export ${name}=\"${value}\"") commonEnv
        ));
    };
  };
}
