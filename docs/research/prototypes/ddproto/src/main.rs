// Scratch prototype: ContextDB v0 AND/OR support semantics encoded in differential dataflow.
// Two encodings:
//   A = pure positive Datalog (premise-prefix chain), least fixed point
//   B = counting (sat premises == arity) inside iterate
// Checked against a full-recompute oracle over random DAG transaction sequences,
// then timed on chain / fanout shapes.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use differential_dataflow::input::Input;
use differential_dataflow::operators::iterate::Iterate;
use differential_dataflow::VecCollection;
use timely::dataflow::operators::probe::Handle;

type Rev = u32;
type RuleId = u32;
type Lid = u32;

// premise row: (rule, idx, premise_rev, requires_current)
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

#[derive(Clone, Copy, PartialEq)]
enum Enc { A, B }

// Fact encoded as (tag, a, b): tag 0 = S(a), tag 1 = Ok(rule a, prefix b)
#[allow(non_snake_case)] fn S(r: Rev) -> (u8, u32, u32) { (0, r, 0) }
#[allow(non_snake_case)] fn OK(j: RuleId, i: u32) -> (u8, u32, u32) { (1, j, i) }

fn build<'s>(
    enc: Enc,
    base: VecCollection<'s, u64, Rev>,
    premise: VecCollection<'s, u64, (RuleId, u32, Rev, bool)>,
    rule: VecCollection<'s, u64, (RuleId, Rev, u32)>, // (j, conclusion, arity)
    enabled: VecCollection<'s, u64, RuleId>,
    head: VecCollection<'s, u64, (Lid, Rev)>,
    logical: VecCollection<'s, u64, (Rev, Lid)>,
) -> VecCollection<'s, u64, Rev> {
    // currency filter (non-recursive stratum): premises usable under current heads
    let cur_heads = head.map(|(l, r)| (r, l)).join(logical).filter(|(_, (l1, l2))| l1 == l2).map(|(r, _)| r);
    let plain = premise.clone().filter(|x| !x.3).map(|(j, i, p, _)| (j, i, p));
    let req = premise.filter(|x| x.3).map(|(j, i, p, _)| (p, (j, i))).semijoin(cur_heads).map(|(p, (j, i))| (j, i, p));
    let usable = plain.concat(req);
    let rule_en = rule.map(|(j, c, n)| (j, (c, n))).semijoin(enabled); // (j,(c,n))

    match enc {
        Enc::A => {
            // ok(j,0) :- rule_en(j). ok(j,i+1) :- ok(j,i), usable(j,i,p), S(p).
            // S(c) :- base(c).  S(c) :- ok(j,n), rule_en(j,(c,n)).
            let seeds = base.map(S).concat(rule_en.clone().map(|(j, _)| OK(j, 0)));
            let out = seeds.clone().iterate(|scope, inner| {
                let seeds = seeds.enter(scope);
                let usable = usable.enter(scope).map(|(j, i, p)| (p, (j, i)));
                let rule_en = rule_en.enter(scope).map(|(j, (c, n))| ((j, n), c));
                let s = inner.clone().flat_map(|f| if f.0 == 0 { Some(f.1) } else { None });
                let ok = inner.flat_map(|f| if f.0 == 1 { Some(((f.1, f.2), ())) } else { None });
                let step = usable.semijoin(s).map(|(_, (j, i))| ((j, i), ())).join(ok.clone()).map(|((j, i), _)| OK(j, i + 1));
                let concl = ok.join(rule_en).map(|(_, (_, c))| S(c));
                seeds.concat(step).concat(concl).distinct().map(|x| x)
            });
            out.flat_map(|f| if f.0 == 0 { Some(f.1) } else { None })
        }
        Enc::B => {
            // active(j) :- count{ i : usable(j,i,p), S(p) } == arity(j)
            base.clone().iterate(|scope, s| {
                let base = base.enter(scope);
                let usable = usable.enter(scope).map(|(j, _i, p)| (p, j));
                let rule_en = rule_en.enter(scope);
                let zero = rule_en.clone().filter(|(_, (_, n))| *n == 0).map(|(_, (c, _))| c);
                let sat = usable.semijoin(s).map(|(_, j)| j).count(); // (j, k)
                let act = sat.join(rule_en).filter(|(_, (k, (_, n)))| *k == *n as isize).map(|(_, (_, (c, _)))| c);
                base.concat(zero).concat(act).distinct()
            })
        }
    }
}

