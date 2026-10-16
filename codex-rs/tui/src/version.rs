/// The current Codex CLI version as embedded at compile time.
// Render unit-test fixtures with a stable version. Installed-package checks
// separately verify the real release version, including the downstream suffix.
pub const CODEX_CLI_VERSION: &str = if cfg!(test) {
    "0.0.0"
} else {
    env!("CARGO_PKG_VERSION")
};
