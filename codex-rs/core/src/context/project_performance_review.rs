//! Recognition only for persisted review-worker context from older runtimes.
//! New reviews are Coordinator obligations delivered as generic alarm reminders.
pub(crate) fn is_legacy_project_review(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("<project_performance_review>")
        && text.ends_with("</project_performance_review>")
}
