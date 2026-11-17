//! P10 G4: `accept` on hostile entries. After a real genesis, an arbitrary
//! commit object and tree are decided, never a panic; the replica's digest
//! changes only when the outcome says something was decided.
#![no_main]
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::gitobj;
use contextdb_team::ops::{TeamBatch, TeamOp};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

const TEAM: [u8; 16] = [0x4f; 16];
const KEY: [u8; 32] = [3; 32];

fn genesis() -> &'static (Replica, String) {
    static G: OnceLock<(Replica, String)> = OnceLock::new();
    G.get_or_init(|| {
        let doc = TeamDoc::from_toml(&format!(
            "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"a\"]\n\
             [members.a]\nkeys = [\"{}\"]\n[lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n",
            hex(&TEAM),
            sshsig::public_key(&KEY).to_line()
        ))
        .unwrap();
        let batch = TeamBatch {
            proposals: vec![vec![TeamOp::SetTeamAccess { doc: Box::new(doc) }]],
        }
        .encode();
        let tr: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", "0".into()),
            ("Ctx-Author", "a".into()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".into()),
            ("Ctx-Proposal", hex(&[1; 16])),
        ];
        let tree = vec![("batch".to_owned(), batch)];
        let t = hex(&gitobj::tree_id(ObjectFormat::Sha1, &tree).unwrap());
        let unsigned = commit::build(&t, &[], "g", &tr);
        let raw = commit::with_signature(&unsigned, &sshsig::sign(&KEY, "git", &unsigned), ObjectFormat::Sha1);
        let oid = hex(&gitobj::object_id(ObjectFormat::Sha1, "commit", &raw).unwrap());
        let mut r = Replica::new(TEAM, ObjectFormat::Sha1);
        let out = r.accept(&EntryInput {
            oid: &oid,
            seq: 0,
            first_in_generation: true,
            commit: &raw,
            tree: &tree,
        });
        assert!(matches!(out, Outcome::Decided(_)));
        (r, oid)
    })
}

fuzz_target!(|data: &[u8]| {
    let (base, oid) = genesis();
    let mut r = base.clone();
    let cut = data.first().map_or(0, |b| *b as usize).min(data.len());
    let (commit_bytes, tree_bytes) = data.split_at(cut);
    let tree = vec![
        ("batch".to_owned(), tree_bytes.to_vec()),
    ];
    let before = r.digest();
    let out = r.accept(&EntryInput {
        oid: &format!("{:040x}", blake3::hash(data).as_bytes()[0]),
        seq: 1,
        first_in_generation: false,
        commit: commit_bytes,
        tree: &tree,
    });
    let _ = oid;
    match out {
        Outcome::Stall(_) | Outcome::Ignored | Outcome::InvalidSuccessor(_) => {
            assert_eq!(r.digest(), before, "nothing decided, nothing changed")
        }
        _ => {}
    }
});
