// Links against the system's vo-amrwbenc (AMR-WB encoder) and
// opencore-amrwb (AMR-WB decoder) libraries via pkg-config, if present.
// The FFI surface is tiny and stable (3 functions per library, unchanged
// since the 3GPP reference code), so unlike pjsua-sys this crate hand-writes
// the `extern "C"` declarations in src/lib.rs rather than running bindgen —
// build.rs's only job is emitting the right link flags.
//
// If the libraries aren't installed, we simply don't emit link flags (same
// graceful-degradation approach as pjsua-sys): the crate still compiles
// (the extern declarations don't need the library to exist), and nothing
// actually pulls in the symbols unless amr-safe's `amr-linked` feature is
// enabled and its code paths are reached — see docker/Dockerfile for
// where the real libraries get installed for a linked build.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let enc = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("vo-amrwbenc");
    let dec = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("opencore-amrwb");
    // AMR *narrowband* — one library, both directions. Required as well as
    // the wideband pair: a carrier may offer AMR/8000 alone on a terminating
    // call (observed on Airtel), which is unanswerable without it.
    let nb = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("opencore-amrnb");

    // All three must be present to link: `amr-safe`'s `amr-linked` feature is
    // a single switch covering both codecs, so a partial set would compile
    // and then fail at link time with an undefined symbol.
    match (enc, dec, nb) {
        (Ok(enc), Ok(dec), Ok(nb)) => {
            for lib in enc.libs.iter().chain(dec.libs.iter()).chain(nb.libs.iter()) {
                println!("cargo:rustc-link-lib={lib}");
            }
            for path in enc
                .link_paths
                .iter()
                .chain(dec.link_paths.iter())
                .chain(nb.link_paths.iter())
            {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
        }
        (enc, dec, nb) => {
            if enc.is_err() {
                println!("cargo:warning=amr-sys: vo-amrwbenc not found via pkg-config; AMR-WB encoding unavailable unless amr-linked is built with it installed");
            }
            if dec.is_err() {
                println!("cargo:warning=amr-sys: opencore-amrwb not found via pkg-config; AMR-WB decoding unavailable unless amr-linked is built with it installed");
            }
            if nb.is_err() {
                println!("cargo:warning=amr-sys: opencore-amrnb not found via pkg-config; AMR-NB unavailable unless amr-linked is built with it installed");
            }
        }
    }
}
