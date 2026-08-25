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

/// Names of the `platform()` targets declared in a generated `platforms/BUCK`,
/// in file order, paired with the exact text of each block.
///
/// The scan is deliberately literal rather than a Starlark parse: the file is
/// `@generated`, every block opens with `platform(` in column 0 and closes with
/// a bare `)`, and anything that does not match that shape is content we must
/// leave alone.
fn platform_blocks(contents: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut lines = contents.lines().peekable();

    while let Some(line) = lines.next() {
        if line != "platform(" && !line.starts_with("platform(") {
            continue;
        }
        let mut block = String::from(line);
        block.push('\n');
        let mut name = None;
        for body in lines.by_ref() {
            block.push_str(body);
            block.push('\n');
            if let Some(rest) = body.trim().strip_prefix("name = \"")
                && let Some(value) = rest.strip_suffix("\",")
            {
                name = Some(value.to_string());
            }
            if body == ")" {
                break;
            }
        }
        if let Some(name) = name {
            blocks.push((name, block));
        }
    }

    blocks
}

/// Add every `platform()` target the embedded template declares that `dest`'s
/// `platforms/BUCK` is missing, and return the names added in template order.
///
/// This is additive on purpose. A `platforms/BUCK` is generated but routinely
/// hand-extended (extra `config_setting`s, comments), so rewriting it would
/// cost more than the drift it fixes. What it must not be missing is a
/// `platform()` for a supported (os, cpu) pair: `os_deps` keyed to that pair
/// lowers to a `select()` branch nothing can match, and the dependencies in it
/// are dropped from the build with no diagnostic. See `docs/multi-platform.md`
/// and `//platforms/verify_deps.bxl:check`.
///
/// A repo with no `platforms/` directory is left alone — creating one is
/// `migrate --init`'s job, not an upgrade's.
pub fn upgrade_platform_assets(dest: &Path) -> io::Result<Vec<String>> {
    let buck_file = dest.join("platforms").join("BUCK");
    if !buck_file.is_file() {
        return Ok(Vec::new());
    }

    let template = PLATFORMS_ASSET
        .get_file("BUCK.template")
        .ok_or_else(|| io::Error::other("embedded platforms/BUCK.template is missing"))?;
    let template = String::from_utf8(normalize_line_endings(template.contents()))
        .map_err(|_| io::Error::other("embedded platforms/BUCK.template is not valid UTF-8"))?;

    let existing = std::fs::read_to_string(&buck_file)?;
    let have: Vec<String> = platform_blocks(&existing)
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    let mut added = Vec::new();
    let mut appended = String::new();
    for (name, block) in platform_blocks(&template) {
        if have.contains(&name) {
            continue;
        }
        appended.push('\n');
        appended.push_str(&block);
        added.push(name);
    }

    if added.is_empty() {
        return Ok(added);
    }

    let mut updated = existing;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&appended);
    std::fs::write(&buck_file, updated)?;

    Ok(added)
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
    use super::{extract_buck2_assets, platform_blocks, upgrade_platform_assets};
    use tempfile::TempDir;

    /// A `platforms/BUCK` as an older cargo-buckal generated it: four of the
    /// six pairs, plus the sort of hand-written additions real repos collect.
    const LEGACY_PLATFORMS_BUCK: &str = r#"# @generated by `cargo buckal`

platform(
    name = "aarch64-apple-darwin",
    constraint_values = [
        "prelude//os/constraints:macos",
        "prelude//cpu/constraints:arm64",
    ],
    visibility = ["PUBLIC"],
)

platform(
    name = "x86_64-pc-windows-msvc",
    constraint_values = [
        "prelude//os/constraints:windows",
        "prelude//cpu/constraints:x86_64",
        "prelude//abi/constraints:msvc",
    ],
    visibility = ["PUBLIC"],
)

platform(
    name = "x86_64-unknown-linux-gnu",
    constraint_values = [
        "prelude//os/constraints:linux",
        "prelude//cpu/constraints:x86_64",
        "prelude//abi/constraints:gnu",
    ],
    visibility = ["PUBLIC"],
)

