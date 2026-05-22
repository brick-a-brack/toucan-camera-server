use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    // backend-gphoto2 links `libgphoto2` via pkg-config (brew/apt). For a
    // self-contained binary we also copy libgphoto2 + its camlibs/iolibs plugins
    // and their non-system dependency closure next to the binary. On Linux they
    // get an $ORIGIN rpath and are usable straight away; on macOS the files are
    // staged here and the CI packaging step rewrites install names and lipo-merges
    // the two arches (build scripts run before the link, so they cannot rewrite
    // the binary's own Homebrew-baked absolute load commands).
    if std::env::var_os("CARGO_FEATURE_BACKEND_GPHOTO2").is_some() {
        copy_gphoto2_bundle();
    }

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

// ---------------------------------------------------------------------------
// gphoto2 runtime bundle
//
// Copies libgphoto2, libgphoto2_port, their camlibs/iolibs plugins and the
// non-system dependency closure next to the produced binary so the server can
// run without a system libgphoto2. The runtime points CAMLIBS/IOLIBS at the
// copied plugin dirs (see GPhoto2Backend::new). On Linux the copies get an
// $ORIGIN rpath and work straight away; on macOS the files are staged and the CI
// packaging step rewrites install names and lipo-merges the two arches — build
// scripts run before the link, so they cannot rewrite the binary's own
// Homebrew-baked absolute load commands.
// ---------------------------------------------------------------------------

fn copy_gphoto2_bundle() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let is_mac = target.contains("apple");
    let is_linux = target.contains("linux");
    if !is_mac && !is_linux {
        return;
    }

    let libdir = match pkg_config_var("libgphoto2", "libdir") {
        Some(d) => PathBuf::from(d),
        None => {
            println!("cargo:warning=gphoto2 bundle: pkg-config libdir not found, skipping");
            return;
        }
    };

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    let lib_ext = if is_mac { "dylib" } else { "so" };
    let camlibs_dir = locate_camlibs(&libdir);
    let iolibs_dir = pkg_config_var("libgphoto2_port", "driverdir").map(PathBuf::from);

    // Core libraries seed the dependency-closure walk.
    let mut roots: Vec<PathBuf> = Vec::new();
    for name in ["libgphoto2", "libgphoto2_port"] {
        let p = libdir.join(format!("{name}.{lib_ext}"));
        if p.exists() {
            roots.push(p);
        }
    }

    // Camera drivers: skip the toy-camera camlibs that depend on libgd (which
    // drags in a tree of image/AV1 codecs). Every serious driver — ptp2
    // (Canon/Nikon/Sony/Fuji/Olympus/Panasonic over USB), the serial Canon and
    // Olympus libs, etc. — has no libgd dependency, so this only drops novelty
    // cameras that cannot be remote-controlled anyway. Unsupported cameras just
    // don't get detected; libgphoto2 degrades gracefully (no crash). I/O drivers
    // are tiny — keep them all.
    let mut camlib_plugins: Vec<PathBuf> = Vec::new();
    let mut skipped = 0usize;
    if let Some(dir) = &camlibs_dir {
        for p in shared_objects(dir) {
            if depends_on_libgd(&p, is_mac) {
                skipped += 1;
            } else {
                camlib_plugins.push(p);
            }
        }
    }
    let iolib_plugins: Vec<PathBuf> =
        iolibs_dir.as_ref().map(|d| shared_objects(d)).unwrap_or_default();

    // Plugins are dlopen'd `.so` modules; they go into their own subdirs (not
    // flat), so we collect their dependency closure but not the plugins themselves.
    let mut plugins = camlib_plugins.clone();
    plugins.extend(iolib_plugins.iter().cloned());
    let closure = collect_closure(&roots, &plugins, is_mac);
    for lib in &closure {
        copy_into(lib, &profile_dir);
    }
    copy_plugins(&camlib_plugins, &profile_dir.join("camlibs"));
    copy_plugins(&iolib_plugins, &profile_dir.join("iolibs"));
    println!(
        "cargo:warning=gphoto2 bundle: {} libs, {} camlibs (+{} libgd-only skipped), {} iolibs in {}",
        closure.len(),
        camlib_plugins.len(),
        skipped,
        iolib_plugins.len(),
        profile_dir.display()
    );

    // Record the bundled flat dylibs so the macOS CI packaging step knows exactly
    // which files to lipo-merge and relink (plugin dirs are always camlibs/ iolibs/).
    let manifest: Vec<&str> = closure
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .collect();
    // Trailing newline is required: the CI packaging scripts read this file with
    // `while read`, which silently drops a final line that is not newline-
    // terminated — leaving that lib unbundled while the binary still gets
    // relinked to it (dangling @executable_path/$ORIGIN reference → launch crash).
    let _ = std::fs::write(
        profile_dir.join("gphoto2-bundle.manifest"),
        format!("{}\n", manifest.join("\n")),
    );

    if is_linux {
        // Flat libs find siblings via $ORIGIN; plugins (one dir down) via $ORIGIN/..
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        for lib in &closure {
            if let Some(name) = lib.file_name() {
                patch_rpath(&profile_dir.join(name), "$ORIGIN");
            }
        }
        for sub in ["camlibs", "iolibs"] {
            let dir = profile_dir.join(sub);
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                patch_rpath(&entry.path(), "$ORIGIN/..");
            }
        }
    }
}

