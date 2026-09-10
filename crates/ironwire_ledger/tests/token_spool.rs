use ironwire_ledger::token_spool::*;
#[test]
fn releasing_one_destination_never_deletes_another_or_a_late_turn() {
    let home = tempfile::tempdir().unwrap();
    let spool = TokenSpool::open(home.path(), 1024, 100).unwrap();
    let first = spool
        .record(
            "session",
            1,
            "openai.chat",
            true,
            (b"request", b"response"),
            1,
        )
        .unwrap();
    let a = spool
        .acquire(
            "session",
            std::slice::from_ref(&first.capture_id),
            "commons-a",
            2,
            90,
        )
        .unwrap();
    let b = spool
        .acquire(
            "session",
            std::slice::from_ref(&first.capture_id),
            "commons-b",
            2,
            90,
        )
        .unwrap();
    let late = spool
        .record("session", 2, "openai.chat", true, (b"late", b"turn"), 3)
        .unwrap();
    spool
        .release(&a.lease_id, &a.owner, &a.snapshot_digest, 4)
        .unwrap();
    assert!(
        spool
            .read(&b.lease_id, &b.owner, &first.capture_id, 4)
            .is_ok()
    );
    assert!(
        spool
            .release(&b.lease_id, "wrong-owner", &b.snapshot_digest, 4)
            .is_err()
    );
    spool
        .release(&b.lease_id, &b.owner, &b.snapshot_digest, 5)
        .unwrap();
    let remaining = spool.list("session", 5).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].capture_id, late.capture_id);
    spool
        .release(&b.lease_id, &b.owner, &b.snapshot_digest, 5)
        .unwrap();
}
#[test]
fn a_lease_survives_restart_and_pruning_and_checks_exact_bytes() {
    let home = tempfile::tempdir().unwrap();
    let (lease, id) = {
        let spool = TokenSpool::open(home.path(), 1024, 10).unwrap();
        let capture = spool
            .record(
                "session",
                1,
                "openai.chat",
                false,
                (b"{  \"x\":1}", b"original"),
                1,
            )
            .unwrap();
        let lease = spool
            .acquire(
                "session",
                std::slice::from_ref(&capture.capture_id),
                "owner",
                2,
                100,
            )
            .unwrap();
        (lease, capture.capture_id)
    };
    let spool = TokenSpool::open(home.path(), 1024, 10).unwrap();
    spool.prune(20).unwrap();
    assert_eq!(
        spool
            .read(&lease.lease_id, &lease.owner, &id, 20)
            .unwrap()
            .0,
        b"{  \"x\":1}"
    );
    std::fs::write(home.path().join(format!("{id}.res")), b"tampered").unwrap();
    assert!(spool.read(&lease.lease_id, &lease.owner, &id, 20).is_err());
    spool.prune(103).unwrap();
    assert!(spool.read(&lease.lease_id, &lease.owner, &id, 103).is_err());
}
#[test]
fn capacity_never_evicts_pending_captures_and_ids_cannot_select_paths() {
    let home = tempfile::tempdir().unwrap();
    let spool = TokenSpool::open(home.path(), 10, 100).unwrap();
    let capture = spool
        .record("session", 1, "openai.chat", false, (b"12345", b"67890"), 1)
        .unwrap();
    assert!(matches!(
        spool.record("session", 2, "openai.chat", false, (b"x", b"x"), 2),
        Err(SpoolError::Capacity)
    ));
    assert_eq!(spool.list("session", 2).unwrap().len(), 1);
    assert!(
        spool
            .acquire("other-session", &[capture.capture_id], "owner", 2, 10)
            .is_err()
    );
    assert!(
        spool
            .acquire("session", &["../private".into()], "owner", 2, 10)
            .is_err()
    );
}
#[test]
fn garbage_collection_does_not_follow_payload_symlinks() {
    #[cfg(unix)]
    {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"keep").unwrap();
        let spool = TokenSpool::open(home.path(), 1024, 10).unwrap();
        let capture = spool
            .record("session", 1, "openai.chat", false, (b"a", b"b"), 1)
            .unwrap();
        let path = home.path().join(format!("{}.res", capture.capture_id));
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        spool.prune(20).unwrap();
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"keep");
    }
}

#[test]
fn renewal_preserves_snapshot_and_has_a_hard_lifetime() {
    use ironwire_ledger::token_spool::MAX_LIFETIME_SECONDS;
    let home = tempfile::tempdir().unwrap();
    let spool = TokenSpool::open(home.path(), 1024, 10).unwrap();
    let capture = spool
        .record(
            "session",
            1,
            "openai.chat",
            false,
            (b"request", b"response"),
            1,
        )
        .unwrap();
    let lease = spool
        .acquire(
            "session",
            std::slice::from_ref(&capture.capture_id),
            "owner",
            2,
            20,
        )
        .unwrap();
    assert!(
        spool
            .renew(&lease.lease_id, "other", &lease.snapshot_digest, 3, 30)
            .is_err()
    );
    let end = spool
        .renew(
            &lease.lease_id,
            &lease.owner,
            &lease.snapshot_digest,
            3,
            MAX_LIFETIME_SECONDS,
        )
        .unwrap();
    assert_eq!(end, 1 + MAX_LIFETIME_SECONDS);
    spool.prune(100).unwrap();
    assert!(
        spool
            .read(&lease.lease_id, &lease.owner, &capture.capture_id, 100)
            .is_ok()
    );
    spool.prune(end).unwrap();
    assert!(
        spool
            .renew(
                &lease.lease_id,
                &lease.owner,
                &lease.snapshot_digest,
                end,
                1
            )
            .is_err()
    );
    spool
        .release(
            &lease.lease_id,
            &lease.owner,
            &lease.snapshot_digest,
            end + 1,
        )
        .unwrap();
}
