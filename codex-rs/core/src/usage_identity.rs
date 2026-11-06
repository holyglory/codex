use std::path::Path;

use sha1::Digest;
use sha1::Sha1;

/// Linked worktrees share the validated Git common directory. Only its opaque
/// local identity enters usage facts; native paths and remotes stay out of them.
pub(crate) fn repository_id(cwd: &Path) -> Option<String> {
    let identity = codex_git_utils::repository_identity(cwd)?;
    let digest = Sha1::digest(identity.common_dir.as_os_str().as_encoded_bytes());
    Some(format!("repo_{digest:x}"))
}
