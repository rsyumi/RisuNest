use super::*;
use risunest_sync_wire::library_format::{HEADER, MAX_HEADER, PRECONDITION_HEADER};
use serde_json::json;

fn pending(store: &PersistentStore) -> usize {
    store.lww_read_outbox(store.lww_binding_authority().unwrap(), 256).unwrap().entries.len()
}

/// Publishes one unit as a released app does, declaring no level.
fn released_write(server: &Store) {
    let credential = server.add_device().unwrap();
    let device = server.authenticate(&credential.library_id, &credential.token).unwrap();
    let writer = uuid::Uuid::new_v4().to_string();
    let change = UnitChange {
        key: UnitKey::new(&["root", "askRemoval"]).unwrap(),
        stamp: Stamp { physical_ms: 1.into(), logical: 0, writer_id: writer.clone() },
        value: UnitValue::inline(b"true").unwrap(),
    };
    let request = PushRequest {
        library_id: credential.library_id,
        writer_id: writer,
        operation_id: uuid::Uuid::new_v4().to_string(),
        changes: vec![change],
    };
    server.push(&device, &request).unwrap();
}

#[test]
fn an_empty_library_is_switched_before_the_first_send_or_receive() {
    for send in [true, false] {
        let server = LocalServerFixture::new();
        let (_a, mut a) = local();
        let ca = server.client(&a);
        if send {
            save(&mut a, &["root", "language"], json!("ja"));
            drain_publications(&ca, &mut a, &[]).unwrap();
            assert_eq!(pending(&a), 0);
        } else {
            receive_available(&ca, &mut a, &[]).unwrap();
        }
        assert_eq!(server.server.library_format(), 2);
    }
}

#[test]
fn a_library_with_content_below_the_level_pauses_until_it_is_switched() {
    let server = LocalServerFixture::new();
    released_write(&server.server);
    let (_a, mut a) = local();
    let ca = server.client(&a);
    save(&mut a, &["root", "language"], json!("ja"));
    let request = header(&a);
    let sent = ca.push(&mut a, &request, &[]).unwrap_err();
    let received = receive_cycle(&ca, &mut a, &[]).err().unwrap();
    let checked = ca.check_library_format(&mut a, &request, false).unwrap_err();
    for error in [sent, received, checked] {
        assert_eq!((error.code.as_str(), error.retryable), ("library-format-switch-required", false));
    }
    assert_eq!((server.server.library_format(), pending(&a)), (1, 1));
    ca.check_library_format(&mut a, &request, true).unwrap();
    assert_eq!(server.server.library_format(), 2);
    drain_publications(&ca, &mut a, &[]).unwrap();
    receive_available(&ca, &mut a, &[]).unwrap();
    assert_eq!(pending(&a), 0);
    assert_eq!(a.read_root(None).unwrap().value["askRemoval"], json!(true));
}

/// A server whose replies carry `headers` in place of the level headers.
fn reporting(headers: &'static [(&'static str, &'static str)]) -> LocalServerFixture {
    LocalServerFixture::with_router(move |router| {
        router.layer(axum::middleware::map_response(move |mut response: axum::response::Response| async move {
            let map = response.headers_mut();
            map.remove(HEADER);
            map.remove(MAX_HEADER);
            for (name, value) in headers {
                map.insert(*name, value.parse().unwrap());
            }
            response
        }))
    })
}

#[test]
fn a_reply_without_this_level_stops_sync_and_no_level_is_pushed() {
    for (headers, code) in [
        (&[][..], "sync-server-update-required"),
        (&[(HEADER, "1"), (MAX_HEADER, "1")][..], "sync-server-update-required"),
        (&[(HEADER, "3"), (MAX_HEADER, "3")][..], "app-update-required"),
        (&[(HEADER, "2")][..], "invalid-library-format"),
    ] {
        let server = reporting(headers);
        let (_a, mut a) = local();
        let ca = server.client(&a);
        save(&mut a, &["root", "language"], json!("ja"));
        let request = header(&a);
        assert_eq!(ca.push(&mut a, &request, &[]).unwrap_err().code, code);
        assert_eq!(ca.check_library_format(&mut a, &request, true).unwrap_err().code, code);
        assert_eq!(server.server.head().unwrap().seq.as_str(), "0");
        assert_eq!(pending(&a), 1);
    }
}

#[test]
fn a_library_seen_at_this_level_is_checked_again_once_a_reply_reports_otherwise() {
    use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
    let rolled_back = Arc::new(AtomicBool::new(false));
    let server = LocalServerFixture::with_router({
        let rolled_back = rolled_back.clone();
        move |router| {
            router.layer(axum::middleware::map_response(move |mut response: axum::response::Response| {
                let rolled_back = rolled_back.clone();
                async move {
                    if rolled_back.load(Ordering::SeqCst) {
                        response.headers_mut().remove(HEADER);
                        response.headers_mut().remove(MAX_HEADER);
                    }
                    response
                }
            }))
        }
    });
    let (_a, mut a) = local();
    let ca = server.client(&a);
    save(&mut a, &["root", "language"], json!("ja"));
    drain_publications(&ca, &mut a, &[]).unwrap();
    let request = header(&a);
    // A switch reads the level again although this process has seen it.
    rolled_back.store(true, Ordering::SeqCst);
    assert_eq!(ca.check_library_format(&mut a, &request, true).unwrap_err().code, "sync-server-update-required");
    rolled_back.store(false, Ordering::SeqCst);
    ca.check_library_format(&mut a, &request, true).unwrap();
    // Once a reply arrives without the level headers, the next check reads them again.
    rolled_back.store(true, Ordering::SeqCst);
    save(&mut a, &["root", "language"], json!("en"));
    drain_publications(&ca, &mut a, &[]).unwrap();
    assert_eq!(ca.check_library_format(&mut a, &request, false).unwrap_err().code, "sync-server-update-required");
}

#[test]
fn a_lost_race_for_an_empty_library_follows_the_level_its_refusal_reports() {
    for (level, outcome) in [("1", Some("library-format-switch-required")), ("2", None)] {
        let server = LocalServerFixture::with_router(move |router| {
            router.layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| async move {
                    if request.headers().contains_key(PRECONDITION_HEADER) {
                        // Read the push before refusing it, as the Sync server does; an unread body
                        // can reset the connection before the client reads the refusal.
                        axum::body::to_bytes(request.into_body(), MAX_METADATA_BYTES).await.unwrap();
                        return axum::response::Response::builder()
                            .status(409)
                            .header(HEADER, level)
                            .header(MAX_HEADER, "2")
                            .body(axum::body::Body::from(r#"{"error":"library-not-empty"}"#))
                            .unwrap();
                    }
                    next.run(request).await
                },
            ))
        });
        let (_a, mut a) = local();
        let ca = server.client(&a);
        let request = header(&a);
        let result = ca.check_library_format(&mut a, &request, false);
        assert_eq!(result.err().map(|error| error.code), outcome.map(str::to_owned));
    }
}
