# cargo-buckal

Seamlessly build Cargo projects with Buck2.

![demo](docs/demo.gif)

## Install

You can install the latest stable release from crates.io:

```bash
cargo install cargo-buckal
```

Or, to install the latest development version from the active repository:

```bash
cargo install --git https://github.com/buck2hub/cargo-buckal.git
```

> [!NOTE]
>
> Buckal requires [Buck2](https://buck2.build/). Please ensure it's installed on your system before proceeding.

## Usage

Run `cargo buckal --help` for more information, and visit https://buck2hub.com/docs for comprehensive documentation.

Common commands:

- `cargo buckal init|new`: Create a new package or a Buck2 project in the directory.
- `cargo buckal migrate`: Migrate an existing Cargo project to Buck2 (generate/update BUCK files).
- `cargo buckal add|remove|update|patch`: Manage dependencies, applying the changes to both `Cargo.toml`/`buckal.toml` and `BUCK` files.
- `cargo buckal build`: Build the current package with Buck2.
- `cargo buckal test`: Compile and execute unit and integration tests with Buck2.
- `cargo buckal clean`: Remove `buck-out` directory.

## Migrate existing Cargo projects

For any Cargo project that builds successfully, you can migrate to Buck2 with zero configuration by running the following command in a valid directory (one containing `Cargo.toml`). Buckal will automatically initialize the Buck2 project configuration and convert the Cargo dependency graph into `BUCK` files.

```bash
cargo buckal migrate --init <repo_root>
```

This is equivalent to running `cargo buckal init --repo` at `<repo_root>` followed by `cargo buckal migrate` in the current directory.

## Supported platforms

Platform-aware dependency mapping and bundled sample platforms target these triples:

| | x86_64 | arm64 |
|---|---|---|
| Linux | `x86_64-unknown-linux-gnu` | `aarch64-unknown-linux-gnu` |
| Windows | `x86_64-pc-windows-msvc` | `aarch64-pc-windows-msvc` |
| macOS | `x86_64-apple-darwin` | `aarch64-apple-darwin` |

## Multi-platform builds

Buckal preserves platform-conditional Cargo dependencies by emitting `os_deps`/`os_named_deps`, so the same generated BUCK files can be built for different target platforms without regenerating on each host.

Keys are OS names (`linux`) or OS/CPU pairs (`linux-arm64`) — the latter for dependencies conditional on *(arch, os)*, such as `cpufeatures`' `libc` under `cfg(all(target_arch = "aarch64", target_os = "linux"))`. See [docs/multi-platform.md](docs/multi-platform.md); note that your platform definitions must declare a CPU constraint.

See https://buck2hub.com/docs/multi-platform.

## Configuration

You can configure cargo-buckal by creating a configuration file at `~/.config/buckal/config.toml`.

### Custom Buck2 Binary Path

If you have buck2 installed in a custom location, you can specify the path:

```toml
buck2_binary = "/path/to/your/buck2"
```

If no configuration file exists, cargo-buckal will use `buck2` (searches your PATH).

`buckal.toml` is the repo-local configuration, read from the Buck2 project root. Every key lives at
the file root — there is no enclosing section — and an unrecognised key is an error rather than a
silent fallback, because the defaults are not neutral.

```toml
# Generate `rust_test` targets. Defaults to `true`, meaning no test targets
# are generated at all: no library `unittest`, no per-binary `<bin>-unittest`,
# no integration tests. Set it to false to get them.
ignore_tests = false

# Fields to preserve in existing BUCK rules when regenerating, so hand-written
# additions survive a `migrate`. Empty by default.
patch_fields = ["env"]

# Redirect dependency labels from one resolved version to another.
[patch.version]
pyo3 = { from = "0.26.0", to = "0.27.2" }
```

You can also write `[patch.version]` entries with `cargo buckal patch pyo3@0.27.2`.

### Test targets

With `ignore_tests = false`, `migrate` mirrors Cargo's own test layout:

| Cargo | generated rule |
| --- | --- |
| `src/lib.rs` `#[cfg(test)]` | `rust_test` named `unittest` |
| each `src/bin/*.rs` (or `[[bin]]`) | `rust_test` named `<bin>-unittest` |
| each `tests/*.rs` | one `rust_test` per file |

A binary gets its rule whenever Cargo's per-target `test` flag is on, which is
the default — not when the file happens to contain `#[cfg(test)]`. This matches
`cargo test`, which builds and runs a harness for every binary and reports
`0 passed` for one with no tests. Keying off the presence of test code instead
would miss tests declared in a sibling file (`mod tests;`), behind `cfg_attr`,
or produced by a macro, and would silently generate nothing for them — a suite
that passes over tests that were never compiled is worse than one that is
honestly empty.

The cost is one extra compile and link per binary, so if a binary will never
have unit tests, opt it out in `Cargo.toml` rather than working around it:

```toml
[[bin]]
name = "scale_seed"
path = "src/bin/scale_seed.rs"
test = false
```

`cargo` and `cargo buckal` both honour that, so the two lanes stay in step.

Note that `build`, `check` and `clippy` never build test targets — only
`cargo buckal test`, and any command given `--tests` or `--all-targets`.

## Pre-commit Hooks

This project uses [prek](https://github.com/j178/prek) for pre-commit hooks (configured in `.pre-commit-config.yaml`).
Install `prek` following the project instructions, then set up the git hooks:

```
prek install
```

To run hooks on all files at any time:

```
prek run --all-files
```

## Repos using cargo-buckal

- [rk8s-dev/rk8s](https://github.com/rk8s-dev/rk8s): A lightweight Kubernetes-compatible container orchestration system written in Rust.
- [web3infra-foundation/libra](https://github.com/web3infra-foundation/libra): High-performance reimplementation and extension of the core Git engine in Rust, focused on foundational VCS primitives and customizable storage semantics compatible with Git workflows.
- [web3infra-foundation/git-internal](https://github.com/web3infra-foundation/git-internal): Internal Git infrastructure, experiments, and foundational components for Git-compatible monorepo systems.
