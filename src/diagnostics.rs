//! Reading the diagnostic streams Buck2's Rust rules produce.
//!
//! The prelude compiles every Rust target a second time under `clippy-driver`
//! and captures the result as the `[clippy.json]` sub-target. That build is
//! *infallible* by construction — `rust_library.bzl` passes
//! `infallible_diagnostics = True` and, unlike the `metadata-fast` path, the
//! clippy emit skips `failure_filter` (`build.bzl`). The action therefore
//! succeeds no matter what clippy found, which is what makes reading the
//! artifact the only way to know.
//!
//! The file is a JSON-lines stream written by the prelude's `rustc_action.py`:
//! one rustc `--error-format=json` record per line, each carrying the
//! human-readable form in `rendered`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{buckal_error, buckal_log, buckal_warn};

/// One record of a rustc JSON diagnostic stream.
///
/// Deliberately partial: rustc's schema is large and unstable, and everything
/// needed here (what to print, whether it was fatal) lives in three fields.
#[derive(Debug, Default, Deserialize)]
pub struct Diagnostic {
    /// `error`, `warning`, `note`, `help`, … Absent on the prelude's own
    /// unused-dependency records.
    #[serde(default)]
    pub level: Option<String>,
    /// The formatted text rustc would have printed to a terminal.
    #[serde(default)]
    pub rendered: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl Diagnostic {
    fn is_error(&self) -> bool {
        // rustc uses `error` and `error: internal compiler error`.
        self.level
            .as_deref()
            .is_some_and(|l| l.starts_with("error"))
    }

    fn is_warning(&self) -> bool {
        self.level.as_deref() == Some("warning")
    }

    /// rustc closes a failing compilation with `aborting due to N previous
    /// errors` and a run with `N warnings emitted`. They are summaries of the
    /// records around them, so counting them would double-report.
    fn is_tally(&self) -> bool {
        let Some(message) = self.message.as_deref() else {
            return false;
        };
        message.starts_with("aborting due to")
            || (message.ends_with("warning emitted") || message.ends_with("warnings emitted"))
    }
}

/// How many real diagnostics a run produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DiagnosticSummary {
    pub errors: usize,
    pub warnings: usize,
}

impl DiagnosticSummary {
    pub fn is_clean(&self) -> bool {
        self.errors == 0 && self.warnings == 0
    }
}

/// Absolute artifact paths from a `buck2 build --show-full-json-output` run.
///
/// Buck2 writes the map to stdout and everything else to stderr, but the
/// parse is defensive about leading noise: a daemon that logs a line to stdout
/// should degrade to a warning, not lose every diagnostic in the build.
/// Targets that produced no output map to `""` and are skipped.
pub fn output_paths(stdout: &str) -> Result<Vec<PathBuf>> {
    let object = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| stdout.trim());

    if object.is_empty() {
        bail!("`buck2 build --show-full-json-output` produced no output map");
    }

    let map: BTreeMap<String, String> = serde_json::from_str(object)
        .context("failed to parse the output map from `buck2 build --show-full-json-output`")?;

    Ok(map
        .into_values()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Parse one `[clippy.json]` artifact.
///
/// Unparseable lines are skipped rather than failing the run: the stream is
/// append-only and a future rustc record we cannot model should cost one lost
/// diagnostic, not the whole report.
pub fn parse_stream(contents: &str) -> Vec<Diagnostic> {
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Diagnostic>(line).ok())
        .collect()
}

/// Deduplicate and count.
///
/// Returns the blocks to print, in first-seen order, and the tally. Dedup is
/// on the rendered text because a crate's lib, test, and bin targets each get
/// their own `[clippy.json]` covering overlapping sources — without it, one
/// lint in `lib.rs` prints once per target that compiles it.
pub fn render(diagnostics: Vec<Diagnostic>) -> (Vec<String>, DiagnosticSummary) {
    let mut seen = Vec::new();
    let mut summary = DiagnosticSummary::default();

    for diagnostic in diagnostics {
        let Some(rendered) = diagnostic.rendered.as_deref() else {
            continue;
        };
        let rendered = rendered.trim_end();
        if rendered.is_empty() || seen.iter().any(|s: &String| s == rendered) {
            continue;
        }
        seen.push(rendered.to_owned());

        if diagnostic.is_tally() {
            continue;
        }
        if diagnostic.is_error() {
            summary.errors += 1;
        } else if diagnostic.is_warning() {
            summary.warnings += 1;
        }
    }

    (seen, summary)
}

/// Read every artifact named by a `--show-full-json-output` map and reduce it
/// to the blocks to print and the tally.
///
/// An unreadable artifact warns rather than aborts: one missing file should
/// cost its own diagnostics, not the rest of the run's.
pub fn collect(stdout: &str) -> Result<(Vec<String>, DiagnosticSummary)> {
    let paths = output_paths(stdout)?;

    let mut records = Vec::new();
    for path in &paths {
        match std::fs::read_to_string(path) {
            Ok(contents) => records.extend(parse_stream(&contents)),
            Err(e) => buckal_warn!(format!(
                "could not read diagnostics at `{}`: {e}",
                path.display()
            )),
        }
    }

    Ok(render(records))
}

