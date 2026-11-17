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
    let (headline, _headlines) = mpsc::sync_channel(/*bound*/ 1);
    let (global, _globals) = mpsc::sync_channel(/*bound*/ 1);
    std::thread::scope(|scope| {
        let publisher =
            scope.spawn(move || publish_loop(root, receiver, warm, maintenance, headline, global));
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

#[test]
fn global_analytics_owner_drop_interrupts_duckdb_and_releases_private_candidate() {
    let directory = tempdir().expect("directory");
    let root = directory.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = crate::UsageJournal::new(root.join("events.jsonl"));
    let mut first = crate::UsageRecord::new_millis(
        "first", /*observed_at_ms*/ 1_000, /*input_tokens*/ 1, /*output_tokens*/ 0,
    )
    .with_operation("first")
    .with_thread("agent");
    first.event = Some("model_request_finished".into());
    first.started_at_ms = Some(900);
    first.ended_at_ms = Some(1_000);
    first.provider_total_tokens = Some(1);
    journal.append_record(first).expect("capture");
    crate::UsagePublisher::open_at(&root)
        .expect("canonical publisher")
        .publish_batch(crate::JournalReadLimits {
            max_bytes: 1024 * 1024,
            max_entries: 64,
        })
        .expect("canonical snapshot");
    assert_eq!(
        crate::GlobalReviewPublisher::open_at(&root)
            .expect("initial builder")
            .expect("first source")
            .publish()
            .expect("completed report"),
        crate::GlobalReviewPublication::Published
    );
    let previous = std::fs::read(root.join("global-review.json")).expect("completed report bytes");
    journal
        .append_record(crate::UsageRecord::new_millis(
            "later", /*observed_at_ms*/ 2_000, /*input_tokens*/ 1,
            /*output_tokens*/ 0,
        ))
        .expect("later capture");
    #[cfg(windows)]
    {
        for (label, query) in [("no-query", None), ("completed-query", Some("SELECT 42"))] {
            let database = root.join(format!("cleanup-control-{label}.duckdb"));
            let spill = root.join(format!("cleanup-control-{label}-spill"));
            drop(
                crate::storage_file::open_read_write(&spill.join(".private"))
                    .expect("private close control spill"),
            );
            let connection = crate::duckdb_connection::open_background_report(&database, &spill)
                .expect("bounded close control engine");
            if let Some(query) = query {
                let value = connection
                    .query_row(query, [], |row| row.get::<_, i64>(0))
                    .expect("close control query completed");
                assert_eq!(value, 42);
            }
            let closed = connection.close().map_err(|(_, error)| error);
            eprintln!("global review close control: label={label} close_result={closed:?}");
            closed.expect("close control database closed");
            let removal = std::fs::remove_file(&database);
            eprintln!(
                "global review close control: label={label} remove_succeeded={} error_kind={:?} raw_os_error={:?}",
                removal.is_ok(),
                removal.as_ref().err().map(std::io::Error::kind),
                removal
                    .as_ref()
                    .err()
                    .and_then(std::io::Error::raw_os_error)
            );
        }
    }
    let cancellation = Arc::new(crate::global_review_cancel::GlobalReviewCancellation::default());
    let (started, ready) = mpsc::channel();
    let (completed, result) = mpsc::channel();
    let worker_root = root.clone();
    let worker_cancel = Arc::clone(&cancellation);
    let worker = std::thread::spawn(move || {
        let builder = crate::GlobalReviewPublisher::open_cancellable(
            worker_root.clone(),
            Arc::clone(&worker_cancel),
        )
        .expect("cancellable builder")
        .expect("new source");
        let work = std::fs::read_dir(&worker_root)
            .expect("work directory")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("global-review-work-"))
            })
            .expect("normal producer work path");
        let spill = work.join("spill");
        drop(crate::storage_file::open_read_write(&spill.join(".private")).expect("private spill"));
        let connection =
            crate::duckdb_connection::open_background_report(&work.join("report.duckdb"), &spill)
                .expect("bounded real engine");
        let registration = worker_cancel
            .register(&connection)
            .expect("active connection registration");
        #[cfg(windows)]
        let probe_database = |stage: &str| {
            use std::os::windows::fs::OpenOptionsExt;
            let result = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(work.join("report.duckdb"));
            eprintln!(
                "global review database probe: stage={stage} exclusive_open={} error_kind={:?} raw_os_error={:?}",
                result.is_ok(),
                result.as_ref().err().map(std::io::Error::kind),
                result.as_ref().err().and_then(std::io::Error::raw_os_error)
            );
            drop(result);
        };
        started.send(()).expect("query starting");
        let interrupted = worker_cancel
            .check()
            .and_then(|()| {
                connection
                    .query_row(
                        "SELECT sum(i % 17)::BIGINT FROM range(1000000000000) r(i)",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|_| ())
                    .map_err(crate::reader_query::query_error)
            })
            .is_err();
        #[cfg(windows)]
        probe_database("after-query-error");
        #[cfg(windows)]
        {
            let closed = connection.close().map_err(|(_, error)| error);
            eprintln!("global review interrupted database close_result={closed:?}");
            closed.expect("interrupted database closed");
        }
        #[cfg(not(windows))]
        drop(connection);
        #[cfg(windows)]
        probe_database("after-connection-close");
        drop(registration);
        #[cfg(windows)]
        probe_database("after-registration-drop");
        drop(builder);
        completed.send(interrupted).expect("cancellation result");
    });
    let owner = PublisherOwner {
        signal: Arc::new(PublisherSignal {
            wake: Mutex::new(None),
        }),
        global_cancel: cancellation,
        workers: Mutex::new(vec![worker]),
    };
    ready
        .recv_timeout(Duration::from_secs(/*secs*/ 5))
        .expect("registered query");
    // Match the bundled DuckDB interrupt test's allowance for query startup.
    std::thread::sleep(Duration::from_millis(/*millis*/ 100));
    let started = std::time::Instant::now();
    drop(owner);
    assert!(started.elapsed() < Duration::from_secs(/*secs*/ 5));
    assert!(
        result
            .recv_timeout(Duration::from_secs(/*secs*/ 1))
            .expect("query interrupted")
    );
    assert_eq!(
        std::fs::read(root.join("global-review.json")).expect("no partial publication"),
        previous
    );
    assert!(!std::fs::read_dir(&root).expect("root").any(|entry| {
        entry
            .expect("entry")
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("global-review-work-"))
    }));
    // A DuckDB interrupt delivered while idle is cleared by its next query.
    // The same stage guard used by materialization and report queries must
    // therefore refuse the next SQL closure after cancellation completes.
    let cancellation = Arc::new(crate::global_review_cancel::GlobalReviewCancellation::default());
    let next_query_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::Barrier::new(/*n*/ 2));
    let (completed_first, first_done) = mpsc::channel();
    let (completed_second, second_done) = mpsc::channel();
    let worker_cancel = Arc::clone(&cancellation);
    let worker_started = Arc::clone(&next_query_started);
    let worker_release = Arc::clone(&release);
    let worker_root = root.clone();
    let worker = std::thread::spawn(move || {
        let spill = worker_root.join("between-stages-spill");
        drop(
            crate::storage_file::open_read_write(&spill.join(".private"))
                .expect("private staged spill"),
        );
        let connection = crate::duckdb_connection::open_background_report(
            &worker_root.join("between-stages.duckdb"),
            &spill,
        )
        .expect("bounded staged engine");
        let registration = worker_cancel
            .register(&connection)
            .expect("registered idle connection");
        worker_cancel
            .run_stage(|| {
                connection
                    .execute_batch("SELECT 1")
                    .map_err(crate::reader_query::query_error)
            })
            .expect("first statement completed");
        completed_first.send(()).expect("first statement receipt");
        worker_release.wait();
        let refused = worker_cancel
            .run_stage(|| {
                worker_started.store(true, std::sync::atomic::Ordering::Release);
                connection
                    .query_row(
                        "SELECT sum(i % 17)::BIGINT FROM range(1000000000000) r(i)",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|_| ())
                    .map_err(crate::reader_query::query_error)
            })
            .is_err();
        drop(connection);
        drop(registration);
        completed_second
            .send(refused)
            .expect("second statement admission result");
    });
    let owner = PublisherOwner {
        signal: Arc::new(PublisherSignal {
            wake: Mutex::new(None),
        }),
        global_cancel: Arc::clone(&cancellation),
        workers: Mutex::new(vec![worker]),
    };
    first_done
        .recv_timeout(Duration::from_secs(/*secs*/ 5))
        .expect("completed first statement");
    cancellation.cancel();
    release.wait();
    let started = std::time::Instant::now();
    drop(owner);
    assert!(started.elapsed() < Duration::from_secs(/*secs*/ 5));
    assert!(
        second_done
            .recv_timeout(Duration::from_secs(/*secs*/ 1))
            .expect("next query was refused")
    );
    assert!(!next_query_started.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(
        std::fs::read(root.join("global-review.json")).expect("previous completed prefix"),
        previous
    );
}
