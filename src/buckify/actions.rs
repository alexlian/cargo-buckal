use cargo_metadata::camino::Utf8Path;
use regex::Regex;

use crate::{
    buck::{
        MANUAL_SECTION_MARKER, Rule, generated_region, parse_buck_content, parse_buck_file,
        parse_buck_statements, patch_buck_rules, split_manual_section,
    },
    buckal_error, buckal_log, buckal_warn,
    buckify::emit::emit_export_file,
    cache::{BuckalChange, ChangeType},
    context::BuckalContext,
    resolve::{BuckalNode, NodeKind},
    utils::{UnwrapOrExit, get_vendor_dir, is_path_source},
};

use super::{
    CARGO_MANIFEST_SYMBOL, WRAPPER_SYMBOLS, buckify_dep_node, buckify_root_node, cross,
    gen_buck_content_with_loads, vendor_package, windows,
};

impl BuckalChange {
    /// Apply the changes to the BUCK files based on the detected package changes in the cache diff.
    pub fn apply(&self, ctx: &BuckalContext) {
        let re: Regex = Regex::new(r"^([^+#]+)\+([^#]+)#([^@]+)@([^+#]+)(?:\+(.+))?$")
            .expect("error creating regex");

        let mut workspace_emitted = false;

        for (id, change_type) in &self.changes {
            match change_type {
                ChangeType::Added | ChangeType::Changed => {
                    if let Some(node) = ctx.resolve.nodes().find(|n| &n.package_id == id) {
                        buckal_log!(
                            if let ChangeType::Added = change_type {
                                "Adding"
                            } else {
                                "Flushing"
                            },
                            format!("{} v{}", node.name, node.version)
                        );

                        let is_third_party_pkg = is_third_party(node);

                        // Vendor package sources
                        let vendor_dir = if !is_third_party_pkg {
                            node.manifest_path.parent().unwrap().to_owned()
                        } else {
                            vendor_package(node)
                        };

                        // Generate BUCK rules
                        let mut buck_rules = if !is_third_party_pkg {
                            buckify_root_node(node, ctx)
                        } else {
                            buckify_dep_node(node, ctx)
                        };

                        // Export workspace manifest
                        let workspace_manifest_path = ctx.workspace_root.join("Cargo.toml");
                        if ctx.workspace_inherit
                            && !workspace_emitted
                            && node.manifest_path == workspace_manifest_path
                        {
                            buck_rules.push(Rule::ExportFile(emit_export_file()));
                            workspace_emitted = true;
                        }

                        // Patch BUCK Rules
                        let buck_path = vendor_dir.join("BUCK");
                        let manual = merge_rules(&buck_path, &mut buck_rules, ctx);

                        // Generate the BUCK file. Statements carried over may
                        // call rules no generated rule uses, and those still
                        // need binding in the header.
                        let mut buck_content =
                            gen_buck_content_with_loads(&buck_rules, &loads_for(&manual));
                        if !is_third_party_pkg {
                            buck_content =
                                windows::patch_root_windows_rustc_flags(buck_content, ctx, node);
                        }
                        buck_content = cross::patch_rust_test_target_compatible_with(buck_content);
                        // After the generated-content patches, so those only
                        // ever rewrite rules this run produced.
                        buck_content = append_manual_section(buck_content, &manual);
                        std::fs::write(&buck_path, buck_content)
                            .expect("Failed to write BUCK file");
                    }
                }
                ChangeType::Removed => {
                    // Path-source packages (the workspace root, its members, and any
                    // local `path = "..."` dep) live in the user's source tree, not in
                    // a vendor directory, so there is nothing for us to remove. Skip
                    // them; get_vendor_dir would otherwise abort on the unsupported
                    // source kind.
                    if is_path_source(id).unwrap_or_exit_ctx("failed to classify package source") {
                        continue;
                    }

                    let caps = re.captures(&id.repr).expect("Failed to parse package ID");
                    let name = &caps[3];
                    let version = &caps[4];

                    buckal_log!("Removing", format!("{} v{}", name, version));
                    let vendor_dir =
                        get_vendor_dir(id).unwrap_or_exit_ctx("failed to get vendor directory");
                    if vendor_dir.exists() {
                        std::fs::remove_dir_all(&vendor_dir)
                            .expect("Failed to remove vendor directory");
                    }
                    if let Some(package_dir) = vendor_dir.parent()
                        && package_dir.exists()
                        && package_dir.read_dir().unwrap().next().is_none()
                    {
                        std::fs::remove_dir_all(package_dir)
                            .expect("Failed to remove empty package directory");
                    }
                }
            }
        }

        // Export workspace manifest for virtual workspace.
        //
        // This runs when no changed package owned the workspace manifest, which
        // includes an unchanged root on a later migration -- so it writes a
        // package file that the loop above may already have marked. It has to
        // honour the same contract: parse only the generated region, carry the
        // manual section through untouched, and re-close the file with the
        // marker. Reconstructing the whole file through `Rule` values, as it
        // used to, dropped the marker and rewrote the user's statements.
        if !workspace_emitted && ctx.workspace_inherit {
            let buck_path = ctx.workspace_root.join("BUCK");
            let content = if buck_path.exists() {
                std::fs::read_to_string(&buck_path)
                    .unwrap_or_exit_ctx(format!("Failed to read {}", buck_path))
            } else {
                String::new()
            };
            let manual = split_manual_section(&content)
                .map(|manual| manual.trim().to_owned())
                .unwrap_or_default();

            let mut rules = if content.is_empty() {
                Vec::new()
            } else {
                parse_buck_content(generated_region(&content), buck_path.as_str())
                    .unwrap_or_exit_ctx(format!("Failed to parse {}", buck_path))
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
            };
            let export_file = Rule::ExportFile(emit_export_file());
            if let Some(existing) = rules
                .iter_mut()
                .find(|r| matches!(r, Rule::ExportFile(ef) if ef.name == "workspace"))
            {
                *existing = export_file;
            } else {
                rules.push(export_file);
            }
            let buck_content = gen_buck_content_with_loads(&rules, &loads_for(&manual));
            let buck_content = append_manual_section(buck_content, &manual);
            std::fs::write(&buck_path, buck_content).expect("Failed to write BUCK file");
        }
    }
}

