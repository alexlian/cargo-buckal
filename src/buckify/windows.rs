use std::{collections::BTreeSet as Set, vec};

use starlark_syntax::codemap::{Pos, Span, Spanned};
use starlark_syntax::syntax::ast::{
    ArgumentP, AstExpr, AstLiteral, AstNoPayload, AstStmt, CallArgsP, ExprP, IdentP, Stmt,
};
use starlark_syntax::syntax::{AstModule, Dialect};

use cargo_metadata::{DependencyKind, TargetKind};

use crate::context::BuckalContext;
use crate::resolve::{BuckalNode, is_lib_like};
use crate::utils::{UnwrapOrExit, get_vendor_path_relative};

#[derive(Default)]
struct WindowsImportLibFlags {
    gnu: Vec<String>,
    msvc_x86_64: Vec<String>,
}

pub(super) fn patch_root_windows_rustc_flags(
    mut buck_content: String,
    ctx: &BuckalContext,
    root: &BuckalNode,
) -> String {
    let bin_names: Vec<String> = root
        .targets
        .iter()
        .filter(|t| t.kind.contains(&TargetKind::Bin))
        .map(|t| t.name.clone())
        .collect();

    let mut rust_test_names: Set<String> = root
        .targets
        .iter()
        .filter(|t| t.kind.contains(&TargetKind::Test))
        .map(|t| t.name.clone())
        .collect();

    let lib_targets: Vec<_> = root
        .targets
        .iter()
        .filter(|t| t.kind.iter().any(is_lib_like))
        .collect();

    for lib_target in lib_targets {
        if lib_target.test {
            rust_test_names.insert("unittest".to_owned());
        }
    }

    if bin_names.is_empty() && rust_test_names.is_empty() {
        return buck_content;
    }

    let flags = windows_import_lib_flags(ctx);
    let select_expr = render_windows_rustc_flags_select(&flags);
    if select_expr.is_empty() {
        return buck_content;
    }

    for bin_name in bin_names {
        buck_content = apply_rustc_flags_patch_to_content(
            &buck_content,
            "rust_binary",
            &bin_name,
            &select_expr,
        );
    }

    for test_name in rust_test_names {
        buck_content = apply_rustc_flags_patch_to_content(
            &buck_content,
            "rust_test",
            &test_name,
            &select_expr,
        );
    }

    buck_content
}

/// Give a package's build-script executable the same import-library search
/// paths its binaries and tests get.
///
/// A `build-script-build` target is an ordinary `rust_binary`: it links, and it
/// links the *build-dependency* closure. When something in that closure reaches
/// a crate that ships a prebuilt import library (`windows-sys 0.52` ->
/// `windows-targets` -> `windows_x86_64_msvc`, say), the `#[link(name =
/// "windows.0.52.0")]` the macro expands into the build script needs a
/// `/LIBPATH:` pointing at the shipped `lib/` directory, exactly as a runtime
/// link does. Without it the build script compiles and fails to link
/// (`LNK1181: cannot open input file 'windows.0.52.0.lib'`), which is invisible
/// from any seat that does not link on Windows.
///
/// Unlike [`patch_root_windows_rustc_flags`] this runs for third-party packages
/// too — the exposure is a property of what is under `[build-dependencies]`,
/// not of who owns the crate.
pub(super) fn patch_buildscript_windows_rustc_flags(
    buck_content: String,
    ctx: &BuckalContext,
    node: &BuckalNode,
) -> String {
    let Some(build_script) = build_script_target_name(node) else {
        return buck_content;
    };

    if !has_build_dependencies(ctx, node) {
        return buck_content;
    }

    let flags = windows_import_lib_flags(ctx);

    // Never point a build-script binary at its own `build-script-run`: that
    // runner executes this very binary, so the label would be a cycle. Asking
    // whether the collected flags name *this* package answers that directly,
    // and derives from the same collection above — a second list of provider
    // crate names would be one more thing to keep in step with it.
    if references_own_build_script(&flags, node) {
        return buck_content;
    }

    let select_expr = render_windows_rustc_flags_select(&flags);
    if select_expr.is_empty() {
        return buck_content;
    }

    apply_rustc_flags_patch_to_content(&buck_content, "rust_binary", build_script, &select_expr)
}

