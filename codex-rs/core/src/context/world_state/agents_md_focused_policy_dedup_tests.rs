use super::*;
use crate::context::world_state::AgentsMdState;
use crate::context::world_state::WorldState;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::TempDir;

fn policy_fixture() -> io::Result<(TempDir, AbsolutePathBuf, String)> {
    let directory = tempfile::tempdir()?;
    let source = AbsolutePathBuf::try_from(directory.path().join("AGENTS.md"))?;
    let core = "<!-- codex:focused-policy:v1 -->\nVerified mandatory core\n".to_string();
    fs::write(&source, &core)?;
    fs::create_dir(directory.path().join("modules"))?;
    fs::write(
        directory.path().join("modules/rules.md"),
        "Selected details",
    )?;
    fs::write(
        directory.path().join("modules.json"),
        r#"{"version":1,"modules":[{"id":"rules","relativePath":"modules/rules.md","applicability":["always"]}]}"#,
    )?;
    Ok((directory, source, core))
}

#[tokio::test]
async fn focused_policy_dedup_requires_exact_source_and_core() -> io::Result<()> {
    let (directory, source, core) = policy_fixture()?;
    let canonical = fs::canonicalize(&source)?;
    let other = AbsolutePathBuf::try_from(directory.path().join("other.md"))?;
    let missing = AbsolutePathBuf::try_from(directory.path().join("missing.md"))?;
    fs::write(&other, &core)?;
    let matched = LoadedAgentsMd::new_user(core.clone(), source.clone());
    assert_eq!(
        matched.clone_without_focused_core(&canonical, &core).await,
        Some(LoadedAgentsMd::default())
    );
    let trimmed = LoadedAgentsMd::new_user(core.trim().to_string(), source.clone());
    assert_eq!(
        trimmed.clone_without_focused_core(&canonical, &core).await,
        Some(LoadedAgentsMd::default())
    );
    for original in [
        LoadedAgentsMd::new_user(format!("{core}\nCustom user instruction"), source.clone()),
        LoadedAgentsMd::new_user(core.clone(), other),
        LoadedAgentsMd::new_user(core.clone(), missing),
        LoadedAgentsMd::from_text_for_testing(&core),
    ] {
        let saved = original.clone();
        assert_eq!(
            original.clone_without_focused_core(&canonical, &core).await,
            None
        );
        assert_eq!(original, saved);
        let mut world = WorldState::default();
        assert!(
            AgentsMdState::add_focused_policy(
                &mut world,
                source.as_path(),
                &["specification"],
                Some(&original)
            )
            .await?
            .is_none()
        );
    }
    assert_eq!(matched, LoadedAgentsMd::new_user(core, source));
    Ok(())
}

#[tokio::test]
async fn focused_policy_dedup_preserves_project_entries_and_provenance() -> io::Result<()> {
    let (directory, source, core) = policy_fixture()?;
    let mut original = LoadedAgentsMd::new_user(core.clone(), source.clone());
    for (environment_id, text) in [
        ("first", core.clone()),
        ("second", "Scoped project rules".to_string()),
    ] {
        let cwd = AbsolutePathBuf::try_from(directory.path().join(environment_id))?;
        fs::create_dir(&cwd)?;
        let project_source = cwd.join("AGENTS.md");
        fs::write(&project_source, &text)?;
        original.entries.push(InstructionEntry {
            contents: text,
            provenance: InstructionProvenance::Project {
                source_path: PathUri::from_abs_path(&project_source),
                environment_id: environment_id.to_string(),
                cwd: PathUri::from_abs_path(&cwd),
            },
        });
    }
    original.entries.push(InstructionEntry {
        contents: "Internal instructions".to_string(),
        provenance: InstructionProvenance::Internal,
    });
    let saved = original.clone();
    let expected = LoadedAgentsMd {
        user_instructions: None,
        entries: original.entries.clone(),
    };
    assert_eq!(
        original
            .clone_without_focused_core(&fs::canonicalize(&source)?, &core)
            .await,
        Some(expected.clone())
    );
    assert_eq!(original, saved);
    let mut world = WorldState::default();
    let filtered = AgentsMdState::add_focused_policy(
        &mut world,
        source.as_path(),
        &["specification"],
        Some(&original),
    )
    .await?
    .unwrap();
    world.add_section(filtered);
    let mut expected_legacy = WorldState::default();
    expected_legacy.add_section(AgentsMdState::new(Some(&expected)));
    assert_eq!(
        world.render_full().last().unwrap().render(),
        expected_legacy.render_full()[0].render()
    );
    Ok(())
}

