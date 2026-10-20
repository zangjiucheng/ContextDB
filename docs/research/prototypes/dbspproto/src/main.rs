// Scratch: ContextDB v0 AND/OR support (positive prefix-chain Datalog encoding) in DBSP.
use std::collections::{HashMap, HashSet};
use dbsp::{OrdZSet, Runtime, Stream, RootCircuit, ZSetHandle, OutputHandle, utils::{Tup2, Tup3, Tup4}};
use dbsp::typed_batch::IndexedZSetReader;
type Rev = u32; type RuleId = u32; type Lid = u32;
#[derive(Clone)]
struct World {
    base: HashSet<Rev>,
    rules: HashMap<RuleId, (Rev, Vec<(Rev, bool)>)>, // conclusion, premises
    enabled: HashSet<RuleId>,
    head: HashMap<Lid, Rev>,
    logical: HashMap<Rev, Lid>,
}

fn oracle(w: &World, n: u32) -> HashSet<Rev> {
    // ids are topologically ordered: premises < conclusion
    let mut by_concl: HashMap<Rev, Vec<RuleId>> = HashMap::new();
    for (j, (c, _)) in &w.rules { by_concl.entry(*c).or_default().push(*j); }
    let mut s = HashSet::new();
    for r in 0..n {
        let mut ok = w.base.contains(&r);
        if !ok {
            if let Some(js) = by_concl.get(&r) {
                for j in js {
                    if !w.enabled.contains(j) { continue; }
                    let (_, ps) = &w.rules[j];
                    if ps.iter().all(|(p, rc)| s.contains(p) && (!*rc || w.head.get(&w.logical[p]) == Some(p))) { ok = true; break; }
                }
            }
        }
        if ok { s.insert(r); }
    }
    s
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, k: u64) -> u64 { self.next() % k }
}


struct H { b: ZSetHandle<u32>, p: ZSetHandle<Tup4<u32,u32,u32,bool>>, r: ZSetHandle<Tup3<u32,u32,u32>>, e: ZSetHandle<u32>,
           h: ZSetHandle<Tup2<u32,u32>>, l: ZSetHandle<Tup2<u32,u32>>, out: OutputHandle<OrdZSet<u32>> }

fn circuit(c: &mut RootCircuit) -> anyhow::Result<H> {
    let (base, b) = c.add_input_zset::<u32>();
    let (premise, p) = c.add_input_zset::<Tup4<u32,u32,u32,bool>>();
    let (rule, r) = c.add_input_zset::<Tup3<u32,u32,u32>>();
    let (enabled, e) = c.add_input_zset::<u32>();
    let (head, h) = c.add_input_zset::<Tup2<u32,u32>>();
    let (logical, l) = c.add_input_zset::<Tup2<u32,u32>>();
    // currency stratum
    let cur = head.map_index(|Tup2(l, r)| (Tup2(*r, *l), ())).join(&logical.map_index(|Tup2(r, l)| (Tup2(*r, *l), ())), |k, _, _| k.0);
    let plain = premise.filter(|t| !t.3).map(|Tup4(j, i, p, _)| Tup3(*j, *i, *p));
    let req = premise.filter(|t| t.3).map_index(|Tup4(j, i, p, _)| (*p, Tup2(*j, *i)))
        .join(&cur.map_index(|r| (*r, ())), |p, Tup2(j, i), _| Tup3(*j, *i, *p));
    let usable = plain.plus(&req);
    let rule_en = rule.map_index(|Tup3(j, c, n)| (*j, Tup2(*c, *n))).join(&enabled.map_index(|j| (*j, ())), |j, Tup2(c, n), _| Tup3(*j, *c, *n));
    let ok0 = rule_en.map(|Tup3(j, _, _)| Tup2(*j, 0u32));
    let (s, _ok) = c.recursive(|child, (s, ok): (Stream<_, OrdZSet<u32>>, Stream<_, OrdZSet<Tup2<u32,u32>>>)| {
        let base = base.delta0(child);
        let ok0 = ok0.delta0(child);
        let usable = usable.delta0(child);
        let rule_en = rule_en.delta0(child);
        let step = usable.map_index(|Tup3(j, i, p)| (*p, Tup2(*j, *i)))
            .join(&s.map_index(|r| (*r, ())), |_, ji, _| *ji)
            .map_index(|ji| (*ji, ()))
            .join(&ok.map_index(|ji| (*ji, ())), |Tup2(j, i), _, _| Tup2(*j, *i + 1));
        let concl = ok.map_index(|ji| (*ji, ()))
            .join(&rule_en.map_index(|Tup3(j, c, n)| (Tup2(*j, *n), *c)), |_, _, c| *c);
        Ok((base.plus(&concl), ok0.plus(&step)))
    })?;
    Ok(H { b, p, r, e, h, l, out: s.output() })
}

