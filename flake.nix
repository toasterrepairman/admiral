{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    ...
  } @ inputs:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {inherit system overlays;};
      rustVersion = pkgs.rust-bin.stable.latest.default;
      packageDeps = with pkgs; [
        openssl.dev
        rustc
        cargo
        cairo
        gdk-pixbuf
        gobject-introspection
        graphene
        gtk3.dev
        gtksourceview5
        libadwaita.dev
        hicolor-icon-theme
        pandoc
        pango
        pkg-config
        appstream-glib
        polkit
        gettext
        desktop-file-utils
        meson
        git
        wrapGAppsHook4
        libsecret
        webkitgtk_6_0.dev
        # llvmPackages.libclang
        #
        # cudaPackages.cuda_nvcc
        # cudaPackages.cuda_cudart
        # cudaPackages.cuda_cccl
        # cudaPackages.libcublas
        # cudaPackages.setupCudaHook
        # GStreamer dependencies - comprehensive set for media playback
        gst_all_1.gstreamer
        gst_all_1.gst-plugins-base
        gst_all_1.gst-plugins-good
        gst_all_1.gst-plugins-bad
        gst_all_1.gst-plugins-ugly
        gst_all_1.gst-libav
        # Hardware acceleration and WebKit dependencies
        mesa
        mesa.drivers
        libva
        libva-utils
        vaapiVdpau
        libvdpau-va-gl
        intel-media-driver  # For Intel GPUs
        # Optional: Add GST debugging tools
        libGL
        libglvnd
        xorg.libX11
        xorg.libXcomposite
        xorg.libXdamage
        xorg.libXext
        xorg.libXfixes
        xorg.libXrender
        dbus
        bubblewrap
        xdg-dbus-proxy
      ];

      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustVersion;
        rustc = rustVersion;
      };

      myRustBuild = rustPlatform.buildRustPackage {
        pname = "admiral"; # make this what ever your cargo.toml package.name is
        version = "0.1.0";
        src = ./.; # the folder with the cargo.toml
        nativeBuildInputs = packageDeps;
        buildInputs = packageDeps;

        cargoLock.lockFile = ./Cargo.lock;

        postBuild = ''
            # for desktop files
            install -Dt $out/share/applications resources/admiral.desktop

            install -Dt $out/share/icons resources/icon-admiral.png
        '';
        postInstall = ''
          wrapProgram $out/bin/admiral \
            --set GST_PLUGIN_SYSTEM_PATH_1_0 "${pkgs.lib.makeSearchPath "lib/gstreamer-1.0" [
              pkgs.gst_all_1.gst-plugins-base
              pkgs.gst_all_1.gst-plugins-good
              pkgs.gst_all_1.gst-plugins-bad
              pkgs.gst_all_1.gst-plugins-ugly
              pkgs.gst_all_1.gst-libav
            ]}" \
            --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeSearchPath "lib" [
              pkgs.gst_all_1.gstreamer
              pkgs.gst_all_1.gst-plugins-base
              pkgs.gst_all_1.gst-plugins-good
              pkgs.gst_all_1.gst-plugins-bad
              pkgs.gst_all_1.gst-plugins-ugly
              pkgs.gst_all_1.gst-libav
              pkgs.mesa
              pkgs.mesa.drivers
              pkgs.libva
              pkgs.libglvnd
              pkgs.libGL
            ]}" \
            --prefix XDG_DATA_DIRS : "${pkgs.lib.makeSearchPath "share" [
              pkgs.gtk3
              pkgs.libadwaita
              pkgs.webkitgtk_6_0
            ]}" \
            --set WEBKIT_DISABLE_COMPOSITING_MODE "0" \
            --set WEBKIT_FORCE_SANDBOX "0" \
            --set GDK_GL "gles" \
            --set LIBVA_DRIVER_NAME "iHD" \
            --prefix PATH : "${pkgs.lib.makeBinPath [
              pkgs.bubblewrap
              pkgs.xdg-dbus-proxy
            ]}"'';
      };

    in {
      defaultPackage = myRustBuild;
      devShell = pkgs.mkShell {
        nativeBuildInputs = packageDeps;
        shellHook = ''
            export GST_PLUGIN_SYSTEM_PATH_1_0=$GST_PLUGIN_SYSTEM_PATH_1_0:${pkgs.gst_all_1.gst-plugins-base}/lib/gstreamer-1.0
            export GST_PLUGIN_SYSTEM_PATH_1_0=$GST_PLUGIN_SYSTEM_PATH_1_0:${pkgs.gst_all_1.gst-plugins-good}/lib/gstreamer-1.0
            export GST_PLUGIN_SYSTEM_PATH_1_0=$GST_PLUGIN_SYSTEM_PATH_1_0:${pkgs.gst_all_1.gst-plugins-bad}/lib/gstreamer-1.0
            export GST_PLUGIN_SYSTEM_PATH_1_0=$GST_PLUGIN_SYSTEM_PATH_1_0:${pkgs.gst_all_1.gst-plugins-ugly}/lib/gstreamer-1.0
            export GST_PLUGIN_SYSTEM_PATH_1_0=$GST_PLUGIN_SYSTEM_PATH_1_0:${pkgs.gst_all_1.gst-libav}/lib/gstreamer-1.0

            # Set GStreamer debug level (optional)
            # export GST_DEBUG=3,GstPipeline:4,GstMemory:5,GstPoll:5 RUST_BACKTRACE=1 admiral

            echo "GStreamer development environment ready"
        '';
        # CUDA_ROOT = "${pkgs.cudaPackages.cudatoolkit}";
        buildInputs = [(rustVersion.override {extensions = ["rust-src"];})];
      };

      meta = with nixpkgs.lib; {
        description = "admiral";
        license = licenses.gpl3;
        platforms = platforms.all;
      };
    });
}
