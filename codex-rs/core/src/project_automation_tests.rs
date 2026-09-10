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