fn absorb(acc: &mut HashMap<u32, i64>, h: &H) { for (k, _, w) in h.out.consolidate().iter() { *acc.entry(k).or_insert(0) += w; } }

fn check(seed: u64, n: u32, rules_per: u32, epochs: u32, ops: u32) -> (usize, usize) {
    let (mut dbsp, hs) = Runtime::init_circuit(1, circuit).unwrap();
    let mut acc = HashMap::new();
    let (mut checked, mut mism) = (0, 0);
    let mut rng = Rng(seed * 2654435761 + 1);
    let k = 3u32;
    let mut w = World { base: HashSet::new(), rules: HashMap::new(), enabled: HashSet::new(), head: HashMap::new(), logical: HashMap::new() };
    for r in 0..n { w.logical.insert(r, r / k); hs.l.push(Tup2(r, r / k), 1); }
    for l in 0..n.div_ceil(k) { let r = l * k; w.head.insert(l, r); hs.h.push(Tup2(l, r), 1); }
    let mut next_rule: RuleId = 0;
    for r in 0..n {
        if rng.below(3) == 0 { w.base.insert(r); hs.b.push(r, 1); }
        if r == 0 { continue; }
        for _ in 0..rng.below(rules_per as u64 + 1) {
            let ar = rng.below(3) as u32 + if rng.below(5) == 0 { 0 } else { 1 };
            let mut ps: Vec<(Rev, bool)> = Vec::new();
            for _ in 0..ar { let p = rng.below(r as u64) as Rev; if !ps.iter().any(|x| x.0 == p) { ps.push((p, rng.below(2) == 0)); } }
            let j = next_rule; next_rule += 1;
            for (i, (p, rc)) in ps.iter().enumerate() { hs.p.push(Tup4(j, i as u32, *p, *rc), 1); }
            hs.r.push(Tup3(j, r, ps.len() as u32), 1);
            w.rules.insert(j, (r, ps)); w.enabled.insert(j); hs.e.push(j, 1);
        }
    }
    if std::env::var("DUMP").is_ok() { let mut rs: Vec<_> = w.rules.iter().collect(); rs.sort_by_key(|x| x.0); eprintln!("rules={:?} base={:?} heads={:?}", rs, w.base, w.head); }
    for _ in 0..epochs {
        if std::env::var("DUMP").is_ok() { eprintln!("commit base={:?} en_missing={:?} heads={:?}", { let mut b: Vec<_> = w.base.iter().collect(); b.sort(); b }, (0..next_rule).filter(|j| !w.enabled.contains(j)).collect::<Vec<_>>(), w.head); }
        dbsp.transaction().unwrap();
        absorb(&mut acc, &hs);
        assert!(acc.values().all(|c| *c == 0 || *c == 1));
        let got: HashSet<u32> = acc.iter().filter(|(_, c)| **c > 0).map(|(r, _)| *r).collect();
        checked += 1; let want = oracle(&w, n); if got != want { mism += 1; if std::env::var("DBG").is_ok() { let mut extra: Vec<_> = got.difference(&want).collect(); extra.sort(); let mut missing: Vec<_> = want.difference(&got).collect(); missing.sort(); eprintln!("seed={} epoch={} extra={:?} missing={:?} weights_nonset={}", seed, checked, extra, missing, acc.values().filter(|c| **c != 0 && **c != 1).count()); } }
        for _ in 0..ops {
            let mut op = rng.below(4);
            if let Ok(only) = std::env::var("ONLY") { let o: u64 = only.parse().unwrap(); if !(op == o || (o == 2 && op >= 2)) { op = 99; } }
            match op {
                99 => {}
                0 => { let r = rng.below(n as u64) as Rev; if w.base.remove(&r) { hs.b.push(r, -1) } else { w.base.insert(r); hs.b.push(r, 1) } }
                1 => { if next_rule > 0 { let j = rng.below(next_rule as u64) as RuleId; if w.enabled.remove(&j) { hs.e.push(j, -1) } else { w.enabled.insert(j); hs.e.push(j, 1) } } }
                _ => { if std::env::var("NOHEAD").is_ok() { continue; } let l = rng.below(n.div_ceil(k) as u64) as Lid; let old = w.head[&l]; let cand = (l * k + rng.below(k as u64) as u32).min(n - 1);
                       if cand != old { hs.h.push(Tup2(l, old), -1); hs.h.push(Tup2(l, cand), 1); w.head.insert(l, cand); } }
            }
        }
    }
    dbsp.kill().ok();
    (checked, mism)
}