fn pkg_config_var(pkg: &str, var: &str) -> Option<String> {
    let out = Command::new("pkg-config")
        .args(["--variable", var, pkg])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// camlibs live in `<libdir>/libgphoto2/<modversion>`; Homebrew exposes no usable
/// pkg-config variable for it, so derive it and fall back to the single
/// plugin-bearing subdirectory.
fn locate_camlibs(libdir: &Path) -> Option<PathBuf> {
    let base = libdir.join("libgphoto2");
    if let Ok(out) = Command::new("pkg-config")
        .args(["--modversion", "libgphoto2"])
        .output()
    {
        if out.status.success() {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let cand = base.join(&version);
            if cand.is_dir() {
                return Some(cand);
            }
        }
    }
    std::fs::read_dir(&base).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.is_dir()
            && std::fs::read_dir(p)
                .map(|d| {
                    d.flatten()
                        .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("so"))
                })
                .unwrap_or(false)
    })
}

/// Direct dynamic dependencies of a library, as absolute paths.
fn list_deps(lib: &Path, is_mac: bool) -> Vec<PathBuf> {
    let output = if is_mac {
        Command::new("otool").arg("-L").arg(lib).output()
    } else {
        Command::new("ldd").arg(lib).output()
    };
    let out = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if is_mac {
        // otool -L: first line is the file itself, then "<path> (compat …)".
        text.lines()
            .skip(1)
            .filter_map(|l| l.split_whitespace().next())
            .map(PathBuf::from)
            .collect()
    } else {
        // ldd: "libfoo.so.1 => /abs/path/libfoo.so.1 (0x…)".
        text.lines()
            .filter_map(|l| l.split("=>").nth(1))
            .filter_map(|rhs| rhs.split_whitespace().next())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .collect()
    }
}

/// Whether a dependency should be copied into the bundle (skip the OS base libs).
fn is_bundleworthy(dep: &Path, is_mac: bool) -> bool {
    if is_mac {
        let s = dep.to_string_lossy();
        s.starts_with("/opt/homebrew/") || s.starts_with("/usr/local/")
    } else {
        const SYSTEM: &[&str] = &[
            "libc.", "libm.", "libpthread.", "libdl.", "librt.", "ld-linux",
            "libgcc_s.", "libstdc++.", "libresolv.", "libnsl.", "linux-vdso",
            "libutil.", "libcrypt.", "libz.",
        ];
        let name = dep.file_name().and_then(|n| n.to_str()).unwrap_or("");
        !SYSTEM.iter().any(|s| name.starts_with(s))
    }
}

/// Recursively resolve the flat library closure of the core libs + plugin deps.
fn collect_closure(roots: &[PathBuf], plugins: &[PathBuf], is_mac: bool) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    // Plugins are copied into subdirs, not flat — keep them out of the closure.
    for p in plugins {
        if let Ok(c) = std::fs::canonicalize(p) {
            seen.insert(c);
        }
    }
    let mut stack: Vec<PathBuf> = roots.to_vec();
    for p in plugins {
        for d in list_deps(p, is_mac) {
            if is_bundleworthy(&d, is_mac) {
                stack.push(d);
            }
        }
    }
    let mut out = Vec::new();
    while let Some(lib) = stack.pop() {
        let real = std::fs::canonicalize(&lib).unwrap_or(lib);
        if !seen.insert(real.clone()) {
            continue;
        }
        for d in list_deps(&real, is_mac) {
            if is_bundleworthy(&d, is_mac) {
                stack.push(d);
            }
        }
        out.push(real);
    }
    out
}

fn copy_into(src: &Path, dest_dir: &Path) {
    let Some(name) = src.file_name() else { return };
    let dest = dest_dir.join(name);
    // brew dylibs are read-only, and std::fs::copy copies their mode — so a stale
    // read-only copy from a previous build can't be overwritten. Drop it first.
    let _ = std::fs::remove_file(&dest);
    if let Err(e) = std::fs::copy(src, &dest) {
        println!("cargo:warning=gphoto2 bundle: copy {} failed: {e}", src.display());
        return;
    }
    // brew/apt dylibs are read-only; make the copy writable so the Linux patchelf
    // pass and the macOS CI install_name_tool relink can modify it.
    make_writable(&dest);
}

#[cfg(unix)]
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200));
    }
}

#[cfg(not(unix))]
fn make_writable(_path: &Path) {}

/// All `.so` plugin modules in a directory.
fn shared_objects(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("so") {
            out.push(p);
        }
    }
    out
}

/// Whether a plugin links libgd (directly). The toy-camera camlibs do; the real
/// drivers (ptp2, canon, …) do not. Used to drop the libgd codec tree.
fn depends_on_libgd(lib: &Path, is_mac: bool) -> bool {
    list_deps(lib, is_mac).iter().any(|d| {
        d.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("libgd."))
            .unwrap_or(false)
    })
}

fn copy_plugins(plugins: &[PathBuf], dest: &Path) {
    if plugins.is_empty() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dest) {
        println!("cargo:warning=gphoto2 bundle: mkdir {} failed: {e}", dest.display());
        return;
    }
    for p in plugins {
        copy_into(p, dest);
    }
}

/// Add an rpath to a copied ELF (Linux). Best-effort: needs `patchelf`.
fn patch_rpath(lib: &Path, rpath: &str) {
    let _ = Command::new("patchelf")
        .args(["--add-rpath", rpath])
        .arg(lib)
        .status();
}
