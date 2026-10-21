//! SC-007 (#139): no code under `src/supervise/` may build a `swanctl
//! --initiate` command line except `engines::initiate_argv`. An unbounded
//! initiate pins a charon worker thread for as long as the ePDG is silent and
//! eventually freezes the shared daemon; the one builder bounds it.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Lines of `src` that are neither comments nor inside the file's trailing
/// `#[cfg(test)] mod` (a `#[cfg(test)]` on a lone `impl` is production code
/// compiled differently, and must not end the scan early).
fn production_code_lines(src: &str) -> Vec<(usize, &str)> {
    let all: Vec<&str> = src.lines().collect();
    let mut lines = Vec::new();
    for (i, line) in all.iter().enumerate() {
        let t = line.trim_start();
        if t.starts_with("#[cfg(test)]")
            && all
                .get(i + 1)
                .is_some_and(|next| next.trim_start().starts_with("mod "))
        {
            break;
        }
        if t.starts_with("//") {
            continue;
        }
        lines.push((i + 1, *line));
    }
    lines
}

#[test]
fn swanctl_initiate_is_built_only_by_initiate_argv() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/supervise");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(!files.is_empty());

    let mut offenders = Vec::new();
    for file in files {
        let src = fs::read_to_string(&file).unwrap();
        let mut in_builder = false;
        for (n, line) in production_code_lines(&src) {
            if line.contains("pub fn initiate_argv") {
                in_builder = true;
            } else if in_builder && line.starts_with('}') {
                in_builder = false;
                continue;
            }
            if line.contains("\"--initiate\"") && !in_builder {
                offenders.push(format!("{}:{n}", file.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "an unbounded `swanctl --initiate` can freeze the shared charon (#139). \
         Use StrongswanEngine::initiate, which goes through initiate_argv. Found at: {offenders:?}"
    );
}
