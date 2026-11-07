//! Optional private evidence for reviewed downstream tool-schema changes only.
use super::CapturedRequest;
use super::normalize::fingerprint;
use super::normalize::fingerprint_bytes;
use super::normalize::portable_tool_schema;
use serde_json::Value;
use serde_json::json;
use std::io::Write;
use std::path::Path;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 256;
const NAMES: [&str; 3] = ["account_management", "usage_stats", "usage_activity"];

type AuditResult<T> = Result<T, Box<dyn std::error::Error>>;

pub(super) fn record(requests: &[CapturedRequest]) -> AuditResult<()> {
    let Some(directory) = std::env::var_os("CODEX_TOOL_SCHEMA_EVIDENCE_DIR") else {
        return Ok(());
    };
    let directory = Path::new(&directory);
    let contract_path = directory.join("contract.json");
    if std::fs::metadata(&contract_path)?.len() > MAX_BYTES as u64 {
        return Err("tool-schema contract exceeds evidence limit".into());
    }
    let contract: Value = serde_json::from_slice(&std::fs::read(contract_path)?)?;
    validate_contract(&contract)?;
    for request in requests {
        if let Some(tools) = request
            .settings
            .as_ref()
            .and_then(|value| value.get("tools"))
        {
            record_catalog(directory, tools, &contract)?;
        }
        for item in &request.input {
            if item["type"] == "additional_tools" {
                let tools = item.get("tools").ok_or("catalog has no tool array")?;
                record_catalog(directory, tools, &contract)?;
            }
        }
    }
    Ok(())
}

fn validate_contract(contract: &Value) -> AuditResult<()> {
    let version = match contract.get("schemaVersion") {
        None => 1,
        Some(version) => version
            .as_u64()
            .ok_or("invalid tool-schema contract version")?,
    };
    if version != 1 && version != 2 {
        return Err("unsupported tool-schema contract version".into());
    }
    if version == 1 && contract.get("codeModeTools").is_some() {
        return Err("Code Mode variants require contract version 2".into());
    }
    let tools = contract["tools"]
        .as_array()
        .ok_or("contract tools missing")?;
    let sections = contract["sections"]
        .as_object()
        .ok_or("contract sections missing")?;
    if tools.len() != NAMES.len() || sections.len() != NAMES.len() {
        return Err("contract must contain exactly the three reviewed tools".into());
    }
    for name in NAMES {
        if tools
            .iter()
            .filter(|tool| tool["name"] == name && tool["type"] == "function")
            .count()
            != 1
            || !sections.get(name).is_some_and(Value::is_string)
        {
            return Err("contract tool identity is invalid".into());
        }
        if version == 2 {
            let variants = contract["codeModeTools"]
                .as_array()
                .filter(|variants| variants.len() == NAMES.len())
                .ok_or("contract must contain exactly three Code Mode variants")?;
            let matching = variants
                .iter()
                .filter(|tool| tool["name"] == name)
                .collect::<Vec<_>>();
            let [variant] = matching.as_slice() else {
                return Err("Code Mode contract variant identity is invalid".into());
            };
            let mut expected = tools
                .iter()
                .find(|tool| tool["name"] == name)
                .ok_or("reviewed tool absent from contract")?
                .clone();
            let heading = format!("### `{name}`\n");
            let description = sections[name]
                .as_str()
                .and_then(|section| section.strip_prefix(heading.as_str()))
                .ok_or("Code Mode contract section is not anchored")?;
            let prefix = format!(
                "{}\n\nexec tool declaration:\n```ts\n",
                expected["description"]
                    .as_str()
                    .ok_or("reviewed description missing")?
            );
            if !description.starts_with(&prefix) || !description.ends_with("\n```") {
                return Err(
                    "Code Mode contract description is not the approved augmentation".into(),
                );
            }
            expected["description"] = Value::String(description.to_string());
            if fingerprint_bytes(variant) != fingerprint_bytes(&expected) {
                return Err("Code Mode contract variant changes unapproved fields".into());
            }
        }
    }
    Ok(())
}

fn record_catalog(directory: &Path, tools: &Value, contract: &Value) -> AuditResult<()> {
    let tools = tools.as_array().ok_or("catalog tools must be an array")?;
    let current = Value::Array(tools.iter().map(portable_tool_schema).collect());
    let reconstructed = reconstruct(&current, contract)?;
    if current == reconstructed {
        return Ok(());
    }
    let mut mappings = Vec::new();
    collect_mappings(&current, &reconstructed, "catalog", &mut mappings);
    // Persist the exact canonical hash preimages, never the captured request.
    let current: Value = serde_json::from_slice(&fingerprint_bytes(&current))?;
    let reconstructed: Value = serde_json::from_slice(&fingerprint_bytes(&reconstructed))?;
    let evidence = json!({"schemaVersion":1,"contractHash":fingerprint(contract),"current":current,"reconstructed":reconstructed,"mappings":mappings});
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    if bytes.len() > MAX_BYTES {
        return Err("tool-schema evidence exceeds per-file limit".into());
    }
    let path = directory.join(format!(
        "catalog-{}-{}.json",
        fingerprint(&current),
        std::process::id()
    ));
    if path.exists() {
        if std::fs::read(path)? != bytes {
            return Err("tool-schema evidence identity conflicts with existing bytes".into());
        }
        return Ok(());
    }
    if std::fs::read_dir(directory)?.count() >= MAX_FILES {
        return Err("tool-schema evidence exceeds file-count limit".into());
    }
    let mut output = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(path)? != bytes {
                return Err("concurrent tool-schema evidence has different bytes".into());
            }
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    output.write_all(&bytes)?;
    output.sync_all()?;
    Ok(())
}

