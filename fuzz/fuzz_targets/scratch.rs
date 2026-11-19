// Shared by the archive targets: the readers take paths, so each input is
// written to a file in a directory of this process's own.
use std::path::PathBuf;
use std::sync::OnceLock;

pub fn directory() -> &'static PathBuf {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        let directory =
            std::env::temp_dir().join(format!("osm-tile-compiler-fuzz-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    })
}

/// Writes `data` to `name` in the directory and returns its path.
pub fn file(name: &str, data: &[u8]) -> PathBuf {
    let path = directory().join(name);
    std::fs::write(&path, data).unwrap();
    path
}
