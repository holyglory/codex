use serde::Deserialize;
use std::collections::HashMap;

#[cfg(not(debug_assertions))]
pub(crate) const PACKAGE_URL: &str = "https://registry.npmjs.org/@holyglory%2fcodex";

#[derive(Deserialize, Debug, Clone)]
pub(crate) struct NpmPackageInfo {
    #[serde(rename = "dist-tags")]
    dist_tags: HashMap<String, String>,
    versions: HashMap<String, NpmPackageVersionInfo>,
}

#[derive(Deserialize, Debug, Clone)]
struct NpmPackageVersionInfo {
    dist: Option<NpmPackageDist>,
}

#[derive(Deserialize, Debug, Clone)]
struct NpmPackageDist {
    tarball: Option<String>,
    integrity: Option<String>,
}

pub(crate) fn latest_ready_version(package_info: &NpmPackageInfo) -> anyhow::Result<&str> {
    let version = package_info
        .dist_tags
        .get("latest")
        .ok_or_else(|| anyhow::anyhow!("npm package is missing latest dist-tag"))?;
    let Some((_, revision)) = version.split_once("-multi.") else {
        anyhow::bail!("npm latest dist-tag is not a fork root release");
    };
    if revision.parse::<u64>().is_err()
        || crate::update_versions::is_newer(version, "0.0.0").is_none()
    {
        anyhow::bail!("npm latest dist-tag is not a fork root release");
    }
    version_info_with_dist(package_info, version)?;
    Ok(version)
}

fn version_info_with_dist<'a>(
    package_info: &'a NpmPackageInfo,
    version: &str,
) -> anyhow::Result<&'a NpmPackageVersionInfo> {
    let info = package_info
        .versions
        .get(version)
        .ok_or_else(|| anyhow::anyhow!("npm package version {version} is missing"))?;
    let Some(dist) = info.dist.as_ref() else {
        anyhow::bail!("npm package version {version} is missing dist metadata");
    };
    let has_tarball = dist
        .tarball
        .as_deref()
        .is_some_and(|tarball| !tarball.is_empty());
    if !has_tarball {
        anyhow::bail!("npm package version {version} is missing dist.tarball");
    }
    let has_integrity = dist
        .integrity
        .as_ref()
        .is_some_and(|integrity| !integrity.is_empty());
    if !has_integrity {
        anyhow::bail!("npm package version {version} is missing dist.integrity");
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn version_json(version: &str) -> serde_json::Value {
        serde_json::json!({
            "dist": {
                "integrity": format!("sha512-{version}"),
                "tarball": format!("https://registry.npmjs.org/@holyglory/codex/-/codex-{version}.tgz"),
            }
        })
    }

    fn package_info(available_version: &str, npm_latest: &str) -> NpmPackageInfo {
        let mut versions = serde_json::Map::new();
        versions.insert(
            available_version.to_string(),
            version_json(available_version),
        );

        serde_json::from_value(serde_json::json!({
            "dist-tags": { "latest": npm_latest },
            "versions": serde_json::Value::Object(versions),
        }))
        .expect("valid npm package metadata")
    }

    #[test]
    fn ready_version_requires_latest_dist_tag_and_root_dist() {
        let latest = "1.2.3-multi.4";
        let mut package_info = package_info(latest, latest);
        // A newer upload is not offered until the owner promotes latest.
        package_info.versions.insert(
            "1.2.3-multi.5".to_string(),
            serde_json::from_value(version_json("1.2.3-multi.5")).unwrap(),
        );
        assert_eq!(latest_ready_version(&package_info).unwrap(), latest);
    }

    #[test]
    fn ready_version_rejects_unavailable_latest() {
        let package_info = package_info("1.2.3-multi.4", "1.2.3-multi.5");
        let err = latest_ready_version(&package_info).expect_err("latest must be published");
        assert!(
            err.to_string().contains("version 1.2.3-multi.5 is missing"),
            "error should name unavailable latest: {err}"
        );
    }

    #[test]
    fn ready_version_rejects_missing_root_dist() {
        let package_info: NpmPackageInfo = serde_json::from_value(serde_json::json!({
            "dist-tags": { "latest": "1.2.3-multi.4" },
            "versions": { "1.2.3-multi.4": {} },
        }))
        .expect("valid npm package metadata");

        let err =
            latest_ready_version(&package_info).expect_err("root package must have dist metadata");
        assert!(
            err.to_string().contains("missing dist metadata"),
            "error should name missing dist metadata: {err}"
        );
    }

    #[test]
    fn platform_bootstrap_does_not_offer_a_root_update() {
        let info: NpmPackageInfo = serde_json::from_value(serde_json::json!({
            "dist-tags": { "linux-x64": "1.2.3-multi.4-linux-x64" },
            "versions": { "1.2.3-multi.4-linux-x64": version_json("1.2.3-multi.4-linux-x64") },
        }))
        .unwrap();
        assert!(
            latest_ready_version(&info)
                .unwrap_err()
                .to_string()
                .contains("missing latest")
        );
    }

    #[test]
    fn root_update_rejects_other_channels_and_incomplete_payload_metadata() {
        for version in [
            "1.2.3",
            "1.2.3-alpha.6",
            "1.2.3-multi.4-linux-x64",
            "1.2.3+multi.4",
        ] {
            assert!(
                latest_ready_version(&package_info(version, version)).is_err(),
                "{version}"
            );
        }
        for field in ["tarball", "integrity"] {
            for value in [serde_json::Value::Null, serde_json::json!("")] {
                let version = "1.2.3-multi.4";
                let mut info = package_info(version, version);
                let mut payload = version_json(version);
                payload["dist"][field] = value;
                info.versions.insert(
                    version.to_string(),
                    serde_json::from_value(payload).unwrap(),
                );
                assert!(latest_ready_version(&info).is_err(), "{field}");
            }
        }
    }
}
