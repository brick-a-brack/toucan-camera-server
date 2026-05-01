use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=external/EDSDK");
    println!("cargo:rerun-if-changed=src/backends/webcam_macos/bridge.m");
    println!("cargo:rerun-if-changed=src/backends/webcam_macos/bridge.h");
    println!("cargo:rerun-if-changed=logo/logo.ico");

    #[cfg(target_os = "windows")]
    {
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

    if cfg!(target_os = "macos")
        && std::env::var_os("CARGO_FEATURE_BACKEND_WEBCAM_MACOS").is_some()
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
}

fn link_canon_sdk(manifest_dir: &str) {
    #[cfg(target_os = "windows")]
    {
        println!(
            "cargo:rustc-link-search=native={}/external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Library",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=EDSDK");
    }

    #[cfg(target_os = "macos")]
    {
        println!(
            "cargo:rustc-link-search=framework={}/external/EDSDK/EDSDKv132010M",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=framework=EDSDK");
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}/external/EDSDK/EDSDKv132010M",
            manifest_dir
        );
    }

    #[cfg(target_os = "linux")]
    {
        let arch_dir = canon_linux_arch_dir();
        println!(
            "cargo:rustc-link-search=native={}/external/EDSDK/EDSDKv132010L/Linux/EDSDK/Library/{}",
            manifest_dir, arch_dir
        );
        println!("cargo:rustc-link-lib=EDSDK");
        // libEDSDK.so is shipped next to the binary. $ORIGIN tells the dynamic
        // loader to look in the directory of the executable at runtime, so the
        // bundle is relocatable.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}

/// Maps the Cargo TARGET triple to the Canon Linux SDK's library subdirectory.
#[cfg(target_os = "linux")]
fn canon_linux_arch_dir() -> &'static str {
    let target = std::env::var("TARGET").unwrap_or_default();
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
    #[cfg(target_os = "windows")]
    {
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
}

/// Copies libEDSDK.so next to the output binary on Linux so the binary can be
/// shipped as a relocatable bundle (binary + libEDSDK.so).
fn copy_canon_so(manifest_dir: &str) {
    #[cfg(target_os = "linux")]
    {
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
        let profile_dir = Path::new(&out_dir)
            .ancestors()
            .nth(3)
            .expect("unexpected OUT_DIR structure")
            .to_path_buf();

        let arch_dir = canon_linux_arch_dir();
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = manifest_dir;
    }
}
