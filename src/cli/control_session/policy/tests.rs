use super::*;
use crate::core::service_manager::{RuntimeSnapshot, ServiceSnapshot};
use std::sync::atomic::{AtomicUsize, Ordering};

fn operation(key: &str) -> Operation {
    Operation::Restart {
        target: Target {
            instance_id: "instance".into(),
            run_id: "run".into(),
        },
        service: "api".into(),
        operation_key: Some(key.into()),
    }
}

// A real protocol endpoint with a controllable reply, so cancellation and
// refusal tests don't depend on scheduler timing or an arbitrary sleep.
async fn fixture(
    response: Response,
) -> (
    tempfile::TempDir,
    Session,
    Arc<AtomicUsize>,
    Arc<Semaphore>,
    tokio::task::JoinHandle<()>,
) {
    #[cfg(unix)]
    let root = tempfile::tempdir_in("/tmp").unwrap();
    #[cfg(windows)]
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("control.sock");
    let policy = parse_policy(&["api=1".into()], true).unwrap();
    let session = Session {
        identity: Identity {
            schema_version: 1,
            instance_id: "instance".into(),
            run_id: "run".into(),
            supervisor_pid: 1,
            started_at: chrono::Utc::now(),
            config: "devd.yml".into(),
            state_dir: root.path().into(),
            profile: None,
            project_root: root.path().into(),
            git: None,
        },
        socket: socket.clone(),
        allow: vec![Grant::Restart, Grant::Stop],
        admission: Arc::new(Mutex::new(AdmissionState::new(&policy))),
        policy,
        controls: Arc::new(Semaphore::new(1)),
    };
    let mut listener = crate::cli::transport::Listener::bind(&socket)
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let observed = calls.clone();
    let released = release.clone();
    let identity = session.identity.clone();
    let response = Arc::new(response);
    let task = tokio::spawn(async move {
        while let Ok(mut stream) = listener.accept().await {
            let response = response.clone();
            let calls = observed.clone();
            let release = released.clone();
            let identity = identity.clone();
            tokio::spawn(async move {
                let Request::Scoped { request, .. } =
                    protocol::read_request(&mut stream).await.unwrap()
                else {
                    panic!("unscoped request")
                };
                let response = match *request {
                    Request::Status => Response::Status(RuntimeSnapshot {
                        supervisor_pid: 1,
                        event_run_id: Some("run".into()),
                        services: BTreeMap::from([("api".into(), ServiceSnapshot::default())]),
                    }),
                    Request::Identity => Response::Identity(Box::new(identity)),
                    Request::Restart { .. } => {
                        calls.fetch_add(1, Ordering::SeqCst);
                        release.acquire().await.unwrap().forget();
                        let bytes = serde_json::to_vec(response.as_ref()).unwrap();
                        let reply: Response = serde_json::from_slice(&bytes).unwrap();
                        reply
                    }
                    _ => panic!("unexpected request"),
                };
                let _ = protocol::write(&mut stream, &response).await;
            });
        }
    });
    (root, session, calls, release, task)
}

