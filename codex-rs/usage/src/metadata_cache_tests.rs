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
