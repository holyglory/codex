//! Tests shared command-line option inheritance.

use super::SharedCliOptions;
use pretty_assertions::assert_eq;

#[test]
fn inherits_worktree_from_root_without_clearing_subcommand_choice() {
    let mut options = SharedCliOptions::default();
    let root = SharedCliOptions {
        account: Some("local".to_string()),
        worktree: true,
        ..Default::default()
    };
    options.inherit_exec_root_options(&root);
    assert_eq!(options.account.as_deref(), Some("local"));
    assert!(options.worktree);

    options.inherit_exec_root_options(&SharedCliOptions::default());
    assert!(options.worktree);
}

#[test]
fn applies_worktree_subcommand_override_without_clearing_root_choice() {
    let mut options = SharedCliOptions::default();
    options.apply_subcommand_overrides(SharedCliOptions {
        account: Some("explicit".to_string()),
        worktree: true,
        ..Default::default()
    });
    assert!(options.worktree);

    options.apply_subcommand_overrides(SharedCliOptions::default());
    assert_eq!(options.account.as_deref(), Some("explicit"));
    assert!(options.worktree);
}
