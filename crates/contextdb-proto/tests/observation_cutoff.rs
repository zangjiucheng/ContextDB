use contextdb_model::{Fact, Wire};
use contextdb_proto::outbox::{Outbox, Reply, StreamState, hex};
use std::time::Instant;
#[test]
fn cutoff_is_pre_capture_sequence_not_ack_and_fails_closed_without_panics() {
    let root = std::env::temp_dir().join(format!("p5a-cutoff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let outbox = Outbox::new(&root);
    let stream = [3; 16];
    assert!(outbox.observation_cutoff().unwrap().is_empty());
    outbox
        .update_state(&stream, |s| s.harness = "test".into())
        .unwrap();
    outbox.append(&stream, None, Fact::StreamEnd).unwrap();
    let before = outbox.observation_cutoff().unwrap();
    outbox
        .deliver(&stream, std::time::Duration::from_secs(1), |packets, _| {
            Ok(Reply::Applied(packets.last().unwrap().seq))
        })
        .unwrap();
    assert_eq!(before, outbox.observation_cutoff().unwrap());
    outbox.append(&stream, None, Fact::StreamEnd).unwrap();
    assert_ne!(before, outbox.observation_cutoff().unwrap());
    assert!(outbox.observation_cutoff_until(Instant::now()).is_err());
    outbox.update_state(&stream, |s| s.dropped = 1).unwrap();
    assert!(outbox.observation_cutoff().is_err());
    outbox.update_state(&stream, |s| s.dropped = 0).unwrap();
    let state = root.join("outbox").join(format!("{}.state", hex(&stream)));
    std::fs::write(&state, vec![0; (1 << 20) + 1]).unwrap();
    assert!(outbox.observation_cutoff().is_err());
    std::fs::write(state, StreamState::default().canonical_bytes()).unwrap();
    let unicode = root.join("outbox").join(format!("{}.seq", "é".repeat(16)));
    std::fs::write(&unicode, []).unwrap();
    assert!(outbox.observation_cutoff().is_err());
    std::fs::remove_file(unicode).unwrap();
    let seq = root.join("outbox").join(format!("{}.seq", hex(&stream)));
    std::fs::write(seq, vec![0; 10000]).unwrap();
    assert!(outbox.observation_cutoff().is_err());
    let _ = std::fs::remove_dir_all(root);
}
