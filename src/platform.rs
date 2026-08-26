use std::{
    collections::{BTreeSet, HashMap},
    process::Command,
    str::FromStr,
    sync::OnceLock,
};

use bitflags::bitflags;
use cargo_platform::{Cfg, CfgExpr, Platform};

use crate::buckal_warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Os {
    Windows,
    Macos,
    Linux,
}

impl Os {
    pub fn buck_label(self) -> &'static str {
        match self {
            // Use canonical prelude constraint values so selects work with
            // platform definitions like `prelude//os/constraints:linux`.
            Os::Windows => "prelude//os/constraints:windows",
            Os::Macos => "prelude//os/constraints:macos",
            Os::Linux => "prelude//os/constraints:linux",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Os::Windows => "windows",
            Os::Macos => "macos",
            Os::Linux => "linux",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Arch {
    X86_64,
    Arm64,
}

impl Arch {
    pub fn buck_label(self) -> &'static str {
        match self {
            Arch::X86_64 => "prelude//cpu/constraints:x86_64",
            Arch::Arm64 => "prelude//cpu/constraints:arm64",
        }
    }

    /// The Buck2 CPU name, which is *not* the Rust arch name: Rust says
    /// `aarch64`, Buck2 says `arm64`.
    pub fn key(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Arm64 => "arm64",
        }
    }
}

/// A concrete (OS, CPU) target — the granularity Cargo's `cfg` expressions
/// actually discriminate on, and therefore the granularity we have to evaluate
/// at.
///
/// Collapsing this to [`Os`] alone is what made a dependency conditional on
/// *(arch, os)* inexpressible, and — because Linux was represented only by an
/// x86_64 triple — silently dropped `cfg(all(target_arch = "aarch64", target_os
/// = "linux"))` dependencies altogether.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetPlatform {
    pub os: Os,
    pub arch: Arch,
}

impl TargetPlatform {
    const fn new(os: Os, arch: Arch) -> Self {
        Self { os, arch }
    }

    /// The refined `os_deps` key, e.g. `linux-arm64`.
    pub fn key(self) -> String {
        format!("{}-{}", self.os.key(), self.arch.key())
    }
}

/// Platforms used for cfg evaluation: tier-1 hosts, plus the arm64 targets we
/// build for (arm64 Linux cloud VMs, arm64 Windows).
///
/// Every OS must be listed at every CPU we support. A missing entry does not
/// merely lose precision — the cfg expression matches nothing, and the
/// dependency is dropped from the generated BUCK file.
///
/// Ref: https://doc.rust-lang.org/nightly/rustc/platform-support.html
static SUPPORTED_TARGETS: &[(TargetPlatform, &str)] = &[
    (
        TargetPlatform::new(Os::Macos, Arch::Arm64),
        "aarch64-apple-darwin",
    ),
    (
        TargetPlatform::new(Os::Macos, Arch::X86_64),
        "x86_64-apple-darwin",
    ),
    (
        TargetPlatform::new(Os::Windows, Arch::X86_64),
        "x86_64-pc-windows-msvc",
    ),
    (
        TargetPlatform::new(Os::Windows, Arch::Arm64),
        "aarch64-pc-windows-msvc",
    ),
    (
        TargetPlatform::new(Os::Linux, Arch::X86_64),
        "x86_64-unknown-linux-gnu",
    ),
    (
        TargetPlatform::new(Os::Linux, Arch::Arm64),
        "aarch64-unknown-linux-gnu",
    ),
];

/// Cache of `rustc --print=cfg --target <triple>` output for supported triples.
static CFG_CACHE: OnceLock<HashMap<&'static str, Vec<Cfg>>> = OnceLock::new();