/// Reconstruct the old catalog for audit only. Snapshot rendering always uses
/// the full new catalog, including all three downstream tools.
pub(super) fn reconstruct(current: &Value, contract: &Value) -> AuditResult<Value> {
    validate_contract(contract)?;
    let tools = current.as_array().ok_or("catalog tools must be an array")?;
    let mut previous = Vec::new();
    for tool in tools {
        if let Some(tool) = reconstruct_tool(tool, contract)? {
            previous.push(tool);
        }
    }
    Ok(Value::Array(previous))
}

fn reconstruct_tool(tool: &Value, contract: &Value) -> AuditResult<Option<Value>> {
    let name = tool["name"].as_str().unwrap_or_default();
    if NAMES.contains(&name) {
        let expected = contract["tools"]
            .as_array()
            .and_then(|tools| tools.iter().find(|tool| tool["name"] == name))
            .ok_or("reviewed tool absent from contract")?;
        let matches_code_mode = contract["schemaVersion"] == 2
            && contract["codeModeTools"]
                .as_array()
                .is_some_and(|variants| {
                    variants.iter().any(|variant| {
                        variant["name"] == name
                            && fingerprint_bytes(tool)
                                == fingerprint_bytes(&portable_tool_schema(variant))
                    })
                });
        if fingerprint_bytes(tool) != fingerprint_bytes(&portable_tool_schema(expected))
            && !matches_code_mode
        {
            return Err("captured downstream tool differs from the production contract".into());
        }
        return Ok(None);
    }
    let mut previous = tool.clone();
    if tool["type"] == "namespace" && name == "functions" {
        previous["tools"] = reconstruct(&tool["tools"], contract)?;
    } else if name == "exec" && tool["type"] == "custom" {
        let mut description = tool["description"]
            .as_str()
            .ok_or("exec description missing")?
            .to_string();
        for name in NAMES {
            let section = contract["sections"][name]
                .as_str()
                .ok_or("reviewed section absent")?;
            description = remove_section(&description, name, section)?;
        }
        previous["description"] = Value::String(description);
    }
    Ok(Some(previous))
}

fn remove_section(description: &str, name: &str, expected: &str) -> AuditResult<String> {
    let heading = format!("### `{name}`\n");
    let mut matches = description.match_indices(&heading);
    let Some((start, _)) = matches.next() else {
        return Ok(description.to_string());
    };
    if matches.next().is_some() || (start != 0 && !description[..start].ends_with("\n\n")) {
        return Err("reviewed tool section is duplicated or not anchored".into());
    }
    let rest = &description[start..];
    let end = ["\n\n### `", "\n\n## "]
        .into_iter()
        .filter_map(|separator| rest.find(separator))
        .min()
        .map_or(description.len(), |offset| start + offset);
    if &description[start..end] != expected {
        return Err("captured downstream declaration differs from the production contract".into());
    }
    let (remove_start, remove_end) = if end != description.len() {
        (start, end + 2)
    } else {
        (start.saturating_sub(2), end)
    };
    Ok(format!(
        "{}{}",
        &description[..remove_start],
        &description[remove_end..]
    ))
}

fn collect_mappings(current: &Value, previous: &Value, label: &str, mappings: &mut Vec<Value>) {
    if current == previous {
        return;
    }
    mappings.push(json!({"label":label,"old":fingerprint(previous),"new":fingerprint(current)}));
    let current_tools = current
        .as_array()
        .or_else(|| current.get("tools").and_then(Value::as_array));
    let previous_tools = previous
        .as_array()
        .or_else(|| previous.get("tools").and_then(Value::as_array));
    if let (Some(current_tools), Some(previous_tools)) = (current_tools, previous_tools) {
        for tool in current_tools {
            if let Some(old) = previous_tools
                .iter()
                .find(|old| old["type"] == tool["type"] && old["name"] == tool["name"])
            {
                let label = format!(
                    "{}/{}",
                    tool["type"].as_str().unwrap_or_default(),
                    tool["name"].as_str().unwrap_or_default()
                );
                collect_mappings(tool, old, &label, mappings);
            }
        }
    }
}
