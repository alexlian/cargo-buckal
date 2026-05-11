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
pub struct CheckArgs {
    /// Check optimized artifacts with the release profile
    #[arg(short, long)]
    pub release: bool,

    /// Use verbose output (`-vv` very verbose output)
    #[arg(short, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Package(s) to check (defaults to the package in the current directory)
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub package: Vec<String>,

    /// Check all packages in the workspace
    #[arg(long)]
    pub workspace: bool,

    /// Exclude packages from the check (requires `--workspace`)
    #[arg(long, value_name = "SPEC")]
    pub exclude: Vec<String>,

    /// Check the package's library
    #[arg(long)]
    pub lib: bool,

    /// Check the specified binary
    #[arg(long, value_name = "NAME")]
    pub bin: Vec<String>,

    /// Check all binary targets
    #[arg(long)]
    pub bins: bool,

    /// Check the specified example
    #[arg(long, value_name = "NAME")]
    pub example: Vec<String>,

    /// Check all example targets
    #[arg(long)]
    pub examples: bool,

    /// Check the specified test target
    #[arg(long, value_name = "NAME")]
    pub test: Vec<String>,

    /// Check all test targets
    #[arg(long)]
    pub tests: bool,

    /// Check the specified bench target
    #[arg(long, value_name = "NAME")]
    pub bench: Vec<String>,

    /// Check all bench targets
    #[arg(long)]
    pub benches: bool,

    /// Check all targets
    #[arg(long)]
    pub all_targets: bool,

    /// Check for the target triple (e.g., x86_64-unknown-linux-gnu)
    #[arg(long, value_name = "TRIPLE", conflicts_with = "target_platforms")]
    pub target: Option<String>,

    /// Check for the target platform (passed to buck2 `--target-platforms`)
    #[arg(long, value_name = "PLATFORM", conflicts_with = "target")]
    pub target_platforms: Option<String>,
}

pub fn execute(args: &CheckArgs) {
    ensure_prerequisites().unwrap_or_exit();
    is_inside_buck2_project().unwrap_or_exit();

    if args.verbose > 2 {
        buckal_error!("maximum verbosity");
        std::process::exit(1);
    }

    let buck2_root = get_buck2_root().unwrap_or_exit_ctx("failed to get Buck2 project root");
    let cwd = std::env::current_dir().unwrap_or_exit_ctx("failed to get current directory");
    let mut relative = cwd
        .strip_prefix(&buck2_root)
        .unwrap_or_exit_ctx("check command should invoke inside a Buck2 project")
        .to_string_lossy()
        .into_owned();

    // Normalize path separators for Buck2 (always use forward slashes)
    relative = relative.replace('\\', "/");

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

    let mut target_specified = false;

    // Add [check] sub-targets to the command based on the filter
    for target in &available_targets {
        if target_filter.target_run(target) {
            buck2_cmd = buck2_cmd.arg(format!("{}[check]", target.label()));
            target_specified = true;
        }
    }

    if !target_specified {
        buckal_error!("all targets filtered out, nothing to check");
        buckal_note!(
            "please check the filter arguments and ensure if there are any checkable targets in current directory"
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
    use crate::utils::validate_target_triple;

    use clap::Parser;

    #[test]
    fn cli_check_default() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "check"])
            .expect("failed to parse bare check command");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Check(check_args)) => {
                    assert!(!check_args.release);
                    assert_eq!(check_args.verbose, 0);
                    assert!(!check_args.lib);
                    assert!(check_args.bin.is_empty());
                    assert!(!check_args.all_targets);
                    assert!(check_args.target.is_none());
                }
                other => panic!("expected check subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_check_accepts_target() {
        let cli = Cli::try_parse_from([
            "cargo",
            "buckal",
            "check",
            "--target",
            "x86_64-unknown-linux-gnu",
        ])
        .expect("failed to parse check --target");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Check(check_args)) => {
                    assert_eq!(
                        check_args.target.as_deref(),
                        Some("x86_64-unknown-linux-gnu")
                    );
                }
                other => panic!("expected check subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_check_rejects_target_and_target_platforms() {
        let result = Cli::try_parse_from([
            "cargo",
            "buckal",
            "check",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--target-platforms",
            "//platforms:x86_64-unknown-linux-gnu",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_check_invalid_target_fails_validation() {
        let cli =
            Cli::try_parse_from(["cargo", "buckal", "check", "--target", "not-a-real-target"])
                .expect("failed to parse check with invalid target");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Check(check_args)) => {
                    let err = validate_target_triple(check_args.target.as_deref().unwrap())
                        .expect_err("expected invalid target triple to fail");
                    assert!(err.to_string().contains("not a valid rustc target"));
                }
                other => panic!("expected check subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_check_with_release() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "check", "--release"])
            .expect("failed to parse check --release");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Check(check_args)) => {
                    assert!(check_args.release);
                }
                other => panic!("expected check subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_check_with_lib_and_bins() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "check", "--lib", "--bins"])
            .expect("failed to parse check --lib --bins");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Check(check_args)) => {
                    assert!(check_args.lib);
                    assert!(check_args.bins);
                }
                other => panic!("expected check subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_check_accepts_package_and_workspace() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "check", "-p", "mm_core"])
            .expect("failed to parse check -p");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Check(check_args)) => {
                    assert_eq!(check_args.package, vec!["mm_core"]);
                    assert!(!check_args.workspace);
                }
                other => panic!("expected check subcommand, got {other:?}"),
            },
        }

        let cli = Cli::try_parse_from(["cargo", "buckal", "check", "--workspace"])
            .expect("failed to parse check --workspace");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Check(check_args)) => assert!(check_args.workspace),
                other => panic!("expected check subcommand, got {other:?}"),
            },
        }
    }
}