/// Executes `rustc --print=cfg --target <triple>` to retrieve the cfg values for a given target triple.
///
/// This function is used to determine the platform-specific configuration flags that Cargo uses
/// to evaluate conditional compilation directives (cfg attributes) for different target platforms.
///
/// # Parameters
///
/// * `triple`: A target triple string (e.g., "x86_64-unknown-linux-gnu") that specifies the
///   platform for which to retrieve cfg values.
///
/// # Returns
///
/// * `Some(Vec<Cfg>)`: Returns the cfg values parsed from rustc's output when the command succeeds.
/// * `None`: Returns None when rustc execution fails, which can happen if:
///   - The target triple is not installed (e.g., missing rust target component)
///   - Rustc is not available in the system PATH
///   - The rustc command fails for any other reason
///
/// # Behavior
///
/// When this function returns `None`, the target triple is excluded from platform matching.
/// This is the expected behavior when a target is not installed, allowing the build system
/// to handle missing targets without failing the entire build process.
///
/// # Examples
///
/// ```ignore
/// let cfgs = get_rustc_cfgs_for_triple("x86_64-unknown-linux-gnu");
/// if let Some(cfg_values) = cfgs {
///     // Use cfg_values for platform matching
/// } else {
///     // Target not available, skip platform matching for this triple
/// }
/// ```
fn get_rustc_cfgs_for_triple(triple: &'static str) -> Option<Vec<Cfg>> {
    match Command::new("rustc")
        .args(["--print=cfg", "--target", triple])
        .output()
    {
        Ok(output) if output.status.success() => {
            let cfgs: Vec<Cfg> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| Cfg::from_str(line).ok())
                .collect();
            Some(cfgs)
        }
        Ok(output) => {
            buckal_warn!(
                "Failed to run `rustc --print=cfg --target {}`: {}",
                triple,
                String::from_utf8_lossy(&output.stderr)
            );
            None
        }
        Err(error) => {
            buckal_warn!(
                "Failed to execute `rustc --print=cfg --target {}`: {}",
                triple,
                error
            );
            None
        }
    }
}

fn cfg_cache() -> &'static HashMap<&'static str, Vec<Cfg>> {
    CFG_CACHE.get_or_init(|| {
        // We spawn one thread per target triple. This is acceptable because:
        // 1. This initialization runs exactly once per program execution (OnceLock).
        // 2. The work is I/O-bound (waiting on rustc subprocess execution), not CPU-bound,
        //    so having more threads than cores improves throughput rather than causing
        //    contention - threads spend most of their time blocked on I/O.
        // 3. The bounded number of targets (tier-1 platforms) keeps thread count reasonable.
        let results = std::thread::scope(|scope| {
            let handles = SUPPORTED_TARGETS
                .iter()
                .map(|(_, triple)| {
                    let triple = *triple;
                    scope.spawn(move || (triple, get_rustc_cfgs_for_triple(triple)))
                })
                .collect::<Vec<_>>();

            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .expect("Thread panicked while querying rustc cfg values. This may indicate rustc is not properly installed or accessible.")
                })
                .collect::<Vec<_>>()
        });

        let mut map = HashMap::new();
        for (triple, cfgs) in results {
            if let Some(cfgs) = cfgs {
                map.insert(triple, cfgs);
            }
        }
        map
    })
}

/// The triples of [`SUPPORTED_TARGETS`], for callers that ask *cargo* about
/// each platform rather than evaluating a cfg expression themselves.
pub fn supported_triples() -> impl Iterator<Item = &'static str> {
    SUPPORTED_TARGETS.iter().map(|(_, triple)| *triple)
}

pub fn buck_labels(oses: &BTreeSet<Os>) -> BTreeSet<String> {
    oses.iter().map(|os| os.buck_label().to_string()).collect()
}

/// The platforms we can actually evaluate: those in [`SUPPORTED_TARGETS`] whose
/// triple `rustc` gave us cfg values for.
///
/// [`target_platforms`] can only ever return a subset of this, so callers that
/// need to ask "does this dependency apply to *every* CPU of this OS?" must
/// compare against this set rather than against [`SUPPORTED_TARGETS`] — else a
/// triple `rustc` skipped would be read as a CPU the dependency doesn't cover.
pub fn supported_platforms() -> BTreeSet<TargetPlatform> {
    let cfgs = cfg_cache();
    SUPPORTED_TARGETS
        .iter()
        .filter(|(_, triple)| cfgs.contains_key(triple))
        .map(|(platform, _)| *platform)
        .collect()
}

/// Returns the set of target platforms that satisfy a Cargo [`Platform`].
///
/// This evaluates `platform` against [`SUPPORTED_TARGETS`] by asking `rustc` for each target's cfg
/// values (`rustc --print=cfg --target <triple>`) and then using [`Platform::matches`] to determine
/// which target triples match.
///
/// # Notes
///
/// - The `rustc` cfg output is cached for the lifetime of the process.
/// - Results depend on which targets `rustc` knows. If it cannot produce cfg output for a triple,
///   that triple is skipped, which can cause this function to return an empty set even when the
///   `Platform` would match on a machine with more targets available. (`--print=cfg` only needs the
///   target *spec*, not an installed std, so in practice this only bites on an unknown triple.)
/// - Named platforms (`Platform::Name`) only match if they exactly equal one of the supported
///   target triples.
pub fn target_platforms(platform: &Platform) -> BTreeSet<TargetPlatform> {
    let cfgs = cfg_cache();
    SUPPORTED_TARGETS
        .iter()
        .filter_map(|(target, triple)| {
            cfgs.get(triple).and_then(|cfgs| {
                if platform.matches(triple, cfgs) {
                    Some(*target)
                } else {
                    None
                }
            })
        })
        .collect()
}

