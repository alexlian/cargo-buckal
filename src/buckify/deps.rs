use std::{collections::BTreeSet as Set, path::PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    buck::{CargoTargetKind, RustRule},
    buckal_note, buckal_warn,
    buckify::actions::is_third_party,
    context::BuckalContext,
    platform::{
        Arch, Os, TargetPlatform, platform_is_target_only, supported_platforms, target_platforms,
    },
    resolve::{BuckalDep, BuckalNode, BuckalTarget, is_lib_like},
    utils::{get_buck2_root, get_vendor_path_relative},
};

use cargo_metadata::{DependencyKind, TargetKind};

/// Check if a dependency kind matches the expected target kind.
pub(super) fn dep_kind_matches(target_kind: CargoTargetKind, dep_kind: DependencyKind) -> bool {
    match target_kind {
        CargoTargetKind::CustomBuild => dep_kind == DependencyKind::Build,
        // Cargo test targets can depend on both dev-deps and regular deps.
        // NOTE: Because dev deps are scoped to test targets here, dev-dependency
        // cycles *could* lower to acyclic Buck2 graphs. However, resolve.rs
        // currently rejects all dev-dependency cycles at DAG construction time.
        // See resolve.rs from_metadata() Pass 2 if relaxing that restriction.
        CargoTargetKind::Test => {
            dep_kind == DependencyKind::Development || dep_kind == DependencyKind::Normal
        }
        _ => dep_kind == DependencyKind::Normal,
    }
}

fn get_lib_targets(node: &BuckalNode) -> Vec<&BuckalTarget> {
    node.targets
        .iter()
        .filter(|t| t.kind.iter().any(is_lib_like))
        .collect()
}

fn resolve_first_party_label(dep_node: &BuckalNode) -> Result<String> {
    let buck2_root = get_buck2_root().context("failed to get buck2 root")?;
    let manifest_path = PathBuf::from(dep_node.manifest_path.as_str());
    let manifest_dir = manifest_path
        .parent()
        .context("manifest_path should always have a parent directory")?;
    let relative_path = manifest_dir
        .strip_prefix(&buck2_root)
        .with_context(|| {
            format!(
                "dependency manifest dir `{}` is not under Buck2 root `{}`",
                manifest_dir.display(),
                buck2_root
            )
        })?
        .to_string_lossy()
        // Normalize path separators for Buck2 (always use forward slashes)
        .replace('\\', "/");

    let dep_bin_targets: Vec<_> = dep_node
        .targets
        .iter()
        .filter(|t| t.kind.contains(&TargetKind::Bin))
        .collect();

    let dep_lib_targets = get_lib_targets(dep_node);

    if dep_lib_targets.len() != 1 {
        bail!(
            "Expected exactly one library target for dependency {}, but found {}",
            dep_node.name,
            dep_lib_targets.len()
        );
    }

    let buckal_name = resolve_buckal_name(&dep_bin_targets, &dep_lib_targets);

    Ok(format!("//{relative_path}:{buckal_name}"))
}

fn resolve_buckal_name(
    dep_bin_targets: &[&BuckalTarget],
    dep_lib_targets: &[&BuckalTarget],
) -> String {
    if dep_bin_targets
        .iter()
        .any(|b| b.name == dep_lib_targets[0].name)
    {
        format!("{}-lib", dep_lib_targets[0].name)
    } else {
        dep_lib_targets[0].name.to_owned()
    }
}

fn resolve_dep_label(
    dep: &BuckalDep,
    dep_node: &BuckalNode,
    ctx: &BuckalContext,
) -> Result<(String, Option<String>)> {
    let dep_node = ctx.patched_node(dep_node)?;
    let dep_package_name = dep_node.name.to_string();
    let is_renamed = dep.name != dep_package_name.replace("-", "_");
    let alias = if is_renamed {
        Some(dep.name.clone())
    } else {
        None
    };

    if !is_third_party(dep_node) {
        let label = resolve_first_party_label(dep_node).with_context(|| {
            format!(
                "failed to resolve first-party label for `{}`",
                dep_node.name
            )
        })?;
        Ok((label, alias))
    } else {
        // third-party dependency
        Ok((
            format!(
                "//{}:{}",
                get_vendor_path_relative(&dep_node.package_id)?,
                dep_node.name
            ),
            alias,
        ))
    }
}

