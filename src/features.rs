//! Platform-aware feature narrowing.
//!
//! `cargo metadata`'s `resolve.nodes[].features` is **platform-blind**: it is
//! the union of every feature any platform activates, and `--filter-platform`
//! does not change that. Measured against one workspace of 792 resolve nodes:
//! filtering to `x86_64-unknown-linux-gnu` drops 106 nodes and their dep edges,
//! and leaves the feature set of every one of the 686 survivors *byte
//! identical*. The flag filters what a `cfg` guards; a feature is not guarded
//! by a `cfg`, so it survives.
//!
//! That union is what the emitter writes into a generated BUCK file, and it is
//! wrong in a way that breaks builds rather than merely bloating them. The
//! standing example: `surrealdb-core` declares
//! `[target.'cfg(target_family = "wasm")'.dependencies.jsonwebtoken]` with
//! `features = ["rust_crypto"]`. Nothing ever compiles that arm on Windows or
//! Linux, but `rust_crypto` lands in `jsonwebtoken`'s one `features` list
//! anyway, alongside the `aws_lc_rs` the workspace actually asked for — and
//! jsonwebtoken's provider auto-detect matches no arm with both on, so every
//! JWT operation panics at runtime under buck2 while passing under cargo.
//!
//! The error runs both directions, which is why this is a class and not a
//! crate: `errno`'s `cfg(windows)` arm contributes `Win32_System_Diagnostics`
//! to `windows-sys` in a graph where `errno` is reached only through `rustix`
//! on unix and never compiles on Windows at all. Unreachable nodes contribute
//! too — a package nothing depends on still sits in the metadata resolve, and
//! its feature requests still land in the union.
//!
//! `cargo tree --target <triple>` uses cargo's *platform-aware* resolver, so
//! this module asks it once per supported triple and unions the answers. That
//! union is exactly "what some platform we support actually enables", which is
//! what the emitter should write. Being the union, it is a superset of every
//! individual platform's set, so narrowing to it can never starve one platform
//! to satisfy another.
//!
//! # Narrowing has to keep the graph coherent
//!
//! Features cannot be narrowed in isolation, because the emitter does **not**
//! prune the dep edges a disabled feature gated. Measured the hard way: with
//! `jsonwebtoken`'s `rust_crypto` off, `p384` becomes unreachable — no
//! supported platform resolves it — but `jsonwebtoken`'s BUCK still lists it
//! as a dep, so buck2 still builds it, still with its own un-narrowed features
//! including `ecdh`. Narrow `elliptic-curve` (which *is* reachable) down to
//! its resolved set, and `ecdh` leaves it, and `p384` fails to compile with
//! `could not find 'ecdh' in 'elliptic_curve'`.
//!
//! So a package keeps cargo's full union whenever anything that survives in
//! the emitted graph might still ask for it. Concretely: any package a
//! *unreachable* package depends on, transitively, is pinned to the union.
//! The narrowing then applies only where every possible consumer has also been
//! narrowed — which still covers the case this exists for, because
//! `jsonwebtoken` itself is reachable and nothing unreachable depends on it.
//!
//! A pleasant side effect: the pin covers the `windows-sys` entries too.
//! `quinn-udp` is in the metadata resolve but reachable from nothing, so
//! `windows-sys 0.60.2` keeps the `Win32_Networking_WinSock` that `quinn-udp`
//! asked for, rather than losing it on the word of a resolve no platform
//! performs.
//!
//! Set `BUCKAL_FEATURES_ALL_PLATFORMS=1` to skip the narrowing and emit
//! cargo's raw all-platform union — an escape hatch for bisecting a suspected
//! narrowing bug, not a supported mode.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    process::Command,
};

use cargo_metadata::PackageId;

use crate::{buckal_warn, platform::supported_triples};

/// Feature sets keyed by `(package name, version)`, unioned over supported
/// triples. An empty map means narrowing is disabled or unavailable, and
/// callers must fall back to cargo's union rather than to nothing.
pub type PlatformFeatures = HashMap<(String, String), BTreeSet<String>>;

pub fn narrowing_disabled() -> bool {
    std::env::var_os("BUCKAL_FEATURES_ALL_PLATFORMS").is_some()
}

/// Parse `cargo tree --prefix none --format "{p}|{f}"` output.
///
/// Lines look like `serde v1.0.0|default,derive`, with an optional
/// ` (/path/to/crate)` or ` (registry+...)` source between the version and the
/// separator, and a trailing ` (*)` on a subtree cargo has already printed.
/// One package can appear more than once with *different* feature sets — the
/// resolver splits normal, build and proc-macro units — so entries union
/// rather than overwrite.
fn parse_tree(stdout: &str, into: &mut PlatformFeatures) {
    for raw in stdout.lines() {
        let line = raw
            .trim_end()
            .strip_suffix(" (*)")
            .unwrap_or(raw.trim_end());
        let Some((spec, feats)) = line.rsplit_once('|') else {
            continue;
        };
        // `spec` is `name vVERSION` possibly followed by ` (source)`.
        let spec = match spec.find(" (") {
            Some(idx) => &spec[..idx],
            None => spec,
        };
        let Some((name, version)) = spec.rsplit_once(" v") else {
            continue;
        };
        if name.is_empty() || version.is_empty() {
            continue;
        }
        let entry = into
            .entry((name.to_string(), version.to_string()))
            .or_default();
        entry.extend(
            feats
                .split(',')
                .filter(|f| !f.is_empty())
                .map(str::to_string),
        );
    }
}