/// The rule name the generator gives this package's build-script executable.
///
/// Cargo names the target after its source file, so `build = "custom_build.rs"`
/// produces `build-script-custom_build`, not `build-script-build`.
/// `emit_buildscript_build` emits `build_target.name` verbatim, so this has to
/// read the same target rather than assume the default filename -- otherwise a
/// package with a renamed build script is silently skipped and fails to link
/// for exactly the reason this patch exists to prevent.
fn build_script_target_name(node: &BuckalNode) -> Option<&str> {
    node.targets
        .iter()
        .find(|target| target.kind.contains(&TargetKind::CustomBuild))
        .map(|target| target.name.as_str())
}

/// Whether the collected flags point at this package's own `build-script-run`.
///
/// True only for the crates that *provide* a search path, and derived from
/// whatever [`windows_import_lib_flags`] collected rather than from a parallel
/// list of their names.
fn references_own_build_script(flags: &WindowsImportLibFlags, node: &BuckalNode) -> bool {
    // First-party packages are not vendored and can never be providers.
    let Ok(vendor_path) = get_vendor_path_relative(&node.package_id) else {
        return false;
    };
    let own = build_script_run_flag(&vendor_path);
    flags
        .gnu
        .iter()
        .chain(flags.msvc_x86_64.iter())
        .any(|flag| *flag == own)
}

/// The one place the label's shape is written.
fn build_script_run_flag(vendor_path: &str) -> String {
    format!("@$(location //{vendor_path}:build-script-run[rustc_flags])")
}

fn has_build_dependencies(ctx: &BuckalContext, node: &BuckalNode) -> bool {
    ctx.resolve
        .deps_of(&node.package_id)
        .iter()
        .any(|(dep, _)| {
            dep.dep_kinds
                .iter()
                .any(|k| k.kind == DependencyKind::Build)
        })
}

fn windows_import_lib_flags(ctx: &BuckalContext) -> WindowsImportLibFlags {
    let mut flags = WindowsImportLibFlags::default();

    let push_build_script_rustc_flags = |package_name: &str, out: &mut Vec<String>| {
        let mut matches: Vec<_> = ctx
            .resolve
            .nodes()
            .filter(|n| n.name == package_name)
            .collect();
        matches.sort_by(|a, b| a.version.cmp(&b.version));
        for node in matches {
            out.push(build_script_run_flag(
                &get_vendor_path_relative(&node.package_id).unwrap_or_exit(),
            ));
        }
    };

    // GNU targets.
    push_build_script_rustc_flags("windows_x86_64_gnu", &mut flags.gnu);
    push_build_script_rustc_flags("winapi-x86_64-pc-windows-gnu", &mut flags.gnu);

    // MSVC targets.
    push_build_script_rustc_flags("windows_x86_64_msvc", &mut flags.msvc_x86_64);

    flags
}

fn render_windows_rustc_flags_select(flags: &WindowsImportLibFlags) -> String {
    const CONSTRAINT_WINDOWS: &str = "prelude//os/constraints:windows";
    const CONSTRAINT_ABI_GNU: &str = "prelude//abi/constraints:gnu";
    const SELECT_DEFAULT: &str = "DEFAULT";

    if flags.gnu.is_empty() && flags.msvc_x86_64.is_empty() {
        return String::new();
    }

    let windows_select = build_select(&[
        (CONSTRAINT_ABI_GNU, build_string_list(&flags.gnu)),
        (SELECT_DEFAULT, build_string_list(&flags.msvc_x86_64)),
    ]);

    let select_expr = build_select(&[
        (CONSTRAINT_WINDOWS, windows_select),
        (SELECT_DEFAULT, build_empty_list()),
    ]);

    // Pretty-print the AST with proper indentation
    let mut out = String::new();
    pretty_print_expr(&select_expr, &mut out, 4);
    out
}

/// Create a dummy span for AST nodes (required by starlark_syntax but not used for our purpose)
fn dummy_span() -> Span {
    Span::new(Pos::new(0), Pos::new(0))
}

