/// The current Codex CLI version as embedded at compile time.
#[cfg(not(test))]
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

// Render unit-test fixtures consistently under Cargo and Bazel. Package and CLI
// integration tests exercise the embedded release version in production builds.
#[cfg(test)]
pub const CODEX_CLI_VERSION: &str = "0.0.0";
