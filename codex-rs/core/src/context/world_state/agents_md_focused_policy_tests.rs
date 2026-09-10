use super::*;
use crate::agents_md::LoadedAgentsMd;
use crate::context::ContextualUserFragment;
use crate::context::world_state::AgentsMdState;
use crate::context::world_state::WorldState;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

fn fixture(modules: &[(&str, &[&str], &str)]) -> io::Result<TempDir> {
    let directory = tempfile::tempdir()?;
    std::fs::create_dir(directory.path().join("modules"))?;
    std::fs::write(
        directory.path().join("AGENTS.md"),
        format!("{MARKER}\nMandatory core"),
    )?;
    let mut entries = Vec::new();
    for (id, applicability, text) in modules {
        let path = format!("modules/{id}.md");
        std::fs::write(directory.path().join(&path), text)?;
        entries.push(json!({ "id": id, "relativePath": path, "applicability": applicability }));
    }
    std::fs::write(
        directory.path().join("modules.json"),
        json!({ "version": 1, "modules": entries }).to_string(),
    )?;
    Ok(directory)
}

async fn state(directory: &TempDir, applicability: &[&str]) -> io::Result<WorldState> {
    let mut state = WorldState::default();
    let loaded = None;
    AgentsMdState::add_focused_policy(
        &mut state,
        &directory.path().join("AGENTS.md"),
        applicability,
        loaded,
    )
    .await?;
    Ok(state)
}