/// Check if a node represents a third-party dependency
pub(super) fn is_third_party(node: &BuckalNode) -> bool {
    matches!(node.kind, NodeKind::ThirdParty)
}

/// Merge existing BUCK rules with new ones, preserving manual changes in
/// specified fields, and return the file's manual section to carry forward.
fn merge_rules(buck_path: &Utf8Path, buck_rules: &mut [Rule], ctx: &BuckalContext) -> String {
    if !buck_path.exists() {
        std::fs::File::create(buck_path)
            .unwrap_or_exit_ctx(format!("Failed to create {}", buck_path));
        return String::new();
    }

    // Merging is what `--merge` asks for; without it the file is regenerated
    // from the manifest alone.
    if ctx.no_merge {
        return String::new();
    }

    let content = std::fs::read_to_string(buck_path)
        .unwrap_or_exit_ctx(format!("Failed to read {}", buck_path));

    if !ctx.repo_config.patch_fields.is_empty() {
        let existing_rules =
            parse_buck_file(buck_path).unwrap_or_exit_ctx(format!("Failed to parse {}", buck_path));
        patch_buck_rules(&existing_rules, buck_rules, &ctx.repo_config.patch_fields);
    }

    // Whatever is below the marker is the user's, and is reproduced exactly --
    // comments, blank lines and all. Re-emitting it statement by statement
    // would drop the text between statements, which is where the comments are.
    if let Some(manual) = split_manual_section(&content) {
        let manual = manual.trim();
        let clashes = colliding_target_names(manual, buck_path, buck_rules);
        if !clashes.is_empty() {
            buckal_error!(
                "{}: the manual section declares target(s) {} that cargo-buckal now \
                 generates. Buck target names are unique within a package, so this file \
                 was left unchanged. Rename the target(s) below the manual marker and \
                 re-run.",
                buck_path,
                clashes.join(", ")
            );
            std::process::exit(1);
        }
        return manual.to_owned();
    }

    bootstrap_manual_section(&content, buck_path, buck_rules)
}

