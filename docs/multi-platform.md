# Multi-platform builds

`cargo buckal migrate` can generate BUCK files that work across Linux/macOS/Windows without regenerating on each host. The key idea is to preserve Cargo’s platform-conditional dependencies in the generated rules.

## What gets generated

- `os_deps`: platform-scoped dependencies (e.g., a Windows-only dep lands under `os_deps["windows"]`).
- `os_named_deps`: same as `os_deps`, but for renamed dependencies.
- `compatible_with`: applied to a small allowlist of known OS-only crates to prevent Buck2 from building them on the wrong OS.

## Platform keys

An `os_deps` key is either an **OS name** (`linux`) or an **OS/CPU pair** (`linux-arm64`). The pair form exists because plenty of crates are conditional on *(arch, os)* rather than on the OS alone — `cpufeatures` needs `libc` only under `cfg(all(target_arch = "aarch64", target_os = "linux"))`, which an OS-keyed map cannot express at all.

cargo-buckal emits the bare OS key whenever a dependency applies to **every** supported CPU of that OS (which is the case for almost every crate — `cfg(unix)`, `cfg(windows)` and friends), and a refined key only when the CPUs disagree.

The keys lower to Buck constraint labels: `prelude//os/constraints:{linux,macos,windows}` for the bare form, and `buckal//platforms:{linux,macos,windows}-{x86_64,arm64}` for the refined form. Both can match the same platform; Buck2 resolves that by **refinement**, preferring the `config_setting` whose constraints are a strict superset — so the refined branch wins wherever the platform declares a CPU.

> **Your platforms must declare a CPU constraint.** A platform that sets only an OS constraint cannot match a refined key, so a crate whose deps are arch-split will fall through to the default branch and lose them. The platforms generated under `//platforms:*` all declare one, as does `prelude//platforms:default` (it derives both OS and CPU from the host).

## Supported platforms

Platform-aware dependency mapping and bundled sample platforms target these triples:

| | x86_64 | arm64 |
|---|---|---|
| Linux | `x86_64-unknown-linux-gnu` | `aarch64-unknown-linux-gnu` |
| Windows | `x86_64-pc-windows-msvc` | `aarch64-pc-windows-msvc` |
| macOS | `x86_64-apple-darwin` | `aarch64-apple-darwin` |

Every OS is listed at every CPU deliberately. A hole in this table is not merely a loss of precision: a `cfg` expression naming the missing pair matches *no* triple, so the dependency is dropped from the generated BUCK file entirely and the build fails on that platform with an unresolved import.

The same applies one layer down, and it bites in a way that is easy to miss: `//platforms:*` must offer a `platform()` target for each of the six, or a refined `os_deps` key lowers to a `select()` branch nothing can match and its deps are dropped just as silently. Both halves are enforced — `SUPPORTED_TARGETS` in `src/platform.rs` against the generated template by a unit test, and the lowering itself against a real graph by `//platforms/verify_deps.bxl:check` (below).

## How platform matching works

Cargo encodes target-specific dependencies in `cargo metadata` as platform predicates (for example, `cfg(target_os = "windows")`). During `migrate`, cargo-buckal evaluates each predicate against cached `rustc --print=cfg --target <triple>` snapshots for every supported triple, producing the set of (OS, CPU) platforms it matches — and then reduces that set to the platform keys above.

If a predicate can’t be mapped to any supported platform, cargo-buckal treats the dependency as unconditional by default (to preserve build success).

## Using it

1. Generate BUCK files:

   For first-time setup (initializes Buck2 and generates BUCK files):

   ```bash
   cargo buckal migrate --init
   ```

   For incremental updates (when Buck2 is already initialized):

   ```bash
   cargo buckal migrate
   ```

   This regenerates BUCK files based on current `Cargo.toml`/`Cargo.lock` changes without reinitializing Buck2. Use this after adding/removing dependencies or updating `Cargo.lock`.

   To update the pinned Buckal bundles revision (the `buckal` cell), rerun with:

   ```bash
   cargo buckal migrate --fetch
   ```

2. Build with `cargo buckal build`:

   Without `--target-platforms`, builds for the host platform:

   ```bash
   cargo buckal build //...
   ```

   With `--target-platforms`, builds for a specific target platform:

   ```bash
   cargo buckal build //... --target-platforms //platforms:x86_64-pc-windows-msvc
   ```

   You can also use `buck2 build` directly:

   ```bash
   buck2 build //... --target-platforms //platforms:x86_64-pc-windows-msvc
   ```

   `cargo buckal migrate --init` configures a `buckal` cell (Buckal bundles). The bundles provide sample platforms under `//platforms:*`. You can also use your own platform definitions; any platform you use must include the appropriate OS constraint value (`prelude//os/constraints:windows` in the example above) so `select()` picks up the right `os_deps` branch.

   If you want to use the bundled toolchain config too, point the `toolchains` cell at it in `.buckconfig`:

   ```ini
   [cells]
     toolchains = buckal/toolchains
   ```

3. Verify the dependency matrix without building anything:

   ```bash
   buck2 bxl //platforms/verify_deps.bxl:check
   ```

   Building only proves the platforms you can compile for. This check reads the
   graph instead, so a single host proves the lowering for all six pairs —
   including the ones no CI runner compiles. It fails when:

   - a refined `os_deps` key lowers to a `select()` branch no `//platforms:*`
     target can match (the dep is dropped with no diagnostic), or
   - a refined branch drops the OS-level deps it refines (Buck2 picks exactly
     one branch, so the refined branch must repeat what it refines), or
   - a declared dep does not survive select resolution on its own platform.

   Scope it with `--pattern`, and skip the (slower) configured pass with
   `--configured false`:

   ```bash
   buck2 bxl //platforms/verify_deps.bxl:check -- --pattern //third-party/...
   buck2 bxl //platforms/verify_deps.bxl:check -- --configured false
   ```

   `//platforms/verify_deps.bxl:dump` prints the resolved dep set of every Rust
   target on every platform as JSON, which is what you want when a failure needs
   a diff rather than a message.

4. Validate multi-platform builds by building against multiple target platforms:

   Linux:

   ```bash
   cargo buckal build //... --target-platforms //platforms:x86_64-unknown-linux-gnu
   ```

   Windows:

   ```bash
   cargo buckal build //... --target-platforms //platforms:x86_64-pc-windows-msvc
   ```

   macOS (bundled sample platforms):

   ```bash
   cargo buckal build //... --target-platforms //platforms:aarch64-apple-darwin
   ```

### Skipping tests for cross-compilation

When cross-compiling or when the target binaries cannot run on the host, you can
skip `rust_test` targets by passing `-c cross.skip_test=true`. cargo-buckal
marks generated `rust_test` targets with a `target_compatible_with` constraint
that matches the `//platforms:cross` config setting when this config is set.

Examples:

```bash
cargo buckal test //... --target-platforms //platforms:x86_64-unknown-linux-gnu -c cross.skip_test=true
cargo buckal test //... --target-platforms //platforms:x86_64-pc-windows-msvc -c cross.skip_test=true
```

## Troubleshooting

- If you see warnings about `rustc --print=cfg --target ...` failing, install the missing Rust targets (or expect fewer platform predicates to be mapped).
- If OS-specific deps appear in the default `deps` list, the corresponding predicate likely couldn’t be mapped; rerun with more Rust targets installed.
- If Buck2 fails to parse generated BUCK files due to missing support for `os_deps`/`os_named_deps` (or missing symbols like `rust_test` in `wrapper.bzl`), update the Buckal bundles (try `cargo buckal migrate --fetch`) or pin a bundles revision that supports these attributes.