/// Print the diagnostics and a closing summary; report whether the command
/// should succeed.
///
/// Follows cargo: warnings alone are not a failure, errors are. `what` names
/// the command in the summary line (`clippy`, `check`).
pub fn report(blocks: &[String], summary: &DiagnosticSummary, what: &str) -> bool {
    for block in blocks {
        eprintln!("{block}");
    }

    if summary.errors > 0 {
        buckal_error!(format!(
            "{what} found {} error(s) and {} warning(s)",
            summary.errors, summary.warnings
        ));
        return false;
    }

    if summary.warnings > 0 {
        buckal_warn!(format!("{what} found {} warning(s)", summary.warnings));
    } else {
        buckal_log!("Finished", format!("{what} found no issues"));
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn output_paths_reads_the_map_and_skips_outputless_targets() {
        let stdout = r#"{"root//a:a[clippy.json]":"C:\\o\\a.json","root//b:b[clippy.json]":"","root//c:c[clippy.json]":"C:\\o\\c.json"}"#;

        let paths = output_paths(stdout).expect("parse");

        assert_eq!(
            paths,
            vec![PathBuf::from(r"C:\o\a.json"), PathBuf::from(r"C:\o\c.json")]
        );
    }

    #[test]
    fn output_paths_tolerates_a_line_before_the_map() {
        let stdout = "some daemon chatter\n{\"root//a:a\":\"/o/a.json\"}\n";

        let paths = output_paths(stdout).expect("parse");

        assert_eq!(paths, vec![PathBuf::from("/o/a.json")]);
    }

    #[test]
    fn output_paths_errors_on_empty_output() {
        assert!(output_paths("   \n").is_err());
    }

    #[test]
    fn parse_stream_skips_blank_and_unparseable_lines() {
        let contents = stream(&[
            r#"{"level":"warning","message":"unused variable","rendered":"warning: unused variable\n"}"#,
            "",
            "not json at all",
            r#"{"level":"error","message":"mismatched types","rendered":"error: mismatched types\n"}"#,
        ]);

        let diagnostics = parse_stream(&contents);

        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].level.as_deref(), Some("warning"));
        assert_eq!(diagnostics[1].level.as_deref(), Some("error"));
    }

    #[test]
    fn render_counts_errors_and_warnings() {
        let diagnostics = parse_stream(&stream(&[
            r#"{"level":"warning","message":"w1","rendered":"warning: w1"}"#,
            r#"{"level":"warning","message":"w2","rendered":"warning: w2"}"#,
            r#"{"level":"error","message":"e1","rendered":"error: e1"}"#,
        ]));

        let (blocks, summary) = render(diagnostics);

        assert_eq!(blocks.len(), 3);
        assert_eq!(
            summary,
            DiagnosticSummary {
                errors: 1,
                warnings: 2
            }
        );
        assert!(!summary.is_clean());
    }

    /// lib, test and bin targets of one crate each emit a `[clippy.json]`
    /// covering the same sources, so the same lint arrives several times.
    #[test]
    fn render_deduplicates_across_targets() {
        let one = r#"{"level":"warning","message":"unused","rendered":"warning: unused\n --> src/lib.rs:1:1"}"#;
        let diagnostics = parse_stream(&stream(&[one, one, one]));

        let (blocks, summary) = render(diagnostics);

        assert_eq!(blocks.len(), 1);
        assert_eq!(summary.warnings, 1);
    }

    /// rustc's closing tallies restate the records above them.
    #[test]
    fn render_prints_tallies_without_counting_them() {
        let diagnostics = parse_stream(&stream(&[
            r#"{"level":"error","message":"mismatched types","rendered":"error: mismatched types"}"#,
            r#"{"level":"error","message":"aborting due to 1 previous error","rendered":"error: aborting due to 1 previous error"}"#,
            r#"{"level":"warning","message":"1 warning emitted","rendered":"warning: 1 warning emitted"}"#,
        ]));

        let (blocks, summary) = render(diagnostics);

        // Both tallies still reach the user; neither inflates the count.
        assert_eq!(blocks.len(), 3);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.warnings, 0);
    }

    #[test]
    fn render_skips_records_with_nothing_to_print() {
        let diagnostics = parse_stream(&stream(&[
            r#"{"unused_extern_names":[]}"#,
            r#"{"level":"warning","message":"w","rendered":""}"#,
        ]));

        let (blocks, summary) = render(diagnostics);

        assert!(blocks.is_empty());
        assert!(summary.is_clean());
    }

    /// An internal compiler error is still an error for exit-code purposes.
    #[test]
    fn render_treats_ice_as_an_error() {
        let diagnostics = parse_stream(&stream(&[
            r#"{"level":"error: internal compiler error","message":"ice","rendered":"error: internal compiler error"}"#,
        ]));

        let (_, summary) = render(diagnostics);

        assert_eq!(summary.errors, 1);
    }
}
