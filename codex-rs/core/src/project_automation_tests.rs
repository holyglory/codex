use super::*;
use pretty_assertions::assert_eq;

#[test]
fn project_identity_is_shared_by_worktrees_and_never_contains_a_raw_path() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    let worktree = root.path().join("worktree");
    let metadata = main.join(".git/worktrees/second");
    std::fs::create_dir_all(&metadata).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", metadata.display()),
    )
    .unwrap();
    std::fs::write(metadata.join("commondir"), "../..\n").unwrap();
    assert_eq!(
        project_automation_id(&main),
        project_automation_id(&worktree)
    );
    assert!(!project_automation_id(&main).contains(root.path().to_str().unwrap()));
}

#[test]
fn project_identity_exposes_workspace_alias_before_git_metadata_appears() {
    let root = tempfile::tempdir_in("/home/holyglory").unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let before = super::project_identity_candidates(&workspace);
    assert_eq!(
        before.canonical.kind,
        codex_event_subscriptions::ProjectIdentityKind::WorkspacePath
    );
    assert!(before.aliases.is_empty());
    std::fs::create_dir_all(workspace.join(".git")).unwrap();
    let after = super::project_identity_candidates(&workspace);
    assert_eq!(
        after.canonical.kind,
        codex_event_subscriptions::ProjectIdentityKind::GitCommonDirectory
    );
    assert_ne!(before.canonical.project_id, after.canonical.project_id);
    assert_eq!(after.aliases.len(), 1);
    assert_eq!(after.aliases[0].project_id, before.canonical.project_id);
}
