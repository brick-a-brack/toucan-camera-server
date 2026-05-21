use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=external/EDSDK");
    println!("cargo:rerun-if-changed=src/backends/webcam_macos/bridge.m");
    println!("cargo:rerun-if-changed=src/backends/webcam_macos/bridge.h");
    println!("cargo:rerun-if-changed=src/backends/camera2_android/bridge.c");
    println!("cargo:rerun-if-changed=src/backends/camera2_android/bridge.h");
    println!("cargo:rerun-if-changed=logo/logo.ico");

    let target = std::env::var("TARGET").unwrap_or_default();

    // Windows resources (icon) — only when targeting Windows, not when cross-compiling
    // to another target (e.g. Android) from a Windows host.
    if target.contains("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("logo/logo.ico");
        res.compile().expect("failed to compile Windows resources");
    }

    if std::env::var_os("CARGO_FEATURE_BACKEND_CANON").is_some() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        link_canon_sdk(&manifest_dir);
        copy_canon_dlls(&manifest_dir);
        copy_canon_so(&manifest_dir);
    }

    // backend-gphoto2 needs `libgphoto2` discoverable via pkg-config:
    //   - macOS: brew install libgphoto2 pkg-config
    //   - Linux: apt install libgphoto2-dev pkg-config
    // The `gphoto2-sys` crate handles linking; nothing to do here.

    if std::env::var_os("CARGO_FEATURE_BACKEND_WEBCAM_MACOS").is_some()
        && target.contains("apple")
    {
        cc::Build::new()
            .file("src/backends/webcam_macos/bridge.m")
            .include("src/backends/webcam_macos")
            .flag("-fobjc-arc")
            .flag("-fmodules")
            .compile("webcam_macos_bridge");

        println!("cargo:rustc-link-lib=framework=AVFoundation");
        println!("cargo:rustc-link-lib=framework=CoreMedia");
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        println!("cargo:rustc-link-lib=framework=CoreImage");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=IOKit");
    }

    if std::env::var_os("CARGO_FEATURE_BACKEND_CAMERA2_ANDROID").is_some()
        && target.contains("android")
    {
        let ndk_home = std::env::var("ANDROID_NDK_HOME")
            .or_else(|_| std::env::var("NDK_HOME"))
            .expect("ANDROID_NDK_HOME (or NDK_HOME) must be set to build the Android Camera2 backend");

        // Determine the prebuilt host-toolchain directory based on the build host.
        let host_tag = if cfg!(target_os = "windows") {
            "windows-x86_64"
        } else if cfg!(target_os = "macos") {
            "darwin-x86_64"
        } else {
            "linux-x86_64"
        };

        let api_level = "24"; // Camera2 NDK requires API 24+
        let abi_triple = if target.starts_with("aarch64") {
            "aarch64-linux-android"
        } else if target.starts_with("armv7") || target.starts_with("arm-") {
            "armv7a-linux-androideabi"
        } else if target.starts_with("x86_64") {
            "x86_64-linux-android"
        } else {
            "i686-linux-android"
        };

        let sysroot = format!(
            "{ndk_home}/toolchains/llvm/prebuilt/{host_tag}/sysroot"
        );
        // On Windows the NDK ships .cmd wrappers, not ELF executables.
        let clang_ext = if cfg!(target_os = "windows") { ".cmd" } else { "" };
        let clang = format!(
            "{ndk_home}/toolchains/llvm/prebuilt/{host_tag}/bin/{abi_triple}{api_level}-clang{clang_ext}"
        );

        // cargo-ndk injects CFLAGS_<rust-target>=--target=<abi-triple>21 which sets
        // __ANDROID_API__=21 and makes Camera2 NDK symbols appear unavailable (API 24+).
        // Override using the Rust target triple as the var name (matches cargo-ndk's key)
        // but keep the ABI triple in the --target= value for clang.
        std::env::set_var(
            format!("CFLAGS_{target}"),
            format!("--target={abi_triple}{api_level}"),
        );

        cc::Build::new()
            .file("src/backends/camera2_android/bridge.c")
            .include("src/backends/camera2_android")
            .compiler(&clang)
            .flag(&format!("--sysroot={sysroot}"))
            .flag("-std=c11")
            .flag("-Wall")
            .compile("camera2_android_bridge");

        // Tell the Rust linker where the NDK system libraries live.
        println!(
            "cargo:rustc-link-search=native={ndk_home}/toolchains/llvm/prebuilt/{host_tag}/sysroot/usr/lib/{abi_triple}/{api_level}"
        );
        println!("cargo:rustc-link-lib=camera2ndk");
        println!("cargo:rustc-link-lib=mediandk");
        println!("cargo:rustc-link-lib=android");
        println!("cargo:rustc-link-lib=log");
    }
}

fn link_canon_sdk(manifest_dir: &str) {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.contains("windows") {
        println!(
            "cargo:rustc-link-search=native={}/external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Library",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=EDSDK");
    } else if target.contains("apple") {
        println!(
            "cargo:rustc-link-search=framework={}/external/EDSDK/EDSDKv132010M",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=framework=EDSDK");
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}/external/EDSDK/EDSDKv132010M",
            manifest_dir
        );
    } else if target.contains("linux") {
        let arch_dir = canon_linux_arch_dir(&target);
        println!(
            "cargo:rustc-link-search=native={}/external/EDSDK/EDSDKv132010L/Linux/EDSDK/Library/{}",
            manifest_dir, arch_dir
        );
        println!("cargo:rustc-link-lib=EDSDK");
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}

fn canon_linux_arch_dir(target: &str) -> &'static str {
    if target.starts_with("x86_64-") {
        "x86_64"
    } else if target.starts_with("aarch64-") {
        "ARM64"
    } else if target.starts_with("arm") {
        "ARM32"
    } else {
        panic!(
            "unsupported Linux target for Canon EDSDK: {target} \
             (supported: x86_64, aarch64, arm)"
        );
    }
}

fn copy_canon_dlls(manifest_dir: &str) {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    let dll_src = Path::new(manifest_dir)
        .join("external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Dll");

    for dll in &["EDSDK.dll", "EdsImage.dll"] {
        let src = dll_src.join(dll);
        let dst = profile_dir.join(dll);
        if src.exists() {
            std::fs::copy(&src, &dst)
                .unwrap_or_else(|e| panic!("failed to copy {dll} to {dst:?}: {e}"));
            println!("cargo:warning=Copied {dll} to {}", profile_dir.display());
        } else {
            println!("cargo:warning=Canon DLL not found, skipping copy: {}", src.display());
        }
    }
}

fn copy_canon_so(manifest_dir: &str) {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("linux") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    let arch_dir = canon_linux_arch_dir(&target);
    let src = Path::new(manifest_dir)
        .join("external/EDSDK/EDSDKv132010L/Linux/EDSDK/Library")
        .join(arch_dir)
        .join("libEDSDK.so");
    let dst = profile_dir.join("libEDSDK.so");

    if src.exists() {
        std::fs::copy(&src, &dst)
            .unwrap_or_else(|e| panic!("failed to copy libEDSDK.so to {dst:?}: {e}"));
        println!("cargo:warning=Copied libEDSDK.so to {}", profile_dir.display());
    } else {
        println!("cargo:warning=libEDSDK.so not found, skipping copy: {}", src.display());
    }
}
