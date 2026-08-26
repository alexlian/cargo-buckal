use clap::Parser;

use crate::{
    buck2::Buck2Command,
    buckal_error, buckal_log, buckal_note, buckal_warn, diagnostics,
    filter::{FilterCaller, TargetFilter, get_available_targets_in},
    utils::{
        UnwrapOrExit, ensure_prerequisites, get_buck2_root, get_target, is_inside_buck2_project,
        platform_exists, validate_target_triple,
    },
    workspace,
};

#[derive(Parser, Debug)]
pub struct ClippyArgs {
    /// Use verbose output (`-vv` very verbose output)
    #[arg(short, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Package(s) to lint (defaults to the package in the current directory)
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub package: Vec<String>,

    /// Lint all packages in the workspace
    #[arg(long)]
    pub workspace: bool,

    /// Exclude packages from linting (requires `--workspace`)
    #[arg(long, value_name = "SPEC")]
    pub exclude: Vec<String>,

    /// Check only this package's library
    #[arg(long)]
    pub lib: bool,

    /// Check only the specified binary
    #[arg(long, value_name = "NAME")]
    pub bin: Vec<String>,

    /// Check all binary targets
    #[arg(long)]
    pub bins: bool,

    /// Check the specified test target
    #[arg(long, value_name = "NAME")]
    pub test: Vec<String>,

    /// Check all test targets
    #[arg(long)]
    pub tests: bool,

    /// Check the specified example
    #[arg(long, value_name = "NAME")]
    pub example: Vec<String>,

    /// Check all example targets
    #[arg(long)]
    pub examples: bool,

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

    /// Extra clippy lint flags passed after `--`
    #[arg(last = true)]
    pub args: Vec<String>,
}

