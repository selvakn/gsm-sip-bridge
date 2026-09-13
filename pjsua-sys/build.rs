use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PJSUA_SYS_BINDINGS");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let bindings_path = out_dir.join("bindings.rs");

    let Some(lib) = probe_libpjproject() else {
        write_empty_bindings(&bindings_path);
        return;
    };

    emit_pkg_config_links(&lib);

    if let Ok(pregenerated) = env::var("PJSUA_SYS_BINDINGS") {
        let src = Path::new(&pregenerated);
        if src.is_file() {
            fs::copy(src, &bindings_path).expect("copy pre-generated bindings");
            println!("cargo:warning=pjsua-sys: using pre-generated bindings from {pregenerated}");
            return;
        }
        println!(
            "cargo:warning=pjsua-sys: PJSUA_SYS_BINDINGS set but file not found: {pregenerated}"
        );
    }

    let Some(header) = find_pjsua_header(&lib) else {
        println!("cargo:warning=pjsua-sys: pjsua-lib/pjsua.h not found; using empty FFI bindings");
        write_empty_bindings(&bindings_path);
        return;
    };
    warn_if_config_site_incomplete(&header);

    println!("cargo:rerun-if-changed={}", header.display());

    let clang_args = pkg_config_cflags("libpjproject");
    if run_bindgen(&header, &clang_args, &bindings_path).is_ok() {
        return;
    }

    println!("cargo:warning=pjsua-sys: bindgen failed; using empty FFI bindings");
    write_empty_bindings(&bindings_path);
}

fn probe_libpjproject() -> Option<pkg_config::Library> {
    let force_static = env::var("PJSUA_SYS_STATIC").is_ok();
    pkg_config::Config::new()
        .atleast_version("2.14")
        .statik(force_static)
        .cargo_metadata(false)
        .probe("libpjproject")
        .map_err(|err| {
            println!("cargo:warning=pjsua-sys: pkg-config libpjproject >= 2.14 not available ({err}); using empty FFI bindings");
        })
        .ok()
}

fn emit_pkg_config_links(lib: &pkg_config::Library) {
    let force_static = env::var("PJSUA_SYS_STATIC").is_ok();
    let system_libs: HashSet<&str> = [
        "stdc++", "ssl", "crypto", "uuid", "m", "rt", "pthread", "asound",
    ]
    .iter()
    .copied()
    .collect();

    for path in &lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }

    let mut seen = HashSet::<String>::new();
    for name in &lib.libs {
        if seen.insert(name.clone()) {
            if force_static && !system_libs.contains(name.as_str()) {
                println!("cargo:rustc-link-lib=static={name}");
            } else {
                println!("cargo:rustc-link-lib={name}");
            }
        }
    }

    if !force_static {
        for extra in ["srtp", "resample", "ssl", "crypto", "uuid"] {
            if seen.insert(extra.to_string()) {
                println!("cargo:rustc-link-lib={extra}");
            }
        }
    }
}

fn find_pjsua_header(lib: &pkg_config::Library) -> Option<PathBuf> {
    for inc in &lib.include_paths {
        let candidate = inc.join("pjsua-lib/pjsua.h");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// This project ships `docker/pjsip-config-site.h`
/// (`pjlib/include/pj/config_site.h` at pjproject build time) to enable
/// `PJMEDIA_CODEC_L16_HAS_16KHZ_MONO` (the VoWiFi bridge's wideband codec —
/// `specs/011-vowifi-sip-bridge/`) and `PJ_GETHOSTIP_DISABLE_LOCAL_RESOLUTION`
/// (the blocking-hostname-lookup fix). The Docker image always has both; a
/// locally `apt install libpjproject-dev`'d or otherwise separately built
/// libpjproject may not — `docs/development.md`'s documented local-dev setup
/// uses the stock package, which doesn't set either. Neither macro affects
/// whether this crate *compiles* — only PJSIP's own already-compiled runtime
/// codec table and address-discovery behavior — so this can only ever be a
/// warning, not something this build script can fix by itself: the real fix
/// is rebuilding PJSIP with the project's config_site.h in place (see
/// `docs/development.md`).
fn warn_if_config_site_incomplete(pjsua_header: &Path) {
    // pjsua_header is `<include-dir>/pjsua-lib/pjsua.h`.
    let Some(include_dir) = pjsua_header.parent().and_then(Path::parent) else {
        return;
    };
    let path = include_dir.join("pj/config_site.h");
    let Ok(contents) = fs::read_to_string(&path) else {
        return; // nothing to check, e.g. a minimal reinstall without this file at all
    };
    let missing: Vec<&str> = [
        "PJMEDIA_CODEC_L16_HAS_16KHZ_MONO",
        "PJ_GETHOSTIP_DISABLE_LOCAL_RESOLUTION",
    ]
    .into_iter()
    .filter(|define| !contents.contains(define))
    .collect();
    if !missing.is_empty() {
        println!(
            "cargo:warning=pjsua-sys: {} is missing {} — this linked \
             libpjproject was not built with this project's \
             docker/pjsip-config-site.h, so a --features pjsip-linked local \
             build will behave differently from the Docker image (e.g. no \
             L16/16000 wideband codec). See docs/development.md.",
            path.display(),
            missing.join(", ")
        );
    }
}

/// Points `LIBCLANG_PATH` at a mainstream, distro-packaged libclang before
/// bindgen runs, unless the caller already set one.
///
/// Exists because `clang-sys`'s default discovery prefers whatever
/// `llvm-config` happens to be first on `PATH`. On a dev machine with an
/// unusual, very new LLVM toolchain installed alongside the system one
/// (observed in the wild: an unreleased Homebrew LLVM 23 build under
/// `~/.linuxbrew`) that resolves to a libclang with a real parsing
/// regression: several PJSIP structs that are `typedef`'d via a forward
/// declaration before their full body appears later in the header
/// (`pjsua_media_config`, `pjsip_cred_info`, `pjsip_rx_data`,
/// `pjsua_msg_data`, ...) come out with no fields at all — bindgen emits
/// them as an opaque single-byte placeholder — which then fails to compile
/// far downstream in `pjsua-safe` with a wall of "no field X" errors that
/// give no hint the actual cause is upstream, in which libclang got picked.
/// A standard, distro-packaged libclang (verified: Debian/Ubuntu's
/// `libclang-19`/`-20`/`-21`) parses the exact same header correctly.
fn prefer_system_libclang() {
    if env::var_os("LIBCLANG_PATH").is_some() {
        return; // caller already pinned one — never override
    }
    let candidate = system_lib_dirs()
        .into_iter()
        .flat_map(|dir| {
            let entries = fs::read_dir(&dir).into_iter().flatten().flatten();
            let versions: Vec<u32> = entries
                .filter_map(|entry| libclang_version(&entry.file_name().to_string_lossy()))
                .collect();
            versions.into_iter().map(move |v| (v, dir.clone()))
        })
        .max_by_key(|(version, _)| *version);

    if let Some((version, dir)) = candidate {
        println!(
            "cargo:warning=pjsua-sys: LIBCLANG_PATH not set; using the \
             system libclang-{version} found at {} (set LIBCLANG_PATH \
             yourself to override this)",
            dir.display()
        );
        env::set_var("LIBCLANG_PATH", dir);
    }
}

/// Directories a distro package manager installs `libclang-N.so*` into.
/// Deliberately does not search `PATH`-derived / user-local toolchain
/// locations (Homebrew, a manually built LLVM, ...) — those are exactly
/// what this exists to route around.
fn system_lib_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/lib/x86_64-linux-gnu"),
        PathBuf::from("/usr/lib/aarch64-linux-gnu"),
        PathBuf::from("/usr/lib"),
        PathBuf::from("/usr/local/lib"),
    ];
    if let Ok(entries) = fs::read_dir("/usr/lib") {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("llvm-") {
                dirs.push(entry.path().join("lib"));
            }
        }
    }
    dirs
}

