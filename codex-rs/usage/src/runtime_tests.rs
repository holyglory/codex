use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::ManifestStore;
use crate::UsageManifest;

#[test]
fn unrelated_publication_failure_requests_full_maintenance() {
    let directory = tempdir().expect("directory");
    let root = directory.path().to_path_buf();
    let manifest = UsageManifest {
        format_version: 0,
        ..UsageManifest::default()
    };
    ManifestStore::new(root.join("manifest.json"))
        .save(&manifest)
        .expect("unsupported manifest fixture");
    let (wake, receiver) = mpsc::channel();
    let (warm, _ready) = mpsc::channel();
    let (maintenance, requests) = mpsc::sync_channel(/*bound*/ 1);
    std::thread::scope(|scope| {
        let publisher = scope.spawn(move || publish_loop(root, receiver, warm, maintenance));
        wake.send(()).expect("publication wake");
        let request = requests.recv_timeout(Duration::from_secs(/*secs*/ 5));
        drop(wake);
        drop(requests);
        let request = request.expect("failed publication requests maintenance");
        let kind = request.kind;
        request
            .completion
            .send(())
            .expect("release maintenance handoff");
        publisher.join().expect("publisher stops after handoff");
        assert_eq!(kind, MaintenanceKind::Retention);
    });
}