/// Wrap a value in a Spanned with a dummy span
fn spanned<T>(node: T) -> Spanned<T> {
    Spanned {
        span: dummy_span(),
        node,
    }
}

/// Build a string literal AST node
fn build_string_literal(s: &str) -> AstExpr {
    spanned(ExprP::Literal(AstLiteral::String(spanned(s.to_owned()))))
}

/// Build a list of string literals
fn build_string_list(items: &[String]) -> AstExpr {
    let list_items: Vec<AstExpr> = items.iter().map(|s| build_string_literal(s)).collect();
    spanned(ExprP::List(list_items))
}

/// Build an empty list
fn build_empty_list() -> AstExpr {
    spanned(ExprP::List(vec![]))
}

/// Build a select() call with a dictionary argument
fn build_select(entries: &[(&str, AstExpr)]) -> AstExpr {
    let dict_entries: Vec<(AstExpr, AstExpr)> = entries
        .iter()
        .map(|(k, v)| (build_string_literal(k), v.clone()))
        .collect();

    let dict_expr = spanned(ExprP::Dict(dict_entries));

    let select_ident = spanned(ExprP::Identifier(spanned(IdentP {
        ident: "select".to_owned(),
        payload: (),
    })));

    let args = CallArgsP {
        args: vec![spanned(ArgumentP::Positional(dict_expr))],
    };

    spanned(ExprP::Call(Box::new(select_ident), args))
}

/// Pretty-print an AST expression with proper indentation
fn pretty_print_expr(expr: &AstExpr, out: &mut String, indent: usize) {
    match &expr.node {
        ExprP::Literal(AstLiteral::String(s)) => {
            write_string_literal(out, &s.node);
        }
        ExprP::List(items) => {
            if items.is_empty() {
                out.push_str("[]");
            } else {
                out.push_str("[\n");
                for item in items {
                    write_indent(out, indent + 4);
                    pretty_print_expr(item, out, indent + 4);
                    out.push_str(",\n");
                }
                write_indent(out, indent);
                out.push(']');
            }
        }
        ExprP::Call(callee, args) => {
            // Handle select() calls specially
            if let ExprP::Identifier(ident) = &callee.node
                && ident.node.ident == "select"
            {
                out.push_str("select(");
                if let Some(arg) = args.args.first()
                    && let ArgumentP::Positional(dict_expr) = &arg.node
                {
                    pretty_print_dict(dict_expr, out, indent);
                }
                out.push(')');
                return;
            }
            // Generic call handling (not used in our case)
            out.push_str(&format!("{}", expr.node));
        }
        _ => {
            out.push_str(&format!("{}", expr.node));
        }
    }
}

/// Pretty-print a dictionary expression
fn pretty_print_dict(expr: &AstExpr, out: &mut String, indent: usize) {
    if let ExprP::Dict(entries) = &expr.node {
        out.push_str("{\n");
        for (key, value) in entries {
            write_indent(out, indent + 4);
            pretty_print_expr(key, out, indent + 4);
            out.push_str(": ");
            pretty_print_expr(value, out, indent + 4);
            out.push_str(",\n");
        }
        write_indent(out, indent);
        out.push('}');
    }
}

fn write_indent(out: &mut String, spaces: usize) {
    for _ in 0..spaces {
        out.push(' ');
    }
}

fn write_string_literal(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
}

fn apply_rustc_flags_patch_to_content(
    buck_content: &str,
    rule_name: &str,
    bin_name: &str,
    select_expr: &str,
) -> String {
    // Parse the Starlark content into an AST
    let ast = match AstModule::parse("BUCK", buck_content.to_owned(), &Dialect::Extended) {
        Ok(ast) => ast,
        Err(_) => return buck_content.to_owned(),
    };

    // Find the insertion point by walking the AST
    let insert_pos = match find_rustc_flags_end_in_rule(ast.statement(), rule_name, bin_name) {
        Some(pos) => pos,
        None => return buck_content.to_owned(),
    };

    // Insert the select expression at the found position
    let mut out = String::with_capacity(buck_content.len() + select_expr.len() + 4);
    out.push_str(&buck_content[..insert_pos]);
    out.push_str(" + ");
    out.push_str(select_expr);
    out.push_str(&buck_content[insert_pos..]);
    out
}