pub fn execute(args: &ClippyArgs) {
    ensure_prerequisites().unwrap_or_exit();
    is_inside_buck2_project().unwrap_or_exit();

    if args.verbose > 2 {
        buckal_error!("maximum verbosity");
        std::process::exit(1);
    }

    // These were accepted and dropped on the floor. Buck2 has no CLI knob for
    // appending rustc flags to arbitrary targets — the prelude's Rust
    // toolchain takes `rustc_flags` as a rule attribute, not from a
    // `read_config` — so there is nothing to forward them to. Refusing is the
    // honest failure: a silent no-op reads as "clippy ran with my lint levels"
    // when it ran without them.
    if !args.args.is_empty() {
        buckal_error!(format!(
            "`cargo buckal clippy -- {}` is not supported: Buck2 has no way to \
             apply extra lint flags to a target at the command line",
            args.args.join(" ")
        ));
        buckal_note!(
            "set the lint levels in `Cargo.toml` (`[lints.clippy]`), or add `rustc_flags` to the Rust toolchain in `toolchains/BUCK`"
        );
        std::process::exit(1);
    }

    let buck2_root = get_buck2_root().unwrap_or_exit_ctx("failed to get Buck2 project root");
    let cwd = std::env::current_dir().unwrap_or_exit_ctx("failed to get current directory");
    let mut relative = cwd
        .strip_prefix(&buck2_root)
        .unwrap_or_exit_ctx("clippy command should invoke inside a Buck2 project")
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
    if let Some(platform) = &target_platforms {
        buck2_cmd = buck2_cmd.arg("--target-platforms").arg(platform);
    }
    // Absolute paths to every `[clippy.json]` we are about to build. Without
    // this the artifacts are produced and never looked at, which is the whole
    // bug: the clippy emit is infallible, so the build succeeding says nothing
    // about what clippy found.
    buck2_cmd = buck2_cmd.arg("--show-full-json-output");

    let mut target_specified = false;

    // Add [clippy.json] sub-targets to the command based on the filter.
    // Buck2's Rust prelude exposes `clippy.json` and `clippy.txt` sub-targets.
    for target in &available_targets {
        if target_filter.target_run(target) {
            buck2_cmd = buck2_cmd.arg(format!("{}[clippy.json]", target.label()));
            target_specified = true;
        }
    }

    if !target_specified {
        buckal_error!("all targets filtered out, nothing to lint");
        buckal_note!(
            "please check the filter arguments and ensure if there are any lintable targets in current directory"
        );
        std::process::exit(1);
    }

    let output = match buck2_cmd.output_capturing_stdout() {
        // A failed build is already reported by Buck2 on the stderr we let
        // through, and leaves us no output map to read.
        Ok(output) if !output.status.success() => std::process::exit(1),
        Ok(output) => output,
        Err(e) => {
            buckal_error!(format!("failed to execute buck2 build: {e}"));
            std::process::exit(1);
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths = diagnostics::output_paths(&stdout).unwrap_or_exit();

    let mut records = Vec::new();
    for path in &paths {
        match std::fs::read_to_string(path) {
            Ok(contents) => records.extend(diagnostics::parse_stream(&contents)),
            Err(e) => buckal_warn!(format!(
                "could not read clippy diagnostics at `{}`: {e}",
                path.display()
            )),
        }
    }

    let (blocks, summary) = diagnostics::render(records);
    for block in &blocks {
        eprintln!("{block}");
    }

    if summary.errors > 0 {
        buckal_error!(format!(
            "clippy found {} error(s) and {} warning(s)",
            summary.errors, summary.warnings
        ));
        std::process::exit(1);
    }
    if summary.warnings > 0 {
        buckal_warn!(format!("clippy found {} warning(s)", summary.warnings));
    } else if summary.is_clean() {
        buckal_log!("Finished", "clippy found no issues");
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{BuckalSubCommands, Cli, Commands};

    use clap::Parser;

    #[test]
    fn cli_clippy_default() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "clippy"])
            .expect("failed to parse clippy args");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Clippy(clippy_args)) => {
                    assert!(!clippy_args.lib);
                    assert!(!clippy_args.bins);
                    assert!(clippy_args.bin.is_empty());
                    assert!(!clippy_args.all_targets);
                    assert!(clippy_args.target.is_none());
                    assert!(clippy_args.args.is_empty());
                    assert_eq!(clippy_args.verbose, 0);
                }
                other => panic!("expected clippy subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_clippy_accepts_target() {
        let cli = Cli::try_parse_from([
            "cargo",
            "buckal",
            "clippy",
            "--target",
            "x86_64-unknown-linux-gnu",
        ])
        .expect("failed to parse clippy --target");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Clippy(clippy_args)) => {
                    assert_eq!(
                        clippy_args.target.as_deref(),
                        Some("x86_64-unknown-linux-gnu")
                    );
                }
                other => panic!("expected clippy subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_clippy_rejects_target_and_target_platforms() {
        let result = Cli::try_parse_from([
            "cargo",
            "buckal",
            "clippy",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--target-platforms",
            "//platforms:x86_64-unknown-linux-gnu",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_clippy_passthrough_args() {
        let cli =
            Cli::try_parse_from(["cargo", "buckal", "clippy", "--", "-W", "clippy::pedantic"])
                .expect("failed to parse clippy passthrough args");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Clippy(clippy_args)) => {
                    assert_eq!(clippy_args.args, vec!["-W", "clippy::pedantic"]);
                }
                other => panic!("expected clippy subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_clippy_with_all_targets() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "clippy", "--all-targets"])
            .expect("failed to parse clippy --all-targets");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Clippy(clippy_args)) => {
                    assert!(clippy_args.all_targets);
                }
                other => panic!("expected clippy subcommand, got {other:?}"),
            },
        }
    }

    #[test]
    fn cli_clippy_accepts_package_and_workspace() {
        let cli = Cli::try_parse_from(["cargo", "buckal", "clippy", "-p", "mm_ai", "--workspace"])
            .expect("failed to parse clippy -p --workspace");
        match cli.command {
            Commands::Buckal(args) => match args.subcommands {
                Some(BuckalSubCommands::Clippy(clippy_args)) => {
                    assert_eq!(clippy_args.package, vec!["mm_ai"]);
                    assert!(clippy_args.workspace);
                }
                other => panic!("expected clippy subcommand, got {other:?}"),
            },
        }
    }
}
