use serde::Deserialize;
use serde::Serialize;
use sha1::Digest;
use sha1::Sha1;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

const MARKER: &str = "<!-- codex:focused-policy:v1 -->";
const MAX_FILE_BYTES: usize = 32 * 1024;
const MAX_MANIFEST_BYTES: usize = 16 * 1024;
const MAX_CONTEXT_BYTES: usize = 120 * 1024;
pub(super) const CHUNK_BYTES: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    modules: Vec<Module>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Module {
    id: String,
    relative_path: String,
    applicability: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct PolicyChunk {
    pub(super) revision: String,
    pub(super) text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct LoadedFocusedPolicy {
    pub(super) source: PathBuf,
    pub(super) core: String,
    pub(super) chunks: Vec<PolicyChunk>,
}

pub(super) async fn load(
    policy_file: &Path,
    applicability: &[&str],
) -> io::Result<Option<LoadedFocusedPolicy>> {
    let policy_file = match tokio::fs::canonicalize(policy_file).await {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let core = read_bytes(&policy_file, MAX_FILE_BYTES).await?;
    if !core.starts_with(b"<!-- codex:focused-policy:") {
        return Ok(None);
    }
    if !core.starts_with(MARKER.as_bytes()) {
        return Err(invalid("unsupported focused-policy version"));
    }
    let core = checked_text(core, MAX_FILE_BYTES)?;
    let mut text = core.clone();
    let root = policy_file
        .parent()
        .ok_or_else(|| invalid("focused policy has no parent directory"))?;
    let manifest_text = read_confined(root, "modules.json", MAX_MANIFEST_BYTES).await?;
    let manifest: Manifest = serde_json::from_str(&manifest_text)
        .map_err(|_| invalid("invalid focused-policy manifest"))?;
    if manifest.version != 1 || manifest.modules.is_empty() || manifest.modules.len() > 32 {
        return Err(invalid("unsupported or oversized focused-policy manifest"));
    }
    let uncertain =
        applicability.is_empty() || applicability.iter().any(|tag| !known_applicability(tag));
    if uncertain {
        text.push_str("\n\nApplicability is uncertain: all policy details follow. Delivery still requires an authorized, meaningful delivery target.\n");
    }
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for module in manifest.modules {
        if !valid_tag(&module.id)
            || !ids.insert(module.id.clone())
            || !paths.insert(module.relative_path.clone())
            || !module.relative_path.starts_with("modules/")
            || !valid_relative_path(&module.relative_path)
            || module.applicability.is_empty()
            || module.applicability.len() > 16
            || module.applicability.iter().any(|tag| !valid_tag(tag))
        {
            return Err(invalid("invalid focused-policy module entry"));
        }
        let selected = uncertain
            || module.applicability.iter().any(|tag| {
                tag == "always" || !known_applicability(tag) || applies(tag, applicability)
            });
        if selected {
            let contents = read_confined(root, &module.relative_path, MAX_FILE_BYTES).await?;
            text.push_str(&format!("\n\n# Policy module: {}\n\n{contents}", module.id));
            if text.len() > MAX_CONTEXT_BYTES {
                return Err(invalid("focused policy exceeds its total context budget"));
            }
        }
    }
    let revision = format!("{:x}", Sha1::digest(text.as_bytes()));
    let mut remaining = text.as_str();
    let mut chunks = Vec::new();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(CHUNK_BYTES);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(PolicyChunk {
            revision: revision.clone(),
            text: remaining[..end].to_string(),
        });
        remaining = &remaining[end..];
    }
    Ok(Some(LoadedFocusedPolicy {
        source: policy_file,
        core,
        chunks,
    }))
}

fn known_applicability(tag: &str) -> bool {
    matches!(
        tag,
        "always"
            | "discussion"
            | "specification"
            | "analysis"
            | "recovery"
            | "research"
            | "performance-only"
            | "planning"
            | "implementation"
            | "operations"
            | "testing"
            | "delegation"
            | "delivery"
            | "web"
            | "desktop"
            | "review"
            | "documentation"
            | "ui"
            | "diagnosis"
            | "feedback"
            | "data"
            | "security"
    )
}

fn applies(tag: &str, applicability: &[&str]) -> bool {
    applicability.contains(&tag)
        || applicability.iter().any(|purpose| match *purpose {
            "discussion" => tag == "planning",
            "specification" => matches!(tag, "planning" | "documentation"),
            "analysis" => matches!(tag, "planning" | "diagnosis" | "review"),
            "implementation" => matches!(tag, "planning" | "testing"),
            "recovery" => matches!(
                tag,
                "implementation" | "operations" | "diagnosis" | "testing" | "delivery"
            ),
            _ => false,
        })
}

fn valid_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains(['\\', ':', '\0'])
        && value
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

async fn read_confined(root: &Path, relative: &str, limit: usize) -> io::Result<String> {
    let path = tokio::fs::canonicalize(root.join(relative)).await?;
    if !path.starts_with(root) {
        return Err(invalid("focused-policy path escapes its directory"));
    }
    checked_text(read_bytes(&path, limit).await?, limit)
}

async fn read_bytes(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    if !tokio::fs::metadata(path).await?.is_file() {
        return Err(invalid("focused policy requires regular files"));
    }
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

fn checked_text(bytes: Vec<u8>, limit: usize) -> io::Result<String> {
    if bytes.len() > limit {
        return Err(invalid("focused-policy file exceeds its byte budget"));
    }
    let text = String::from_utf8(bytes).map_err(|_| invalid("focused policy must be UTF-8"))?;
    if text.trim().is_empty() {
        return Err(invalid("focused-policy file is empty"));
    }
    Ok(text)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[path = "agents_md_focused_policy_tests.rs"]
mod tests;
