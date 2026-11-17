//! T117, `[team_lanes]` (ADR 0018 §5.4, m11): the access document's entry
//! for every `team:` lane it does not list. Absent, team lanes fall to
//! `default_lane` and `SetAccess` encodes (and hashes) byte for byte as a
//! P9 document; present, it decides membership, writers, curators and
//! egress there, in the engine and in the oracle alike.
use contextdb_core::access::{AccessState, LogicalFacts};
use contextdb_model::*;
use contextdb_oracle::AccessModel;
use std::collections::{BTreeMap, BTreeSet};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
fn lane(members: &[&str], writers: &[&str]) -> LaneEntry {
    LaneEntry {
        members: set(members),
        writers: set(writers),
        curators: set(&["alice"]),
        egress: true,
        grants: BTreeMap::new(),
        deny: BTreeSet::new(),
    }
}
fn doc(team_lanes: Option<LaneEntry>) -> AccessDoc {
    let team_lanes = team_lanes.map(Box::new);
    AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals: [("alice".into(), 501), ("bob".into(), 502)].into(),
        groups: BTreeMap::new(),
        lanes: [("team:aa/listed".into(), lane(&["alice"], &["alice"]))].into(),
        code_lanes: LaneEntry::code_default(),
        personal_egress: true,
        default_lane: LaneEntry::owner_only(),
        team_lanes,
    }
}

/// SPEC §6.1's P9 encoding of an access document, field by field.
fn p9_bytes(d: &AccessDoc) -> Vec<u8> {
    let mut o = Vec::new();
    d.version.encode(&mut o);
    d.owner.encode(&mut o);
    d.principals.encode(&mut o);
    d.groups.encode(&mut o);
    d.lanes.encode(&mut o);
    d.code_lanes.encode(&mut o);
    d.personal_egress.encode(&mut o);
    d.default_lane.encode(&mut o);
    o
}

fn roundtrip(d: &AccessDoc) -> Vec<u8> {
    let mut bytes = Vec::new();
    d.encode(&mut bytes);
    let mut dec = Decoder::new(&bytes, DecodeLimits::DURABLE);
    let back = AccessDoc::decode(&mut dec).unwrap();
    assert_eq!(dec.remaining(), 0);
    assert_eq!(&back, d);
    bytes
}

#[test]
fn t117_an_absent_team_lanes_section_keeps_the_p9_bytes_and_hash() {
    for egress in [false, true] {
        let mut d = doc(None);
        d.personal_egress = egress;
        let bytes = roundtrip(&d);
        assert_eq!(bytes, p9_bytes(&d));
        let op = Op::SetAccess { doc: Box::new(d) };
        // The batch hash is over the same bytes as before P10.
        let mut b = Batch::new(vec![op]);
        b.channel = Channel::Admin;
        assert!(b.canonical_bytes().ends_with(&bytes));
    }
    // With the section the encoding differs and still round-trips.
    let with = doc(Some(lane(&["*members"], &["bob"])));
    assert_ne!(roundtrip(&with), p9_bytes(&with));
    // A flag byte outside {0, 1, 2, 3} is refused.
    let mut bad = p9_bytes(&doc(None));
    let at = bad.len()
        - {
            let mut tail = Vec::new();
            LaneEntry::owner_only().encode(&mut tail);
            tail.len()
        }
        - 1;
    bad[at] = 4;
    assert!(AccessDoc::decode(&mut Decoder::new(&bad, DecodeLimits::DURABLE)).is_err());
}

#[test]
fn t117_team_lanes_decide_unlisted_team_lanes_in_engine_and_oracle() {
    let bob = PrincipalId(16);
    let none = BTreeSet::new();
    for team_lanes in [None, Some(lane(&["*members"], &["bob"]))] {
        let d = doc(team_lanes.clone());
        let engine = AccessState::apply(None, &d).unwrap();
        let oracle = AccessModel::apply(None, &d).unwrap();
        assert_eq!(engine.principal_of_name("bob"), Some(bob));
        for l in [
            "team:aa/main",
            "team:bb/x",
            "team:aa/listed",
            "repo",
            "code:x",
        ] {
            let facts = LogicalFacts {
                key: "team/aa/k",
                lane: l,
                owner: PrincipalId::STORE_OWNER,
                dirs: &none,
            };
            let e = engine.authorized(bob, &facts);
            let o = oracle.authorized(bob, "team/aa/k", l, PrincipalId::STORE_OWNER, &none);
            assert_eq!((e.allowed, e.step.name()), o, "{l} with {team_lanes:?}");
            let want = match l {
                // Unlisted team lanes: `[team_lanes]`, else `default_lane`.
                "team:aa/main" | "team:bb/x" => team_lanes.is_some(),
                // Listed lanes and code lanes are unchanged.
                "code:x" => true,
                _ => false,
            };
            assert_eq!(e.allowed, want, "{l} with {team_lanes:?}");
            assert_eq!(
                *engine.entry(l),
                match (l, &team_lanes) {
                    ("team:aa/listed", _) => d.lanes["team:aa/listed"].clone(),
                    ("code:x", _) => LaneEntry::code_default(),
                    (t, Some(e)) if t.starts_with("team:") => e.clone(),
                    _ => LaneEntry::owner_only(),
                }
            );
        }
        // Recall's lane prefilter: a superset of `Authorized` (as for
        // `code:` lanes, a listed team lane is under the prefix too).
        let lanes = engine.read_lanes(bob).expect("bob is not in default_lane");
        assert_eq!(lanes.contains("team:cc/any"), team_lanes.is_some());
        for l in ["team:aa/main", "team:bb/x", "team:aa/listed"] {
            let facts = LogicalFacts {
                key: "team/aa/k",
                lane: l,
                owner: PrincipalId::STORE_OWNER,
                dirs: &none,
            };
            assert!(!engine.authorized(bob, &facts).allowed || lanes.contains(l));
        }
    }
}
