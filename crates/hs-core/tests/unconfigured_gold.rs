//! A server without Gold remains readable; clone operations fail before I/O.

mod common;

use common::{FakePve, test_config};
use hs_core::capabilities::{DEFAULT_POOL, Operation, evaluate, probe_paths};
use hs_core::lifecycle::{CreateRequest, LifecycleTiming, plan_create};
use hs_core::refresh::{plan_refresh, refresh_workspace};
use hs_core::tasks::NoObserver;
use serde_json::json;

#[tokio::test]
async fn clone_operations_refuse_unconfigured_gold_before_any_pve_request() {
    let mut config = test_config();
    config.gold_vmid = None;
    let pve = FakePve::new();
    let request = CreateRequest {
        gold_vmid: None,
        vmid: 200,
        name: "workspace".into(),
        home_size: None,
        storage: None,
        node: None,
    };
    for result in [
        plan_create(&pve, &config, DEFAULT_POOL, &request).await,
        plan_refresh(&pve, &config, DEFAULT_POOL, 200, false).await,
        refresh_workspace(
            &pve,
            &config,
            DEFAULT_POOL,
            &json!({}),
            &LifecycleTiming::default(),
            &NoObserver,
        )
        .await,
    ] {
        assert!(result.is_err_and(|error| error.to_string().contains("Gold is not configured")));
    }
    assert!(pve.calls().is_empty());
}

#[test]
fn a_missing_gold_does_not_disable_unrelated_capabilities_or_probe_a_fake_vmid() {
    let mut config = test_config();
    config.gold_vmid = None;
    assert!(
        probe_paths(&config, DEFAULT_POOL)
            .iter()
            .all(|path| !path.starts_with("/vms/"))
    );
    for capability in evaluate(&config, DEFAULT_POOL, |_, _| true) {
        let needs_gold = matches!(
            capability.operation,
            Operation::GoldReadiness | Operation::Create | Operation::Refresh
        );
        assert_eq!(capability.available, !needs_gold, "{:?}", capability.operation);
    }
}
