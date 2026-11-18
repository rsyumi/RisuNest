use super::*;

#[test]
fn routine_limit_is_optional_positive_and_forbidden_for_exit_drain() {
    let request = |extra: serde_json::Value| {
        let mut value = serde_json::json!({"connectionId":"synthetic", "bindingAuthority":"0", "requestId":"test"});
        value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<Request>(value)
    };
    assert!(request(serde_json::json!({})).unwrap().check_publish_limit().is_ok());
    let limited = request(serde_json::json!({"turnLimit":4})).unwrap();
    assert_eq!(limited.turn_limit, Some(4));
    assert!(limited.check_publish_limit().is_ok());
    assert_eq!(request(serde_json::json!({"turnLimit":0})).unwrap().check_publish_limit().unwrap_err().kind, ErrorKind::PreconditionFailed);
    assert!(request(serde_json::json!({"turnLimit":-1})).is_err());
    assert!(request(serde_json::json!({"turnLimit":1.5})).is_err());
    let exit = serde_json::json!({"revision":"1", "libraryEpoch":"library", "selectionEpoch":"selection"});
    assert!(request(serde_json::json!({"exitTarget":exit})).unwrap().check_publish_limit().is_ok());
    assert_eq!(request(serde_json::json!({"turnLimit":4,"exitTarget":exit})).unwrap().check_publish_limit().unwrap_err().kind, ErrorKind::PreconditionFailed);
    assert_eq!(serde_json::to_value(PublicationResult::default()).unwrap()["more"], false);
}

#[test]
fn removed_connection_invalidates_inflight_cache_and_cancellation() {
    tauri::async_runtime::block_on(async {
        let id = format!("synthetic-removal-{}", uuid::Uuid::new_v4());
        let context = context(&id).unwrap();
        let cancellation = context.cancel.lock().unwrap().clone();
        invalidate_connection_cache(&id).await.unwrap();
        assert_eq!(cancellation.check().unwrap_err().kind, ErrorKind::Cancelled);
        invalidate_connection_cache("synthetic-never-opened").await.unwrap();
    });
}

#[test]
fn a_resume_inside_a_batch_applies_its_stored_pages_without_reading_their_segments_again() {
    tauri::async_runtime::block_on(async {
        use crate::external_storage::{lww_engine::Admission, lww_tests::CycleFixture};
        use crate::persistent_store::{lww::{ApplyReceive, UnitMutation}, WorkingSetCommit};
        async fn edit(a: &mut crate::persistent_store::PersistentStore, sender: &mut ExternalLwwEngine, index: usize) {
            a.commit(&WorkingSetCommit {
                expected_revision: a.revision().unwrap(),
                unit_mutations: Some(vec![UnitMutation::Set {
                    key: risunest_sync_wire::unit::UnitKey::new(&["root", "language"]).unwrap(),
                    value: serde_json::json!(format!("synthetic-{index}")),
                }]),
                ..Default::default()
            }).unwrap();
            sender.publish(a, 0.into(), &[], &Cancellation::default()).await.unwrap();
        }
        let mut f = CycleFixture::new();
        for index in 0..6 {
            edit(&mut f.a, &mut f.sender, index).await;
        }
        let segments = f.sender.listing(&Cancellation::default()).await.unwrap()
            .into_iter().map(|receipt| receipt.locator.object).collect::<Vec<_>>();
        assert_eq!(segments.len(), 6);
        let now = f.now_ms();
        let provider = f.provider.clone();
        let mut session = Session::new();
        session.engine = Some(f.receiver);
        let checkpoints = Default::default();
        let mut applied = 0;
        loop {
            let (page, _) = receive_page(&mut f.b, &mut session, &checkpoints, 0.into(), false).await.unwrap();
            let Some(page) = page else { break };
            f.b.lww_stage_receive(&page).unwrap();
            f.b.lww_apply_receive(&ApplyReceive { header: page.header.clone(), generating: vec![] }).unwrap();
            f.b.lww_finish_receive(&page.header).unwrap();
            applied += 1;
            if applied == 1 {
                // The return to the screen comes while the first batch has pages left; the clock is read again.
                session.resume(Cancellation::default());
                session.engine.as_mut().unwrap().admission = Some(Admission::synthetic(now));
            }
            if applied == 6 {
                // Another return comes right after the batch's last page, and another device has published since.
                edit(&mut f.a, &mut f.sender, 6).await;
                session.resume(Cancellation::default());
                session.engine.as_mut().unwrap().admission = Some(Admission::synthetic(now));
            }
        }
        assert_eq!(applied, 7, "the receive after a return stops at the stored pages without listing again");
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "synthetic-6");
        assert_eq!(segments.iter().map(|id| provider.read_attempts(id)).collect::<Vec<_>>(), [1; 6],
            "a segment whose pages were stored is read again");
    });
}

