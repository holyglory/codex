use super::*;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::sync::Condvar;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;

#[tokio::test]
async fn coalesces_before_admission_and_bounds_active_and_pending_reads() {
    let admission = Arc::new(Admission::default());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Semaphore::new(0));
    let mut requests = Vec::new();
    let mut all_pending = true;
    for index in 0..10 {
        let (gate, reads, active, peak, started) = (
            Arc::clone(&gate),
            Arc::clone(&reads),
            Arc::clone(&active),
            Arc::clone(&peak),
            Arc::clone(&started),
        );
        let mut request = Box::pin(Arc::clone(&admission).read(
            (PathBuf::from("fixture"), vec![index]),
            move || {
                reads.fetch_add(1, Ordering::SeqCst);
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                started.add_permits(1);
                let guard = gate.0.lock().unwrap();
                let _released = gate
                    .1
                    .wait_timeout_while(guard, Duration::from_secs(30), |released| !*released)
                    .unwrap();
                active.fetch_sub(1, Ordering::SeqCst);
                Err(internal_error("fixture source result"))
            },
        ));
        all_pending &=
            std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx).is_pending())).await;
        requests.push(request);
    }
    let duplicate_reads = Arc::clone(&reads);
    let mut duplicate = Box::pin(Arc::clone(&admission).read(
        (PathBuf::from("fixture"), vec![0]),
        move || {
            duplicate_reads.fetch_add(1, Ordering::SeqCst);
            Err(internal_error("duplicate must share the original read"))
        },
    ));
    let duplicate_pending =
        std::future::poll_fn(|cx| Poll::Ready(duplicate.as_mut().poll(cx).is_pending())).await;
    let rejected = Arc::clone(&admission)
        .read((PathBuf::from("fixture"), vec![10]), || {
            Err(internal_error("excess read must not start"))
        })
        .await;
    let started_result = tokio::time::timeout(
        Duration::from_secs(10),
        Arc::clone(&started).acquire_many_owned(2),
    )
    .await;
    // Release every worker before assertions, including on an observed failure.
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let mut results = Vec::new();
    for request in requests {
        results.push(request.await);
    }
    let duplicate = duplicate.await;
    assert!(all_pending && duplicate_pending);
    assert!(started_result.is_ok());
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(reads.load(Ordering::SeqCst), 10);
    assert_eq!(rejected.unwrap_err().data.unwrap()["reason"], "reader_busy");
    assert_eq!(duplicate, results[0]);
    assert!(
        results
            .iter()
            .all(|result| result.as_ref().unwrap_err().message == "fixture source result")
    );
}
