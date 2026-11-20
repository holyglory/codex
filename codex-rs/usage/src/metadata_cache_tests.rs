use std::io::Write;
use std::sync::Arc;

use tempfile::tempdir;

use super::JsonCache;

#[test]
fn snapshot_metadata_caches_large_files_and_evicts_prior_snapshot_without_enlarging_default() {
    let directory = tempdir().expect("directory");
    let large = directory.path().join("large.json");
    {
        let mut file = std::fs::File::create(&large).expect("large fixture");
        file.write_all(b"\"").expect("JSON start");
        let block = vec![b'x'; 1024 * 1024];
        for _ in 0..33 {
            file.write_all(&block)
                .expect("metadata beyond default cache limit");
        }
        file.write_all(b"\"").expect("JSON end");
    }
    let other = directory.path().join("other.json");
    std::fs::write(&other, b"\"other snapshot\"").expect("second snapshot");
    {
        let cache = JsonCache::<String>::for_snapshot_metadata();
        let first = cache.load(&large).expect("large snapshot");
        let same = cache.load(&large).expect("reuse parsed large snapshot");
        assert!(Arc::ptr_eq(&first.value, &same.value));
        cache.load(&other).expect("replace cached snapshot");
        let reloaded = cache.load(&large).expect("prior snapshot no longer cached");
        assert!(!Arc::ptr_eq(&first.value, &reloaded.value));
    }
    let cache = JsonCache::<String>::new();
    let first = cache.load(&large).expect("ordinary cache read");
    let second = cache
        .load(&large)
        .expect("ordinary cache still excludes oversized metadata");
    assert!(!Arc::ptr_eq(&first.value, &second.value));
}

#[test]
fn parsed_metadata_keeps_opened_file_identity_when_path_is_replaced_during_decode() {
    use pretty_assertions::assert_eq;
    use serde::Deserialize;

    #[derive(Debug)]
    struct Replacing(String);

    impl<'de> Deserialize<'de> for Replacing {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let (value, replacement): (String, Option<(std::path::PathBuf, std::path::PathBuf)>) =
                Deserialize::deserialize(deserializer)?;
            if let Some((source, target)) = replacement {
                std::fs::rename(source, target).map_err(serde::de::Error::custom)?;
            }
            Ok(Self(value))
        }
    }

    let directory = tempdir().expect("directory");
    let path = directory.path().join("metadata.json");
    let replacement = directory.path().join("replacement.json");
    std::fs::write(
        &replacement,
        serde_json::to_vec(&("new", Option::<(String, String)>::None)).expect("new metadata"),
    )
    .expect("replacement file");
    std::fs::write(
        &path,
        serde_json::to_vec(&("old", Some((&replacement, &path)))).expect("old metadata"),
    )
    .expect("original file");
    let opened = Some(super::Identity::from_metadata(
        &std::fs::metadata(&path).expect("old identity"),
    ));
    let cache = JsonCache::<Replacing>::new();
    let old = cache
        .load(&path)
        .expect("atomic replacement does not invalidate opened source");
    assert_eq!((&old.value.0, &old.identity), (&"old".to_owned(), &opened));
    let current = Some(super::Identity::from_metadata(
        &std::fs::metadata(&path).expect("replacement identity"),
    ));
    assert_ne!(current, opened);
    let new = cache.load(&path).expect("replacement value");
    assert_eq!((&new.value.0, &new.identity), (&"new".to_owned(), &current));
    let cached = cache.load(&path).expect("cached replacement");
    assert!(Arc::ptr_eq(&new.value, &cached.value));
    assert_eq!(cached.identity, new.identity);
    assert_eq!(old.identity, opened);
}