/// Infer the manual section of a file written before the marker existed.
///
/// This runs once per file. Everything the generator is not about to emit is
/// treated as the user's and moved below a marker written on this run, after
/// which ownership is recorded and never guessed again. Some of what moves may
/// be a rule the generator itself emitted and has since stopped emitting, which
/// is exactly what cannot be distinguished without the marker -- so say so
/// rather than pretend otherwise.
fn bootstrap_manual_section(content: &str, buck_path: &Utf8Path, generated: &[Rule]) -> String {
    let statements = parse_buck_statements(content, buck_path.as_str())
        .unwrap_or_exit_ctx(format!("Failed to parse {}", buck_path));

    let generated_names: std::collections::BTreeSet<&str> =
        generated.iter().filter_map(|r| r.target_name()).collect();

    let kept: Vec<&str> = statements
        .iter()
        .filter(|stmt| {
            // A load of a module cargo-buckal synthesizes is part of the old
            // header and is rebuilt; a load of anything else was written by
            // hand, and whatever it binds is needed by a statement below.
            //
            // An alias is never ours: `manual_test = "rust_test"` binds a name
            // the generator would not produce, so the load is the user's
            // however familiar its module looks -- and nothing could rebuild it,
            // since the local name appears in no rule.
            if stmt.is_load {
                let ours = stmt
                    .load_module
                    .as_deref()
                    .is_some_and(is_generated_load_module)
                    && !stmt.load_bindings.iter().any(|b| b.is_alias());
                return !ours;
            }
            match &stmt.target_name {
                // Buck target names are unique within a package across rule
                // types, so a generated name always wins: keeping both would
                // leave a package Buck cannot load.
                Some(name) => !generated_names.contains(name.as_str()),
                None => true,
            }
        })
        .map(|stmt| stmt.text.trim_end())
        .filter(|text| !text.is_empty())
        .collect();

    if kept.is_empty() {
        return String::new();
    }

    buckal_warn!(
        "{}: {} statement(s) predating the manual-section marker were moved below it. \
         Rules you added by hand belong there; delete any left over from a target this \
         package no longer has.",
        buck_path,
        kept.len()
    );

    format!("{}\n", kept.join("\n\n"))
}

fn is_generated_load_module(module: &str) -> bool {
    matches!(
        module,
        "@buckal//:wrapper.bzl" | "@buckal//:cargo_manifest.bzl"
    )
}

/// Symbols the manual section calls but does not bind itself.
///
/// A carried `rust_test` in a package whose generated rules contain no test
/// would otherwise reach Buck's native `rust_test` rather than the buckal
/// wrapper -- the file loads, and quietly builds against a different rule
/// implementation.
fn loads_for(manual: &str) -> std::collections::BTreeSet<String> {
    let Ok(statements) = parse_buck_statements(manual, "manual section") else {
        // Not parsing is the user's business; carrying the text unchanged is
        // still right, and guessing at its imports is not.
        return std::collections::BTreeSet::new();
    };

    // Exact local bindings. A substring search over the load's text would
    // answer a different question: `//my:rust_test_helpers.bzl` contains
    // "rust_test" and binds nothing of the sort.
    let bound_here: std::collections::BTreeSet<&str> = statements
        .iter()
        .filter(|stmt| stmt.is_load)
        .flat_map(|stmt| stmt.load_bindings.iter())
        .map(|binding| binding.local.as_str())
        .collect();

    statements
        .iter()
        .filter_map(|stmt| stmt.call_name.as_deref())
        .filter(|call| WRAPPER_SYMBOLS.contains(call) || *call == CARGO_MANIFEST_SYMBOL)
        // A load the user wrote in their own section already binds it.
        .filter(|call| !bound_here.contains(call))
        .map(|call| call.to_owned())
        .collect()
}

/// Names declared in both the manual section and the generated rules.
///
/// Buck target names are unique within a package, so emitting both produces a
/// package Buck cannot load. The marker assigns those statements to the user,
/// which makes silently dropping one the wrong answer too -- the caller stops
/// and says which name clashes, leaving the file as it was.
fn colliding_target_names(manual: &str, buck_path: &Utf8Path, generated: &[Rule]) -> Vec<String> {
    let Ok(statements) = parse_buck_statements(manual, buck_path.as_str()) else {
        // Unparseable manual content is the user's business; it is carried
        // unchanged, and no claim is made about what it declares.
        return Vec::new();
    };

    let generated_names: std::collections::BTreeSet<&str> = generated
        .iter()
        .filter_map(|rule| rule.target_name())
        .collect();

    statements
        .iter()
        .filter_map(|stmt| stmt.target_name.as_deref())
        .filter(|name| generated_names.contains(name))
        .map(|name| name.to_owned())
        .collect()
}