/// The `os_deps` / `os_named_deps` keys under which a dependency matching
/// `matched` must be recorded.
///
/// A key is either an OS name (`linux`) or an OS/CPU pair (`linux-arm64`). Per
/// OS, if the dependency applies to *every* supported CPU of that OS we emit the
/// bare OS key — which is what every non-arch-conditional dependency does, so
/// their generated output is unchanged. Otherwise we emit a refined key per
/// matching CPU, which Buck2's select refinement prefers over the bare OS key.
///
/// `supported` is passed in rather than read from [`supported_platforms`] so this
/// stays a pure function (and so the tests don't need `rustc`).
fn platform_keys(matched: &Set<TargetPlatform>, supported: &Set<TargetPlatform>) -> Set<String> {
    let mut keys = Set::new();
    for os in matched.iter().map(|p| p.os).collect::<Set<Os>>() {
        let matched_archs: Set<Arch> = matched
            .iter()
            .filter(|p| p.os == os)
            .map(|p| p.arch)
            .collect();
        let supported_archs: Set<Arch> = supported
            .iter()
            .filter(|p| p.os == os)
            .map(|p| p.arch)
            .collect();

        if matched_archs == supported_archs {
            keys.insert(os.key().to_owned());
        } else {
            for arch in matched_archs {
                keys.insert(TargetPlatform { os, arch }.key());
            }
        }
    }
    keys
}

/// Insert a dependency label into `rust_rule` in the appropriate attribute.
///
/// `target` is the Buck label we want the rule to depend on. If `alias` is `Some`, the
/// dependency is recorded as a *named* dependency (used for renamed crates); otherwise it is
/// recorded as an unnamed dependency.
///
/// # Platforms
///
/// `platforms` controls whether the dependency is unconditional or platform-specific:
/// - `None` means the dependency applies on all platforms and is inserted into `deps` or
///   `named_deps`.
/// - `Some(keys)` means the dependency is conditional and is inserted into `os_deps` or
///   `os_named_deps` under each platform key in `keys` (see [`platform_keys`]).
///
/// # Conflict handling
///
/// - For unconditional named dependencies (`named_deps`), if an alias is encountered more than
///   once with different targets, we emit a warning and keep the first value.
/// - For platform-specific named dependencies (`os_named_deps`), an alias may map to only one
///   target per platform key. Conflicting targets for the same `(alias, key)` are treated as an
///   error.
fn insert_dep(
    rust_rule: &mut dyn RustRule,
    target: &str,
    alias: Option<&str>,
    platforms: Option<&Set<String>>,
) -> Result<()> {
    if let Some(platforms) = platforms {
        for os_key in platforms {
            let os_key = os_key.to_owned();
            if let Some(alias) = alias {
                let entries = rust_rule
                    .os_named_deps_mut()
                    .entry(alias.to_owned())
                    .or_default();

                if let Some(existing) = entries.get(&os_key) {
                    if existing != target {
                        bail!(
                            "os_named_deps alias '{}' had conflicting targets for platform key '{}': '{}' vs '{}'",
                            alias,
                            os_key,
                            existing,
                            target
                        );
                    }
                } else {
                    entries.insert(os_key.clone(), target.to_owned());
                }
            } else {
                rust_rule
                    .os_deps_mut()
                    .entry(os_key)
                    .or_default()
                    .insert(target.to_owned());
            }
        }
    } else if let Some(alias) = alias {
        let entry = rust_rule.named_deps_mut().entry(alias.to_owned());
        match entry {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(target.to_owned());
            }
            std::collections::btree_map::Entry::Occupied(o) => {
                if o.get() != target {
                    buckal_warn!(
                        "named_deps alias '{}' had conflicting targets: '{}' vs '{}'",
                        alias,
                        o.get(),
                        target
                    );
                }
            }
        }
    } else {
        rust_rule.deps_mut().insert(target.to_owned());
    }
    Ok(())
}

