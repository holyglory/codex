use pretty_assertions::assert_eq;

use super::ExecutableIdentity;
use super::executable_identity;
use super::managed_codex_bin;
use super::managed_codex_file_name;
use super::parse_codex_version;

#[test]
fn resolves_managed_install_as_package_layout_changes() -> std::io::Result<()> {
    let codex_home = tempfile::tempdir()?;
    assert_eq!(
        managed_codex_bin(codex_home.path()),
        codex_home
            .path()
            .join("packages/app-server-daemon/current/bin")
            .join(managed_codex_file_name())
    );
    let legacy_state = codex_home.path().join("app-server-daemon");
    std::fs::create_dir(&legacy_state)?;
    std::fs::write(
        legacy_state.join("app-server.stderr.log"),
        b"prior launch fixture",
    )?;
    let current = codex_home.path().join("packages/standalone/current");
    let flat = current.join(managed_codex_file_name());
    let packaged = current.join("bin").join(managed_codex_file_name());

    assert_eq!(
        &managed_codex_bin(codex_home.path()),
        if cfg!(windows) { &packaged } else { &flat }
    );
    std::fs::create_dir_all(current.join("bin"))?;
    std::fs::write(&packaged, b"packaged executable fixture")?;
    assert_eq!(managed_codex_bin(codex_home.path()), packaged);

    std::fs::write(&flat, b"flat executable fixture")?;
    assert_eq!(managed_codex_bin(codex_home.path()), packaged);

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

#[tokio::test]
async fn executable_identity_uses_binary_contents() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let executable = directory.path().join("codex");
    // Span multiple reads, including a partial final buffer, and preserve the
    // digest stored by older clients that hashed the complete file in memory.
    let mut bytes: Vec<u8> = (0..200_003).map(|index| (index % 251) as u8).collect();
    for contents in [&bytes[..], &[][..]] {
        std::fs::write(&executable, contents).expect("write executable");
        assert_eq!(
            executable_identity(&executable).await.expect("identity"),
            ExecutableIdentity {
                digest: *blake3::hash(contents).as_bytes(),
            }
        );
    }
    std::fs::write(&executable, &bytes).expect("write executable");
    let old = executable_identity(&executable).await.expect("identity");
    bytes[100_000] ^= 1;
    std::fs::write(&executable, bytes).expect("replace executable");
    assert_ne!(
        executable_identity(&executable)
            .await
            .expect("new identity"),
        old
    );
}
