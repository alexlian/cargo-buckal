use std::collections::{BTreeMap as Map, BTreeSet as Set};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    buckal_error, buckal_note,
    utils::{UnwrapOrExit, get_buck2_root},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(
        default = "default_buck2_binary",
        skip_serializing_if = "is_default_buck2_binary"
    )]
    pub buck2_binary: String,
    #[serde(default, skip_serializing_if = "RegistryDefault::if_skip")]
    pub registry: RegistryDefault,
    #[serde(default = "default_registries", skip_serializing_if = "Map::is_empty")]
    pub registries: Map<String, RegistryEntry>,
}

fn is_default_buck2_binary(value: &str) -> bool {
    value == "buck2"
}

fn default_buck2_binary() -> String {
    "buck2".to_string()
}

fn default_registries() -> Map<String, RegistryEntry> {
    let mut registries = Map::new();
    registries.insert(
        "buck2hub".to_string(),
        RegistryEntry {
            base: "https://app.buck2hub.com".to_string(),
            api: "https://git.buck2hub.com".to_string(),
            token: None,
        },
    );
    registries
}

impl Default for Config {
    fn default() -> Self {
        Self {
            buck2_binary: default_buck2_binary(),
            registry: RegistryDefault::default(),
            registries: default_registries(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryDefault {
    pub default: Option<String>,
}

impl RegistryDefault {
    pub fn if_skip(&self) -> bool {
        self.default.is_none()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub base: String,
    pub api: String,
    pub token: Option<String>,
}

impl Config {
    /// Load configuration from ~/.config/buckal/config.toml
    pub fn load() -> Self {
        let config_path = Self::config_path();

        if !config_path.exists() {
            return Self::default();
        }

        match fs::read_to_string(&config_path) {
            Ok(content) => match toml::from_str::<Config>(&content) {
                Ok(config) => config,
                Err(_) => {
                    eprintln!(
                        "Warning: Failed to parse config file at {}, using defaults",
                        config_path.display()
                    );
                    Self::default()
                }
            },
            Err(_) => {
                eprintln!(
                    "Warning: Failed to read config file at {}, using defaults",
                    config_path.display()
                );
                Self::default()
            }
        }
    }

    /// Save configuration to ~/.config/buckal/config.toml
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path();
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;

        // Write with owner-only permissions (0600 on Unix)
        // Following Cargo's approach: Unix gets 0600, other platforms use default permissions
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&config_path)?;

        file.write_all(content.as_bytes())?;

        // Set permissions after writing (Unix only)
        set_permissions(&file)?;

        Ok(())
    }

    /// Get the configuration file path
    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".config")
            .join("buckal")
            .join("config.toml")
    }

    /// Get the default registry name, or "buck2hub" if not set
    pub fn default_registry(&self) -> &str {
        self.registry.default.as_deref().unwrap_or("buck2hub")
    }
}

/// Set file permissions to owner-only (Unix only, following Cargo's approach)
#[cfg(unix)]
fn set_permissions(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = file.metadata()?.permissions();
    perms.set_mode(0o600);
    file.set_permissions(perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_permissions(_file: &File) -> Result<()> {
    // On non-Unix platforms, rely on default file system permissions
    // This is the same approach used by Cargo
    Ok(())
}

/// Repo-local settings, read from `buckal.toml` at the Buck2 project root.
///
/// `deny_unknown_fields` is load-bearing rather than tidiness. Every field
/// here has a default, and the default for `ignore_tests` is `true` -- so a
/// key that fails to bind does not degrade, it silently switches test-target
/// generation off and `rust_test` rules vanish from every BUCK file. A
/// singular `ignore_test`, or settings nested under a `[repo]` section that
/// does not exist, both used to parse cleanly and do exactly that.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    pub ignore_tests: bool,
    pub patch_fields: Set<String>,
    pub patch: RepoPatchConfig,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            ignore_tests: true,
            patch_fields: Set::new(),
            patch: RepoPatchConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RepoPatchConfig {
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub version: Map<String, VersionPatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionPatch {
    pub from: String,
    pub to: String,
}

impl RepoConfig {
    pub fn load() -> Self {
        let repo_config_path = Self::repo_config_path();

        if !repo_config_path.exists() {
            return Self::default();
        }

        // A file that exists but cannot be honoured is an error, not a
        // fallback. Continuing on defaults would generate a BUCK graph that
        // ignores what the repo asked for -- and since `ignore_tests` defaults
        // to `true`, the most likely shape of that is a graph with no test
        // rules at all, produced with a single warning that scrolls past.
        let content = fs::read_to_string(&repo_config_path).unwrap_or_else(|e| {
            buckal_error!(format!(
                "failed to read `{}`: {e}",
                repo_config_path.display()
            ));
            std::process::exit(1);
        });

        toml::from_str::<RepoConfig>(&content).unwrap_or_else(|e| {
            buckal_error!(format!(
                "failed to parse `{}`: {e}",
                repo_config_path.display()
            ));
            buckal_note!(
                "supported keys are `ignore_tests`, `patch_fields` and `[patch.version]`, all at the file root"
            );
            std::process::exit(1);
        })
    }

    pub fn repo_config_path() -> PathBuf {
        let buck2_root = get_buck2_root().unwrap_or_exit();
        buck2_root.join("buckal.toml").into()
    }
}

#[cfg(test)]
mod tests {
    use super::RepoConfig;

    /// A key that does not bind must not be shrugged off: `ignore_tests`
    /// defaults to `true`, so a typo silently removes every `rust_test` rule.
    #[test]
    fn test_repo_config_rejects_a_misspelled_key() {
        let err = toml::from_str::<RepoConfig>("ignore_test = false\n")
            .expect_err("a misspelled key must not parse");

        assert!(
            err.to_string().contains("ignore_test"),
            "the error should name the offending key, got: {err}"
        );
    }

    /// The shape both known consumers warn about in a hand-written comment:
    /// settings nested under a `[repo]` section that has never existed.
    #[test]
    fn test_repo_config_rejects_a_nonexistent_section() {
        let err = toml::from_str::<RepoConfig>("[repo]\nignore_tests = false\n")
            .expect_err("an unknown section must not parse");

        assert!(
            err.to_string().contains("repo"),
            "the error should name the offending section, got: {err}"
        );
    }

    #[test]
    fn test_repo_config_still_accepts_every_supported_key() {
        let config: RepoConfig = toml::from_str(
            r#"
                ignore_tests = false
                patch_fields = ["env"]

                [patch.version]
                pyo3 = { from = "0.26.0", to = "0.27.2" }
            "#,
        )
        .expect("the documented keys must keep parsing");

        assert!(!config.ignore_tests);
        assert!(config.patch_fields.contains("env"));
    }

    #[test]
    fn test_repo_config_deserializes_version_patch() {
        let config: RepoConfig = toml::from_str(
            r#"
                [patch.version]
                pyo3 = { from = "0.26.0", to = "0.27.2" }
            "#,
        )
        .expect("failed to deserialize repo config");

        let patch = config
            .patch
            .version
            .get("pyo3")
            .expect("missing pyo3 patch");

        assert_eq!(patch.from, "0.26.0");
        assert_eq!(patch.to, "0.27.2");
    }
}