fn cfg_is_target_only(cfg: &Cfg) -> bool {
    match cfg {
        Cfg::Name(name) => matches!(name.as_str(), "windows" | "unix"),
        Cfg::KeyPair(key, _) => matches!(
            key.as_str(),
            "target_arch"
                | "target_os"
                | "target_family"
                | "target_env"
                | "target_vendor"
                | "target_endian"
                | "target_pointer_width"
                | "target_feature"
        ),
    }
}

fn cfg_expr_is_target_only(expr: &CfgExpr) -> bool {
    match expr {
        CfgExpr::Not(inner) => cfg_expr_is_target_only(inner),
        CfgExpr::All(items) | CfgExpr::Any(items) => items.iter().all(cfg_expr_is_target_only),
        CfgExpr::Value(cfg) => cfg_is_target_only(cfg),
        CfgExpr::True | CfgExpr::False => false,
    }
}

pub fn platform_is_target_only(platform: &Platform) -> bool {
    match platform {
        Platform::Name(_) => true,
        Platform::Cfg(expr) => cfg_expr_is_target_only(expr),
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PlatformMask: u32 {
        const WINDOWS = 0b0001;
        const MACOS   = 0b0010;
        const LINUX   = 0b0100;
    }
}

impl PlatformMask {
    pub fn to_oses(self) -> BTreeSet<Os> {
        let mut set = BTreeSet::new();
        if self.contains(Self::WINDOWS) {
            set.insert(Os::Windows);
        }
        if self.contains(Self::MACOS) {
            set.insert(Os::Macos);
        }
        if self.contains(Self::LINUX) {
            set.insert(Os::Linux);
        }
        set
    }
}

static PACKAGE_PLATFORMS: phf::Map<&'static str, PlatformMask> = phf::phf_map! {
    "android_system_properties" => PlatformMask::LINUX,
    "hyper-named-pipe" => PlatformMask::WINDOWS,
    "libredox" => PlatformMask::LINUX,
    "redox_syscall" => PlatformMask::LINUX,
    "system-configuration" => PlatformMask::MACOS,
    "windows-future" => PlatformMask::WINDOWS,
    "windows" => PlatformMask::WINDOWS,
    "winreg" => PlatformMask::WINDOWS,
};

