use super::format_wake_policy;
use codex_app_server_protocol::EventSubscriptionWakePolicyResponse;
use codex_app_server_protocol::EventWakePolicy;
use codex_app_server_protocol::EventWakePolicyEntry;
use codex_app_server_protocol::EventWakeScope;

#[test]
fn wake_permission_status_explains_stop_suspension() {
    let response = EventSubscriptionWakePolicyResponse {
        revision: 12,
        running: false,
        pending_alarm_count: 2,
        data: vec![EventWakePolicyEntry {
            scope: EventWakeScope::Thread,
            policy: EventWakePolicy::AllowBackground,
            suspended: true,
            authorization_ref: "user-request".into(),
        }],
        next_cursor: None,
    };
    insta::assert_snapshot!(format_wake_policy(&response), @r#"
    Revision: 12
    Running: no
    Pending alarms: 2
    Default: running only
    {"type":"thread"}	allow background (suspended until user resume)
    "#);
}
