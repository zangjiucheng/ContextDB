//! One real `claude -p` call through the sandboxed adapter (manual check of
//! the flags against the installed CLI; makes one LLM call).
use contextdb_model::*;
use contextdb_validate::claude::ClaudeCli;
use contextdb_validate::*;
use std::time::{Duration, Instant};

fn main() {
    let item = |h: &str, body: &str| Item {
        handle: h.into(),
        key: h.split('@').next().unwrap().into(),
        kind: RecordKind::Decision,
        body: body.into(),
    };
    let task = Task {
        job: [7; 16],
        branch: MAIN,
        rule: RuleId(0),
        curvec: vec![(LogicalId(0), RevisionId(2))],
        conclusion: item(
            "plan@1",
            "Use the official PostgreSQL driver for all storage.",
        ),
        premises: vec![PremiseChange {
            key: "db".into(),
            kind: RecordKind::Decision,
            asserted: item("db@0", "The service stores its data in PostgreSQL 15."),
            current: item(
                "db@2",
                "The service stores its data in MongoDB; PostgreSQL is dropped.",
            ),
        }],
        pair: None,
    };
    let cli = ClaudeCli {
        name: "claude-cli".into(),
        program: std::env::args()
            .nth(1)
            .unwrap_or_else(|| "claude".into())
            .into(),
        model: std::env::args()
            .nth(2)
            .unwrap_or_else(|| "claude-haiku-5-5".into()),
        version: "smoke".into(),
        isolated_login: None,
        token_file: std::env::args().nth(3).map(Into::into),
    };
    let start = Instant::now();
    let got = run(&cli, &task, Instant::now() + Duration::from_secs(120));
    println!("{:?} in {:?}", got, start.elapsed());
}
