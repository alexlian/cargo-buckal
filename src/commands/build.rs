use clap::Parser;

use crate::{
    buck2::Buck2Command,
    buckal_error, buckal_note,
    filter::{FilterCaller, TargetFilter, get_available_targets_in},
    utils::{
        UnwrapOrExit, ensure_prerequisites, get_buck2_root, get_target, is_inside_buck2_project,
        platform_exists, validate_target_triple,
    },
    workspace,
};

#[derive(Parser, Debug)]
pub struct BuildArgs {
    /// Build optimized artifacts with the release profile
    #[arg(short, long)]
    pub release: bool,

    /// Use verbose output (`-vv` very verbose output)
    #[arg(short, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Package(s) to build (defaults to the package in the current directory)
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub package: Vec<String>,

    /// Build all packages in the workspace
    #[arg(long)]
    pub workspace: bool,

    /// Exclude packages from the build (requires `--workspace`)
    #[arg(long, value_name = "SPEC")]
    pub exclude: Vec<String>,

    /// Build only the package’s library
    #[arg(long)]
    pub lib: bool,

    /// Build only the specified binary
    #[arg(long, value_name = "NAME")]
    pub bin: Vec<String>,

    /// Build all binaries
    #[arg(long)]
    pub bins: bool,

    /// Build only the specified example
    #[arg(long, value_name = "NAME")]
    pub example: Vec<String>,

    /// Build all examples
    #[arg(long)]
    pub examples: bool,

    /// Build only the specified test target
    #[arg(long, value_name = "NAME")]
    pub test: Vec<String>,

    /// Build all test targets
    #[arg(long)]
    pub tests: bool,

    /// Build only the specified bench target
    #[arg(long, value_name = "NAME")]
    pub bench: Vec<String>,

    /// Build all bench targets
    #[arg(long)]
    pub benches: bool,

    /// Build all targets
    #[arg(long)]
    pub all_targets: bool,

    /// Build for the target triple (e.g., x86_64-unknown-linux-gnu)
    #[arg(long, value_name = "TRIPLE", conflicts_with = "target_platforms")]
    pub target: Option<String>,

    /// Build for the target platform (passed to buck2 `--target-platforms`)
    #[arg(long, value_name = "PLATFORM", conflicts_with = "target")]
    pub target_platforms: Option<String>,

    /// Forward a raw argument to `buck2` (repeatable). Applied after buckal's
    /// computed flags so user values win on conflict.
    #[arg(long = "buck2-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub buck2_arg: Vec<String>,
}

pub fn execute(args: &BuildArgs) {
    // Ensure all prerequisites are installed before proceeding
    ensure_prerequisites().unwrap_or_exit();

    // Check if the current directory is a valid Buck2 package
    is_inside_buck2_project().unwrap_or_exit();

    if args.verbose > 2 {
        buckal_error!("maximum verbosity");
        std::process::exit(1);
    }

    // Get the root directory of the Buck2 project
    let buck2_root = get_buck2_root().unwrap_or_exit_ctx("failed to get Buck2 project root");
    let cwd = std::env::current_dir().unwrap_or_exit_ctx("failed to get current directory");
    let mut relative = cwd
        .strip_prefix(&buck2_root)
        .unwrap_or_exit_ctx("build command should invoke inside a Buck2 project.")
        .to_string_lossy()
        .into_owned();

    // Normalize path separators for Buck2 (always use forward slashes)
    relative = relative.replace('\\', "/");

    // Construct the target filter based on provided arguments
    let target_filter = TargetFilter::from_raw_arguments(
        args.lib,
        args.bin.clone(),
        args.bins,
        args.test.clone(),
        args.tests,
        args.example.clone(),
        args.examples,
        args.bench.clone(),
        args.benches,
        args.all_targets,
        FilterCaller::Build,
    )
    .unwrap_or_exit();

    let scope = workspace::resolve_scope(&args.package, args.workspace, &args.exclude, &relative)
        .unwrap_or_exit();
    let available_targets = get_available_targets_in(&scope).unwrap_or_exit();

    let target_platforms = if let Some(triple) = &args.target {
        // Validate the target triple and get the corresponding platform
        match validate_target_triple(triple) {
            Ok(platform) => Some(platform),
            Err(e) => {
                buckal_error!(e);
                std::process::exit(1);
            }
        }
    } else if let Some(platform) = &args.target_platforms {
        Some(platform.clone())
    } else {
        let platform = format!("//platforms:{}", get_target());
        if platform_exists(&platform) {
            Some(platform)
        } else {
            None
        }
    };

    let mut buck2_cmd = Buck2Command::build().verbosity(args.verbose);
    if args.release {
        buck2_cmd = buck2_cmd.arg("-m").arg("release");
    }
    if let Some(platform) = &target_platforms {
        buck2_cmd = buck2_cmd.arg("--target-platforms").arg(platform);
    }
    for raw in &args.buck2_arg {
        buck2_cmd = buck2_cmd.arg(raw);
    }

    let mut target_specified = false;

    // Add build targets to the command based on the filter
    for target in &available_targets {
        if target_filter.target_run(target) {
            buck2_cmd = buck2_cmd.arg(target.label());
            target_specified = true;
        }
    }

    if !target_specified {
        buckal_error!("all targets filtered out, nothing to build");
        buckal_note!(
            "please check the filter arguments and ensure if there are any buildable targets in current directory"
        );
        std::process::exit(1);
    }

    match buck2_cmd.status() {
        Ok(status) if status.success() => {}
        _ => std::process::exit(1),
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{BuckalSubCommands, Cli, Commands};

    use clap::Parser;

    fn build_args(argv: &[&str]) -> crate::commands::build::BuildArgs {
        let cli = Cli::try_parse_from(argv).expect("failed to parse build args");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Build(build_args)) => build_args,
                other => panic!("expected build subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_build_accepts_package_short_and_long() {
        let args = build_args(&["cargo", "buckal", "build", "-p", "mm_core"]);
        assert_eq!(args.package, vec!["mm_core"]);
        assert!(!args.workspace);

        let args = build_args(&["cargo", "buckal", "build", "--package", "mm_core"]);
        assert_eq!(args.package, vec!["mm_core"]);
    }

    #[test]
    fn cli_build_accepts_repeated_package() {
        let args = build_args(&["cargo", "buckal", "build", "-p", "a", "-p", "b"]);
        assert_eq!(args.package, vec!["a", "b"]);
    }

    #[test]
    fn cli_build_accepts_repeated_buck2_arg() {
        let args = build_args(&[
            "cargo",
            "buckal",
            "build",
            "--buck2-arg=--show-json-output",
            "--buck2-arg",
            "--show-output",
        ]);
        assert_eq!(args.buck2_arg, vec!["--show-json-output", "--show-output"]);
    }

    #[test]
    fn cli_build_accepts_workspace_and_exclude() {
        let args = build_args(&[
            "cargo",
            "buckal",
            "build",
            "--workspace",
            "--exclude",
            "mm_ai",
        ]);
        assert!(args.workspace);
        assert_eq!(args.exclude, vec!["mm_ai"]);
    }
}