/// Close the generated region with the ownership marker, then re-emit the
/// manual section beneath it.
///
/// The marker is written **always**, not only when something is carried. It is
/// the record of where cargo-buckal's output stops, so a file that omits it is
/// one whose ownership was never recorded; if it appeared only alongside manual
/// content, every file without manual content would read as legacy forever and
/// the generated region could never be safely replaced.
fn append_manual_section(buck_content: String, manual: &str) -> String {
    let mut out = buck_content.trim_end().to_owned();
    out.push_str("\n\n");
    out.push_str(MANUAL_SECTION_MARKER);
    out.push('\n');
    if !manual.trim().is_empty() {
        out.push('\n');
        out.push_str(manual.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use cargo_metadata::{PackageId, camino::Utf8PathBuf};
    use daggy::Dag;

    use super::{Rule, Utf8Path, colliding_target_names};
    use crate::{
        buck::MANUAL_SECTION_MARKER,
        cache::{BuckalChange, ChangeType},
        config::RepoConfig,
        context::BuckalContext,
        resolve::{BuckalNode, BuckalResolve, BuckalTarget, NodeKind},
    };

    use cargo_metadata::Edition;
    use cargo_metadata::TargetKind;

    fn mock_target(name: &str, kind: TargetKind, src_path: Utf8PathBuf) -> BuckalTarget {
        BuckalTarget {
            name: name.to_string(),
            kind: vec![kind],
            src_path,
            doctest: true,
            test: true,
        }
    }

    fn mock_first_party_node(
        name: &str,
        manifest_path: Utf8PathBuf,
        targets: Vec<BuckalTarget>,
    ) -> BuckalNode {
        BuckalNode {
            package_id: PackageId {
                repr: format!("path+file://{name}#0.1.0"),
            },
            name: name.to_string(),
            version: "0.1.0".to_string(),
            features: vec![],
            kind: NodeKind::FirstParty {
                relative_path: "".to_string(),
            },
            edition: Edition::E2021,
            manifest_path,
            targets,
            source: None,
            links: None,
            checksum: None,
        }
    }

    #[test]
    fn test_apply_generates_root_buck_file() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let tmp_path =
            Utf8PathBuf::try_from(tmp.path().to_path_buf()).expect("temp dir is not valid UTF-8");

        let manifest_path = tmp_path.join("Cargo.toml");
        std::fs::write(
            &manifest_path,
            "[package]\nname = \"myroot\"\nversion = \"0.1.0\"\n",
        )
        .expect("write Cargo.toml");

        // Create a src/lib.rs so the target src_path exists
        let src_dir = tmp_path.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        std::fs::write(src_dir.join("lib.rs"), "").expect("write lib.rs");

        let lib_target = mock_target("myroot", TargetKind::Lib, tmp_path.join("src/lib.rs"));
        let node = mock_first_party_node("myroot", manifest_path, vec![lib_target]);
        let package_id = node.package_id.clone();

        // Build a BuckalResolve with the node in the graph
        let mut dag = Dag::new();
        let idx = dag.add_node(node);
        let mut index_map = HashMap::new();
        index_map.insert(package_id.clone(), idx);
        let resolve = BuckalResolve { dag, index_map };

        // BuckalChange with Added for our root package
        let mut changes = BTreeMap::new();
        changes.insert(package_id.clone(), ChangeType::Added);
        let change = BuckalChange { changes };

        // BuckalContext with root set to our package (this is the key scenario)
        let ctx = BuckalContext {
            root: Some(package_id),
            resolve,
            workspace_root: tmp_path.clone(),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig::default(),
        };

        change.apply(&ctx);

        let buck_path = tmp_path.join("BUCK");
        assert!(
            buck_path.exists(),
            "BUCK file should be generated for root package"
        );

        let content = std::fs::read_to_string(&buck_path).expect("read BUCK file");
        assert!(
            content.contains("rust_library"),
            "BUCK file should contain a rust_library rule, got:\n{content}"
        );
        assert!(
            content.contains("load("),
            "BUCK file should contain load statements, got:\n{content}"
        );
    }

    /// Set up a root package whose BUCK file already exists, so `apply` takes
    /// the merge path. `specs` are (target name, kind, path relative to root).
    fn merge_fixture(
        existing_buck: &str,
        specs: Vec<(&str, TargetKind, &str)>,
    ) -> (tempfile::TempDir, Utf8PathBuf, BuckalChange, BuckalContext) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let tmp_path =
            Utf8PathBuf::try_from(tmp.path().to_path_buf()).expect("temp dir is not valid UTF-8");

        std::fs::write(
            tmp_path.join("Cargo.toml"),
            "[package]\nname = \"myroot\"\nversion = \"0.1.0\"\n",
        )
        .expect("write Cargo.toml");

        let src_dir = tmp_path.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        std::fs::write(src_dir.join("lib.rs"), "").expect("write lib.rs");
        std::fs::write(src_dir.join("main.rs"), "").expect("write main.rs");
        std::fs::write(tmp_path.join("BUCK"), existing_buck).expect("write existing BUCK");

        let targets = specs
            .into_iter()
            .map(|(name, kind, rel)| mock_target(name, kind, tmp_path.join(rel)))
            .collect();
        let node = mock_first_party_node("myroot", tmp_path.join("Cargo.toml"), targets);
        let package_id = node.package_id.clone();

        let mut dag = Dag::new();
        let idx = dag.add_node(node);
        let mut index_map = HashMap::new();
        index_map.insert(package_id.clone(), idx);
        let resolve = BuckalResolve { dag, index_map };

        let mut changes = BTreeMap::new();
        changes.insert(package_id.clone(), ChangeType::Changed);
        let change = BuckalChange { changes };

        let ctx = BuckalContext {
            root: Some(package_id),
            resolve,
            workspace_root: tmp_path.clone(),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig::default(),
        };

        (tmp, tmp_path, change, ctx)
    }

    fn lib_only() -> Vec<(&'static str, TargetKind, &'static str)> {
        vec![("myroot", TargetKind::Lib, "src/lib.rs")]
    }

    /// The marker records where generated output stops, so it has to be written
    /// unconditionally. If it appeared only when something was carried, a file
    /// with no hand-written content would be indistinguishable from one that
    /// predates the marker, and its generated region could never be safely
    /// replaced.
    #[test]
    fn test_marker_is_written_even_with_no_manual_content() {
        let (_tmp, tmp_path, change, ctx) = merge_fixture("", lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert!(
            content.contains(MANUAL_SECTION_MARKER),
            "every generated file records where its generated region ends, got:\n{content}"
        );
    }

    /// An empty manual section is a claim -- "nothing here is mine" -- and
    /// dropping the marker would silently retract it.
    #[test]
    fn test_empty_manual_section_keeps_its_marker() {
        let existing = format!("# @generated by `cargo buckal`\n\n{MANUAL_SECTION_MARKER}\n");
        let (_tmp, tmp_path, change, ctx) = merge_fixture(&existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert!(
            content.contains(MANUAL_SECTION_MARKER),
            "the marker must survive an empty manual section, got:\n{content}"
        );
    }

    /// Everything above the marker is the generator's and is replaced. A rule
    /// it used to emit and no longer emits -- a renamed or deleted Cargo target
    /// -- goes with it, instead of outliving every later migration pointing at
    /// a source file that is gone.
    #[test]
    fn test_rules_above_the_marker_are_regenerated() {
        let existing = format!(
            concat!(
                "# @generated by `cargo buckal`\n\n",
                "rust_binary(\n",
                "    name = \"gone-bin\",\n",
                "    srcs = [\":vendor\"],\n",
                "    crate = \"gone_bin\",\n",
                "    crate_root = \"vendor/src/bin/gone.rs\",\n",
                "    edition = \"2021\",\n",
                ")\n\n",
                "{}\n",
            ),
            MANUAL_SECTION_MARKER
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(&existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert!(
            !content.contains("gone-bin"),
            "a rule above the marker is generator-owned, got:\n{content}"
        );
    }

    /// The manual section comes back exactly as written -- including the text
    /// between statements, which is where comments and spacing live. Re-emitting
    /// it statement by statement would drop all of it.
    #[test]
    fn test_manual_section_is_reproduced_byte_for_byte() {
        let manual = concat!(
            "# why this rule exists, and a note nobody should lose\n",
            "load(\"//my:defs.bzl\", \"my_macro\")\n",
            "\n",
            "my_macro(\n",
            "    name = \"widget\",\n",
            "    srcs = glob([\"widgets/**/*.txt\"]),\n",
            "    weird_attr = select({\"DEFAULT\": 1}),\n",
            ")\n",
        );
        let existing =
            format!("# @generated by `cargo buckal`\n\n{MANUAL_SECTION_MARKER}\n\n{manual}");
        let (_tmp, tmp_path, change, ctx) = merge_fixture(&existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        let (_, carried) = content
            .split_once(MANUAL_SECTION_MARKER)
            .expect("marker present");
        assert_eq!(
            carried.trim(),
            manual.trim(),
            "the manual section must come back unchanged, got:\n{carried}"
        );
    }

    /// Two migrations in a row must produce the same bytes.
    #[test]
    fn test_merge_is_idempotent() {
        let existing = format!(
            "# @generated by `cargo buckal`\n\n{MANUAL_SECTION_MARKER}\n\n# keep me\nmy_macro(\n    name = \"widget\",\n)\n"
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(&existing, lib_only());

        change.apply(&ctx);
        let first = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        change.apply(&ctx);
        let second = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");

        assert_eq!(first, second, "second run changed the file:\n{second}");
    }

    /// Without `--merge` the file is regenerated from the manifest alone.
    #[test]
    fn test_without_merge_the_manual_section_is_not_carried() {
        let existing = format!(
            "# @generated by `cargo buckal`\n\n{MANUAL_SECTION_MARKER}\n\nmy_macro(\n    name = \"widget\",\n)\n"
        );
        let (_tmp, tmp_path, change, mut ctx) = merge_fixture(&existing, lib_only());
        ctx.no_merge = true;

        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert!(
            !content.contains("my_macro"),
            "--merge is what asks for carrying, got:\n{content}"
        );
    }

    /// A file written before the marker existed has its ownership inferred
    /// once, so hand-written rules are not lost on the migration that
    /// introduces the marker.
    #[test]
    fn test_legacy_file_carries_its_manual_rules_below_a_new_marker() {
        let existing = concat!(
            "load(\"@buckal//:wrapper.bzl\", \"rust_library\")\n\n",
            "rust_test(\n",
            "    name = \"hand-written-it\",\n",
            "    srcs = glob([\"tests/**/*.rs\"]),\n",
            "    crate = \"hand_written_it\",\n",
            "    crate_root = \"tests/it.rs\",\n",
            "    edition = \"2021\",\n",
            ")\n",
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        let (generated, carried) = content
            .split_once(MANUAL_SECTION_MARKER)
            .expect("marker written");
        assert!(
            carried.contains("hand-written-it"),
            "the hand-written rule belongs below the marker, got:\n{carried}"
        );
        assert!(
            carried.contains("glob([\"tests/**/*.rs\"])"),
            "and it must arrive unchanged, got:\n{carried}"
        );
        assert!(
            generated.contains("rust_library("),
            "generated rules stay above it, got:\n{generated}"
        );
    }

    /// The old `load(...)` header is rebuilt, not carried: it is cargo-buckal's
    /// own, and re-emitting it would grow the file on every run.
    #[test]
    fn test_legacy_generated_loads_are_not_carried() {
        let existing = concat!(
            "load(\"@buckal//:wrapper.bzl\", \"rust_library\")\n",
            "load(\"@buckal//:cargo_manifest.bzl\", \"cargo_manifest\")\n\n",
            "my_macro(\n    name = \"widget\",\n)\n",
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert_eq!(
            content.matches("@buckal//:wrapper.bzl").count(),
            1,
            "the synthesized header must appear exactly once, got:\n{content}"
        );
        assert_eq!(
            content.matches("@buckal//:cargo_manifest.bzl").count(),
            1,
            "and so must the manifest load, got:\n{content}"
        );
    }

    /// A load of anything else was written by hand, and the statement below it
    /// needs whatever it binds. Dropping it leaves a call with no binding.
    #[test]
    fn test_legacy_custom_loads_are_carried_with_their_macro() {
        let existing = concat!(
            "load(\"@buckal//:wrapper.bzl\", \"rust_library\")\n",
            "load(\"//my:defs.bzl\", \"my_macro\")\n\n",
            "my_macro(\n    name = \"widget\",\n)\n",
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert!(
            content.contains("load(\"//my:defs.bzl\", \"my_macro\")"),
            "a hand-written import must survive with the call that needs it, got:\n{content}"
        );
        assert!(
            content.contains("my_macro("),
            "the macro call must survive, got:\n{content}"
        );
    }

    /// A carried rule may name a wrapper rule that no generated rule uses. If
    /// the header does not bind it, the file still loads -- against Buck's
    /// *native* rule of that name rather than the buckal wrapper, which is a
    /// different implementation and fails nowhere visible.
    #[test]
    fn test_carried_rules_get_their_wrapper_binding() {
        let existing = format!(
            concat!(
                "# @generated by `cargo buckal`\n\n{}\n\n",
                "rust_test(\n",
                "    name = \"hand-written-it\",\n",
                "    crate = \"hand_written_it\",\n",
                "    crate_root = \"tests/it.rs\",\n",
                "    edition = \"2021\",\n",
                ")\n",
            ),
            MANUAL_SECTION_MARKER
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(&existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        let header: String = content
            .lines()
            .filter(|line| line.contains("wrapper.bzl"))
            .collect();
        assert!(
            header.contains("rust_test"),
            "the header must bind rust_test for the carried rule; \
             no generated rule needs it. Header was: {header}"
        );
    }

    /// ...but not when the manual section binds it itself, which would be a
    /// duplicate import.
    #[test]
    fn test_manual_section_binding_is_not_duplicated() {
        let existing = format!(
            concat!(
                "# @generated by `cargo buckal`\n\n{}\n\n",
                "load(\"//other:defs.bzl\", \"rust_test\")\n\n",
                "rust_test(\n",
                "    name = \"hand-written-it\",\n",
                ")\n",
            ),
            MANUAL_SECTION_MARKER
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(&existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        let header: String = content
            .lines()
            .filter(|line| line.contains("@buckal//:wrapper.bzl"))
            .collect();
        assert!(
            !header.contains("rust_test"),
            "the manual section already binds rust_test; the header must not \
             shadow it. Header was: {header}"
        );
        assert!(
            content.contains("load(\"//other:defs.bzl\", \"rust_test\")"),
            "the user's own binding must survive, got:\n{content}"
        );
    }

    /// A legacy rule whose name the generator now uses is dropped, not carried:
    /// keeping both would leave two targets with one name.
    #[test]
    fn test_legacy_name_collisions_resolve_to_the_generated_rule() {
        let existing = concat!(
            "rust_library(\n",
            "    name = \"myroot\",\n",
            "    srcs = [\":vendor\"],\n",
            "    crate = \"myroot\",\n",
            "    crate_root = \"vendor/src/lib.rs\",\n",
            "    edition = \"2021\",\n",
            ")\n",
        );
        let specs = vec![
            ("myroot", TargetKind::Lib, "src/lib.rs"),
            ("myroot", TargetKind::Bin, "src/main.rs"),
        ];
        let (_tmp, tmp_path, change, ctx) = merge_fixture(existing, specs);
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert_eq!(
            content.matches("name = \"myroot\",").count(),
            1,
            "exactly one target may be named myroot, got:\n{content}"
        );
        assert!(
            content.contains("name = \"myroot-lib\""),
            "the library should be regenerated under its new name, got:\n{content}"
        );
    }

    /// The bootstrap is a one-time guess; every run after it reads the marker.
    #[test]
    fn test_merge_is_idempotent_across_the_bootstrap() {
        let existing = concat!(
            "load(\"@buckal//:wrapper.bzl\", \"rust_library\")\n\n",
            "# a note worth keeping\n",
            "my_macro(\n    name = \"widget\",\n)\n",
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(existing, lib_only());

        change.apply(&ctx);
        let first = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        change.apply(&ctx);
        let second = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        change.apply(&ctx);
        let third = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");

        assert_eq!(first, second, "second run changed the file:\n{second}");
        assert_eq!(second, third, "third run changed the file:\n{third}");
    }

    /// An aliased import binds a name the generator would never produce, so
    /// nothing could rebuild it -- the load is the user's however familiar its
    /// module looks. Dropping it left the call with no binding.
    #[test]
    fn test_legacy_aliased_wrapper_load_survives() {
        let existing = concat!(
            "load(\"@buckal//:wrapper.bzl\", \"rust_library\")\n",
            "load(\"@buckal//:wrapper.bzl\", manual_test = \"rust_test\")\n\n",
            "manual_test(\n    name = \"manual\",\n)\n",
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert!(
            content.contains("manual_test = \"rust_test\""),
            "the aliased binding must survive, got:\n{content}"
        );
        assert!(
            content.contains("manual_test(\n    name = \"manual\","),
            "and the call that needs it, got:\n{content}"
        );
    }

    /// Whether a symbol is already bound is a question about bindings, not about
    /// text. A module path may contain the symbol's name and bind nothing of the
    /// sort.
    #[test]
    fn test_import_substring_is_not_a_binding() {
        let existing = format!(
            concat!(
                "# @generated by `cargo buckal`\n\n{}\n\n",
                "load(\"//my:rust_test_helpers.bzl\", \"helper\")\n\n",
                "rust_test(\n    name = \"manual\",\n)\n",
            ),
            MANUAL_SECTION_MARKER
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(&existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        let header: String = content
            .lines()
            .filter(|line| line.contains("@buckal//:wrapper.bzl"))
            .collect();
        assert!(
            header.contains("rust_test"),
            "the helper module binds only `helper`, so the wrapper's rust_test is \
             still needed. Header was: {header}"
        );
    }

    /// An alias in the manual section *does* bind the name, so the header must
    /// not also import it under that name.
    #[test]
    fn test_manual_alias_counts_as_a_binding() {
        let existing = format!(
            concat!(
                "# @generated by `cargo buckal`\n\n{}\n\n",
                "load(\"//other:defs.bzl\", rust_test = \"their_test\")\n\n",
                "rust_test(\n    name = \"manual\",\n)\n",
            ),
            MANUAL_SECTION_MARKER
        );
        let (_tmp, tmp_path, change, ctx) = merge_fixture(&existing, lib_only());
        change.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        let header: String = content
            .lines()
            .filter(|line| line.contains("@buckal//:wrapper.bzl"))
            .collect();
        assert!(
            !header.contains("rust_test"),
            "the manual section binds rust_test itself; importing it again would \
             shadow the user's choice. Header was: {header}"
        );
    }

    /// The workspace-export fallback writes a package file too, and runs when no
    /// changed package owned the workspace manifest -- an unchanged root on a
    /// later migration. It must honour the same contract as the main path, or a
    /// preserved file does not stay preserved.
    #[test]
    fn test_workspace_fallback_preserves_the_manual_section() {
        let manual = concat!(
            "# a note worth keeping\n",
            "load(\"//my:defs.bzl\", \"my_macro\")\n",
            "\n",
            "my_macro(\n    name = \"widget\",\n)\n",
        );
        let existing =
            format!("# @generated by `cargo buckal`\n\n{MANUAL_SECTION_MARKER}\n\n{manual}");
        let (_tmp, tmp_path, _change, mut ctx) = merge_fixture(&existing, lib_only());

        // No changed package owns the workspace manifest: only the fallback runs.
        ctx.workspace_inherit = true;
        let empty = BuckalChange {
            changes: BTreeMap::new(),
        };
        empty.apply(&ctx);

        let content = std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK");
        assert!(
            content.contains(MANUAL_SECTION_MARKER),
            "the fallback must not drop the marker, got:\n{content}"
        );
        assert!(
            content.contains("# a note worth keeping"),
            "nor the user's comments, got:\n{content}"
        );
        assert!(
            content.contains("my_macro(") && content.contains("//my:defs.bzl"),
            "nor the macro and its import, got:\n{content}"
        );
        assert!(
            content.contains("export_file("),
            "while still writing the workspace export, got:\n{content}"
        );
        assert_eq!(
            content.matches(MANUAL_SECTION_MARKER).count(),
            1,
            "and exactly one marker, got:\n{content}"
        );
    }

    /// A manual target and a generated one may not share a name. The marker
    /// says the statement is the user's, so the answer is neither to drop it nor
    /// to emit an unloadable package: stop and say which name clashes.
    #[test]
    fn test_manual_and_generated_name_collisions_are_reported() {
        let manual = "rust_test(\n    name = \"myroot\",\n)\n";
        let generated = vec![Rule::RustLibrary(crate::buck::RustLibrary {
            name: "myroot".to_owned(),
            ..Default::default()
        })];

        let clashes = colliding_target_names(manual, Utf8Path::new("BUCK"), &generated);
        assert_eq!(clashes, vec!["myroot".to_owned()]);
    }

    #[test]
    fn test_distinct_manual_names_do_not_collide() {
        let manual = "rust_test(\n    name = \"hand-written-it\",\n)\n";
        let generated = vec![Rule::RustLibrary(crate::buck::RustLibrary {
            name: "myroot".to_owned(),
            ..Default::default()
        })];

        assert!(colliding_target_names(manual, Utf8Path::new("BUCK"), &generated).is_empty());
    }
}
