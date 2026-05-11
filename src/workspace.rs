//! Resolution of Cargo workspace members for the `--package` / `--workspace`
//! selection flags shared by `build`, `check`, `clippy`, `run`, and `test`.
//!
//! NOTE: this is a deliberately small shim. Upstream cargo-buckal plans to
//! rework target filtering; when that lands this module should fold into it.

use anyhow::{Result, anyhow, bail};
use cargo_metadata::MetadataCommand;
use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};

use crate::utils::get_buck2_root;

/// A Cargo workspace member located relative to the Buck2 project root.
#[derive(Debug, Clone)]
pub struct WorkspaceMember {
    /// The Cargo package name.
    pub name: String,
    /// Path to the package directory, relative to the Buck2 project root,
    /// using forward slashes. An empty string means the project root itself.
    pub buck_rel_path: String,
}

/// Run `cargo metadata --no-deps` and map every workspace member to its
/// directory relative to the Buck2 project root.
pub fn workspace_members() -> Result<Vec<WorkspaceMember>> {
    let buck2_root = get_buck2_root()?;
    let metadata = MetadataCommand::new().no_deps().exec()?;
    let mut members = Vec::new();
    for package in &metadata.packages {
        // `--no-deps` already restricts `packages` to workspace members, but
        // be defensive in case that behavior ever changes.
        if !metadata.workspace_members.contains(&package.id) {
            continue;
        }
        let manifest_dir = package
            .manifest_path
            .parent()
            .ok_or_else(|| anyhow!("package `{}` has no manifest directory", package.name))?;
        members.push(WorkspaceMember {
            name: package.name.to_string(),
            buck_rel_path: buck_relative_path(&buck2_root, manifest_dir)?,
        });
    }
    Ok(members)
}

fn buck_relative_path(buck2_root: &Utf8PathBuf, dir: &Utf8Path) -> Result<String> {
    let rel = dir.strip_prefix(buck2_root).map_err(|_| {
        anyhow!("package directory `{dir}` is outside the Buck2 project root `{buck2_root}`")
    })?;
    Ok(rel.as_str().replace('\\', "/"))
}

/// Resolve the set of Buck-relative package paths a subcommand should operate
/// on, given the selection flags. When neither `--workspace` nor `--package`
/// is set, falls back to `cwd_relative` (the package implied by the current
/// directory) — preserving the historical single-package behavior.
pub fn resolve_scope(
    packages: &[String],
    workspace: bool,
    exclude: &[String],
    cwd_relative: &str,
) -> Result<Vec<String>> {
    if !workspace && packages.is_empty() {
        if !exclude.is_empty() {
            bail!("`--exclude` can only be used together with `--workspace`");
        }
        return Ok(vec![cwd_relative.to_string()]);
    }
    let members = workspace_members()?;
    select_scope_paths(&members, packages, workspace, exclude)
}

/// Pure selection logic for [`resolve_scope`], split out for unit testing.
fn select_scope_paths(
    members: &[WorkspaceMember],
    packages: &[String],
    workspace: bool,
    exclude: &[String],
) -> Result<Vec<String>> {
    let selected: Vec<&WorkspaceMember> = if workspace {
        members
            .iter()
            .filter(|m| !exclude.iter().any(|e| e == &m.name))
            .collect()
    } else {
        if !exclude.is_empty() {
            bail!("`--exclude` can only be used together with `--workspace`");
        }
        let mut out = Vec::with_capacity(packages.len());
        for name in packages {
            match members.iter().find(|m| &m.name == name) {
                Some(m) => out.push(m),
                None => bail!("package `{name}` not found in the Cargo workspace"),
            }
        }
        out
    };

    if selected.is_empty() {
        bail!("no packages left to operate on after applying `--exclude`");
    }

    // If any selected member sits at the project root, a single `//...`
    // pattern subsumes every other selection.
    if selected.iter().any(|m| m.buck_rel_path.is_empty()) {
        return Ok(vec![String::new()]);
    }

    let mut paths: Vec<String> = selected.iter().map(|m| m.buck_rel_path.clone()).collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Resolve a single named workspace member to its Buck-relative package path.
/// Used by `run`, which always targets exactly one package.
pub fn resolve_member_path(name: &str) -> Result<String> {
    workspace_members()?
        .into_iter()
        .find(|m| m.name == name)
        .map(|m| m.buck_rel_path)
        .ok_or_else(|| anyhow!("package `{name}` not found in the Cargo workspace"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, path: &str) -> WorkspaceMember {
        WorkspaceMember {
            name: name.to_string(),
            buck_rel_path: path.to_string(),
        }
    }

    fn members() -> Vec<WorkspaceMember> {
        vec![
            member("mm_core", "src/core"),
            member("mm_ai", "src/ai"),
            member("mm_server", "src/server"),
        ]
    }

    #[test]
    fn workspace_selects_all_members() {
        let paths = select_scope_paths(&members(), &[], true, &[]).expect("scope");
        assert_eq!(paths, vec!["src/ai", "src/core", "src/server"]);
    }

    #[test]
    fn workspace_honors_exclude() {
        let paths =
            select_scope_paths(&members(), &[], true, &["mm_ai".to_string()]).expect("scope");
        assert_eq!(paths, vec!["src/core", "src/server"]);
    }

    #[test]
    fn package_subset_preserves_request_but_dedups() {
        let paths = select_scope_paths(
            &members(),
            &["mm_server".to_string(), "mm_core".to_string()],
            false,
            &[],
        )
        .expect("scope");
        assert_eq!(paths, vec!["src/core", "src/server"]);
    }

    #[test]
    fn unknown_package_is_an_error() {
        let err = select_scope_paths(&members(), &["nope".to_string()], false, &[])
            .expect_err("unknown package should fail");
        assert!(err.to_string().contains("not found in the Cargo workspace"));
    }

    #[test]
    fn exclude_without_workspace_is_an_error() {
        let err = select_scope_paths(
            &members(),
            &["mm_core".to_string()],
            false,
            &["mm_ai".to_string()],
        )
        .expect_err("exclude without workspace should fail");
        assert!(err.to_string().contains("`--workspace`"));
    }

    #[test]
    fn excluding_everything_is_an_error() {
        let names: Vec<String> = members().iter().map(|m| m.name.clone()).collect();
        let err = select_scope_paths(&members(), &[], true, &names)
            .expect_err("excluding all members should fail");
        assert!(err.to_string().contains("no packages left"));
    }

    #[test]
    fn member_at_root_collapses_to_recursive_pattern() {
        let mut ms = members();
        ms.push(member("mm_workspace", ""));
        let paths = select_scope_paths(&ms, &[], true, &[]).expect("scope");
        assert_eq!(paths, vec![String::new()]);
    }
}
