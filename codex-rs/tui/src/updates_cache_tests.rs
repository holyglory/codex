use super::*;
use crate::legacy_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[tokio::test]
async fn dismiss_version_creates_cache_file_when_missing() {
    let codex_home = tempdir().expect("temp codex home");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let version_file = version_filepath(&config);

    dismiss_version(&config, "999.0.0")
        .await
        .expect("dismiss version");

    let info = read_version_info(&version_file).expect("read version info");
    assert_eq!(info.last_checked_at, DateTime::<Utc>::UNIX_EPOCH);
    assert_eq!(
        (
            info.latest_version.as_str(),
            info.dismissed_version.as_deref()
        ),
        ("999.0.0", Some("999.0.0"))
    );
}

#[tokio::test]
async fn fork_update_cache_does_not_consume_or_overwrite_upstream_cache() {
    let codex_home = tempdir().expect("temp codex home");
    let upstream_file = codex_home.path().join("version.json");
    let upstream = serde_json::to_string(&VersionInfo {
        latest_version: "999.0.0".to_string(),
        last_checked_at: Utc::now(),
        dismissed_version: Some("999.0.0".to_string()),
    })
    .unwrap();
    std::fs::write(&upstream_file, &upstream).unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .unwrap();
    assert!(read_version_info(&version_filepath(&config)).is_err());

    dismiss_version(&config, "0.153.0-multi.2").await.unwrap();

    let info = read_version_info(&version_filepath(&config)).unwrap();
    assert_eq!(
        (
            info.latest_version,
            info.dismissed_version,
            info.last_checked_at
        ),
        (
            "0.153.0-multi.2".to_string(),
            Some("0.153.0-multi.2".to_string()),
            DateTime::<Utc>::UNIX_EPOCH
        )
    );
    assert_eq!(std::fs::read_to_string(&upstream_file).unwrap(), upstream);
}