#[tokio::test]
async fn focused_policy_core_order_and_unknown_work_fallback() -> io::Result<()> {
    let directory = fixture(&[
        ("common", &["always"], "Always required"),
        (
            "implementation",
            &["implementation"],
            "Implementation details",
        ),
        ("future", &["future-work"], "Uncertain selector details"),
    ])?;
    let known = load(&directory.path().join("AGENTS.md"), &["specification"])
        .await?
        .unwrap()
        .chunks;
    let expected = format!(
        "{MARKER}\nMandatory core\n\n# Policy module: common\n\nAlways required\n\n# Policy module: future\n\nUncertain selector details"
    );
    assert_eq!(
        known,
        vec![PolicyChunk {
            revision: format!("{:x}", Sha1::digest(expected.as_bytes())),
            text: expected
        }]
    );
    for applicability in [vec![], vec!["unknown"], vec!["specification", "unknown"]] {
        let fallback = load(&directory.path().join("AGENTS.md"), &applicability)
            .await?
            .unwrap()
            .chunks;
        let text = fallback
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>();
        assert!(text.contains("Applicability is uncertain"));
        let positions = [
            "Always required",
            "Implementation details",
            "Uncertain selector details",
        ]
        .map(|part| text.find(part).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }
    Ok(())
}

#[tokio::test]
async fn focused_policy_tracks_purpose_and_action_without_delivery_for_specification()
-> io::Result<()> {
    let directory = fixture(&[
        ("docs", &["documentation"], "Document work"),
        ("analysis", &["diagnosis"], "Diagnosis work"),
        ("code", &["implementation"], "Code work"),
        ("delivery", &["delivery"], "Deliver a target"),
        ("interface", &["ui"], "Interface work"),
    ])?;
    for (tags, selected) in [
        (vec!["discussion"], vec![]),
        (vec!["specification"], vec!["docs"]),
        (vec!["analysis"], vec!["analysis"]),
        (vec!["implementation"], vec!["code"]),
        (vec!["recovery"], vec!["analysis", "code", "delivery"]),
        (vec!["specification", "ui"], vec!["docs", "interface"]),
    ] {
        let chunks = load(&directory.path().join("AGENTS.md"), &tags)
            .await?
            .unwrap()
            .chunks;
        let text = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>();
        let actual = text
            .lines()
            .filter_map(|line| line.strip_prefix("# Policy module: "))
            .collect::<Vec<_>>();
        assert_eq!(actual, selected);
    }
    Ok(())
}

#[tokio::test]
async fn focused_policy_updates_once_and_preserves_project_precedence_and_history() -> io::Result<()>
{
    let directory = fixture(&[("rules", &["always"], "First revision")])?;
    let mut before = state(&directory, &["analysis"]).await?;
    let project = LoadedAgentsMd::from_text_for_testing("Project-specific instructions");
    before.add_section(AgentsMdState::new(Some(&project)));
    let history = before
        .render_full()
        .into_iter()
        .map(codex_extension_api::ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();
    let saved_history = history.clone();
    assert!(before.render_diff(&before.snapshot()).is_empty());
    std::fs::write(directory.path().join("modules/rules.md"), "Second revision")?;
    let mut after = state(&directory, &["analysis"]).await?;
    after.add_section(AgentsMdState::new(Some(&project)));
    let update = after.render_diff(&before.snapshot());
    assert_eq!(update.len(), 1);
    assert_eq!(update[0].role(), "user");
    assert!(update[0].render().contains("Second revision"));
    assert!(update[0].requires_separate_message());
    assert!(AgentsMdState::matches_focused_policy(&update[0].render()));
    assert!(!AgentsMdState::matches_focused_policy(
        &project.contextual_user_fragment().render()
    ));
    assert_eq!(
        after.render_full().last().unwrap().render(),
        before.render_full().last().unwrap().render()
    );
    assert!(after.render_diff(&after.snapshot()).is_empty());
    assert_eq!(history, saved_history);
    let updated_history = history
        .into_iter()
        .chain(
            update
                .into_iter()
                .map(codex_extension_api::ContextualUserFragment::into_boxed_response_item),
        )
        .collect::<Vec<_>>();
    assert!(
        after
            .render_history_diff(Some(&after.snapshot()), &updated_history)
            .is_empty()
    );
    assert_eq!(
        after
            .render_history_diff(Some(&after.snapshot()), std::iter::empty())
            .len(),
        1
    );
    assert_eq!(after.render_history_diff(None, &updated_history).len(), 2);
    Ok(())
}

#[tokio::test]
async fn focused_policy_is_chunked_and_selection_removal_is_explicit() -> io::Result<()> {
    let large = "Значение 🦀\n".repeat(1300);
    let directory = fixture(&[("large", &["implementation"], &large)])?;
    let before = state(&directory, &["implementation"]).await?;
    let rendered = before.render_full();
    assert!(rendered.len() > 1);
    assert!(rendered[0].render().contains("Policy v1: ordered parts"));
    assert!(
        rendered
            .iter()
            .skip(1)
            .all(|fragment| !fragment.render().contains("Policy v1: ordered parts"))
    );
    assert!(rendered.iter().all(|fragment| {
        fragment
            .render()
            .starts_with("<focused_universal_policy>\nrevision=")
    }));
    assert!(
        rendered
            .iter()
            .all(|fragment| fragment.render().len() < CHUNK_BYTES + 512
                && fragment.requires_separate_message())
    );
    let chunks = load(&directory.path().join("AGENTS.md"), &["implementation"])
        .await?
        .unwrap()
        .chunks;
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>(),
        format!("{MARKER}\nMandatory core\n\n# Policy module: large\n\n{large}")
    );
    let after = state(&directory, &["specification"]).await?;
    let changes = after.render_diff(&before.snapshot());
    assert_eq!(changes.len(), rendered.len());
    assert!(
        changes
            .iter()
            .skip(1)
            .all(|fragment| fragment.render().contains("no longer applies"))
    );
    assert!(after.render_diff(&after.snapshot()).is_empty());
    Ok(())
}

#[tokio::test]
async fn focused_policy_leaves_full_agents_repositories_unchanged() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let text = "Full legacy instruction\n".repeat(4000);
    std::fs::write(directory.path().join("AGENTS.md"), &text)?;
    std::fs::write(
        directory.path().join("modules.json"),
        "not a policy manifest",
    )?;
    let mut loaded = state(&directory, &["implementation"]).await?;
    let legacy = LoadedAgentsMd::from_text_for_testing(&text);
    loaded.add_section(AgentsMdState::new(Some(&legacy)));
    assert_eq!(loaded.render_full().len(), 1);
    assert_eq!(
        loaded.render_full()[0].render(),
        legacy.contextual_user_fragment().render()
    );
    Ok(())
}

