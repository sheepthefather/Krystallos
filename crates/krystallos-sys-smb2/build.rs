//! Builds the vendored libsmb2 with CMake and links the result in statically.
//!
//! # Why CMake and not `cc`
//!
//! The first attempt drove the compiler directly with the `cc` crate, copying
//! the source list out of `lib/CMakeLists.txt`. That works on Android and Linux
//! — and it is what the known-good Android ports of this library do — but it
//! does not work on Windows, for a reason worth recording:
//!
//! libsmb2's `HAVE_*` feature macros do not merely gate optional extras. Some
//! `#else` branches are unguarded POSIX includes, e.g. `libdcerpc/dcerpc.c`:
//!
//! ```c
//! #ifdef HAVE_FCNTL_H
//! #include <fcntl.h>
//! #else
//! #include <sys/fcntl.h>
//! #endif
//! ```
//!
//! On Linux `<sys/fcntl.h>` exists, so leaving every `HAVE_*` undefined produces
//! a working build by luck. On MSVC it does not exist, and the build fails.
//! Supplying the macros by hand amounts to reimplementing `configure`, which is
//! how this file ended up three platform workarounds deep before the real
//! problem was visible.
//!
//! CMake is the supported path for both targets and generates `config.h` from
//! upstream's own checks, so the platform knowledge stays upstream's problem.
//! The cost is a CMake dependency at build time, which is a fair trade for not
//! hand-maintaining a `config.h` that can rot silently — a wrong feature macro
//! produces subtly wrong behaviour, not a compile error.

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let relative = Path::new("../../vendor/libsmb2");
    // `dunce`, not `std::fs::canonicalize`: the std version can return a
    // verbatim path (`\\?\D:\...`) on Windows, which CMake and MSVC do not
    // accept.
    let libsmb2 = dunce::canonicalize(manifest_dir.join(relative)).unwrap_or_else(|_| {
        panic!(
            "libsmb2 sources not found at {}.\n\
             The submodule is most likely not initialised. Run:\n\
             \n    git submodule update --init --recursive\n",
            manifest_dir.join(relative).display()
        )
    });

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    let mut cfg = cmake::Config::new(&libsmb2);

    // Static, so that the final Android artifact is a single `.so` with no
    // separate libsmb2 to ship and no ~170 exported `smb2_*` symbols to leak.
    cfg.define("BUILD_SHARED_LIBS", "OFF");
    cfg.define("ENABLE_EXAMPLES", "OFF");

    // Kerberos needs MIT krb5 plus GSSAPI, neither of which exists on Android.
    // These must be passed explicitly rather than left to default: upstream's
    // CMakeLists takes the `CMAKE_SYSTEM_NAME MATCHES "...|Android"` branch and
    // runs `find_package(GSSAPI)`, which can resolve against the *host* and
    // link the host's krb5 into an Android build.
    cfg.define("ENABLE_LIBKRB5", "OFF");
    cfg.define("ENABLE_GSSAPI", "OFF");

    // Only skips the full DCE/RPC stack in `libdcerpc/`. The minimal wrappers in
    // `lib/` that share enumeration needs are part of the core source list and
    // are still built.
    cfg.define("ENABLE_LIBDCERPC", "OFF");

    // CMake 4 dropped support for `cmake_minimum_required` below 3.5, and
    // libsmb2 still declares versions as low as 3.2 on some branches. Without
    // this the configure step fails outright on any modern CMake.
    cfg.define("CMAKE_POLICY_VERSION_MINIMUM", "3.5");

    if target_os == "windows" {
        // `_WINDOWS` and `WIN32` do not come from the compiler: MSVC defines
        // `_WIN32`, not `_WINDOWS`. CMake normally supplies them through
        // `CMAKE_C_FLAGS_INIT` in its Windows-MSVC platform module, which is why
        // upstream's own build files never mention them — and which is exactly
        // why they go missing here. The `cmake` crate assigns `CMAKE_C_FLAGS`
        // outright (to `-nologo -MD -Brepro -W0`, matching Rust's CRT), and
        // that assignment replaces the initialised value, taking `/DWIN32
        // /D_WINDOWS` with it.
        //
        // The consequence is not subtle. Without `_WINDOWS`, `lib/compat.h`
        // (line 26) and `lib/socket.c` (line 1460) never take their Windows
        // branch, `<stdint.h>` is never included, and every use of `uint8_t`
        // fails to parse — the whole library stops compiling.
        //
        // `cflag` appends rather than replaces, so the crate's own flags
        // survive. The other Windows definitions upstream needs
        // (`WIN32_LEAN_AND_MEAN`, `HAVE_LINGER`, `NEED_*`) come from
        // `add_definitions` in CMakeLists.txt:194 and so are unaffected by any
        // of this.
        cfg.cflag("/DWIN32");
        cfg.cflag("/D_WINDOWS");
    }

    if target_os == "android" {
        // The generator has to be forced. On Windows the `cmake` crate
        // defaults to the Visual Studio generator, which is multi-config and
        // native-only — asked to cross-compile to Android it fails at the
        // `project()` call with a VCTargetsPath error rather than anything that
        // mentions Android. Ninja is a single-config generator, which is what
        // cross-compiling with a toolchain file requires.
        cfg.generator("Ninja");

        // Point CMake at the NDK toolchain explicitly instead of relying on
        // cmake-rs's Android heuristics. cargo-ndk exports `ANDROID_ABI` and
        // `ANDROID_PLATFORM`, which those heuristics key off, but setting the
        // toolchain file is what actually makes the compiler, sysroot and
        // linker all agree with the Rust target.
        let ndk = find_ndk().unwrap_or_else(|| {
            panic!(
                "Android NDK not found. Set ANDROID_NDK_HOME, or install the NDK so \
                 that $ANDROID_HOME/ndk/<version> exists."
            )
        });
        let toolchain = ndk.join("build").join("cmake").join("android.toolchain.cmake");
        assert!(
            toolchain.exists(),
            "NDK toolchain file missing at {}",
            toolchain.display()
        );

        cfg.define("CMAKE_TOOLCHAIN_FILE", toolchain);
        cfg.define("ANDROID_ABI", android_abi(&target_arch));

        // cargo-ndk defaults `ANDROID_PLATFORM` to API 21 when `-p` is not
        // given. That is below this project's minSdk, so it is worth saying so
        // rather than silently building against an API level nothing else in
        // the project targets.
        let platform =
            env::var("ANDROID_PLATFORM").unwrap_or_else(|_| DEFAULT_ANDROID_PLATFORM.to_string());
        if api_level(&platform).is_some_and(|level| level < MIN_SDK) {
            println!(
                "cargo:warning=ANDROID_PLATFORM is {platform}, below this project's minSdk \
                 of {MIN_SDK}. libsmb2 will be built against an older API level than the rest \
                 of the app. Pass `-p {MIN_SDK}` to cargo-ndk to match."
            );
        }
        cfg.define("ANDROID_PLATFORM", platform);
    }

    let dst = cfg.build();

    // The `cmake` crate drives a multi-configuration generator on Windows, so
    // an installed archive can land either directly under `lib/` or under a
    // per-configuration subdirectory. Both are cheap to search, and missing
    // either one shows up as an unhelpful "cannot find -lsmb2" at link time.
    let lib_dir = dst.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    for config in ["Release", "Debug", "RelWithDebInfo", "MinSizeRel"] {
        let dir = lib_dir.join(config);
        if dir.is_dir() {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
    }
    println!("cargo:rustc-link-lib=static=smb2");

    // Winsock, on the Windows host build (upstream CMakeLists.txt:193).
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=ws2_32");
    }

    // Rebuild when the vendored sources change — i.e. whenever the submodule
    // pointer moves, which is the only way this library is ever updated.
    println!("cargo:rerun-if-changed={}", libsmb2.join("lib").display());
    println!("cargo:rerun-if-changed={}", libsmb2.join("include").display());
    println!("cargo:rerun-if-changed={}", libsmb2.join("CMakeLists.txt").display());
    println!("cargo:rerun-if-changed=build.rs");
}

