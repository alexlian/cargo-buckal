use std::io;
use std::path::Path;

use include_dir::{Dir, DirEntry, include_dir};

static TOOLCHAINS_ASSET: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/toolchains");
static PLATFORMS_ASSET: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/platforms");

fn normalize_line_endings(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            result.push(b'\n');
            i += 2;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    result
}

pub fn extract_buck2_assets(dest: &Path) -> io::Result<()> {
    let toolchains_root = dest.join("toolchains");
    let platforms_root = dest.join("platforms");
    std::fs::create_dir_all(&toolchains_root)?;
    std::fs::create_dir_all(&platforms_root)?;
    extract_dir(&toolchains_root, &TOOLCHAINS_ASSET)?;
    extract_dir(&platforms_root, &PLATFORMS_ASSET)?;
    Ok(())
}

fn extract_dir(dest: &Path, dir: &Dir) -> io::Result<()> {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(sub_dir) => {
                let target_dir = dest.join(sub_dir.path());
                std::fs::create_dir_all(&target_dir)?;
                extract_dir(dest, sub_dir)?;
            }
            DirEntry::File(file) => {
                let mut target_path = dest.join(file.path());
                // Rename BUCK.template to BUCK during extraction.
                // We use .template extension in source to avoid Buck2 package boundaries
                // which would prevent glob from including these files in the vendor output.
                if target_path.file_name() == Some(std::ffi::OsStr::new("BUCK.template")) {
                    target_path.set_file_name("BUCK");
                }
                if let Some(parent) = target_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Normalize line endings to LF for cross-platform consistency.
                // When compiled on Windows, embedded files may have CRLF endings
                // from git checkout, causing generated files to differ across platforms.
                let contents = normalize_line_endings(file.contents());
                std::fs::write(target_path, contents)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::extract_buck2_assets;
    use tempfile::TempDir;

    #[test]
    fn extract_buck2_assets_creates_expected_files() {
        let dest = TempDir::new().expect("failed to create temp dir");

        extract_buck2_assets(dest.path()).expect("failed to extract assets");

        assert!(dest.path().join("toolchains").is_dir());
        assert!(dest.path().join("platforms").is_dir());

        let toolchains_buck = dest.path().join("toolchains").join("BUCK");
        let platforms_buck = dest.path().join("platforms").join("BUCK");
        let verify_deps_bxl = dest.path().join("platforms").join("verify_deps.bxl");
        let demo_cxx = dest
            .path()
            .join("toolchains")
            .join("cxx")
            .join("demo_cxx.bzl");
        let demo_rust = dest
            .path()
            .join("toolchains")
            .join("rust")
            .join("demo_rust.bzl");

        assert!(toolchains_buck.is_file());
        assert!(platforms_buck.is_file());
        assert!(verify_deps_bxl.is_file());
        assert!(demo_cxx.is_file());
        assert!(demo_rust.is_file());

        let toolchains_contents =
            std::fs::read_to_string(&toolchains_buck).expect("read toolchains BUCK");
        assert!(!toolchains_contents.trim().is_empty());

        let platforms_contents =
            std::fs::read_to_string(&platforms_buck).expect("read platforms BUCK");
        assert!(!platforms_contents.trim().is_empty());
    }
}