fn run(enc: Enc, seed: u64, n: u32, rules_per: u32, epochs: u32, ops: u32) -> (usize, usize) {
    timely::execute_directly(move |worker| {
        let mut mismatches = 0usize;
        let mut checked = 0usize;
        let result = Rc::new(RefCell::new(HashMap::<Rev, isize>::new()));
        let mut probe = Handle::new();
        let r3 = result.clone();
        let (mut ib, mut ip, mut ir, mut ie, mut ih, mut il) = worker.dataflow::<u64, _, _>(|scope| {
            let (ib, b) = scope.clone().new_collection();
            let (ip, p) = scope.clone().new_collection();
            let (ir, r) = scope.clone().new_collection();
            let (ie, e) = scope.clone().new_collection();
            let (ih, h) = scope.clone().new_collection();
            let (il, l) = scope.clone().new_collection();
            build(enc, b, p, r, e, h, l)
                .inspect(move |(x, _t, d)| { *r3.borrow_mut().entry(*x).or_insert(0) += *d; })
                .probe_with(&mut probe);
            (ib, ip, ir, ie, ih, il)
        });
        let mut rng = Rng(seed * 2654435761 + 1);
        let k = 3u32; // revisions per logical id
        let mut w = World { base: HashSet::new(), rules: HashMap::new(), enabled: HashSet::new(), head: HashMap::new(), logical: HashMap::new() };
        for r in 0..n { w.logical.insert(r, r / k); il.insert((r, r / k)); }
        for l in 0..n.div_ceil(k) { let r = l * k; w.head.insert(l, r); ih.insert((l, r)); }
        let mut next_rule: RuleId = 0;
        for r in 0..n {
            if rng.below(3) == 0 { w.base.insert(r); ib.insert(r); }
            if r == 0 { continue; }
            for _ in 0..rng.below(rules_per as u64 + 1) {
                let ar = rng.below(3) as u32 + if rng.below(5) == 0 { 0 } else { 1 };
                let mut ps: Vec<(Rev, bool)> = Vec::new();
                for _ in 0..ar { let p = rng.below(r as u64) as Rev; if !ps.iter().any(|x| x.0 == p) { ps.push((p, rng.below(2) == 0)); } }
                let j = next_rule; next_rule += 1;
                for (i, (p, rc)) in ps.iter().enumerate() { ip.insert((j, i as u32, *p, *rc)); }
                ir.insert((j, r, ps.len() as u32));
                w.rules.insert(j, (r, ps));
                w.enabled.insert(j); ie.insert(j);
            }
        }
        for epoch in 1..=epochs as u64 {
            ib.advance_to(epoch); ip.advance_to(epoch); ir.advance_to(epoch); ie.advance_to(epoch); ih.advance_to(epoch); il.advance_to(epoch);
            ib.flush(); ip.flush(); ir.flush(); ie.flush(); ih.flush(); il.flush();
            worker.step_while(|| probe.less_than(&epoch));
            // compare state as of epoch-1
            let got: HashSet<Rev> = result.borrow().iter().filter(|(_, c)| **c > 0).map(|(r, _)| *r).collect();
            assert!(result.borrow().values().all(|c| *c == 0 || *c == 1), "non-set output");
            let want = oracle(&w, n);
            checked += 1;
            if got != want { mismatches += 1; }
            // batch of random ops (may flip same item twice within one batch)
            for _ in 0..ops {
                match rng.below(4) {
                    0 => { let r = rng.below(n as u64) as Rev; if w.base.remove(&r) { ib.remove(r) } else { w.base.insert(r); ib.insert(r) } }
                    1 => { if next_rule > 0 { let j = rng.below(next_rule as u64) as RuleId; if w.enabled.remove(&j) { ie.remove(j) } else { w.enabled.insert(j); ie.insert(j) } } }
                    _ => { let l = rng.below(n.div_ceil(k) as u64) as Lid; let old = w.head[&l]; let cand = (l * k + rng.below(k as u64) as u32).min(n - 1);
                           if cand != old { ih.remove((l, old)); ih.insert((l, cand)); w.head.insert(l, cand); } }
                }
            }
        }
        (checked, mismatches)
    })
}