pub fn lookup_platforms(package_name: &str) -> Option<BTreeSet<Os>> {
    PACKAGE_PLATFORMS
        .get(package_name)
        .map(|mask| mask.to_oses())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    fn test_rustc_cfgs_for_triple_with_available_rustc() {
        let cfgs = get_rustc_cfgs_for_triple("x86_64-unknown-linux-gnu").expect(
            "expected `rustc --print=cfg --target x86_64-unknown-linux-gnu` to succeed (rustc missing or target not installed?)",
        );
        assert!(!cfgs.is_empty(), "rustc cfgs should not be empty");

        let rendered_cfgs: BTreeSet<String> = cfgs.iter().map(ToString::to_string).collect();
        let has_name = |name: &str| {
            cfgs.iter()
                .any(|cfg| matches!(cfg, Cfg::Name(n) if n == name))
        };
        let has_key_pair = |key: &str, val: &str| {
            cfgs.iter()
                .any(|cfg| matches!(cfg, Cfg::KeyPair(k, v) if k == key && v == val))
        };

        // Verify essential cfg values for x86_64-unknown-linux-gnu using structured matching.
        assert!(
            has_key_pair("target_arch", "x86_64"),
            "missing `target_arch = \"x86_64\"` in rustc cfgs: {rendered_cfgs:?}"
        );
        assert!(
            has_key_pair("target_os", "linux"),
            "missing `target_os = \"linux\"` in rustc cfgs: {rendered_cfgs:?}"
        );
        assert!(
            has_key_pair("target_env", "gnu"),
            "missing `target_env = \"gnu\"` in rustc cfgs: {rendered_cfgs:?}"
        );
        assert!(
            has_key_pair("target_family", "unix"),
            "missing `target_family = \"unix\"` in rustc cfgs: {rendered_cfgs:?}"
        );
        assert!(
            has_name("unix"),
            "missing `unix` flag in rustc cfgs: {rendered_cfgs:?}"
        );
        assert!(
            has_name("debug_assertions"),
            "missing `debug_assertions` flag in rustc cfgs: {rendered_cfgs:?}"
        );

        // Demonstrate evaluating a compound cfg expression against the active cfg set.
        let expr = cargo_platform::CfgExpr::from_str(
            "all(target_arch = \"x86_64\", target_os = \"linux\", target_env = \"gnu\", unix)",
        )
        .expect("test cfg expression should parse");
        assert!(
            expr.matches(&cfgs),
            "expected cfg expression `{expr}` to match rustc cfgs: {rendered_cfgs:?}"
        );
    }

    #[test]
    fn test_cfg_parsing_direct() {
        // Test the cfg parsing logic directly by simulating rustc output
        // This tests the core logic without relying on external rustc execution
        let test_output = "target_arch=\"x86_64\"\ntarget_os=\"linux\"\ntarget_endian=\"little\"\n";
        let cfgs: Vec<Cfg> = String::from_utf8_lossy(test_output.as_bytes())
            .lines()
            .filter_map(|line| Cfg::from_str(line).ok())
            .collect();

        assert_eq!(cfgs.len(), 3);

        // Test that we can find specific cfgs by checking their string representation
        let target_arch_cfg = cfgs
            .iter()
            .find(|cfg| cfg.to_string().contains("target_arch"));
        assert!(target_arch_cfg.is_some());
        assert!(target_arch_cfg.unwrap().to_string().contains("x86_64"));

        let target_os_cfg = cfgs
            .iter()
            .find(|cfg| cfg.to_string().contains("target_os"));
        assert!(target_os_cfg.is_some());
        assert!(target_os_cfg.unwrap().to_string().contains("linux"));
    }

    #[test]
    fn test_cfg_parsing_boolean() {
        // Test parsing boolean cfg values
        let test_output = "debug_assertions\nverbose_errors\n";
        let cfgs: Vec<Cfg> = String::from_utf8_lossy(test_output.as_bytes())
            .lines()
            .filter_map(|line| Cfg::from_str(line).ok())
            .collect();

        assert_eq!(cfgs.len(), 2);
        assert!(cfgs.iter().any(|cfg| cfg.to_string() == "debug_assertions"));
        assert!(cfgs.iter().any(|cfg| cfg.to_string() == "verbose_errors"));
    }

    #[test]
    fn test_cfg_parsing_invalid_lines() {
        // Test that invalid cfg lines are filtered out
        let test_output = "target_arch=\"x86_64\"\ninvalid_line=bad_value\nrandom text\n";
        let cfgs: Vec<Cfg> = String::from_utf8_lossy(test_output.as_bytes())
            .lines()
            .filter_map(|line| Cfg::from_str(line).ok())
            .collect();

        // Only the valid cfg should be parsed (invalid_line=bad_value should fail)
        assert_eq!(cfgs.len(), 1);
        assert!(
            cfgs.iter()
                .any(|cfg| cfg.to_string().contains("target_arch"))
        );
    }

    #[test]
    fn test_platform_mask_operations() {
        // Test PlatformMask operations
        let mask = PlatformMask::WINDOWS | PlatformMask::LINUX;
        assert!(mask.contains(PlatformMask::WINDOWS));
        assert!(!mask.contains(PlatformMask::MACOS));
        assert!(mask.contains(PlatformMask::LINUX));

        let oses = mask.to_oses();
        let mut expected = BTreeSet::new();
        expected.insert(Os::Windows);
        expected.insert(Os::Linux);
        assert_eq!(oses, expected);
    }

    #[test]
    fn test_os_buck_labels() {
        // Test Os enum methods
        assert_eq!(Os::Windows.buck_label(), "prelude//os/constraints:windows");
        assert_eq!(Os::Macos.buck_label(), "prelude//os/constraints:macos");
        assert_eq!(Os::Linux.buck_label(), "prelude//os/constraints:linux");

        assert_eq!(Os::Windows.key(), "windows");
        assert_eq!(Os::Macos.key(), "macos");
        assert_eq!(Os::Linux.key(), "linux");
    }

    #[test]
    fn test_lookup_platforms() {
        // Test package platform lookup
        let windows_pkgs = lookup_platforms("windows-future").unwrap();
        let mut expected = BTreeSet::new();
        expected.insert(Os::Windows);
        assert_eq!(windows_pkgs, expected);

        let macos_pkgs = lookup_platforms("system-configuration").unwrap();
        let mut expected = BTreeSet::new();
        expected.insert(Os::Macos);
        assert_eq!(macos_pkgs, expected);

        // Test unknown package returns None
        assert!(lookup_platforms("unknown-package").is_none());
    }

    #[test]
    fn test_buck_labels_utility() {
        // Test the buck_labels utility function
        let mut oses = BTreeSet::new();
        oses.insert(Os::Windows);
        oses.insert(Os::Linux);

        let labels = buck_labels(&oses);
        let mut expected = BTreeSet::new();
        expected.insert("prelude//os/constraints:windows".to_string());
        expected.insert("prelude//os/constraints:linux".to_string());
        assert_eq!(labels, expected);
    }

    #[test]
    fn test_supported_targets() {
        // Test that supported targets are defined and non-empty
        assert!(!SUPPORTED_TARGETS.is_empty());

        // Test that each supported target has a valid OS/arch and triple
        for (target, triple) in SUPPORTED_TARGETS {
            assert!(matches!(target.os, Os::Windows | Os::Macos | Os::Linux));
            assert!(matches!(target.arch, Arch::X86_64 | Arch::Arm64));
            assert!(!triple.is_empty());
        }
    }

    /// Every OS must be listed at every arch. A hole here is not a loss of
    /// precision — a cfg expression naming the missing (arch, os) pair matches no
    /// triple at all, and the dependency is silently dropped from the generated
    /// BUCK file. That hole (Linux at x86_64 only) is exactly what made an
    /// aarch64-linux build fail on `cpufeatures` with an unresolved `libc` import.
    #[test]
    fn test_supported_targets_cover_every_os_arch_pair() {
        let listed: BTreeSet<TargetPlatform> = SUPPORTED_TARGETS
            .iter()
            .map(|(target, _)| *target)
            .collect();

        for os in [Os::Windows, Os::Macos, Os::Linux] {
            for arch in [Arch::X86_64, Arch::Arm64] {
                assert!(
                    listed.contains(&TargetPlatform { os, arch }),
                    "SUPPORTED_TARGETS is missing {}-{}; cfg expressions naming that \
                     pair would match nothing and their deps would be dropped",
                    os.key(),
                    arch.key()
                );
            }
        }
    }

    /// The Rust side lowering against six (os, cpu) pairs is only half the
    /// contract: the generated `//platforms:*` package has to offer a
    /// `platform()` target for each of them, or a refined `os_deps` key lowers
    /// to a `select()` branch no platform can match and the deps in it are
    /// dropped with no diagnostic. This drifted once already — the template
    /// carried four of the six, so `macos-x86_64` and `windows-arm64` branches
    /// were unreachable in every generated repo.
    ///
    /// `//platforms/verify_deps.bxl:check` catches this in a real graph; this
    /// test catches it before one is ever generated.
    #[test]
    fn test_platforms_template_covers_every_supported_triple() {
        let template = include_str!("../assets/platforms/BUCK.template");

        for (target, triple) in SUPPORTED_TARGETS {
            let decl = format!("name = \"{triple}\"");
            assert!(
                template.contains(&decl),
                "assets/platforms/BUCK.template has no `platform()` for {triple} \
                 ({}-{}); deps keyed to that pair would lower to an unreachable \
                 select() branch and be silently dropped",
                target.os.key(),
                target.arch.key()
            );
        }
    }

    #[test]
    fn test_supported_targets_have_unique_triples() {
        let triples: BTreeSet<&str> = SUPPORTED_TARGETS.iter().map(|(_, t)| *t).collect();
        assert_eq!(
            triples.len(),
            SUPPORTED_TARGETS.len(),
            "each (os, arch) pair must map to a distinct triple"
        );
    }

    #[test]
    fn test_arch_labels_and_keys() {
        // Buck2's CPU name, not Rust's: `aarch64` in a target triple is `arm64` here.
        assert_eq!(Arch::Arm64.key(), "arm64");
        assert_eq!(Arch::X86_64.key(), "x86_64");
        assert_eq!(Arch::Arm64.buck_label(), "prelude//cpu/constraints:arm64");
        assert_eq!(Arch::X86_64.buck_label(), "prelude//cpu/constraints:x86_64");
    }

    #[test]
    fn test_target_platform_key() {
        assert_eq!(
            TargetPlatform::new(Os::Linux, Arch::Arm64).key(),
            "linux-arm64"
        );
        assert_eq!(
            TargetPlatform::new(Os::Windows, Arch::X86_64).key(),
            "windows-x86_64"
        );
    }
}
