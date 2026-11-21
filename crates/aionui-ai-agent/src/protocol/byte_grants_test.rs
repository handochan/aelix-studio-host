use super::*;
use std::task::Poll;

fn admission() -> RelayWriteAdmission {
    RelayWriteAdmission::new(None, Instant::now() + PREPARE_TTL, |write| write())
}

fn local(path: &str) -> ChatFileRef {
    ChatFileRef::Local { path: path.into() }
}

fn request(id: &str, reader: &str, offset: usize, action: ReadAction) -> ReadRequest {
    ReadRequest {
        session_id: "session".into(),
        binding_id: "binding".into(),
        reference: id.into(),
        reader_id: reader.into(),
        offset,
        action,
    }
}

fn read_action(max_bytes: usize) -> ReadAction {
    ReadAction::Read { max_bytes }
}

fn reply(binding: &ByteGrantBinding, request: &ReadRequest) -> Option<Value> {
    let (line, _) = binding.try_reply(request, "7", |line| Poll::Ready(Ok(line.len())))?;
    assert!(line.len() - 1 <= FRAME_LIMIT);
    Some(serde_json::from_str::<Value>(&line).unwrap()["result"].clone())
}

#[tokio::test(start_paused = true)]
async fn byte_grant_prepare_joins_and_accounts_for_an_abandoned_preparation() {
    let binding = Arc::new(ByteGrantBinding::default());
    let pool = Arc::new(ByteGrantPool::default());
    let (send, copied) = tokio::sync::oneshot::channel();
    let first = binding
        .prepare(&pool, "owner", "p".into(), &[local("a")], None, admission(), || async {
            copied.await.map_err(|_| ByteGrantError::File)
        })
        .unwrap();
    drop(first);
    let unexpected = || async { panic!("duplicate preparation started another copy") };
    let joined = binding
        .prepare(&pool, "owner", "p".into(), &[local("a")], None, admission(), unexpected)
        .unwrap();
    assert!(matches!(
        binding.prepare(&pool, "owner", "p".into(), &[local("b")], None, admission(), unexpected),
        Err(ByteGrantError::Conflict)
    ));
    assert!(matches!(
        binding.prepare(
            &pool,
            "owner",
            "p".into(),
            &[local("a")],
            Some(1),
            admission(),
            unexpected
        ),
        Err(ByteGrantError::Conflict)
    ));
    assert!(matches!(
        binding.prepare(
            &pool,
            "owner",
            "p2".into(),
            &[local("a")],
            None,
            admission(),
            unexpected
        ),
        Err(ByteGrantError::Limit)
    ));
    assert_eq!(pool.0.lock().unwrap().bytes, SNAPSHOT_LIMIT);
    assert!(matches!(joined.result().await, Err(ByteGrantError::Expired)));
    assert_eq!(pool.0.lock().unwrap().bytes, SNAPSHOT_LIMIT);
    send.send(vec![b"copied after deadline".to_vec()]).unwrap();
    tokio::task::yield_now().await;
    assert_eq!(pool.0.lock().unwrap().bytes, 0);

    let prepared = binding
        .prepare(&pool, "owner", "p".into(), &[local("a")], None, admission(), || async {
            Ok(vec![b"fixed".to_vec()])
        })
        .unwrap()
        .result()
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(31)).await;
    let replay = binding
        .prepare(&pool, "owner", "p".into(), &[local("a")], None, admission(), unexpected)
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(prepared.grants[0].id, replay.grants[0].id);
    assert_eq!(prepared.expires_at_ms, replay.expires_at_ms);
    assert_eq!(replay.expires_in_ms, 29_000);
    binding.close();
    assert_eq!(pool.0.lock().unwrap().bytes, 0);
}