#[tokio::test]
async fn focused_policy_dedup_is_append_only_and_quiet_after_first_update() -> io::Result<()> {
    let (_directory, source, core) = policy_fixture()?;
    let original = LoadedAgentsMd::new_user(core.clone(), source.clone());
    let mut before = WorldState::default();
    let no_legacy = None;
    AgentsMdState::add_focused_policy(&mut before, source.as_path(), &["specification"], no_legacy)
        .await?;
    before.add_section(AgentsMdState::new(Some(&original)));
    let history = before
        .render_full()
        .into_iter()
        .map(codex_extension_api::ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();
    let saved_history = history.clone();
    let mut after = WorldState::default();
    let filtered = AgentsMdState::add_focused_policy(
        &mut after,
        source.as_path(),
        &["specification"],
        Some(&original),
    )
    .await?
    .unwrap();
    after.add_section(filtered);
    let full = after.render_full();
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].render().matches(&core).count(), 1);
    let update = after.render_diff(&before.snapshot());
    assert_eq!(update.len(), 1);
    assert!(
        update[0]
            .render()
            .contains("focused universal policy remains in effect")
    );
    assert!(!update[0].render().contains(&core));
    assert!(after.render_diff(&after.snapshot()).is_empty());
    assert_eq!(history, saved_history);
    assert_eq!(original, LoadedAgentsMd::new_user(core, source));
    Ok(())
}

#[tokio::test]
async fn focused_policy_dedup_keeps_legacy_files_and_mismatched_revisions() -> io::Result<()> {
    let (_directory, source, core) = policy_fixture()?;
    let original = LoadedAgentsMd::new_user(core.clone(), source.clone());
    let changed = format!("{core}Changed core revision\n");
    fs::write(&source, &changed)?;
    let mut world = WorldState::default();
    assert!(
        AgentsMdState::add_focused_policy(
            &mut world,
            source.as_path(),
            &["specification"],
            Some(&original)
        )
        .await?
        .is_none()
    );
    let legacy = "Ordinary full AGENTS instructions";
    fs::write(&source, legacy)?;
    let original = LoadedAgentsMd::new_user(legacy.to_string(), source.clone());
    let mut world = WorldState::default();
    assert!(
        AgentsMdState::add_focused_policy(
            &mut world,
            source.as_path(),
            &["specification"],
            Some(&original)
        )
        .await?
        .is_none()
    );
    assert!(world.render_full().is_empty());
    assert_eq!(original.text(), legacy);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn focused_policy_dedup_accepts_canonical_equivalent_symlink_sources() -> io::Result<()> {
    let (_directory, source, core) = policy_fixture()?;
    let home = tempfile::tempdir()?;
    let alias = AbsolutePathBuf::try_from(home.path().join("AGENTS.md"))?;
    std::os::unix::fs::symlink(&source, &alias)?;
    let original = LoadedAgentsMd::new_user(core.clone(), alias);
    assert_eq!(
        original
            .clone_without_focused_core(&fs::canonicalize(&source)?, &core)
            .await,
        Some(LoadedAgentsMd::default())
    );
    let mut world = WorldState::default();
    assert!(
        AgentsMdState::add_focused_policy(
            &mut world,
            source.as_path(),
            &["specification"],
            Some(&original)
        )
        .await?
        .is_some()
    );
    Ok(())
}
