//! P9 G4 (ADR 0017 §11): requests from a non-owner principal. Arbitrary
//! bytes decoded as a request and sent as `bob` (agent socket, principal
//! 16) never panic, and the response never names a logical in a lane bob
//! cannot read: its key, body or content hash appear only when the request
//! itself carried them (an `Unavailable{target}` echoes the target).
#![no_main]
use contextdb_daemon::{
    actor,
    principals::PrincipalMap,
    proto::{Request, decode_request},
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, TailFate, session::SessionLog};
use libfuzzer_sys::fuzz_target;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::OnceLock,
};

const HIDDEN_KEY: &str = "vault-hidden-zq";
const HIDDEN_BODY: &str = "body-hidden-zq";
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const BOB: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(16),
};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
fn principals() -> PrincipalMap {
    PrincipalMap::new(
        Some("alice"),
        contextdb_proto::fsguard::current_uid(),
        [("bob".to_owned(), 4242, PrincipalId(16))],
    )
    .unwrap()
}
fn service(disk: &SimDisk) -> Service<SimDisk> {
    let dir = Path::new("/db");
    let (store, _) = Store::open(
        disk.clone(),
        dir,
        &OpenOptions {
            create: true,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    let s = Service::new(actor::spawn(store).unwrap(), log, events).unwrap();
    s.set_principals(principals());
    s
}
/// Stops the service as `Daemon::stop` does, releasing the store lock
/// before the next run opens it.
fn stop(s: Service<SimDisk>) {
    s.stop_indexer();
    s.stop_writer();
    let _ = s.actor.request(|reply| actor::Message::Shutdown { reply });
}
fn create(s: &Service<SimDisk>, key: &str, lane: &str, body: &str) {
    let r = s.handle(
        ADMIN,
        Request::Raw {
            blobs: vec![body.as_bytes().to_vec()],
            batch: Batch::new(vec![Op::CreateLogical {
                key: key.into(),
                kind: RecordKind::Fact,
                scope_lane: lane.into(),
                content: *blake3::hash(body.as_bytes()).as_bytes(),
                adopt_as_base: true,
            }]),
        },
    );
    assert!(format!("{:?}", r.response).starts_with("Committed"), "{r:?}");
}
/// The durable state every run starts from (each run on its own copy): an access document where bob
/// reads `repo` but not `vault`, a record in each, and a rule from the
/// visible record to the hidden one.
fn base() -> &'static SimDisk {
    static BASE: OnceLock<SimDisk> = OnceLock::new();
    BASE.get_or_init(|| {
        let disk = SimDisk::new();
        let s = service(&disk);
        let doc = AccessDoc {
            version: 1,
            owner: "alice".into(),
            principals: [
                ("alice".to_owned(), contextdb_proto::fsguard::current_uid()),
                ("bob".to_owned(), 4242),
            ]
            .into(),
            groups: Default::default(),
            lanes: [(
                "repo".to_owned(),
                LaneEntry {
                    members: set(&["*members"]),
                    writers: set(&["*members"]),
                    ..LaneEntry::owner_only()
                },
            )]
            .into(),
            code_lanes: LaneEntry::code_default(),
            personal_egress: false,
            default_lane: LaneEntry::owner_only(),
        };
        let r = s.handle(
            ADMIN,
            Request::SetAccess {
                doc,
                client_txn_id: None,
            },
        );
        assert!(format!("{:?}", r.response).starts_with("Committed"), "{r:?}");
        create(&s, "visible-fact", "repo", "a visible body");
        create(&s, HIDDEN_KEY, "vault", HIDDEN_BODY);
        stop(s);
        // A deep copy of the durable state: `restart` shares it, so writes
        // of one run would reach the next (a branch bob created from an
        // input holding a needle then read as a leak in a later run).
        disk.crash(TailFate::Torn, 0)
    })
}
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fuzz_target!(|data: &[u8]| {
    let Ok(mut request) = decode_request(data) else {
        return;
    };
    // Long polls would only slow the fuzzer down.
    match &mut request {
        Request::Changes { wait_ms, .. } => *wait_ms = 0,
        Request::Recall { index_wait_ms, .. } => *index_wait_ms = 0,
        _ => {}
    }
    // Every run starts from its own copy of the base state.
    let s = service(&base().crash(TailFate::Torn, 0));
    let response = format!("{:?}", s.handle(BOB, request.clone()));
    stop(s);
    let sent = format!("{request:?}");
    let hash = format!("{:?}", blake3::hash(HIDDEN_BODY.as_bytes()).as_bytes());
    for needle in [HIDDEN_KEY, HIDDEN_BODY, hash.as_str()] {
        if !contains(sent.as_bytes(), needle.as_bytes()) && !contains(data, needle.as_bytes()) {
            assert!(
                !contains(response.as_bytes(), needle.as_bytes()),
                "{needle} leaked to bob: {response}"
            );
        }
    }
});