fn chain_bench(enc: Enc, depth: u32, fanout_tree: bool) {
    let timer = std::time::Instant::now();
    timely::execute_directly(move |worker| {
        let mut probe = Handle::new();
        let (mut ib, mut ip, mut ir, mut ie, mut ih, mut il) = worker.dataflow::<u64, _, _>(|scope| {
            let (ib, b) = scope.clone().new_collection();
            let (ip, p) = scope.clone().new_collection();
            let (ir, r) = scope.clone().new_collection();
            let (ie, e) = scope.clone().new_collection();
            let (ih, h) = scope.clone().new_collection::<(Lid, Rev), isize>();
            let (il, l) = scope.clone().new_collection::<(Rev, Lid), isize>();
            build(enc, b, p, r, e, h, l).probe_with(&mut probe);
            (ib, ip, ir, ie, ih, il)
        });
        // chain: r -> r+1 ; tree: parent (r-1)/8 -> r
        ib.insert(0);
        for r in 1..depth { let p = if fanout_tree { (r - 1) / 8 } else { r - 1 }; ip.insert((r, 0, p, false)); ir.insert((r, r, 1)); ie.insert(r); }
        let mut t = 1u64;
        for h in [&mut ih] { h.advance_to(t); h.flush(); }
        il.advance_to(t); il.flush();
        ib.advance_to(t); ip.advance_to(t); ir.advance_to(t); ie.advance_to(t); ib.flush(); ip.flush(); ir.flush(); ie.flush();
        worker.step_while(|| probe.less_than(&t));
        let load = timer.elapsed();
        let mut lat = Vec::new();
        for round in 0..6 {
            if round % 2 == 0 { ib.remove(0) } else { ib.insert(0) }
            t += 1;
            ib.advance_to(t); ip.advance_to(t); ir.advance_to(t); ie.advance_to(t); ih.advance_to(t); il.advance_to(t);
            ib.flush(); ip.flush(); ir.flush(); ie.flush(); ih.flush(); il.flush();
            let t0 = std::time::Instant::now();
            worker.step_while(|| probe.less_than(&t));
            lat.push(t0.elapsed());
        }
        // leaf-local update: toggle a rule near the bottom (affects ~1 node)
        let mut leaf = Vec::new();
        for round in 0..6 {
            let j = depth - 1;
            if round % 2 == 0 { ie.remove(j) } else { ie.insert(j) }
            t += 1;
            ib.advance_to(t); ip.advance_to(t); ir.advance_to(t); ie.advance_to(t); ih.advance_to(t); il.advance_to(t);
            ib.flush(); ip.flush(); ir.flush(); ie.flush(); ih.flush(); il.flush();
            let t0 = std::time::Instant::now();
            worker.step_while(|| probe.less_than(&t));
            leaf.push(t0.elapsed());
        }
        println!("enc={} shape={} n={} load={:?} root_flip={:?} leaf_flip={:?}",
            if enc == Enc::A { "A-positive" } else { "B-count" }, if fanout_tree { "tree8" } else { "chain" }, depth, load, lat, leaf);
    });
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "native" {
        // minimal event-driven counter propagation (spec 4.2), single-premise rules, for scale reference only
        let n: u32 = std::env::args().nth(2).unwrap().parse().unwrap();
        let tree = std::env::args().nth(3).map(|s| s == "tree").unwrap_or(false);
        let mut consumers: Vec<Vec<u32>> = vec![Vec::new(); n as usize]; // premise -> rules (rule j concl j)
        for r in 1..n { let p = if tree { (r - 1) / 8 } else { r - 1 }; consumers[p as usize].push(r); }
        let mut unsat = vec![0u32; n as usize]; let mut cnt = vec![0u32; n as usize]; let mut base = vec![false; n as usize];
        base[0] = true; for r in 1..n as usize { cnt[r] = 1; } // all supported initially
        let mut lat = Vec::new();
        for round in 0..6 {
            let t0 = std::time::Instant::now();
            let was = base[0] || cnt[0] > 0; base[0] = round % 2 == 1; let now = base[0] || cnt[0] > 0;
            let mut stack = Vec::new(); if was != now { stack.push((0u32, now)); }
            let mut visited = 0u64;
            while let Some((p, up)) = stack.pop() {
                for &j in &consumers[p as usize] { visited += 1;
                    let wa = unsat[j as usize] == 0; if up { unsat[j as usize] -= 1 } else { unsat[j as usize] += 1 }
                    let na = unsat[j as usize] == 0;
                    if wa != na { let c = j as usize; let w = base[c] || cnt[c] > 0; if na { cnt[c] += 1 } else { cnt[c] -= 1 } let nw = base[c] || cnt[c] > 0; if w != nw { stack.push((j, nw)); } }
                }
            }
            lat.push((t0.elapsed(), visited));
        }
        println!("native shape={} n={} root_flip={:?}", if tree {"tree8"} else {"chain"}, n, lat);
        return;
    }
    if mode == "check" {
        for enc in [Enc::A, Enc::B] {
            let (mut c, mut m) = (0, 0);
            for seed in 0..200u64 {
                let (cc, mm) = run(enc, seed, 40, 2, 12, 4);
                c += cc; m += mm;
            }
            println!("enc={} epochs_checked={} mismatches={}", if enc == Enc::A { "A" } else { "B" }, c, m);
        }
    } else {
        let n: u32 = std::env::args().nth(2).unwrap().parse().unwrap();
        let tree = std::env::args().nth(3).map(|s| s == "tree").unwrap_or(false);
        let enc = if mode == "A" { Enc::A } else { Enc::B };
        chain_bench(enc, n, tree);
    }
}