fn cargo_tree(triple: &str, manifest_path: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "tree",
        "--target",
        triple,
        // Dev and build edges included: buckal emits test rules (`ignore_tests
        // = false`) and build-script rules, so their features are as
        // load-bearing as the library's.
        "--edges",
        "all",
        "--prefix",
        "none",
        "--format",
        "{p}|{f}",
        "--workspace",
    ]);
    if let Some(manifest) = manifest_path {
        cmd.args(["--manifest-path", manifest]);
    }
    match cmd.output() {
        Ok(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(output) => {
            buckal_warn!(
                "`cargo tree --target {}` failed: {}",
                triple,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            None
        }
        Err(error) => {
            buckal_warn!(
                "failed to execute `cargo tree --target {}`: {}",
                triple,
                error
            );
            None
        }
    }
}

/// Resolve features for every supported triple and union them.
///
/// Returns an empty map if *any* triple fails to resolve. A partial union is
/// the one genuinely dangerous outcome: it looks like a successful narrowing
/// while silently omitting whatever the missing platform needed, which is how
/// a build breaks on the platform nobody is sitting at. Failing open — back to
/// cargo's over-approximating union — costs compile time and nothing else.
pub fn platform_features(manifest_path: Option<&str>) -> PlatformFeatures {
    if narrowing_disabled() {
        return PlatformFeatures::new();
    }

    // One thread per triple: the work is a blocked `cargo tree` subprocess, and
    // the triple count is bounded by SUPPORTED_TARGETS. Same shape as the
    // `rustc --print=cfg` cache in `platform.rs`.
    let results = std::thread::scope(|scope| {
        let handles = supported_triples()
            .map(|triple| scope.spawn(move || (triple, cargo_tree(triple, manifest_path))))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("cargo tree thread panicked"))
            .collect::<Vec<_>>()
    });

    let mut features = PlatformFeatures::new();
    for (triple, stdout) in results {
        let Some(stdout) = stdout else {
            buckal_warn!(
                "feature narrowing disabled: no resolve for {}; falling back to \
                 cargo's all-platform feature union",
                triple
            );
            return PlatformFeatures::new();
        };
        parse_tree(&stdout, &mut features);
    }
    features
}

/// Packages that must keep cargo's full feature union: everything reachable,
/// transitively, from a package no supported platform resolves.
///
/// Such a package is still emitted and still built (the emitter does not prune
/// dep edges), still with its original features, so anything it depends on has
/// to keep the features it was compiled against.
pub fn pinned_to_union(
    nodes_map: &HashMap<PackageId, cargo_metadata::Node>,
    packages_map: &HashMap<PackageId, cargo_metadata::Package>,
    platform_features: &PlatformFeatures,
) -> HashSet<PackageId> {
    let unreachable = |id: &PackageId| match packages_map.get(id) {
        Some(pkg) => {
            !platform_features.contains_key(&(pkg.name.to_string(), pkg.version.to_string()))
        }
        None => true,
    };

    let mut pinned = HashSet::new();
    let mut queue: Vec<PackageId> = nodes_map
        .keys()
        .filter(|id| unreachable(id))
        .cloned()
        .collect();
    while let Some(id) = queue.pop() {
        let Some(node) = nodes_map.get(&id) else {
            continue;
        };
        for dep in &node.deps {
            if pinned.insert(dep.pkg.clone()) {
                queue.push(dep.pkg.clone());
            }
        }
    }
    pinned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> PlatformFeatures {
        let mut out = PlatformFeatures::new();
        parse_tree(text, &mut out);
        out
    }

    #[test]
    fn strips_the_repeat_marker() {
        let out = parse("serde v1.0.0|default,derive\nserde v1.0.0|default,derive (*)\n");
        assert_eq!(
            out[&("serde".to_string(), "1.0.0".to_string())],
            BTreeSet::from(["default".to_string(), "derive".to_string()])
        );
    }

    #[test]
    fn ignores_a_source_suffix() {
        let out = parse("gamma v0.1.0 (/home/x/src/gamma)|default,http\n");
        assert_eq!(
            out[&("gamma".to_string(), "0.1.0".to_string())],
            BTreeSet::from(["default".to_string(), "http".to_string()])
        );
    }

    /// The resolver splits normal / build / proc-macro units, so one package
    /// prints more than once with different sets. Overwriting instead of
    /// unioning would drop whichever unit printed first.
    #[test]
    fn unions_repeated_packages() {
        let out = parse("getrandom v0.4.2|std\ngetrandom v0.4.2|sys_rng\n");
        assert_eq!(
            out[&("getrandom".to_string(), "0.4.2".to_string())],
            BTreeSet::from(["std".to_string(), "sys_rng".to_string()])
        );
    }

    #[test]
    fn handles_an_empty_feature_list() {
        let out = parse("quinn-udp v0.5.14|\n");
        assert!(out[&("quinn-udp".to_string(), "0.5.14".to_string())].is_empty());
    }

    #[test]
    fn ignores_lines_that_are_not_packages() {
        let out = parse("some warning text\n\nserde v1.0.0|default\n");
        assert_eq!(out.len(), 1);
    }
}
