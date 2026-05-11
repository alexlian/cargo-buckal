pub mod assets;
pub mod buck;
pub mod buck2;
pub mod buckify;
pub mod bundles;
pub mod cache;
pub mod cli;
pub mod commands;
pub mod config;
pub mod context;
pub mod filter;
pub mod platform;
pub mod registry;
pub mod resolve;
pub mod utils;
pub mod workspace;

use std::sync::OnceLock;

pub const RUST_ROOT: &str = "third-party/rust";
pub const RUST_CRATES_ROOT: &str = "third-party/rust/crates";
pub const RUST_GIT_ROOT: &str = "third-party/rust/git";
pub const RUST_LOCAL_ROOT: &str = "third-party/rust/local";
pub const BUCKAL_BUNDLES_REPO: &str = "alexlian/buckal-bundles";
// fallback commit hash used when fetching the latest from BUCKAL_BUNDLES_REPO fails
pub const DEFAULT_BUNDLE_HASH: &str = "1697cc5eef8977b7073e357c17c0fb63fe4b361d";

pub fn build_version() -> &'static str {
    static VERSION_STRING: OnceLock<String> = OnceLock::new();
    VERSION_STRING.get_or_init(|| {
        let pkg_version = env!("CARGO_PKG_VERSION");
        let is_dev = option_env!("DEV_BUILD").unwrap_or("false") == "true";
        if is_dev {
            let git_hash = option_env!("GIT_HASH").unwrap_or("unknown");
            let commit_date = option_env!("COMMIT_DATE").unwrap_or("unknown");
            format!("{}-dev ({} {})", pkg_version, git_hash, commit_date)
        } else {
            pkg_version.to_string()
        }
    })
}

pub fn user_agent() -> &'static str {
    static USER_AGENT_STRING: OnceLock<String> = OnceLock::new();
    USER_AGENT_STRING.get_or_init(|| {
        let pkg_version = env!("CARGO_PKG_VERSION");
        format!("buckal/{}", pkg_version)
    })
}
