use std::path::Path;

use sha1::Digest;
use sha1::Sha1;

/// Version of the opaque identity shared by linked worktrees.
pub const REPOSITORY_ID_FORMAT_VERSION: u32 = 3;

/// Linked worktrees share the validated Git common directory. Only its opaque
/// local identity enters usage facts; native paths and remotes stay out of them.
pub fn repository_id(cwd: &Path) -> Option<String> {
    let identity = codex_git_utils::repository_identity(cwd)?;
    let digest = Sha1::digest(identity.common_dir.as_os_str().as_encoded_bytes());
    Some(format!("repo_{digest:x}"))
}
