# Multi-platform builds

`cargo buckal migrate` can generate BUCK files that work across Linux/macOS/Windows without regenerating on each host. The key idea is to preserve Cargo’s platform-conditional dependencies in the generated rules.

## What gets generated

- `os_deps`: OS-scoped dependencies (e.g., a Windows-only dep lands under `os_deps["windows"]`).
- `os_named_deps`: same as `os_deps`, but for renamed dependencies.
- `compatible_with`: applied to a small allowlist of known OS-only crates to prevent Buck2 from building them on the wrong OS.

The generated rules use canonical Buck prelude OS constraint labels: `prelude//os/constraints:{linux,macos,windows}`.

## Supported platforms

Platform-aware dependency mapping and bundled sample platforms currently target these Rust tier-1
host triples:

- Linux: `x86_64-unknown-linux-gnu`
- Windows: `x86_64-pc-windows-msvc`
- macOS: `aarch64-apple-darwin`

## How platform matching works

Cargo encodes target-specific dependencies in `cargo metadata` as platform predicates (for example, `cfg(target_os = "windows")`). During `migrate`, cargo-buckal maps those predicates to a set of OS keys (`linux`/`macos`/`windows`) by evaluating them against cached `rustc --print=cfg --target <triple>` snapshots for Rust Tier-1 host targets.

If a predicate can’t be mapped to `linux`/`macos`/`windows`, cargo-buckal treats the dependency as unconditional by default (to preserve build success).

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

3. Validate multi-platform builds by building against multiple target platforms:

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

## Windows import-library search paths

A few crates ship a prebuilt import library and print
`cargo:rustc-link-search=native=<their lib/>` from their build script —
`windows_x86_64_msvc`, `windows_x86_64_gnu` and `winapi-x86_64-pc-windows-gnu`.
The `#[link(name = "windows.0.52.0")]` that needs that path is expanded in a
*consumer* crate, so the search path has to reach whatever actually links, not
just the crate that emitted it.

Cargo does this by handing every linking unit the `-L` flags of every build
script in its closure. cargo-buckal approximates it by appending, to the linking
rule's `rustc_flags`, a `select()` that pulls in those crates'
`build-script-run[rustc_flags]` on Windows. The set is collected from the whole
resolve graph rather than from each rule's own closure, so a linking rule may
carry a search path it does not need:

```starlark
rustc_flags = ["@$(location :manifest[env_flags])"] + select({
    "prelude//os/constraints:windows": select({
        "prelude//abi/constraints:gnu": [...],
        "DEFAULT": [
            "@$(location //third-party/rust/crates/windows_x86_64_msvc/0.52.6:build-script-run[rustc_flags])",
        ],
    }),
    "DEFAULT": [],
})
```

The rules that get it are the ones that link:

- every `rust_binary` and `rust_test` of a first-party package, and
- the build-script executable of **any** package — first-party or vendored —
  that has `[build-dependencies]`.

That second rule is named for the package's Cargo custom-build target, not for a
filename: `build = "custom_build.rs"` produces `build-script-custom_build`, and
the patch follows the target rather than assuming the default.

That set is not a tracked input of the cache, which has a consequence worth
knowing. An unused search path is inert while the crate providing it is still in
the graph. If that crate later leaves — a `cargo update` that drops the last
dependant, say — its vendored directory is removed, but a package that only
carried the label incidentally has an unchanged fingerprint and is not
regenerated, so its BUCK file keeps a label pointing at a target that no longer
exists. Buck2 then fails to load that package **on Windows**, where the `select()`
branch carrying the label is live; other hosts take the empty branch and notice
nothing. `cargo buckal migrate --no-cache` regenerates past it.

Upgrading an existing project does not rewrite BUCK files on its own: a newer
cargo-buckal does not change any package's cache fingerprint, so nothing is
regenerated until something else about the package changes. Run
`cargo buckal migrate --no-cache` (with `--merge` if the project relies on it) to
pick the flags up across a tree that was generated before this existed.

The build-script case is easy to miss because a build-script executable links
its *build*-dependency closure, not its runtime one. Omitting it fails only at
link time (`LNK1181: cannot open input file 'windows.0.52.0.lib'`), which no
`check`-only build and no non-Windows host will ever surface. The crates that
provide the search path are skipped: they have no `[build-dependencies]`, and
patching them would point a build-script binary at its own `build-script-run`.

## Troubleshooting

- If you see warnings about `rustc --print=cfg --target ...` failing, install the missing Rust targets (or expect fewer platform predicates to be mapped).
- If OS-specific deps appear in the default `deps` list, the corresponding predicate likely couldn’t be mapped; rerun with more Rust targets installed.
- If Buck2 fails to parse generated BUCK files due to missing support for `os_deps`/`os_named_deps` (or missing symbols like `rust_test` in `wrapper.bzl`), update the Buckal bundles (try `cargo buckal migrate --fetch`) or pin a bundles revision that supports these attributes.