#[tokio::test]
async fn focused_policy_rejects_unsafe_paths_versions_and_duplicates() -> io::Result<()> {
    let directory = fixture(&[("rules", &["always"], "Required detail")])?;
    for path in [
        "../outside.md",
        "/tmp/outside.md",
        "modules/../../outside.md",
        "modules\\outside.md",
        "C:/outside.md",
        "https://example.test/policy",
        "modules//rules.md",
        "modules/./rules.md",
    ] {
        std::fs::write(directory.path().join("modules.json"), json!({"version":1,"modules":[{"id":"rules","relativePath":path,"applicability":["implementation"]}]}).to_string())?;
        assert_eq!(
            state(&directory, &["specification"])
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    let entry = json!({"id":"rules","relativePath":"modules/rules.md","applicability":["always"]});
    for manifest in [
        json!({"version":2,"modules":[entry.clone()]}),
        json!({"version":1,"modules":[entry.clone(),entry.clone()]}),
        json!({"version":1,"modules":[entry.clone(),{"id":"other","relativePath":"modules/rules.md","applicability":["always"]}]}),
        json!({"version":1,"modules":[]}),
        json!({"version":1,"modules":vec![entry;33]}),
    ] {
        std::fs::write(directory.path().join("modules.json"), manifest.to_string())?;
        assert_eq!(
            state(&directory, &["analysis"]).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "<!-- codex:focused-policy:v2 -->\nCore",
    )?;
    assert_eq!(
        state(&directory, &["analysis"]).await.unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}

#[tokio::test]
async fn focused_policy_errors_do_not_silently_drop_required_details() -> io::Result<()> {
    let directory = fixture(&[("rules", &["always"], "Required detail")])?;
    for text in [vec![b'x'; MAX_FILE_BYTES + 1], vec![0xff], Vec::new()] {
        std::fs::write(directory.path().join("modules/rules.md"), text)?;
        assert_eq!(
            state(&directory, &["analysis"]).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    std::fs::remove_file(directory.path().join("modules/rules.md"))?;
    assert_eq!(
        state(&directory, &["analysis"]).await.unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    std::fs::write(
        directory.path().join("modules.json"),
        " ".repeat(MAX_MANIFEST_BYTES + 1),
    )?;
    assert_eq!(
        state(&directory, &["analysis"]).await.unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}

#[tokio::test]
async fn focused_policy_failure_is_atomic_and_core_is_bounded() -> io::Result<()> {
    let directory = fixture(&[("rules", &["always"], "Required detail")])?;
    let mut current = WorldState::default();
    let instructions = LoadedAgentsMd::from_text_for_testing("Existing project rules");
    current.add_section(AgentsMdState::new(Some(&instructions)));
    let before = current.snapshot();
    std::fs::remove_file(directory.path().join("modules.json"))?;
    assert_eq!(
        AgentsMdState::add_focused_policy(
            &mut current,
            &directory.path().join("AGENTS.md"),
            &["analysis"],
            Some(&instructions)
        )
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::NotFound
    );
    assert!(current.render_diff(&before).is_empty());
    std::fs::write(
        directory.path().join("AGENTS.md"),
        format!("{MARKER}\n{}", "x".repeat(MAX_FILE_BYTES)),
    )?;
    assert_eq!(
        state(&directory, &["analysis"]).await.unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}

#[tokio::test]
async fn focused_policy_total_budget_fails_instead_of_truncating() -> io::Result<()> {
    let detail = "x".repeat(MAX_FILE_BYTES);
    let directory = fixture(&[
        ("one", &["always"], &detail),
        ("two", &["always"], &detail),
        ("three", &["always"], &detail),
        ("four", &["always"], &detail),
    ])?;
    assert_eq!(
        state(&directory, &["analysis"]).await.unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn focused_policy_supports_installed_core_links_but_confines_module_links() -> io::Result<()>
{
    use std::os::unix::fs::symlink;
    let directory = fixture(&[("rules", &["always"], "Required detail")])?;
    let home = tempfile::tempdir()?;
    symlink(
        directory.path().join("AGENTS.md"),
        home.path().join("AGENTS.md"),
    )?;
    assert_eq!(
        load(&home.path().join("AGENTS.md"), &["analysis"]).await?,
        load(&directory.path().join("AGENTS.md"), &["analysis"]).await?
    );
    std::fs::remove_file(directory.path().join("modules/rules.md"))?;
    std::fs::write(home.path().join("outside.md"), "Outside")?;
    symlink(
        home.path().join("outside.md"),
        directory.path().join("modules/rules.md"),
    )?;
    assert_eq!(
        state(&directory, &["analysis"]).await.unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    std::fs::remove_file(directory.path().join("modules.json"))?;
    symlink(
        home.path().join("outside.md"),
        directory.path().join("modules.json"),
    )?;
    assert_eq!(
        state(&directory, &["analysis"]).await.unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}
