//! The `access.toml` parser (ADR 0017 §4.1, §11 G4): any input parses or is
//! refused, never a panic; a document that parses either validates or is
//! refused by the kernel; and for one that validates, `read_lanes` covers
//! every logical `Authorized` lets a principal read.
#![no_main]
use contextdb_core::access::{AccessState, LogicalFacts};
use contextdb_model::PrincipalId;
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeSet;

fuzz_target!(|data: &[u8]| {
    let src = String::from_utf8_lossy(data);
    let Ok(doc) = contextdb_cli::access_doc::access_doc(&src, 501) else {
        return;
    };
    let Ok(state) = AccessState::apply(None, &doc) else {
        return;
    };
    let mut ids: Vec<PrincipalId> = doc
        .principals
        .keys()
        .filter_map(|n| state.principal_of_name(n))
        .collect();
    ids.push(PrincipalId(u32::MAX - 1));
    let mut probe: Vec<String> = doc.lanes.keys().cloned().collect();
    probe.extend(doc.principals.keys().map(|n| format!("personal:{n}")));
    probe.extend(["code:x".to_owned(), "elsewhere".to_owned()]);
    let dirs: Vec<BTreeSet<String>> = std::iter::once(BTreeSet::new())
        .chain(
            (doc.lanes.values())
                .flat_map(|e| e.grants.values().flatten())
                .take(4)
                .map(|d| [d.clone()].into()),
        )
        .collect();
    let keys: Vec<String> = std::iter::once("k".to_owned())
        .chain(
            (doc.lanes.values())
                .flat_map(|e| e.deny.iter().map(|(_, p)| format!("{p}k")))
                .take(4),
        )
        .collect();
    for &p in &ids {
        let Some(r) = state.read_lanes(p) else {
            continue;
        };
        for lane in &probe {
            for d in &dirs {
                for key in &keys {
                    let facts = LogicalFacts {
                        key,
                        lane,
                        owner: PrincipalId::STORE_OWNER,
                        dirs: d,
                    };
                    if state.authorized(p, &facts).allowed {
                        assert!(r.contains(lane), "{p:?} reads {lane} outside {r:?}");
                    }
                }
            }
        }
    }
});