platform(
    name = "aarch64-unknown-linux-gnu",
    constraint_values = [
        "prelude//os/constraints:linux",
        "prelude//cpu/constraints:arm64",
        "prelude//abi/constraints:gnu",
    ],
    visibility = ["PUBLIC"],
)

# Hand-written below this line.
config_setting(
    name = "linux-arm64",
    constraint_values = [
        "prelude//os/constraints:linux",
        "prelude//cpu/constraints:arm64",
    ],
    visibility = ["PUBLIC"],
)
"#;

    fn legacy_repo() -> TempDir {
        let dest = TempDir::new().expect("failed to create temp dir");
        let platforms = dest.path().join("platforms");
        std::fs::create_dir_all(&platforms).expect("create platforms dir");
        std::fs::write(platforms.join("BUCK"), LEGACY_PLATFORMS_BUCK).expect("write BUCK");
        dest
    }

    fn platforms_buck(dest: &TempDir) -> String {
        std::fs::read_to_string(dest.path().join("platforms").join("BUCK")).expect("read BUCK")
    }

    #[test]
    fn platform_blocks_reads_names_and_ignores_other_rules() {
        let names: Vec<String> = platform_blocks(LEGACY_PLATFORMS_BUCK)
            .into_iter()
            .map(|(name, _)| name)
            .collect();

        // `config_setting(name = "linux-arm64")` must not be mistaken for a platform.
        assert_eq!(
            names,
            vec![
                "aarch64-apple-darwin",
                "x86_64-pc-windows-msvc",
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
            ]
        );
    }

    #[test]
    fn upgrade_adds_only_the_missing_platforms() {
        let dest = legacy_repo();

        let added = upgrade_platform_assets(dest.path()).expect("upgrade failed");

        assert_eq!(
            added,
            vec!["x86_64-apple-darwin", "aarch64-pc-windows-msvc"]
        );

        let names: Vec<String> = platform_blocks(&platforms_buck(&dest))
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for triple in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        ] {
            assert!(names.contains(&triple.to_string()), "missing {triple}");
        }
    }

    /// The upgrade appends; it must not disturb a line the user wrote.
    #[test]
    fn upgrade_preserves_existing_content() {
        let dest = legacy_repo();

        upgrade_platform_assets(dest.path()).expect("upgrade failed");

        let updated = platforms_buck(&dest);
        assert!(updated.starts_with(LEGACY_PLATFORMS_BUCK));
        assert!(updated.contains("# Hand-written below this line."));
        assert!(updated.contains("config_setting("));
    }

    #[test]
    fn upgrade_is_idempotent() {
        let dest = legacy_repo();

        upgrade_platform_assets(dest.path()).expect("first upgrade failed");
        let after_first = platforms_buck(&dest);

        let added = upgrade_platform_assets(dest.path()).expect("second upgrade failed");

        assert!(added.is_empty(), "second run should add nothing");
        assert_eq!(after_first, platforms_buck(&dest));
    }

    /// A freshly extracted tree is already complete, so an upgrade over it is
    /// a no-op. This is what ties the upgrade to the template: if the two ever
    /// disagree, `migrate --init` would be followed by a spurious "Adding".
    #[test]
    fn upgrade_is_a_noop_on_a_freshly_extracted_tree() {
        let dest = TempDir::new().expect("failed to create temp dir");
        extract_buck2_assets(dest.path()).expect("failed to extract assets");

        let added = upgrade_platform_assets(dest.path()).expect("upgrade failed");

        assert!(added.is_empty(), "fresh extraction should need no upgrade");
    }

    #[test]
    fn upgrade_skips_a_repo_with_no_platforms_dir() {
        let dest = TempDir::new().expect("failed to create temp dir");

        let added = upgrade_platform_assets(dest.path()).expect("upgrade failed");

        assert!(added.is_empty());
        assert!(!dest.path().join("platforms").exists());
    }

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