#[test]
fn invalid_routine_admission_refreshes_once_and_preserves_clock_failure_details() {
    for (phase, corrected, reason) in [("entry", false, None), ("entry", true, None),
        ("entry", false, Some("server-offset")), ("entry", false, Some("date-missing")),
        ("entry", false, Some("date-invalid")), ("entry", false, Some("sample-cached")),
        ("publish", false, None), ("publish", false, Some("date-missing")),
        ("receive", false, None), ("receive", false, Some("date-missing"))] {
        tauri::async_runtime::block_on(async move {
            use crate::external_storage::{fake, lww_engine::Admission, lww_tests::CycleFixture, providers::webdav, wire_fixture::{Reply, WireServer}};
            use std::sync::atomic::{AtomicI64, Ordering};
            struct AdjustableClock(AtomicI64);
            impl crate::external_storage::http::Clock for AdjustableClock {
                fn now_ms(&self) -> u64 {
                    (runtime::now_ms() as i64 + self.0.load(Ordering::SeqCst)) as u64
                }
            }
            let mut fixture = CycleFixture::new();
            let now = runtime::now_ms();
            let listing = |offset: u64, entries: &[&str]| Reply::Http {
                status: 207,
                headers: vec![("Content-Type".into(), "application/xml".into()), ("Date".into(),
                    httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_millis(now + offset)))],
                body: format!("<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\">{}</D:multistatus>",
                    entries.iter().map(|href| format!("<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>")).collect::<String>()).into_bytes(),
            };
            let mut refreshed = listing(if reason == Some("server-offset") { 20 * 60_000 } else { 0 },
                &["/synthetic/sync/segments/"]);
            if let Reply::Http { headers, .. } = &mut refreshed {
                match reason {
                    Some("date-missing") => headers.retain(|(name, _)| name != "Date"),
                    Some("date-invalid") => {
                        headers.iter_mut().find(|(name, _)| name == "Date").unwrap().1 = "synthetic-private-header".into();
                    }
                    Some("sample-cached") => headers.push(("Age".into(), "12".into())),
                    _ => {}
                }
            }
            let mut replies = vec![listing(0, &["/synthetic/sync/", "/synthetic/sync/descriptors/"])];
            if phase == "receive" {
                replies.extend([listing(0, &["/synthetic/sync/snapshots/"]), listing(0, &["/synthetic/sync/segments/"])]);
            }
            replies.push(refreshed);
            if phase == "receive" && reason.is_none() {
                replies.extend([listing(0, &["/synthetic/sync/snapshots/"]), listing(0, &["/synthetic/sync/segments/"])]);
            }
            let server = WireServer::start(replies);
            let mut dependencies = fake::loopback_dependencies(fake::MemoryVault::with("synthetic", b"password"), now);
            let clock = Arc::new(AdjustableClock(AtomicI64::new(if corrected { -6000 } else { 0 })));
            dependencies.dependencies.clock = clock.clone();
            let provider = webdav::create(dependencies.dependencies.clone()).unwrap();
            let config = ConnectionConfig { provider: "webdav".into(), profile: None,
                endpoint: server.url.as_str().into(), account_id: "synthetic".into(),
                location: BTreeMap::from([("root".into(), "sync".into())]), oauth_profile: None };
            let before_open = Instant::now();
            let (repository, _) = provider.open_repository(&config, &SecretRef("synthetic".into()), OpenMode::Existing, &Cancellation::default()).await.unwrap();
            clock.0.store(0, Ordering::SeqCst);
            let mut engine = fixture.receiver;
            engine.provider = provider;
            engine.repository = repository;
            let mut admission = Admission::synthetic(runtime::now_ms());
            if phase == "entry" { admission.age_for_test(std::time::Duration::from_secs(16 * 60)); }
            engine.admission = Some(admission);
            if phase == "entry" { assert_eq!(engine.admitted_upper().unwrap_err().kind, ErrorKind::ClockSkew); }
            let mut session = Session::new();
            session.engine = Some(engine);
            session.dependencies = Some(dependencies.dependencies);
            // A recent cached sample must also survive validation against the current wall clock.
            session.fresh_after = if corrected { before_open } else { Instant::now() };
            let result = if phase == "entry" {
                admit_clock(&mut session).await
            } else {
                admit_clock(&mut session).await.unwrap();
                assert!(!session.clock_refresh_attempted);
                let held = super::super::clock::future_change("receive", now + 600_000, now);
                assert_eq!(retry_clock_failure(&mut session, held.clone()).await.unwrap_err().cause.0, held.cause.0);
                assert!(!session.clock_refresh_attempted, "a future stamp does not invalidate a valid admission");
                session.engine.as_mut().unwrap().admission.as_mut().unwrap()
                    .age_for_test(std::time::Duration::from_secs(16 * 60));
                if phase == "publish" {
                    let published = publish_round(&mut session, &mut fixture.b, 0.into(), &[], Some(1)).await;
                    published.map(|value| assert_eq!(value.segments.0, 0))
                } else {
                    let page = receive_page(&mut fixture.b, &mut session, &Default::default(), 0.into(), false).await;
                    page.map(|value| assert!(value.0.is_none()))
                }
            };
            if let Some(reason) = reason {
                let error = crate::native_log::logged::<(), _>("synthetic-clock-refresh", result).unwrap_err();
                assert_eq!(error.kind, ErrorKind::ClockSkew);
                let detail = error.cause.0.as_deref().unwrap();
                assert!(detail.contains(&format!("stage=sample reason={reason}")), "{detail}");
                assert!(detail.contains("refresh_attempted=true"));
                for (value, code_key) in [(serde_json::to_value(&error).unwrap(), "kind"), (runtime::error_dto(&error), "code")] {
                    assert_eq!(value[code_key], "clockSkew");
                    assert_eq!(value["detail"], detail);
                }
                let entry = crate::native_log::global_state().tail(None).into_iter().rev()
                    .find(|entry| entry.message.starts_with("synthetic-clock-refresh failed:")).unwrap();
                assert!(entry.message.contains("code=clockSkew"));
                assert!(entry.message.contains(detail));
                assert!(!entry.message.contains("synthetic-private-header"));
                assert!(session.engine.as_ref().unwrap().admitted_upper().is_err());
                assert!(session.engine.as_ref().unwrap().admission.is_none());
            } else {
                result.unwrap();
                assert!(session.engine.as_ref().unwrap().admitted_upper().is_ok());
                admit_clock(&mut session).await.unwrap();
                let requests_before = server.requests.lock().unwrap().len();
                session.engine.as_mut().unwrap().admission.as_mut().unwrap()
                    .age_for_test(std::time::Duration::from_secs(16 * 60));
                let expired = session.engine.as_ref().unwrap().admitted_upper().unwrap_err();
                let error = retry_clock_failure(&mut session, expired).await.unwrap_err();
                assert!(error.cause.0.unwrap().contains("reason=sample-expired"));
                assert_eq!(server.requests.lock().unwrap().len(), requests_before,
                    "entry and continuation share a single refresh budget");
            }
            if phase != "receive" { assert_eq!(server.requests.lock().unwrap().len(), 2); }
        });
    }
}
