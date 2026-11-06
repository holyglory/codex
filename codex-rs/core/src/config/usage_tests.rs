//! Usage settings exercise the existing file and CLI configuration loading path.

use super::*;
use crate::config::Config;
use codex_config::UsageConfigToml;
use codex_usage::RetentionPolicy;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn usage_retention_settings_resolve_independently() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let cwd = tempdir()?;
    for (contents, overrides, expected) in [
        (
            "[usage]\nraw_retention_days = 14\n",
            Vec::new(),
            RetentionPolicy {
                raw_retention_days: 14,
                inactive_chat_days: 7,
            },
        ),
        (
            "[usage]\ninactive_chat_days = 21\n",
            Vec::new(),
            RetentionPolicy {
                raw_retention_days: 7,
                inactive_chat_days: 21,
            },
        ),
        (
            "[usage]\nraw_retention_days = 14\ninactive_chat_days = 21\n",
            vec![(
                "usage.raw_retention_days".to_string(),
                TomlValue::Integer(3),
            )],
            RetentionPolicy {
                raw_retention_days: 3,
                inactive_chat_days: 21,
            },
        ),
        (
            "[usage]\nraw_retention_days = 14\ninactive_chat_days = 21\n",
            vec![(
                "usage.inactive_chat_days".to_string(),
                TomlValue::Integer(9),
            )],
            RetentionPolicy {
                raw_retention_days: 14,
                inactive_chat_days: 9,
            },
        ),
        (
            "[usage]\nraw_retention_days = 36500\ninactive_chat_days = 1\n",
            Vec::new(),
            RetentionPolicy {
                raw_retention_days: 36_500,
                inactive_chat_days: 1,
            },
        ),
    ] {
        tokio::fs::write(codex_home.path().join(CONFIG_TOML_FILE), contents).await?;
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                cwd: Some(cwd.path().to_path_buf()),
                ..Default::default()
            })
            .cli_overrides(overrides)
            .build()
            .await?;
        assert_eq!(config.usage, expected);
    }
    Ok(())
}

#[tokio::test]
async fn usage_retention_settings_reject_invalid_days() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let cwd = tempdir()?;
    for field in ["raw_retention_days", "inactive_chat_days"] {
        for days in [0, -1, 36_501, i64::from(u32::MAX), i64::from(u32::MAX) + 1] {
            tokio::fs::write(
                codex_home.path().join(CONFIG_TOML_FILE),
                format!("[usage]\n{field} = {days}\n"),
            )
            .await?;
            ConfigBuilder::without_managed_config_for_tests()
                .codex_home(codex_home.path().to_path_buf())
                .harness_overrides(ConfigOverrides {
                    cwd: Some(cwd.path().to_path_buf()),
                    ..Default::default()
                })
                .build()
                .await
                .expect_err("usage retention days must be positive and bounded");
        }
    }
    Ok(())
}

#[tokio::test]
async fn usage_retention_settings_validate_programmatic_config() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    for days in [0, 36_501, u32::MAX] {
        for (usage, field) in [
            (
                UsageConfigToml {
                    raw_retention_days: Some(days),
                    ..Default::default()
                },
                "raw_retention_days",
            ),
            (
                UsageConfigToml {
                    inactive_chat_days: Some(days),
                    ..Default::default()
                },
                "inactive_chat_days",
            ),
        ] {
            let error = Config::load_from_base_config_with_overrides(
                ConfigToml {
                    usage: Some(usage),
                    ..Default::default()
                },
                ConfigOverrides::default(),
                AbsolutePathBuf::from_absolute_path(codex_home.path())?,
            )
            .await
            .expect_err("programmatic settings cannot bypass retention bounds");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(
                error.to_string(),
                format!("usage.{field} must be between 1 and 36500 days")
            );
        }
    }
    Ok(())
}
