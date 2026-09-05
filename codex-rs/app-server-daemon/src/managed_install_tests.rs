use pretty_assertions::assert_eq;

use super::executable_identity_from_bytes;
use super::managed_codex_bin;
use super::managed_codex_file_name;
use super::parse_codex_version;

#[test]
fn resolves_managed_install_as_package_layout_changes() -> std::io::Result<()> {
    let codex_home = tempfile::tempdir()?;
    let current = codex_home.path().join("packages/standalone/current");
    let flat = current.join(managed_codex_file_name());
    let packaged = current.join("bin").join(managed_codex_file_name());

    assert_eq!(managed_codex_bin(codex_home.path()), flat);
    std::fs::create_dir_all(current.join("bin"))?;
    std::fs::write(&packaged, b"packaged executable fixture")?;
    assert_eq!(managed_codex_bin(codex_home.path()), packaged);

    std::fs::write(&flat, b"flat executable fixture")?;
    assert_eq!(managed_codex_bin(codex_home.path()), flat);

    std::fs::remove_file(&flat)?;
    std::fs::create_dir(&flat)?;
    assert_eq!(managed_codex_bin(codex_home.path()), packaged);
    Ok(())
}

#[test]
fn parses_codex_cli_version_output() {
    assert_eq!(
        parse_codex_version("codex 1.2.3\n").expect("version"),
        "1.2.3"
    );
}

#[test]
fn rejects_malformed_codex_cli_version_output() {
    assert!(parse_codex_version("codex\n").is_err());
}

#[test]
fn executable_identity_uses_binary_contents() {
    let old = executable_identity_from_bytes(b"old");
    let same = executable_identity_from_bytes(b"old");
    let new = executable_identity_from_bytes(b"new");

    assert_eq!(old, same);
    assert_ne!(old, new);
}