/// Extracts the version from a distro libclang filename (`libclang-21.so.21`,
/// `libclang-19.so.19.1.0`, ...). Requires the `libclang-N` form
/// specifically — the unversioned `libclang.so` / `libclang.so.N` names are
/// exactly the ambiguous/symlink form a non-distro toolchain (like the
/// Homebrew build above) also produces, so matching those too would defeat
/// the point.
fn libclang_version(filename: &str) -> Option<u32> {
    let rest = filename.strip_prefix("libclang-")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Detects the exact opaque-struct signature `prefer_system_libclang`'s doc
/// comment describes, so a bad libclang produces one clear, actionable
/// warning here instead of a wall of unrelated-looking `no field X` errors
/// once `pjsua-safe` tries to compile against these bindings.
fn warn_if_bindings_look_opaque(generated: &str) {
    // `pjsua_transport_config` is what this crate's `[sip].public_addr`
    // support (`specs/050-sip-public-addr/`) depends on directly; the other
    // two are just additional, independent evidence of the same failure
    // mode, in case a future libclang regression happens to spare that one.
    const CRITICAL_STRUCTS: &[&str] = &[
        "pjsua_transport_config",
        "pjsua_media_config",
        "pjsua_acc_config",
    ];
    let opaque: Vec<&str> = CRITICAL_STRUCTS
        .iter()
        .copied()
        .filter(|name| {
            generated.contains(&format!("pub struct {name} {{\n    pub _address: u8,\n}}"))
        })
        .collect();
    if !opaque.is_empty() {
        println!(
            "cargo:warning=pjsua-sys: bindgen produced opaque (fieldless) \
             bindings for {opaque:?} — this is a known libclang parsing \
             regression seen with some very new/nonstandard LLVM builds. Set \
             LIBCLANG_PATH to a mainstream, distro-packaged libclang (e.g. \
             `export LIBCLANG_PATH=/usr/lib/x86_64-linux-gnu` on \
             Debian/Ubuntu) and rebuild — pjsua-safe will not compile with \
             --features pjsip-linked otherwise."
        );
    }
}

fn pkg_config_cflags(package: &str) -> Vec<String> {
    let output = match Command::new("pkg-config")
        .args(["--cflags", package])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn run_bindgen(header: &Path, pkg_cflags: &[String], out_path: &Path) -> Result<(), ()> {
    prefer_system_libclang();

    let clang_args: Vec<String> = pkg_cflags.to_vec();

    let bindings = bindgen::Builder::default()
        .header(header.to_str().ok_or(())?)
        .clang_args(&clang_args)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .merge_extern_blocks(true)
        .layout_tests(false)
        .generate_comments(false)
        .blocklist_item("IPPORT_RESERVED")
        .blocklist_item("FP_NAN")
        .blocklist_item("FP_INFINITE")
        .blocklist_item("FP_ZERO")
        .blocklist_item("FP_SUBNORMAL")
        .blocklist_item("FP_NORMAL")
        .generate()
        .map_err(|_| ())?;

    warn_if_bindings_look_opaque(&bindings.to_string());

    bindings.write_to_file(out_path).map_err(|_| ())?;
    Ok(())
}

fn write_empty_bindings(path: &Path) {
    let stub = "// pjsua-sys: empty stub — libpjproject headers not found or bindgen failed.\n\
                // Rebuild with libpjproject 2.14+ and clang for real pjsua FFI symbols.\n";
    fs::write(path, stub).expect("write bindings.rs stub");
}
