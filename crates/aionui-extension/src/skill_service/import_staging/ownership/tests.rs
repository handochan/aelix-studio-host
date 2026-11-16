use super::*;

fn fixture() -> (tempfile::TempDir, PathBuf, OwnerLease) {
    let tmp = tempfile::tempdir().unwrap();
    let stage = tmp.path().join("stage");
    fs::create_dir(&stage).unwrap();
    fs::write(stage.join("SKILL.md"), "preserve payload").unwrap();
    let owner = OwnerLease::publish(&stage, Kind::Install).unwrap();
    (tmp, stage, owner)
}

#[test]
fn released_shared_lease_can_be_reacquired_exclusively() {
    let (_tmp, stage, mut owner) = fixture();
    assert!(owner.try_exclusive());
    assert!(OwnerLease::open_verified(&stage).is_none());
    drop(owner);
    assert!(OwnerLease::open_verified(&stage).is_some());
}

#[test]
fn owner_rewritten_during_unlock_gap_is_preserved() {
    let (_tmp, stage, mut owner) = fixture();
    let mut replacement = owner.record.clone();
    replacement.token.push_str("-new-owner");
    let marker = owner.marker.clone();
    assert!(!owner.try_exclusive_with(|| {
        fs::write(&marker, serde_json::to_vec(&replacement).unwrap()).unwrap();
    }));
    let current = OwnerLease::open_verified(&stage).unwrap();
    assert_eq!(current.record, replacement);
    assert_eq!(fs::read_to_string(stage.join("SKILL.md")).unwrap(), "preserve payload");
}

#[test]
fn marker_replaced_during_unlock_gap_is_preserved() {
    let (tmp, stage, mut owner) = fixture();
    let mut replacement = owner.record.clone();
    replacement.token.push_str("-replacement");
    let pending = tmp.path().join("replacement");
    fs::write(&pending, serde_json::to_vec(&replacement).unwrap()).unwrap();
    let marker = owner.marker.clone();
    assert!(!owner.try_exclusive_with(|| {
        fs::rename(&pending, &marker).unwrap();
    }));
    // The cached handle still refers to the old record. Reopening the current
    // path is essential: an exclusive lock on that old inode is not authority.
    assert_eq!(OwnerLease::open_verified(&stage).unwrap().record, replacement);
    assert_eq!(fs::read_to_string(stage.join("SKILL.md")).unwrap(), "preserve payload");
}

#[test]
fn competing_shared_lease_during_unlock_gap_prevents_reclamation() {
    let (_tmp, stage, mut owner) = fixture();
    let mut contender = None;
    assert!(!owner.try_exclusive_with(|| {
        contender = Some(OwnerLease::open_verified(&stage).unwrap());
    }));
    assert!(contender.is_some());
    assert_eq!(fs::read_to_string(stage.join("SKILL.md")).unwrap(), "preserve payload");
    drop(contender);
    // A later sweep opens its own shared lease before the next attempt.
    assert!(OwnerLease::open_verified(&stage).unwrap().try_exclusive());
}
