use super::*;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct Pin(Arc<AtomicUsize>);

impl Drop for Pin {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn protects_slow_reads_and_renews_successful_responses_then_releases_idle_pins() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let cache = LeaseCache::new(/*max_views*/ 1, Duration::from_secs(/*secs*/ 5));
    let home = Path::new("fixture");
    let first = cache
        .reserve()
        .unwrap()
        .insert(
            home.to_owned(),
            "snapshot".into(),
            Pin(Arc::clone(&dropped)),
        )
        .unwrap();
    tokio::time::advance(Duration::from_secs(/*secs*/ 6)).await;
    assert!(matches!(cache.reserve(), Err(LeaseError::Busy)));
    drop(
        cache
            .existing(home, "snapshot")
            .expect("active source stays usable"),
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    first.retain_after_response().unwrap();
    drop(first);
    tokio::time::advance(Duration::from_secs(/*secs*/ 3)).await;
    let second = cache
        .existing(home, "snapshot")
        .expect("five seconds after the response");
    second.retain_after_response().unwrap();
    drop(second);
    tokio::time::advance(Duration::from_secs(/*secs*/ 3)).await;
    // This is beyond both creation+5s and the first completed response+5s.
    drop(
        cache
            .existing(home, "snapshot")
            .expect("a later successful response renewed the lease"),
    );
    assert!(matches!(cache.reserve(), Err(LeaseError::Busy)));
    tokio::time::advance(Duration::from_millis(/*millis*/ 2_100)).await;
    tokio::task::yield_now().await;
    // No new request triggers cleanup: the cache-owned timer drops the file pin.
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(matches!(
        cache.existing(home, "snapshot"),
        Err(LeaseError::Expired)
    ));
    assert!(cache.reserve().is_ok());
}

#[tokio::test(start_paused = true)]
async fn failed_creation_and_failed_reads_release_reserved_slots_without_retention() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let cache = LeaseCache::new(/*max_views*/ 1, Duration::from_secs(/*secs*/ 5));
    let reservation = cache.reserve().unwrap();
    assert!(matches!(cache.reserve(), Err(LeaseError::Busy)));
    drop(reservation);
    let home = Path::new("fixture");
    let failed = cache
        .reserve()
        .unwrap()
        .insert(home.to_owned(), "failed".into(), Pin(Arc::clone(&dropped)))
        .unwrap();
    drop(failed);
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let completed = cache
        .reserve()
        .unwrap()
        .insert(
            home.to_owned(),
            "completed".into(),
            Pin(Arc::clone(&dropped)),
        )
        .unwrap();
    completed.retain_after_response().unwrap();
    drop(completed);
    assert!(matches!(cache.reserve(), Err(LeaseError::Busy)));
    assert!(matches!(
        cache.existing(home, "unknown"),
        Err(LeaseError::Expired)
    ));
    tokio::time::advance(Duration::from_millis(/*millis*/ 5_100)).await;
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert!(cache.reserve().is_ok());
}
