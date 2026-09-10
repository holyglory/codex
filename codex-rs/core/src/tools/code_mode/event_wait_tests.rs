use super::*;
use codex_code_mode::CellId;

#[test]
fn event_wait_suppresses_only_empty_yields_for_its_cell() {
    let registry = EventWaitRegistry::default();
    let response = RuntimeResponse::Yielded {
        cell_id: CellId::new("parked".to_owned()),
        content_items: vec![],
        code_mode_host_duration: None,
    };
    assert!(!registry.coalesces(&response));
    let first = registry.begin("parked".into());
    let second = registry.begin("parked".into());
    assert!(registry.coalesces(&response));
    drop(first);
    assert!(registry.coalesces(&response));
    drop(second);
    assert!(!registry.coalesces(&response));
    let other = registry.begin("other".into());
    assert!(!registry.coalesces(&response));
    drop(other);
}
