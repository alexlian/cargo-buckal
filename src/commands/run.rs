use clap::Parser;

use crate::{
    buck2::Buck2Command,
    buckal_error, buckal_log,
    filter::{BuckTargetEntry, get_available_targets},
    utils::{
        UnwrapOrExit, ensure_prerequisites, get_buck2_root, get_target, is_inside_buck2_project,
        platform_exists, validate_target_triple,
    },
    workspace,
};

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// Build artifacts in release mode, with optimizations
    #[arg(short, long)]
    pub release: bool,

    /// Use verbose output (`-vv` very verbose output)
    #[arg(short, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Package whose binary to run (defaults to the package in the current directory)
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub package: Option<String>,

    /// Run the specified binary
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Run the specified example
    #[arg(long, value_name = "NAME")]
    pub example: Option<String>,

    /// Build for the target triple (e.g., x86_64-unknown-linux-gnu)
    #[arg(long, value_name = "TRIPLE", conflicts_with = "target_platforms")]
    pub target: Option<String>,

    /// Build for the target platform (passed to buck2 `--target-platforms`)
    #[arg(long, value_name = "PLATFORM", conflicts_with = "target")]
    pub target_platforms: Option<String>,

    /// Arguments to pass to the binary
    #[arg(last = true)]
    pub args: Vec<String>,
}

pub fn execute(args: &RunArgs) {
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
        .unwrap_or_exit_ctx("run command should invoke inside a Buck2 project")
        .to_string_lossy()
        .into_owned();

    // Normalize path separators for Buck2 (always use forward slashes)
    relative = relative.replace('\\', "/");

    // `-p <pkg>` overrides the cwd-implied package with the named workspace member.
    if let Some(pkg) = &args.package {
        relative = workspace::resolve_member_path(pkg).unwrap_or_exit();
    }

    // Resolve the single target to run
    let target = resolve_run_target(args, &relative);

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

    let mut buck2_cmd = Buck2Command::run().arg(&target).verbosity(args.verbose);
    if args.release {
        buck2_cmd = buck2_cmd.arg("-m").arg("release");
    }
    if let Some(platform) = &target_platforms {
        buck2_cmd = buck2_cmd.arg("--target-platforms").arg(platform);
    }

    // Pass through arguments to the binary
    if !args.args.is_empty() {
        buck2_cmd = buck2_cmd.arg("--");
        for arg in &args.args {
            buck2_cmd = buck2_cmd.arg(arg);
        }
    }

    let result = buck2_cmd.status();
    match result {
        Ok(status) if status.success() => {
            buckal_log!("Finished", &target);
        }
        Ok(status) => {
            std::process::exit(status.code().unwrap_or(1));
        }
        Err(e) => {
            buckal_error!(format!(
                "failed to execute buck2 run for target {}:\n {}",
                target, e
            ));
            std::process::exit(1);
        }
    }
}

/// Resolve which single target to run
fn resolve_run_target(args: &RunArgs, relative: &str) -> String {
    if let Some(example_name) = &args.example {
        return format!("//{relative}:{example_name}");
    }

    let available_targets = get_available_targets(relative).unwrap_or_exit();

    let binaries: Vec<_> = available_targets
        .iter()
        .filter(|t| t.kind() == cargo_metadata::TargetKind::Bin)
        .collect();

    select_binary(&binaries, args.bin.as_deref())
}

/// Select which binary to run from the filtered list.
///
/// If `bin_name` is provided, find the matching binary by name.
/// Otherwise, there must be exactly one binary target.
fn select_binary(binaries: &[&BuckTargetEntry], bin_name: Option<&str>) -> String {
    if let Some(name) = bin_name {
        let matched: Vec<_> = binaries.iter().filter(|e| e.name() == name).collect();
        if matched.is_empty() {
            buckal_error!(format!(
                "no bin target named `{}` in the current package",
                name
            ));
            std::process::exit(1);
        }
        return matched[0].label();
    }

    match binaries.len() {
        0 => {
            buckal_error!("no binary targets found in the current package");
            std::process::exit(1);
        }
        1 => binaries[0].label(),
        _ => {
            let names: Vec<_> = binaries.iter().map(|e| e.name()).collect();
            buckal_error!(format!(
                "multiple binary targets found; specify one with `--bin <NAME>`:\n    {}",
                names.join("\n    ")
            ));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{BuckalSubCommands, Cli, Commands};
    use crate::utils::validate_target_triple;

    use clap::Parser;

    #[test]
    fn cli_run_default() {
        let cli =
            Cli::try_parse_from(["cargo", "buckal", "run"]).expect("failed to parse run args");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Run(run_args)) => {
                    assert!(run_args.bin.is_none());
                    assert!(run_args.example.is_none());
                    assert!(!run_args.release);
                    assert!(run_args.target.is_none());
                    assert!(run_args.args.is_empty());
                }
                other => panic!("expected run subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_run_with_bin() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "run", "--bin", "myapp"])
            .expect("failed to parse run --bin args");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Run(run_args)) => {
                    assert_eq!(run_args.bin.as_deref(), Some("myapp"));
                }
                other => panic!("expected run subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_run_with_package() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "run", "-p", "mm_cli"])
            .expect("failed to parse run -p args");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Run(run_args)) => {
                    assert_eq!(run_args.package.as_deref(), Some("mm_cli"));
                }
                other => panic!("expected run subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_run_with_example() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "run", "--example", "demo"])
            .expect("failed to parse run --example args");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Run(run_args)) => {
                    assert_eq!(run_args.example.as_deref(), Some("demo"));
                }
                other => panic!("expected run subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_run_with_release_and_target() {
        let cli = Cli::try_parse_from([
            "cargo",
            "buckal",
            "run",
            "--release",
            "--target",
            "x86_64-unknown-linux-gnu",
        ])
        .expect("failed to parse run --release --target");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Run(run_args)) => {
                    assert!(run_args.release);
                    assert_eq!(run_args.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
                    assert!(run_args.target_platforms.is_none());
                }
                other => panic!("expected run subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_run_rejects_target_and_target_platforms() {
        let result = Cli::try_parse_from([
            "cargo",
            "buckal",
            "run",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--target-platforms",
            "//platforms:x86_64-unknown-linux-gnu",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_run_passthrough_args() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "run", "--", "--flag", "value"])
            .expect("failed to parse run passthrough args");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Run(run_args)) => {
                    assert_eq!(run_args.args, vec!["--flag", "value"]);
                }
                other => panic!("expected run subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_run_invalid_target_fails_validation() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "run", "--target", "not-a-real-target"])
            .expect("failed to parse run with invalid target");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Run(run_args)) => {
                    let err = validate_target_triple(run_args.target.as_deref().unwrap())
                        .expect_err("expected invalid target triple to fail validation");
                    assert!(err.to_string().contains("not a valid rustc target"));
                }
                other => panic!("expected run subcommand, got {other:?}"),
            },
        }
    }
}