async fn until(mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !check() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn test_cancelled_reply_concurrent_duplicate_and_grants_do_not_replay() {
    let (_root, session, calls, release, endpoint) =
        fixture(Response::Restarted(ServiceSnapshot::default())).await;
    let s = session.clone();
    let first = tokio::spawn(async move { s.execute(operation("one")).await });
    until(|| calls.load(Ordering::SeqCst) == 1).await;
    first.abort();
    let _ = first.await;
    assert_eq!(
        session.execute(operation("one")).await.unwrap()["operation_status"],
        "executing"
    );
    assert_eq!(
        session.execute(operation("two")).await.unwrap_err().code,
        "busy"
    );
    assert_eq!(session.policy_report()["remaining_restarts"]["api"], 0);
    release.add_permits(1);
    until(|| {
        matches!(
            session.admission.lock().unwrap().operations["one"].state,
            DedupState::Completed(_)
        )
    })
    .await;
    assert_eq!(
        session.execute(operation("one")).await.unwrap()["operation_status"],
        "completed"
    );
    let mut denied = session.clone();
    denied.allow.clear();
    assert_eq!(
        denied.execute(operation("one")).await.unwrap_err().code,
        "permission-denied"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    endpoint.abort();
}

#[tokio::test]
async fn test_definite_supervisor_refusal_refunds_budget_and_key() {
    let (_root, session, calls, release, endpoint) =
        fixture(Response::ControlRejected("already restarting".into())).await;
    release.add_permits(2);
    for _ in 0..2 {
        let error = session.execute(operation("one")).await.unwrap_err();
        assert!(error.rejected);
        assert_eq!(session.policy_report()["remaining_restarts"]["api"], 1);
        assert!(session.admission.lock().unwrap().operations.is_empty());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    endpoint.abort();
}

#[tokio::test]
async fn test_unknown_result_is_retained_without_budget_refund_or_replay() {
    let (_root, session, calls, release, endpoint) =
        fixture(Response::Error("reply lost after acceptance".into())).await;
    release.add_permits(1);
    assert_eq!(
        session.execute(operation("one")).await.unwrap_err().code,
        "outcome-unknown"
    );
    assert_eq!(
        session.execute(operation("one")).await.unwrap()["operation_status"],
        "unknown"
    );
    assert_eq!(session.policy_report()["remaining_restarts"]["api"], 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    endpoint.abort();
}

#[tokio::test]
async fn test_unexpected_control_reply_does_not_refund_or_allow_replay() {
    for response in [Response::Stopping, Response::Logs(Vec::new())] {
        let (_root, session, calls, release, endpoint) = fixture(response).await;
        release.add_permits(1);
        assert_eq!(
            session.execute(operation("one")).await.unwrap_err().code,
            "outcome-unknown"
        );
        assert_eq!(
            session.execute(operation("one")).await.unwrap()["operation_status"],
            "unknown"
        );
        assert_eq!(session.policy_report()["remaining_restarts"]["api"], 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        endpoint.abort();
    }
}

#[tokio::test]
async fn test_invalid_and_out_of_scope_requests_do_not_reserve_budget() {
    let (_root, session, calls, _release, endpoint) =
        fixture(Response::Restarted(ServiceSnapshot::default())).await;
    let mut invalid = operation("one");
    if let Operation::Restart { service, .. } = &mut invalid {
        *service = "missing".into();
    }
    assert_eq!(
        session.execute(invalid).await.unwrap_err().code,
        "invalid-request"
    );
    let mut other_scope = session.clone();
    other_scope.policy = parse_policy(&["other=1".into()], true).unwrap();
    assert_eq!(
        other_scope
            .execute(operation("one"))
            .await
            .unwrap_err()
            .code,
        "budget-out-of-scope"
    );
    assert_eq!(session.policy_report()["remaining_restarts"]["api"], 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    endpoint.abort();
}

#[tokio::test]
async fn test_accepted_duplicate_reports_state_without_forwarding_or_extending_retention() {
    let (_root, session, calls, _release, endpoint) = fixture(Response::Stopping).await;
    let expires = Instant::now() + DEDUP_RETENTION;
    session.admission.lock().unwrap().operations.insert(
        "one".into(),
        DedupEntry {
            digest: operation_digest(&operation("one")).unwrap(),
            state: DedupState::Accepted,
            expires,
            result_bytes: 0,
        },
    );
    assert_eq!(
        session.execute(operation("one")).await.unwrap()["operation_status"],
        "accepted"
    );
    assert_eq!(
        session.admission.lock().unwrap().operations["one"].expires,
        expires
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    endpoint.abort();
}

#[tokio::test(start_paused = true)]
async fn test_retention_keeps_active_entries_and_releases_terminal_bytes() {
    let mut admission = AdmissionState::default();
    for (key, state, bytes) in [
        ("accepted", DedupState::Accepted, 0),
        ("executing", DedupState::Executing, 0),
        ("completed", DedupState::Completed(json!({})), 2),
        ("unknown", DedupState::Unknown, 0),
    ] {
        admission.operations.insert(
            key.into(),
            DedupEntry {
                digest: "digest".into(),
                state,
                expires: Instant::now() + DEDUP_RETENTION,
                result_bytes: bytes,
            },
        );
    }
    tokio::time::advance(DEDUP_RETENTION).await;
    admission.expire();
    assert_eq!(admission.operations.len(), 2);
    assert_eq!(admission.result_bytes, 0);
    assert!(matches!(
        admission.operations["accepted"].state,
        DedupState::Accepted
    ));
}

#[test]
fn test_total_result_limit_retains_unknown_without_discarding_existing_results() {
    let mut admission = AdmissionState::default();
    for n in 0..34 {
        let key = format!("key-{n}");
        admission.operations.insert(
            key.clone(),
            DedupEntry {
                digest: "digest".into(),
                state: DedupState::Executing,
                expires: Instant::now() + DEDUP_RETENTION,
                result_bytes: 0,
            },
        );
        admission.complete(&key, &Ok(json!("x".repeat(MAX_DEDUP_RESULT_BYTES - 2))));
    }
    assert_eq!(admission.result_bytes, MAX_DEDUP_RESULT_BYTES_TOTAL);
    assert!(matches!(
        admission.operations["key-31"].state,
        DedupState::Completed(_)
    ));
    assert!(matches!(
        admission.operations["key-32"].state,
        DedupState::Unknown
    ));
    assert!(matches!(
        admission.operations["key-33"].state,
        DedupState::Unknown
    ));
}

#[tokio::test]
async fn test_capacity_rejects_before_effects_and_oversize_result_retains_tombstone() {
    let (_root, session, calls, release, endpoint) =
        fixture(Response::Restarted(ServiceSnapshot::default())).await;
    release.add_permits(1);
    assert_eq!(
        session.execute(operation("one")).await.unwrap()["operation_status"],
        "completed"
    );
    {
        let mut admission = session.admission.lock().unwrap();
        admission.result_bytes = 0;
        admission.complete(
            "one",
            &Ok(json!({"large": "x".repeat(MAX_DEDUP_RESULT_BYTES)})),
        );
    }
    assert_eq!(
        session.execute(operation("one")).await.unwrap()["operation_status"],
        "unknown"
    );
    assert_eq!(session.policy_report()["retained_result_bytes"], 0);
    {
        let mut admission = session.admission.lock().unwrap();
        for n in 1..MAX_DEDUP_ENTRIES {
            admission.operations.insert(
                format!("key-{n}"),
                DedupEntry {
                    digest: "digest".into(),
                    state: DedupState::Unknown,
                    expires: Instant::now() + DEDUP_RETENTION,
                    result_bytes: 0,
                },
            );
        }
    }
    assert_eq!(
        session.execute(operation("new")).await.unwrap_err().code,
        "deduplication-capacity"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    endpoint.abort();
}

#[test]
fn test_agent_policy_is_independent_bounded_and_strict() {
    assert!(parse_policy(&[], false).unwrap().restart_budgets.is_empty());
    assert!(!parse_policy(&[], false).unwrap().deduplicate);
    assert!(parse_policy(&[], true).unwrap().restart_budgets.is_empty());
    assert!(!parse_policy(&["api=1".into()], false).unwrap().deduplicate);
    let policy = parse_policy(&["api=2".into(), "worker=100".into()], true).unwrap();
    assert_eq!(policy.restart_budgets["api"], 2);
    assert_eq!(policy.restart_budgets["worker"], 100);
    assert!(policy.deduplicate);
    for values in [
        vec!["api=0".into()],
        vec!["api=101".into()],
        vec!["api=no".into()],
        vec!["api=1".into(), "api=2".into()],
        vec!["=2".into()],
        vec!["api=2=3".into()],
    ] {
        assert!(parse_policy(&values, false).is_err(), "{values:?}");
    }
    assert!(parse_policy(
        &(0..65)
            .map(|n| format!("service-{n}=1"))
            .collect::<Vec<_>>(),
        false
    )
    .is_err());
}

#[test]
fn test_agent_operation_digest_ignores_key_but_binds_payload_and_target() {
    let operation = |key: &str, service: &str| Operation::Restart {
        target: Target {
            instance_id: "instance".into(),
            run_id: "run".into(),
        },
        service: service.into(),
        operation_key: Some(key.into()),
    };
    assert_eq!(
        operation_digest(&operation("one", "api")).unwrap(),
        operation_digest(&operation("two", "api")).unwrap()
    );
    assert_ne!(
        operation_digest(&operation("one", "api")).unwrap(),
        operation_digest(&operation("one", "worker")).unwrap()
    );
}

#[test]
fn test_agent_accepted_guard_marks_cancelled_operation_unknown() {
    let admission = Arc::new(Mutex::new(AdmissionState::default()));
    admission.lock().unwrap().operations.insert(
        "key".into(),
        DedupEntry {
            digest: "digest".into(),
            state: DedupState::Executing,
            expires: Instant::now() + DEDUP_RETENTION,
            result_bytes: 0,
        },
    );
    drop(AcceptedGuard {
        admission: admission.clone(),
        key: Some("key".into()),
        armed: true,
    });
    assert!(matches!(
        admission.lock().unwrap().operations["key"].state,
        DedupState::Unknown
    ));
}
