/// The current Codex CLI version as embedded at compile time.
#[cfg(not(test))]
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Keep snapshot geometry stable when tests run from a versioned release tag.
#[cfg(test)]
pub const CODEX_CLI_VERSION: &str = "0.0.0";
