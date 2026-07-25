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

    bindings.write_to_file(out_path).map_err(|_| ())?;
    Ok(())
}

fn write_empty_bindings(path: &Path) {
    let stub = "// pjsua-sys: empty stub — libpjproject headers not found or bindgen failed.\n\
                // Rebuild with libpjproject 2.14+ and clang for real pjsua FFI symbols.\n";
    fs::write(path, stub).expect("write bindings.rs stub");
}
