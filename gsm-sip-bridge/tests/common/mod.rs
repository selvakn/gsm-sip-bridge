pub mod pbx;
pub mod pty;

use tempfile::TempDir;

pub fn temp_store() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("failed to create temp dir");
    let db_path = dir.path().join("store.db");
    (dir, db_path)
}

pub fn null_alsa_device() -> String {
    "null".to_string()
}
