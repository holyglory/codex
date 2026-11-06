use tempfile::tempdir;

use super::*;

#[test]
fn advances_review_cutoff_without_a_projection() {
    let directory = tempdir().expect("directory");
    let mut manifest = UsageManifest::default();
    assert!(
        !retire(directory.path(), &mut manifest, 123, 456, "cutoff-test").expect("cutoff metadata")
    );
    assert_eq!(manifest.review_retention_cutoff_at_ms, Some(123));
}
