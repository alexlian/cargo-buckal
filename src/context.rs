use std::collections::HashMap;

use cargo_metadata::{MetadataCommand, Node, Package, PackageId, camino::Utf8PathBuf};
use cargo_util_schemas::{lockfile::TomlLockfile, manifest::TomlManifest};

use crate::{config::RepoConfig, utils::UnwrapOrExit};

pub struct BuckalContext {
    pub nodes_map: HashMap<PackageId, Node>,
    pub packages_map: HashMap<PackageId, Package>,
    pub checksums_map: HashMap<String, String>,
    pub workspace_root: Utf8PathBuf,
    /// Whether first-party crates inherit keys from workspace Cargo.toml
    pub workspace_inherit: bool,
    /// Whether to skip merging manual changes in BUCK files
    pub no_merge: bool,
    /// Repository configuration
    pub repo_config: RepoConfig,
}

impl BuckalContext {
    pub fn new(manifest_path: Option<String>) -> Self {
        let cargo_metadata = if let Some(manifest) = manifest_path {
            MetadataCommand::new()
                .manifest_path(manifest)
                .exec()
                .unwrap()
        } else {
            MetadataCommand::new().exec().unwrap()
        };
        let packages_map = cargo_metadata
            .packages
            .clone()
            .into_iter()
            .map(|p| (p.id.to_owned(), p))
            .collect::<HashMap<_, _>>();
        let resolve = cargo_metadata.resolve.unwrap();
        let nodes_map = resolve
            .nodes
            .into_iter()
            .map(|n| (n.id.to_owned(), n))
            .collect::<HashMap<_, _>>();
        let lock_path = cargo_metadata.workspace_root.join("Cargo.lock");
        let lock_content =
            std::fs::read_to_string(&lock_path).unwrap_or_exit_ctx("failed to read Cargo.lock");
        let lock_file: TomlLockfile =
            toml::from_str(&lock_content).unwrap_or_exit_ctx("failed to parse Cargo.lock");
        let checksums_map = lock_file
            .package
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                p.checksum
                    .map(|checksum| (format!("{}-{}", p.name, p.version), checksum))
            })
            .collect::<HashMap<_, _>>();
        let repo_config = RepoConfig::load();
        let workspace_toml = cargo_metadata.workspace_root.join("Cargo.toml");
        let workspace_content = std::fs::read_to_string(&workspace_toml)
            .unwrap_or_exit_ctx("failed to read workspace Cargo.toml");
        let workspace_manifest: TomlManifest = toml::from_str(&workspace_content)
            .unwrap_or_exit_ctx("failed to parse workspace Cargo.toml");
        let workspace_inherit = workspace_manifest.workspace.is_some()
            && workspace_manifest
                .workspace
                .as_ref()
                .unwrap()
                .package
                .is_some();

        Self {
            nodes_map,
            packages_map,
            checksums_map,
            workspace_root: cargo_metadata.workspace_root.clone(),
            workspace_inherit,
            no_merge: false,
            repo_config,
        }
    }
}