/// Walk the AST to find a rust rule call with the given name and return the
/// byte position just after the closing `]` of its `rustc_flags` list.
fn find_rustc_flags_end_in_rule(
    stmt: &AstStmt,
    rule_name: &str,
    target_name: &str,
) -> Option<usize> {
    match &stmt.node {
        Stmt::Statements(stmts) => {
            for s in stmts {
                if let Some(pos) = find_rustc_flags_end_in_rule(s, rule_name, target_name) {
                    return Some(pos);
                }
            }
            None
        }
        Stmt::Expression(expr) => find_in_expr(expr, rule_name, target_name),
        _ => None,
    }
}

fn find_in_expr(expr: &AstExpr, rule_name: &str, target_name: &str) -> Option<usize> {
    if let ExprP::Call(callee, args) = &expr.node {
        // Check if this is a call to a target rule
        if let ExprP::Identifier(ident) = &callee.node
            && ident.node.ident == rule_name
        {
            return find_rustc_flags_in_call(&args.args, target_name);
        }
    }
    None
}

fn find_rustc_flags_in_call(
    args: &[Spanned<ArgumentP<AstNoPayload>>],
    target_name: &str,
) -> Option<usize> {
    // First, check if the `name` argument matches
    let mut name_matches = false;
    let mut rustc_flags_end: Option<usize> = None;

    for arg in args {
        if let ArgumentP::Named(name_spanned, value) = &arg.node {
            let arg_name = &name_spanned.node;
            if arg_name == "name" {
                // Check if the value is a string literal matching our target
                if let ExprP::Literal(AstLiteral::String(s)) = &value.node
                    && s.node == target_name
                {
                    name_matches = true;
                }
            } else if arg_name == "rustc_flags" {
                // Get the end position of the rustc_flags value (should be a list)
                if let ExprP::List(_) = &value.node {
                    rustc_flags_end = Some(value.span.end().get() as usize);
                }
            }
        }
    }

    if name_matches { rustc_flags_end } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    use indoc::indoc;

    #[test]
    fn render_windows_rustc_flags_select_empty() {
        let flags = WindowsImportLibFlags::default();
        assert_eq!(render_windows_rustc_flags_select(&flags), "");
    }

    #[test]
    fn render_windows_rustc_flags_select_structured_output() {
        let flags = WindowsImportLibFlags {
            gnu: vec!["@gnu1".to_owned(), "@gnu2".to_owned()],
            msvc_x86_64: vec!["@msvc64".to_owned()],
        };

        let rendered = render_windows_rustc_flags_select(&flags);

        let expected = indoc! {r#"
            select({
                    "prelude//os/constraints:windows": select({
                        "prelude//abi/constraints:gnu": [
                            "@gnu1",
                            "@gnu2",
                        ],
                        "DEFAULT": [
                            "@msvc64",
                        ],
                    }),
                    "DEFAULT": [],
                })"#};

        assert_eq!(rendered, expected);
    }

    #[test]
    fn apply_rustc_flags_patch_to_content_patches_named_binary_only() {
        let input = indoc! {r#"
            rust_library(
                name = "bin",
                rustc_flags = [
                    "libflag",
                ],
            )

            rust_binary(
                name = "bin",
                rustc_flags = [
                    "binflag",
                ],
            )
            "#};

        let expected = indoc! {r#"
            rust_library(
                name = "bin",
                rustc_flags = [
                    "libflag",
                ],
            )

            rust_binary(
                name = "bin",
                rustc_flags = [
                    "binflag",
                ] + select({"DEFAULT": []}),
            )
            "#};

        let patched = apply_rustc_flags_patch_to_content(
            input,
            "rust_binary",
            "bin",
            "select({\"DEFAULT\": []})",
        );
        assert_eq!(patched, expected);
    }

    #[test]
    fn apply_rustc_flags_patch_to_content_patches_the_build_script_binary() {
        let input = indoc! {r#"
            rust_library(
                name = "mm_proto",
                rustc_flags = [
                    "@$(location :build-script-run[rustc_flags])",
                ],
            )

            rust_binary(
                name = "build-script-build",
                rustc_flags = [
                    "@$(location :manifest[env_flags])",
                ],
            )
            "#};

        let expected = indoc! {r#"
            rust_library(
                name = "mm_proto",
                rustc_flags = [
                    "@$(location :build-script-run[rustc_flags])",
                ],
            )

            rust_binary(
                name = "build-script-build",
                rustc_flags = [
                    "@$(location :manifest[env_flags])",
                ] + select({"DEFAULT": []}),
            )
            "#};

        let patched = apply_rustc_flags_patch_to_content(
            input,
            "rust_binary",
            "build-script-build",
            "select({\"DEFAULT\": []})",
        );
        assert_eq!(patched, expected);
    }

    #[test]
    fn apply_rustc_flags_patch_to_content_does_not_touch_other_binaries() {
        let input = indoc! {r#"
            rust_binary(
                name = "a",
                rustc_flags = [
                    "aflag",
                ],
            )

            rust_binary(
                name = "b",
                rustc_flags = [
                    "bflag",
                ],
            )
            "#};

        let expected = indoc! {r#"
            rust_binary(
                name = "a",
                rustc_flags = [
                    "aflag",
                ],
            )

            rust_binary(
                name = "b",
                rustc_flags = [
                    "bflag",
                ] + select({"DEFAULT": []}),
            )
            "#};

        let patched = apply_rustc_flags_patch_to_content(
            input,
            "rust_binary",
            "b",
            "select({\"DEFAULT\": []})",
        );
        assert_eq!(patched, expected);
    }

    #[test]
    fn apply_rustc_flags_patch_to_content_patches_named_test_only() {
        let input = indoc! {r#"
            rust_binary(
                name = "bin",
                rustc_flags = [
                    "binflag",
                ],
            )

            rust_test(
                name = "bin-unittest",
                rustc_flags = [
                    "testflag",
                ],
            )
            "#};

        let expected = indoc! {r#"
            rust_binary(
                name = "bin",
                rustc_flags = [
                    "binflag",
                ],
            )

            rust_test(
                name = "bin-unittest",
                rustc_flags = [
                    "testflag",
                ] + select({"DEFAULT": []}),
            )
            "#};

        let patched = apply_rustc_flags_patch_to_content(
            input,
            "rust_test",
            "bin-unittest",
            "select({\"DEFAULT\": []})",
        );
        assert_eq!(patched, expected);
    }

    /// The gate reads the dependency graph, not the manifest text: only a
    /// `[build-dependencies]` edge puts a crate in the build script's link.
    /// A normal dependency reaches the library, never the build script.
    #[test]
    fn has_build_dependencies_distinguishes_edge_kinds() {
        use cargo_metadata::{DependencyKind, Edition, PackageId};
        use daggy::Dag;
        use std::collections::HashMap;

        use crate::config::RepoConfig;
        use crate::resolve::{BuckalDep, BuckalDepKind, BuckalResolve, NodeKind};

        fn node(name: &str) -> BuckalNode {
            BuckalNode {
                package_id: PackageId {
                    repr: format!("path+file://{name}#0.1.0"),
                },
                name: name.to_owned(),
                version: "0.1.0".to_owned(),
                features: vec![],
                kind: NodeKind::FirstParty {
                    relative_path: String::new(),
                },
                edition: Edition::E2021,
                manifest_path: "/tmp/Cargo.toml".into(),
                targets: vec![],
                source: None,
                links: None,
                checksum: None,
            }
        }

        fn ctx_with_edge(kind: DependencyKind) -> (BuckalContext, BuckalNode) {
            let root = node("root");
            let dep = node("dep");
            let root_id = root.package_id.clone();
            let dep_id = dep.package_id.clone();

            let mut dag = Dag::new();
            let root_idx = dag.add_node(root.clone());
            let dep_idx = dag.add_node(dep);
            dag.add_edge(
                root_idx,
                dep_idx,
                BuckalDep {
                    name: "dep".to_owned(),
                    dep_kinds: vec![BuckalDepKind { kind, target: None }],
                },
            )
            .expect("edge");

            let mut index_map = HashMap::new();
            index_map.insert(root_id.clone(), root_idx);
            index_map.insert(dep_id, dep_idx);

            let ctx = BuckalContext {
                root: Some(root_id),
                resolve: BuckalResolve { dag, index_map },
                workspace_root: "/tmp".into(),
                workspace_inherit: false,
                no_merge: false,
                repo_config: RepoConfig::default(),
            };
            (ctx, root)
        }

        let (build_ctx, root) = ctx_with_edge(DependencyKind::Build);
        assert!(
            has_build_dependencies(&build_ctx, &root),
            "a build-dependency edge is what puts a crate in the build script's link"
        );

        let (normal_ctx, root) = ctx_with_edge(DependencyKind::Normal);
        assert!(
            !has_build_dependencies(&normal_ctx, &root),
            "a normal dependency reaches the library, not the build script"
        );

        let (dev_ctx, root) = ctx_with_edge(DependencyKind::Development);
        assert!(
            !has_build_dependencies(&dev_ctx, &root),
            "nor does a dev-dependency"
        );
    }

    /// Cargo names a build-script target after its source file. `build =
    /// "custom_build.rs"` yields `build-script-custom_build`, and
    /// `emit_buildscript_build` emits that name, so looking for the default one
    /// silently skips the package -- which then fails to link for exactly the
    /// reason this patch exists to prevent.
    #[test]
    fn build_script_target_name_follows_the_cargo_target() {
        use cargo_metadata::{Edition, PackageId};

        use crate::resolve::{BuckalTarget, NodeKind};

        fn node_with(targets: Vec<BuckalTarget>) -> BuckalNode {
            BuckalNode {
                package_id: PackageId {
                    repr: "path+file://p#0.1.0".to_owned(),
                },
                name: "p".to_owned(),
                version: "0.1.0".to_owned(),
                features: vec![],
                kind: NodeKind::FirstParty {
                    relative_path: String::new(),
                },
                edition: Edition::E2021,
                manifest_path: "/tmp/Cargo.toml".into(),
                targets,
                source: None,
                links: None,
                checksum: None,
            }
        }

        fn target(name: &str, kind: TargetKind) -> BuckalTarget {
            BuckalTarget {
                name: name.to_owned(),
                kind: vec![kind],
                src_path: "/tmp/x.rs".into(),
                doctest: false,
                test: false,
            }
        }

        let default = node_with(vec![
            target("p", TargetKind::Lib),
            target("build-script-build", TargetKind::CustomBuild),
        ]);
        assert_eq!(
            build_script_target_name(&default),
            Some("build-script-build")
        );

        let renamed = node_with(vec![
            target("p", TargetKind::Lib),
            target("build-script-custom_build", TargetKind::CustomBuild),
        ]);
        assert_eq!(
            build_script_target_name(&renamed),
            Some("build-script-custom_build"),
            "the name comes from the Cargo target, not a filename convention"
        );

        let none = node_with(vec![target("p", TargetKind::Lib)]);
        assert_eq!(
            build_script_target_name(&none),
            None,
            "a package with no build script has no rule to patch"
        );
    }

    /// And the patch itself must accept that name.
    #[test]
    fn apply_rustc_flags_patch_to_content_patches_a_renamed_build_script() {
        let input = indoc! {r#"
            rust_binary(
                name = "build-script-custom_build",
                rustc_flags = [
                    "@$(location :manifest[env_flags])",
                ],
            )
            "#};

        let patched = apply_rustc_flags_patch_to_content(
            input,
            "rust_binary",
            "build-script-custom_build",
            "select({\"DEFAULT\": []})",
        );
        assert!(
            patched.contains("] + select({\"DEFAULT\": []}),"),
            "got:\n{patched}"
        );
    }

    /// Drives the patch itself, not its parts: a package whose build script is
    /// named after a custom source file must still have its rule patched. The
    /// pieces below were each right in isolation while the function ignored
    /// them, which a test of either piece alone cannot show.
    #[test]
    fn patch_buildscript_uses_the_packages_own_build_script_name() {
        use cargo_metadata::{DependencyKind, Edition, PackageId, TargetKind};
        use daggy::Dag;
        use std::collections::HashMap;

        use crate::config::RepoConfig;
        use crate::resolve::{BuckalDep, BuckalDepKind, BuckalResolve, BuckalTarget, NodeKind};

        fn target(name: &str, kind: TargetKind) -> BuckalTarget {
            BuckalTarget {
                name: name.to_owned(),
                kind: vec![kind],
                src_path: "/tmp/x.rs".into(),
                doctest: false,
                test: false,
            }
        }

        fn first_party(name: &str, targets: Vec<BuckalTarget>) -> BuckalNode {
            BuckalNode {
                package_id: PackageId {
                    repr: format!("path+file:///tmp/{name}#0.1.0"),
                },
                name: name.to_owned(),
                version: "0.1.0".to_owned(),
                features: vec![],
                kind: NodeKind::FirstParty {
                    relative_path: String::new(),
                },
                edition: Edition::E2021,
                manifest_path: "/tmp/Cargo.toml".into(),
                targets,
                source: None,
                links: None,
                checksum: None,
            }
        }

        // A provider has to be in the graph, or the select renders empty and
        // the patch is a no-op for reasons unrelated to the name.
        fn registry(name: &str, version: &str) -> BuckalNode {
            BuckalNode {
                package_id: PackageId {
                    repr: format!(
                        "registry+https://github.com/rust-lang/crates.io-index#{name}@{version}"
                    ),
                },
                name: name.to_owned(),
                version: version.to_owned(),
                features: vec![],
                kind: NodeKind::ThirdParty,
                edition: Edition::E2021,
                manifest_path: "/tmp/Cargo.toml".into(),
                targets: vec![],
                source: Some("registry".to_owned()),
                links: None,
                checksum: None,
            }
        }

        let root = first_party(
            "myroot",
            vec![
                target("myroot", TargetKind::Lib),
                target("build-script-custom_build", TargetKind::CustomBuild),
            ],
        );
        let helper = first_party("helper", vec![]);
        let provider = registry("windows_x86_64_msvc", "0.52.6");

        let root_id = root.package_id.clone();
        let helper_id = helper.package_id.clone();
        let provider_id = provider.package_id.clone();

        let mut dag = Dag::new();
        let root_idx = dag.add_node(root.clone());
        let helper_idx = dag.add_node(helper);
        let provider_idx = dag.add_node(provider);
        dag.add_edge(
            root_idx,
            helper_idx,
            BuckalDep {
                name: "helper".to_owned(),
                dep_kinds: vec![BuckalDepKind {
                    kind: DependencyKind::Build,
                    target: None,
                }],
            },
        )
        .expect("build edge");

        let mut index_map = HashMap::new();
        index_map.insert(root_id.clone(), root_idx);
        index_map.insert(helper_id, helper_idx);
        index_map.insert(provider_id, provider_idx);

        let ctx = BuckalContext {
            root: Some(root_id),
            resolve: BuckalResolve { dag, index_map },
            workspace_root: "/tmp".into(),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig::default(),
        };

        let generated = indoc! {r#"
            rust_binary(
                name = "build-script-custom_build",
                rustc_flags = [
                    "@$(location :manifest[env_flags])",
                ],
            )
            "#};

        let patched = patch_buildscript_windows_rustc_flags(generated.to_owned(), &ctx, &root);

        assert!(
            patched.contains("windows_x86_64_msvc/0.52.6:build-script-run[rustc_flags]"),
            "a renamed build script must still get the import-lib search path, got:\n{patched}"
        );
    }

    /// A provider's own build script must not be pointed at its own
    /// `build-script-run` -- that runner executes this very binary. The check is
    /// derived from the collected flags, so it cannot fall out of step with the
    /// list of crates those flags come from.
    #[test]
    fn a_provider_is_not_pointed_at_its_own_build_script_run() {
        use cargo_metadata::PackageId;

        let provider_id = PackageId {
            repr:
                "registry+https://github.com/rust-lang/crates.io-index#windows_x86_64_msvc@0.52.6"
                    .to_owned(),
        };
        let vendor = get_vendor_path_relative(&provider_id).expect("vendor path");
        let flags = WindowsImportLibFlags {
            gnu: vec![],
            msvc_x86_64: vec![build_script_run_flag(&vendor)],
        };

        let mut provider = mock_node();
        provider.package_id = provider_id;
        assert!(
            references_own_build_script(&flags, &provider),
            "the crate the flags come from must be recognised without naming it"
        );

        let mut other = mock_node();
        other.package_id = PackageId {
            repr: "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0".to_owned(),
        };
        assert!(!references_own_build_script(&flags, &other));

        // First-party packages are not vendored and can never be providers.
        assert!(!references_own_build_script(&flags, &mock_node()));
    }

    fn mock_node() -> BuckalNode {
        use cargo_metadata::{Edition, PackageId};

        use crate::resolve::NodeKind;

        BuckalNode {
            package_id: PackageId {
                repr: "path+file:///tmp/p#0.1.0".to_owned(),
            },
            name: "p".to_owned(),
            version: "0.1.0".to_owned(),
            features: vec![],
            kind: NodeKind::FirstParty {
                relative_path: String::new(),
            },
            edition: Edition::E2021,
            manifest_path: "/tmp/Cargo.toml".into(),
            targets: vec![],
            source: None,
            links: None,
            checksum: None,
        }
    }

    /// Drives the patch, not the guard: a provider crate that happens to have a
    /// build-dependency must come back untouched. Testing
    /// `references_own_build_script` alone passes whether or not the patch
    /// consults it.
    #[test]
    fn patch_buildscript_leaves_a_provider_alone() {
        use cargo_metadata::{DependencyKind, Edition, PackageId, TargetKind};
        use daggy::Dag;
        use std::collections::HashMap;

        use crate::config::RepoConfig;
        use crate::resolve::{BuckalDep, BuckalDepKind, BuckalResolve, BuckalTarget, NodeKind};

        let provider_id = PackageId {
            repr:
                "registry+https://github.com/rust-lang/crates.io-index#windows_x86_64_msvc@0.52.6"
                    .to_owned(),
        };
        let helper_id = PackageId {
            repr: "registry+https://github.com/rust-lang/crates.io-index#helper@1.0.0".to_owned(),
        };

        let target = |name: &str, kind: TargetKind| BuckalTarget {
            name: name.to_owned(),
            kind: vec![kind],
            src_path: "/tmp/x.rs".into(),
            doctest: false,
            test: false,
        };
        let registry_node = |id: &PackageId, name: &str, targets: Vec<BuckalTarget>| BuckalNode {
            package_id: id.clone(),
            name: name.to_owned(),
            version: "0.52.6".to_owned(),
            features: vec![],
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: "/tmp/Cargo.toml".into(),
            targets,
            source: Some("registry".to_owned()),
            links: None,
            checksum: None,
        };

        let provider = registry_node(
            &provider_id,
            "windows_x86_64_msvc",
            vec![
                target("windows_x86_64_msvc", TargetKind::Lib),
                target("build-script-build", TargetKind::CustomBuild),
            ],
        );
        let helper = registry_node(&helper_id, "helper", vec![]);

        let mut dag = Dag::new();
        let provider_idx = dag.add_node(provider.clone());
        let helper_idx = dag.add_node(helper);
        dag.add_edge(
            provider_idx,
            helper_idx,
            BuckalDep {
                name: "helper".to_owned(),
                dep_kinds: vec![BuckalDepKind {
                    kind: DependencyKind::Build,
                    target: None,
                }],
            },
        )
        .expect("build edge");

        let mut index_map = HashMap::new();
        index_map.insert(provider_id, provider_idx);
        index_map.insert(helper_id, helper_idx);

        let ctx = BuckalContext {
            root: None,
            resolve: BuckalResolve { dag, index_map },
            workspace_root: "/tmp".into(),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig::default(),
        };

        let generated = indoc! {r#"
            rust_binary(
                name = "build-script-build",
                rustc_flags = [
                    "@$(location :manifest[env_flags])",
                ],
            )
            "#};

        let patched = patch_buildscript_windows_rustc_flags(generated.to_owned(), &ctx, &provider);

        assert_eq!(
            patched, generated,
            "a provider must not be pointed at the build-script-run that runs it"
        );
    }
}