#[tokio::test(start_paused = true)]
async fn byte_grant_read_deadlines_and_finish_replay() {
    let pool = Arc::new(ByteGrantPool::default());
    let binding = Arc::new(ByteGrantBinding::default());
    let prepared = binding
        .prepare(
            &pool,
            "owner",
            "p".into(),
            &[local("1"), local("2"), local("3")],
            None,
            admission(),
            || async { Ok(vec![b"abcdef".to_vec(), b"unused".to_vec(), b"timeout".to_vec()]) },
        )
        .unwrap()
        .result()
        .await
        .unwrap();
    let id = &prepared.grants[0].id;
    let first = request(id, "reader", 0, read_action(3));
    assert!(binding.try_reply(&first, "1", |_| Poll::Pending).is_some());
    assert!(
        binding
            .try_reply(&request(id, "other", 0, read_action(3)), "2", |_| Poll::Pending)
            .is_some()
    );
    assert_eq!(reply(&binding, &first).unwrap()["data"], "YWJj");
    assert!(reply(&binding, &request(id, "other", 0, read_action(3))).is_none());
    assert!(reply(&binding, &request(id, "reader", 4, read_action(3))).is_none());
    assert!(reply(&binding, &request(id, "reader", 6, ReadAction::Finish)).is_none());
    let final_chunk = request(id, "reader", 3, read_action(3));
    let last = reply(&binding, &final_chunk).unwrap();
    assert_eq!(last["eof"], true);
    assert_eq!(last["data"], "ZGVm");
    assert!(reply(&binding, &first).is_none(), "only the last offset is replayable");
    let finish = request(id, "reader", 6, ReadAction::Finish);
    assert!(binding.try_reply(&finish, "3", |_| Poll::Pending).is_some());
    assert_eq!(reply(&binding, &final_chunk), Some(last));
    assert_eq!(reply(&binding, &finish), Some(json!({"released":true})));
    assert_eq!(reply(&binding, &finish), Some(json!({"released":true})));
    assert!(reply(&binding, &final_chunk).is_none());
    assert_eq!(pool.0.lock().unwrap().bytes, 13);

    let timeout = request(&prepared.grants[2].id, "reader", 0, read_action(3));
    reply(&binding, &timeout).unwrap();
    tokio::time::advance(READ_TTL).await;
    assert!(reply(&binding, &timeout).is_none());
    let unused = request(&prepared.grants[1].id, "reader", 0, read_action(3));
    assert!(binding.authorization(&unused.reference).is_some());
    tokio::time::advance(UNUSED_TTL - READ_TTL).await;
    assert!(reply(&binding, &unused).is_none());
    assert_eq!(pool.0.lock().unwrap().bytes, 0);
    tokio::time::advance(FINISHED_TTL - UNUSED_TTL).await;
    assert!(reply(&binding, &finish).is_none());
    assert_eq!(pool.0.lock().unwrap().records, 0);
}

#[test]
fn byte_grant_owner_host_and_ram_reservations() {
    let pool = Arc::new(ByteGrantPool::default());
    let a = pool.reserve("one").unwrap();
    let b = pool.reserve("one").unwrap();
    assert!(matches!(pool.reserve("one"), Err(ByteGrantError::Limit)));
    let c = pool.reserve("two").unwrap();
    let d = pool.reserve("two").unwrap();
    assert!(matches!(pool.reserve("three"), Err(ByteGrantError::Limit)));
    drop((a, b, c, d));
    let mut live = Vec::new();
    for _ in 0..5 {
        let mut reservation = pool.reserve("one").unwrap();
        reservation.settle(SNAPSHOT_LIMIT);
        live.push(reservation);
    }
    assert!(matches!(pool.reserve("two"), Err(ByteGrantError::Limit)));
    drop(live);
    assert_eq!(pool.0.lock().unwrap().records, 0);
}

#[test]
fn byte_grant_rejects_ambiguous_reverse_parameters() {
    use serde_json::value::RawValue;
    let input = |action| {
        format!(
            r#"{{"sessionId":"s","bindingId":"b","ref":"8ec16a6b-d75c-41c1-94b5-ef19b2b9d4ec","readerId":"r","offset":0,{action}}}"#
        )
    };
    for action in [
        r#""maxBytes":0"#,
        r#""maxBytes":147264,"finish":true"#,
        r#""maxBytes":1,"maxBytes":2"#,
        r#""maxBytes":1,"finish":null"#,
        r#""finish":false"#,
        r#""finish":true,"extra":"ignored""#,
    ] {
        assert!(ReadRequest::parse(&RawValue::from_string(input(action)).unwrap()).is_none());
    }
    assert!(ReadRequest::parse(&RawValue::from_string(input(r#""maxBytes":147264"#)).unwrap()).is_some());
}
