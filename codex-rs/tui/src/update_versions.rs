pub(crate) fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => Some(l > c),
        _ => None,
    }
}

pub(crate) fn is_source_build_version(version: &str) -> bool {
    parse_version(version) == Some((0, 0, 0, 0))
}

// Rust build metadata and npm prerelease spelling identify the same fork
// revision. Other prereleases and platform payload versions are not updates.
fn parse_version(v: &str) -> Option<(u64, u64, u64, u64)> {
    let v = v.trim();
    let (base, revision) = match v.split_once("+multi.").or_else(|| v.split_once("-multi.")) {
        Some((base, revision)) => (base, revision.parse::<u64>().ok()?),
        None => (v, 0),
    };
    let mut iter = base.split('.');
    let maj = iter.next()?.parse::<u64>().ok()?;
    let min = iter.next()?.parse::<u64>().ok()?;
    let pat = iter.next()?.parse::<u64>().ok()?;
    if iter.next().is_some() {
        return None;
    }
    Some((maj, min, pat, revision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn fork_revisions_compare_across_rust_and_npm_spellings() {
        for (latest, current, expected) in [
            ("0.153.0-multi.2", "0.153.0+multi.1", Some(true)),
            ("0.153.0-multi.10", "0.153.0+multi.9", Some(true)),
            ("0.153.0-multi.1", "0.153.0+multi.1", Some(false)),
            ("0.153.0-multi.1", "0.153.0+multi.2", Some(false)),
            ("0.154.0-multi.1", "0.153.0+multi.10", Some(true)),
            ("0.153.0-multi.2", "0.154.0+multi.1", Some(false)),
            ("0.153.0-multi.2-linux-x64", "0.153.0+multi.1", None),
            (
                "0.153.0-multi.18446744073709551616",
                "0.153.0+multi.1",
                None,
            ),
            ("0.153.0.1", "0.153.0+multi.1", None),
        ] {
            assert_eq!(is_newer(latest, current), expected, "{latest} / {current}");
        }
    }

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), None);
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), None);
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    #[test]
    fn source_build_version_is_not_checked() {
        assert!(is_source_build_version("0.0.0"));
        assert!(!is_source_build_version("0.1.0"));
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(parse_version(" 1.2.3 \n"), Some((1, 2, 3, 0)));
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }
}