fn bench(n: u32, tree: bool) {
    let (mut dbsp, hs) = Runtime::init_circuit(1, circuit).unwrap();
    let t = std::time::Instant::now();
    hs.b.push(0, 1);
    for r in 1..n { let p = if tree { (r - 1) / 8 } else { r - 1 }; hs.p.push(Tup4(r, 0, p, false), 1); hs.r.push(Tup3(r, r, 1), 1); hs.e.push(r, 1); }
    dbsp.transaction().unwrap(); let _ = hs.out.consolidate();
    let load = t.elapsed();
    let mut root = Vec::new();
    for round in 0..6 { hs.b.push(0, if round % 2 == 0 { -1 } else { 1 }); let t0 = std::time::Instant::now(); dbsp.transaction().unwrap(); let d = hs.out.consolidate(); root.push((t0.elapsed(), d.iter().count())); }
    let mut leaf = Vec::new();
    for round in 0..6 { hs.e.push(n - 1, if round % 2 == 0 { -1 } else { 1 }); let t0 = std::time::Instant::now(); dbsp.transaction().unwrap(); let _ = hs.out.consolidate(); leaf.push(t0.elapsed()); }
    println!("dbsp shape={} n={} load={:?} root_flip={:?} leaf_flip={:?}", if tree {"tree8"} else {"chain"}, n, load, root, leaf);
    dbsp.kill().ok();
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "replay" {
        // failing trace from seed=2,N=8 (heads static: 0->0, 1->3, 2->6)
        let rules: Vec<(u32, u32, Vec<(u32, bool)>)> = vec![(0,1,vec![(0,false)]),(1,1,vec![(0,true)]),(2,2,vec![(0,false),(1,false)]),(3,4,vec![(1,false),(0,true)]),
            (4,4,vec![(2,true),(3,false),(0,true)]),(5,5,vec![(1,false)]),(6,6,vec![(5,false),(4,false)]),(7,7,vec![(2,false)]),(8,7,vec![(1,true),(3,false)])];
        let drop_rc = std::env::var("NORC").is_ok();
        let keep: Vec<u32> = std::env::var("KEEP").map(|k| k.split(',').map(|x| x.parse().unwrap()).collect()).unwrap_or((0..9).collect());
        let seq: Vec<Vec<u32>> = std::env::var("SEQ").unwrap().split(';').map(|b| if b.is_empty() { vec![] } else { b.split(',').map(|x| x.parse().unwrap()).collect() }).collect();
        let (mut dbsp, hs) = Runtime::init_circuit(1, circuit).unwrap();
        for r in 0..8u32 { hs.l.push(Tup2(r, r / 3), 1); }
        for (l, r) in [(0u32, 0u32), (1, 3), (2, 6)] { hs.h.push(Tup2(l, r), 1); }
        let mut w = World { base: HashSet::new(), rules: HashMap::new(), enabled: HashSet::new(), head: [(0,0),(1,3),(2,6)].into_iter().collect(), logical: (0..8).map(|r| (r, r/3)).collect() };
        for (j, c, ps) in rules { if !keep.contains(&j) { continue; } let ps: Vec<(u32,bool)> = ps.into_iter().map(|(p, rc)| (p, rc && !drop_rc)).collect();
            for (i, (p, rc)) in ps.iter().enumerate() { hs.p.push(Tup4(j, i as u32, *p, *rc), 1); }
            hs.r.push(Tup3(j, c, ps.len() as u32), 1); hs.e.push(j, 1); w.enabled.insert(j); w.rules.insert(j, (c, ps)); }
        let mut acc = HashMap::new();
        for b in seq {
            for r in w.base.clone() { if !b.contains(&r) { hs.b.push(r, -1); w.base.remove(&r); } }
            for r in &b { if w.base.insert(*r) { hs.b.push(*r, 1); } }
            dbsp.transaction().unwrap(); absorb(&mut acc, &hs);
            let mut got: Vec<u32> = acc.iter().filter(|(_, c)| **c > 0).map(|(r, _)| *r).collect(); got.sort();
            let mut want: Vec<u32> = oracle(&w, 8).into_iter().collect(); want.sort();
            println!("base={:?} got={:?} want={:?} {}", b, got, want, if got == want { "ok" } else { "MISMATCH" });
        }
        return;
    }
    if mode == "check" {
        let (mut c, mut m) = (0, 0);
        let n: u32 = std::env::var("N").ok().map(|x| x.parse().unwrap()).unwrap_or(40);
        let first: u64 = std::env::var("FIRST").ok().map(|x| x.parse().unwrap()).unwrap_or(0);
        let seeds: u64 = std::env::var("SEEDS").ok().map(|x| x.parse().unwrap()).unwrap_or(200);
        for seed in first..seeds { let (cc, mm) = check(seed, n, 2, 12, 4); c += cc; m += mm; if mm > 0 && std::env::var("DBG").is_ok() { eprintln!("first failing seed {}", seed); break; } }
        println!("dbsp epochs_checked={} mismatches={}", c, m);
    } else {
        bench(std::env::args().nth(2).unwrap().parse().unwrap(), std::env::args().nth(3).map(|s| s == "tree").unwrap_or(false));
    }
}