/// The API level this project targets. Keep in step with `minSdk` in the
/// Android app; the warning below fires if cargo-ndk is invoked without `-p`.
const MIN_SDK: u32 = 29;
const DEFAULT_ANDROID_PLATFORM: &str = "android-29";

/// Rust target architecture -> Android ABI name, as the NDK spells it.
fn android_abi(target_arch: &str) -> &'static str {
    match target_arch {
        "aarch64" => "arm64-v8a",
        "arm" => "armeabi-v7a",
        "x86_64" => "x86_64",
        "x86" => "x86",
        other => panic!("unsupported Android target architecture: {other}"),
    }
}

/// Pull the numeric level out of `android-29` or a bare `29`.
fn api_level(platform: &str) -> Option<u32> {
    platform.rsplit(['-', ' ']).next()?.parse().ok()
}

/// Locate the Android NDK, trying the variables the NDK and cargo-ndk use, in
/// the same order cargo-ndk does.
fn find_ndk() -> Option<PathBuf> {
    for var in [
        "ANDROID_NDK_HOME",
        "ANDROID_NDK_ROOT",
        "ANDROID_NDK_PATH",
        "NDK_HOME",
    ] {
        if let Ok(path) = env::var(var) {
            if !path.is_empty() {
                let p = PathBuf::from(path);
                if p.join("build/cmake/android.toolchain.cmake").exists() {
                    return Some(p);
                }
            }
        }
    }

    // Fall back to the SDK layout: $ANDROID_HOME/ndk/<version>. Picking the
    // highest version keeps this working when a second NDK is installed.
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Ok(sdk) = env::var(var) {
            if sdk.is_empty() {
                continue;
            }
            let ndk_root = PathBuf::from(sdk).join("ndk");
            let mut versions: Vec<PathBuf> = std::fs::read_dir(&ndk_root)
                .ok()?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.join("build/cmake/android.toolchain.cmake").exists())
                .collect();
            versions.sort();
            if let Some(latest) = versions.pop() {
                return Some(latest);
            }
        }
    }

    None
}