pub(super) fn set_deps(
    rust_rule: &mut dyn RustRule,
    node: &BuckalNode,
    kind: CargoTargetKind,
    ctx: &BuckalContext,
) -> Result<()> {
    let supported = supported_platforms();

    for (dep, dep_node) in ctx.resolve.deps_of(&node.package_id) {
        let mut unconditional = false;
        let mut matched = Set::<TargetPlatform>::new();
        let mut has_unsupported_platform = false;

        for dk in dep
            .dep_kinds
            .iter()
            .filter(|dk| dep_kind_matches(kind, dk.kind))
        {
            match &dk.target {
                None => unconditional = true,
                Some(platform) => {
                    let targets = target_platforms(platform);
                    if targets.is_empty() {
                        if platform_is_target_only(platform) {
                            has_unsupported_platform = true;
                            continue;
                        }
                        unconditional = true;
                        continue;
                    }
                    matched.extend(targets);
                }
            }
        }

        if !unconditional && matched.is_empty() {
            if has_unsupported_platform {
                buckal_note!(
                    "Dependency '{}' (package '{}') targets only unsupported platforms and will be omitted.",
                    dep.name,
                    dep_node.name
                );
            }
            continue;
        }

        let (target_label, alias) = resolve_dep_label(dep, dep_node, ctx).with_context(|| {
            format!(
                "failed to resolve dependency label for '{}' (package '{}')",
                dep.name, dep_node.name
            )
        })?;

        if unconditional {
            insert_dep(rust_rule, &target_label, alias.as_deref(), None)?;
        } else {
            let keys = platform_keys(&matched, &supported);
            insert_dep(rust_rule, &target_label, alias.as_deref(), Some(&keys))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod platform_key_tests {
    use super::*;
    use crate::platform::{Arch, Os, TargetPlatform};

    fn p(os: Os, arch: Arch) -> TargetPlatform {
        TargetPlatform { os, arch }
    }

    fn all_supported() -> Set<TargetPlatform> {
        [Os::Windows, Os::Macos, Os::Linux]
            .into_iter()
            .flat_map(|os| {
                [Arch::X86_64, Arch::Arm64]
                    .into_iter()
                    .map(move |a| p(os, a))
            })
            .collect()
    }

    fn keys(matched: &[TargetPlatform]) -> Set<String> {
        platform_keys(&matched.iter().copied().collect(), &all_supported())
    }

    /// The common case, and the one that keeps regeneration diffs small: a dep that
    /// applies to every CPU of an OS (`cfg(unix)`, `cfg(windows)`) still emits the
    /// bare OS key, exactly as before arch-awareness.
    #[test]
    fn covers_every_arch_of_an_os_emits_the_bare_os_key() {
        let unix = keys(&[
            p(Os::Linux, Arch::X86_64),
            p(Os::Linux, Arch::Arm64),
            p(Os::Macos, Arch::X86_64),
            p(Os::Macos, Arch::Arm64),
        ]);
        assert_eq!(unix, Set::from(["linux".to_string(), "macos".to_string()]));
    }

    /// `cpufeatures`: `libc` under `cfg(all(target_arch = "aarch64", target_os =
    /// "linux"))`. The whole point — an OS key cannot express this, and emitting
    /// `linux` would wrongly add libc on x86_64.
    #[test]
    fn one_arch_of_an_os_emits_the_refined_key() {
        assert_eq!(
            keys(&[p(Os::Linux, Arch::Arm64)]),
            Set::from(["linux-arm64".to_string()])
        );
    }

    /// A dep can be bare on one OS and refined on another simultaneously —
    /// `cfg(target_arch = "x86_64")` covers both Windows CPUs only if both are
    /// supported, but covers just one CPU on Linux and macOS.
    #[test]
    fn mixed_oses_key_independently() {
        let x86_only = keys(&[
            p(Os::Linux, Arch::X86_64),
            p(Os::Macos, Arch::X86_64),
            p(Os::Windows, Arch::X86_64),
        ]);
        assert_eq!(
            x86_only,
            Set::from([
                "linux-x86_64".to_string(),
                "macos-x86_64".to_string(),
                "windows-x86_64".to_string(),
            ])
        );
    }

    #[test]
    fn no_match_emits_no_keys() {
        assert!(keys(&[]).is_empty());
    }

    /// `supported` is what "covers every arch" is measured against — not the full
    /// table. If `rustc` could not evaluate the arm64 Linux triple, then Linux is
    /// effectively x86_64-only and an x86_64 dep covers it, so the bare key is
    /// correct. Keying it `linux-x86_64` would be wrong: nothing would then match a
    /// plain-`linux` platform.
    #[test]
    fn coverage_is_relative_to_the_evaluable_platforms() {
        let supported = Set::from([
            p(Os::Linux, Arch::X86_64),
            p(Os::Macos, Arch::X86_64),
            p(Os::Macos, Arch::Arm64),
        ]);
        let matched = Set::from([p(Os::Linux, Arch::X86_64)]);
        assert_eq!(
            platform_keys(&matched, &supported),
            Set::from(["linux".to_string()])
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use cargo_metadata::{DependencyKind, Edition, PackageId, camino::Utf8PathBuf};
    use daggy::Dag;

    use crate::{
        buck::{CargoTargetKind, RustLibrary},
        config::{RepoConfig, RepoPatchConfig, VersionPatch},
        context::BuckalContext,
        resolve::{BuckalDep, BuckalDepKind, BuckalResolve, NodeKind},
    };

    fn mock_target(name: &str, kind: TargetKind) -> BuckalTarget {
        BuckalTarget {
            name: name.to_string(),
            kind: vec![kind],
            src_path: cargo_metadata::camino::Utf8PathBuf::from("/tmp/dummy.rs"),
            doctest: true,
            test: true,
        }
    }

    #[test]
    fn test_resolve_buckal_name_with_collision() {
        let lib = mock_target("foo", TargetKind::Lib);
        let bin = mock_target("foo", TargetKind::Bin);

        let lib_targets = vec![&lib];
        let bin_targets = vec![&bin];

        let name = resolve_buckal_name(&bin_targets, &lib_targets);
        assert_eq!(name, "foo-lib");
    }

    #[test]
    fn test_resolve_buckal_name_without_collision() {
        let lib = mock_target("foo", TargetKind::Lib);
        let bin = mock_target("bar", TargetKind::Bin);

        let lib_targets = vec![&lib];
        let bin_targets = vec![&bin];

        let name = resolve_buckal_name(&bin_targets, &lib_targets);
        assert_eq!(name, "foo");
    }

    #[test]
    fn test_set_deps_redirects_to_patched_version() {
        let root_id = PackageId {
            repr: "path+file:///tmp/root#root@0.1.0".to_string(),
        };
        let old_id = PackageId {
            repr: "registry+https://github.com/rust-lang/crates.io-index#foo@0.1.0".to_string(),
        };
        let new_id = PackageId {
            repr: "registry+https://github.com/rust-lang/crates.io-index#foo@0.2.0".to_string(),
        };

        let root_node = BuckalNode {
            package_id: root_id.clone(),
            name: "root".to_string(),
            version: "0.1.0".to_string(),
            features: vec![],
            kind: NodeKind::FirstParty {
                relative_path: "".to_string(),
            },
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from("/tmp/root/Cargo.toml"),
            targets: vec![mock_target("root", TargetKind::Lib)],
            source: None,
            links: None,
            checksum: None,
        };

        let old_node = BuckalNode {
            package_id: old_id.clone(),
            name: "foo".to_string(),
            version: "0.1.0".to_string(),
            features: vec![],
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from("/tmp/vendor/foo/0.1.0/Cargo.toml"),
            targets: vec![mock_target("foo", TargetKind::Lib)],
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            links: None,
            checksum: None,
        };

        let new_node = BuckalNode {
            package_id: new_id.clone(),
            name: "foo".to_string(),
            version: "0.2.0".to_string(),
            features: vec![],
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from("/tmp/vendor/foo/0.2.0/Cargo.toml"),
            targets: vec![mock_target("foo", TargetKind::Lib)],
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            links: None,
            checksum: None,
        };

        let mut dag = Dag::new();
        let root_idx = dag.add_node(root_node);
        let old_idx = dag.add_node(old_node);
        let new_idx = dag.add_node(new_node);

        dag.add_edge(
            root_idx,
            old_idx,
            BuckalDep {
                name: "foo".to_string(),
                dep_kinds: vec![BuckalDepKind {
                    kind: DependencyKind::Normal,
                    target: None,
                }],
            },
        )
        .expect("failed to add edge");

        let mut index_map = HashMap::new();
        index_map.insert(root_id.clone(), root_idx);
        index_map.insert(old_id, old_idx);
        index_map.insert(new_id, new_idx);

        let ctx = BuckalContext {
            root: Some(root_id.clone()),
            resolve: BuckalResolve { dag, index_map },
            workspace_root: Utf8PathBuf::from("/tmp/root"),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig {
                patch: RepoPatchConfig {
                    version: [(
                        "foo".to_string(),
                        VersionPatch {
                            from: "0.1.0".to_string(),
                            to: "0.2.0".to_string(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                },
                ..RepoConfig::default()
            },
        };

        let root = ctx.resolve.get(&root_id).expect("missing root node");
        let mut rule = RustLibrary::default();

        set_deps(&mut rule, root, CargoTargetKind::Lib, &ctx).expect("failed to set deps");

        assert!(
            rule.deps
                .contains("//third-party/rust/crates/foo/0.2.0:foo")
        );
        assert!(
            !rule
                .deps
                .contains("//third-party/rust/crates/foo/0.1.0:foo")
        );
    }
}
