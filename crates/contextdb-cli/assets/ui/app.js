// ctx ui front end. Records: every head grouped by key path, with its
// status and abstract. Graph: the justification graph, whole or around the
// selection, as a network (force layout, clustered by key directory) or in
// layers (premises above their conclusions). Timeline: the commits.
// Refreshed when the op_log changes. Read-only: every request is a GET.
"use strict";

const SVGNS = "http://www.w3.org/2000/svg";
const NODE_W = 236, NODE_H = 60, GAP_X = 32, GAP_Y = 74;
// Network layout: compact nodes, laid out by forces.
const NET_W = 172, NET_H = 28;
const CLASSES = ["certified", "stale", "pending", "conflicted", "unavailable"];
const KINDS = ["pin", "cur", "sem", "ref"];
const TEAM_COPIES = ["merged", "shown", "hidden"];
/// A per-viewer preference; storage may be absent (private window).
function stored(key, fallback) {
  try { return localStorage.getItem(`ctxui.${key}`) ?? fallback; } catch { return fallback; }
}
function store(key, value) {
  try { localStorage.setItem(`ctxui.${key}`, value); } catch { /* not kept */ }
}

const state = {
  graph: null, timeline: null, fingerprint: null, byRev: new Map(), byLogical: new Map(),
  selected: null, allRevs: false, search: "", hidden: new Set(),
  view: { x: 40, y: 40, k: 1 }, highlight: null, tab: "records", laidOut: false,
  branch: 0, focus: 0, collapsed: new Set(), changedSummaries: false, level: "abstract",
  layout: "network",
  // Team copies (P10 mirrors): "merged" | "shown" | "hidden" (see teamView).
  // `raw`: the graph as served; `folded`: mirror revision -> local twin.
  teamCopies: stored("teamCopies", "merged"), raw: null, folded: new Map(),
  // Network layout: the live simulation, kept across refreshes.
  // `sig`: the structure the network scene was built for (see structure()).
  sim: { pos: new Map(), alpha: 0, raf: 0, drag: null, tick: null, sig: null },
};
const $ = (id) => document.getElementById(id);
const el = (tag, attrs = {}, parent) => {
  const e = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  if (parent) parent.appendChild(e);
  return e;
};
const html = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
};

// The launch token (multi-user mode, ADR 0017 §5.8): taken once from the
// URL fragment, kept in sessionStorage, sent as a header, never a cookie.
const TOKEN = (() => {
  const m = location.hash.match(/token=([0-9a-f]+)/);
  if (m) {
    sessionStorage.setItem("ctx-ui-token", m[1]);
    history.replaceState(null, "", location.pathname + location.search);
  }
  return sessionStorage.getItem("ctx-ui-token");
})();
async function getJSON(url) {
  const headers = TOKEN ? { Authorization: "Bearer " + TOKEN } : {};
  const r = await fetch(url, { cache: "no-store", headers });
  const v = await r.json();
  if (!r.ok) throw new Error(v.error || r.statusText);
  return v;
}
function showError(e) {
  const box = $("error");
  if (!e) { box.hidden = true; return; }
  box.textContent = String(e.message || e);
  box.hidden = false;
}

// ---- model -------------------------------------------------------------

/// Nodes and edges for the current mode. Heads mode: one node per logical
/// (its head); an edge to a premise that is no longer its logical's head is
/// marked with the old revision. All-revisions mode: one node per revision,
/// plus dotted supersede chains.
function build(g) {
  const revs = new Map(g.revisions.map((r) => [r.id, r]));
  const logicals = new Map(g.logicals.map((l) => [l.id, l]));
  const nodes = new Map(), edges = [];
  const nodeOf = (rev) => (state.allRevs ? `r${rev}` : `l${revs.get(rev)?.logical}`);
  for (const r of g.revisions) {
    if (!state.allRevs && !r.head) continue;
    const l = logicals.get(r.logical);
    nodes.set(nodeOf(r.id), { id: nodeOf(r.id), rev: r, logical: l });
  }
  for (const rule of g.rules) {
    const to = nodeOf(rule.conclusion);
    if (!nodes.has(to)) continue;
    const incidences = [...rule.premises, ...rule.references.map((rev) => ({ rev, kind: "ref" }))];
    for (const p of incidences) {
      const from = nodeOf(p.rev);
      if (!nodes.has(from) || from === to) continue;
      const premise = revs.get(p.rev);
      edges.push({
        from, to, kind: p.kind, rule: rule.id, disabled: !rule.enabled,
        old: !state.allRevs && premise && !premise.head ? p.rev : null,
      });
    }
  }
  if (state.allRevs) {
    const byLogical = new Map();
    for (const r of g.revisions) {
      if (!byLogical.has(r.logical)) byLogical.set(r.logical, []);
      byLogical.get(r.logical).push(r.id);
    }
    for (const ids of byLogical.values()) {
      ids.sort((a, b) => a - b);
      for (let i = 1; i < ids.length; i++) edges.push({ from: `r${ids[i - 1]}`, to: `r${ids[i]}`, kind: "chain" });
    }
  }
  return focusOn({ nodes, edges }, nodeOf);
}
/// With a focus of N hops and a selection: only the nodes within N edges of
/// the selected one, either direction, and the edges between them.
function focusOn(model, nodeOf) {
  if (!state.focus || state.selected === null) return model;
  const start = nodeOf(state.selected);
  if (!model.nodes.has(start)) return model;
  const near = new Map([...model.nodes.keys()].map((id) => [id, []]));
  for (const e of model.edges) { near.get(e.from)?.push(e.to); near.get(e.to)?.push(e.from); }
  const keep = new Set([start]);
  let frontier = [start];
  for (let hop = 0; hop < state.focus; hop++) {
    const next = [];
    for (const id of frontier) for (const n of near.get(id) || []) if (!keep.has(n)) { keep.add(n); next.push(n); }
    frontier = next;
  }
  const nodes = new Map([...model.nodes].filter(([id]) => keep.has(id)));
  return { nodes, edges: model.edges.filter((e) => keep.has(e.from) && keep.has(e.to)) };
}

/// Longest-path layering (back edges ignored), then barycenter sweeps to
/// reduce crossings.
function layout(model) {
  const ids = [...model.nodes.keys()];
  const preds = new Map(ids.map((i) => [i, []]));
  const succs = new Map(ids.map((i) => [i, []]));
  for (const e of model.edges) { preds.get(e.to).push(e.from); succs.get(e.from).push(e.to); }
  const layer = new Map(), visiting = new Set();
  const depth = (id) => {
    if (layer.has(id)) return layer.get(id);
    if (visiting.has(id)) return 0; // a cycle through old revisions: cut it
    visiting.add(id);
    let d = 0;
    for (const p of preds.get(id)) d = Math.max(d, depth(p) + 1);
    visiting.delete(id);
    layer.set(id, d);
    return d;
  };
  ids.forEach(depth);
  const layers = [];
  for (const id of ids) (layers[layer.get(id)] ||= []).push(id);
  for (const L of layers) L.sort((a, b) => keyOf(model, a).localeCompare(keyOf(model, b)));
  const pos = new Map();
  const index = () => layers.forEach((L) => L.forEach((id, i) => pos.set(id, i)));
  index();
  const bary = (id, nb) => {
    const xs = nb.get(id).map((n) => pos.get(n)).filter((v) => v !== undefined);
    return xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : pos.get(id);
  };
  for (let sweep = 0; sweep < 6; sweep++) {
    const down = sweep % 2 === 0;
    const order = down ? layers.keys() : [...layers.keys()].reverse();
    for (const li of order) {
      const L = layers[li];
      if (!L) continue;
      const nb = down ? preds : succs;
      L.sort((a, b) => bary(a, nb) - bary(b, nb));
      L.forEach((id, i) => pos.set(id, i));
    }
  }
  const width = Math.max(1, ...layers.map((L) => (L ? L.length : 0)));
  const xy = new Map();
  layers.forEach((L, li) => {
    if (!L) return;
    const offset = ((width - L.length) * (NODE_W + GAP_X)) / 2;
    L.forEach((id, i) => xy.set(id, { x: offset + i * (NODE_W + GAP_X), y: li * (NODE_H + GAP_Y) }));
  });
  return xy;
}
/// Repulsion between points closer than √cutoff2: point i gets
/// strength·(pᵢ − q)/(|pᵢ − q|² + eps) from every other point q, added to
/// (fx, fy). Barnes–Hut on a quadtree: a cell far enough away (size < θ ×
/// distance) and not containing i acts as one point at its centre of mass,
/// so a pass costs O(n log n) instead of O(n²). Deterministic.
function repel(x, y, fx, fy, strength, cutoff2, eps) {
  const n = x.length;
  if (n < 2) return;
  const LEAF = 8, THETA2 = 0.81;
  let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
  for (let i = 0; i < n; i++) {
    if (x[i] < x0) x0 = x[i]; if (x[i] > x1) x1 = x[i];
    if (y[i] < y0) y0 = y[i]; if (y[i] > y1) y1 = y[i];
  }
  const cell = (cx, cy, s) => ({ x: cx, y: cy, s, m: 0, sx: 0, sy: 0, kids: null, pts: [] });
  const root = cell(x0, y0, Math.max(x1 - x0, y1 - y0, 1) * 1.0001);
  const place = (c, i, depth) => {
    const h = c.s / 2, q = (x[i] >= c.x + h ? 1 : 0) + (y[i] >= c.y + h ? 2 : 0);
    const k = c.kids[q] ||= cell(c.x + (q & 1 ? h : 0), c.y + (q & 2 ? h : 0), h);
    insert(k, i, depth + 1);
  };
  const insert = (c, i, depth) => {
    c.m++; c.sx += x[i]; c.sy += y[i];
    if (c.kids) { place(c, i, depth); return; }
    c.pts.push(i);
    // Coincident points would split forever: a deep leaf just grows.
    if (c.pts.length <= LEAF || depth > 32) return;
    c.kids = [null, null, null, null];
    for (const j of c.pts) place(c, j, depth);
    c.pts = null;
  };
  for (let i = 0; i < n; i++) insert(root, i, 0);
  for (let i = 0; i < n; i++) {
    const px = x[i], py = y[i];
    let ax = 0, ay = 0;
    const stack = [root];
    while (stack.length) {
      const c = stack.pop();
      // The box's nearest point is out of reach: nothing in it repels.
      const nx = Math.max(c.x, Math.min(px, c.x + c.s)) - px, ny = Math.max(c.y, Math.min(py, c.y + c.s)) - py;
      if (nx * nx + ny * ny > cutoff2) continue;
      if (!c.kids) {
        for (const j of c.pts) {
          if (j === i) continue;
          const ex = px - x[j], ey = py - y[j], d2 = ex * ex + ey * ey + eps;
          if (d2 > cutoff2) continue;
          const f = strength / d2;
          ax += ex * f; ay += ey * f;
        }
        continue;
      }
      const inside = px >= c.x && px < c.x + c.s && py >= c.y && py < c.y + c.s;
      if (!inside) {
        const ex = px - c.sx / c.m, ey = py - c.sy / c.m, d2 = ex * ex + ey * ey + eps;
        if (c.s * c.s < THETA2 * d2) {
          if (d2 <= cutoff2) { const f = (strength * c.m) / d2; ax += ex * f; ay += ey * f; }
          continue;
        }
      }
      for (const k of c.kids) if (k) stack.push(k);
    }
    fx[i] += ax; fy[i] += ay;
  }
}
/// Force-directed layout (Fruchterman–Reingold) for graphs too deep to read
/// in layers: every node repels every other, each edge pulls its ends
/// together, and nodes of the same key directory are drawn to their
/// group's centre, so a long dependency chain coils up instead of making
/// one tall column. Deterministic (fixed start, fixed iterations), so the
/// 2 s refresh never moves anything that did not change. Computed with x
/// compressed by ASPECT, since nodes are wide, then overlaps are removed.
function layoutNetwork(model) {
  const ids = [...model.nodes.keys()].sort((a, b) => keyOf(model, a).localeCompare(keyOf(model, b)));
  const n = ids.length;
  const xy = new Map();
  if (!n) return xy;
  const ASPECT = 2.4, L = 74, CUTOFF2 = (4 * L) * (4 * L);
  const index = new Map(ids.map((id, i) => [id, i]));
  const groupOf = ids.map((id) => splitKey(label(model.nodes.get(id).logical))[0]);
  const groups = [...new Set(groupOf)];
  const gi = groupOf.map((g) => groups.indexOf(g));
  // Start: groups on a circle, members on a golden-angle spiral around it.
  const x = new Float64Array(n), y = new Float64Array(n);
  const ring = L * Math.sqrt(n) * 0.9;
  const seen = new Array(groups.length).fill(0);
  for (let i = 0; i < n; i++) {
    const a = (2 * Math.PI * gi[i]) / groups.length;
    const j = seen[gi[i]]++;
    const r = L * 0.6 * Math.sqrt(j + 1), t = j * 2.39996;
    x[i] = (groups.length > 1 ? ring * Math.cos(a) : 0) + r * Math.cos(t);
    y[i] = (groups.length > 1 ? ring * Math.sin(a) : 0) + r * Math.sin(t);
  }
  const pairs = new Set();
  for (const e of model.edges) {
    const u = index.get(e.from), v = index.get(e.to);
    if (u === undefined || v === undefined || u === v) continue;
    pairs.add(u < v ? `${u},${v}` : `${v},${u}`);
  }
  const springs = [...pairs].map((p) => p.split(",").map(Number));
  const iterations = n > 600 ? 120 : 300;
  const dx = new Float64Array(n), dy = new Float64Array(n);
  const cx = new Float64Array(groups.length), cy = new Float64Array(groups.length), cn = new Float64Array(groups.length);
  for (let it = 0; it < iterations; it++) {
    dx.fill(0); dy.fill(0);
    // Repulsion only nearby (grid variant): unconnected nodes would
    // otherwise drift far out and shrink everything else on fit.
    repel(x, y, dx, dy, L * L, CUTOFF2, 0.01);
    for (const [u, v] of springs) {
      const ex = x[u] - x[v], ey = y[u] - y[v];
      const f = Math.sqrt(ex * ex + ey * ey) / L;
      dx[u] -= ex * f; dy[u] -= ey * f; dx[v] += ex * f; dy[v] += ey * f;
    }
    cx.fill(0); cy.fill(0); cn.fill(0);
    for (let i = 0; i < n; i++) { cx[gi[i]] += x[i]; cy[gi[i]] += y[i]; cn[gi[i]]++; }
    for (let i = 0; i < n; i++) {
      const g = gi[i];
      dx[i] += (cx[g] / cn[g] - x[i]) * 0.5 - x[i] * 0.06;
      dy[i] += (cy[g] / cn[g] - y[i]) * 0.5 - y[i] * 0.06;
    }
    const temp = L * 2 * (1 - it / iterations) + 1;
    for (let i = 0; i < n; i++) {
      const d = Math.sqrt(dx[i] * dx[i] + dy[i] * dy[i]) || 1;
      const m = Math.min(d, temp);
      x[i] += (dx[i] / d) * m; y[i] += (dy[i] / d) * m;
    }
  }
  // Back to real proportions, then push overlapping boxes apart.
  for (let i = 0; i < n; i++) x[i] *= ASPECT;
  const padX = NET_W + 14, padY = NET_H + 12;
  const order = ids.map((_, i) => i);
  for (let pass = 0; pass < 40; pass++) {
    let moved = false;
    // Only boxes less than padX apart in x can overlap: a sweep in x order
    // compares each box with the few after it, not with all of them.
    order.sort((i, j) => x[i] - x[j] || i - j);
    for (let a = 0; a < n; a++) {
      for (let b = a + 1; b < n; b++) {
        const i = Math.min(order[a], order[b]), j = Math.max(order[a], order[b]);
        if (Math.abs(x[order[b]] - x[order[a]]) >= padX) break;
        const ox = padX - Math.abs(x[i] - x[j]), oy = padY - Math.abs(y[i] - y[j]);
        if (ox <= 0 || oy <= 0) continue;
        moved = true;
        if (ox / padX < oy / padY) {
          const s = (x[i] < x[j] || (x[i] === x[j] && i < j) ? -1 : 1) * (ox / 2 + 0.5);
          x[i] += s; x[j] -= s;
        } else {
          const s = (y[i] < y[j] || (y[i] === y[j] && i < j) ? -1 : 1) * (oy / 2 + 0.5);
          y[i] += s; y[j] -= s;
        }
      }
    }
    if (!moved) break;
  }
  const minX = Math.min(...x), minY = Math.min(...y);
  ids.forEach((id, i) => xy.set(id, { x: x[i] - minX, y: y[i] - minY + 24, group: groupOf[i] }));
  return xy;
}
/// Where a line from a box's centre toward (dx, dy) leaves the box.
const boxExit = (cx, cy, dx, dy, hw, hh) => {
  const t = Math.min(dx ? hw / Math.abs(dx) : Infinity, dy ? hh / Math.abs(dy) : Infinity);
  return [cx + dx * t, cy + dy * t];
};
const keyOf = (model, id) => model.nodes.get(id)?.logical?.key || "";
/// `a/b/c` → [`a/b/`, `c`]; a key without `/` has no path.
const splitKey = (k) => { const i = k.lastIndexOf("/"); return i < 0 ? ["", k] : [k.slice(0, i + 1), k.slice(i + 1)]; };
/// Cuts to `n` characters, keeping the end (`tail`) or the start.
const clip = (s, n, tail) => (s.length <= n ? s : tail ? "…" + s.slice(-(n - 1)) : s.slice(0, n - 1) + "…");
/// Cuts the middle: keys that share a long prefix differ at the end.
const clipMiddle = (s, n) => (s.length <= n ? s : s.slice(0, Math.ceil((n - 1) * 0.45)) + "…" + s.slice(-Math.floor((n - 1) * 0.55)));
/// A Summary's changed premises (ADR 0016 §6).
const premiseNote = (r, short) => {
  const p = r.premises;
  if (!p || !p.moved) return "";
  return short ? ` · ${p.moved}/${p.total} changed` : `summary; ${p.moved} of ${p.total} premises changed`;
};
/// A code anchor shows its path (and symbol), not `code:<repo id>/…`.
const label = (l) => l.anchor ?? l.key;
const kindLabel = (l) => (l.kind === "CodeAnchor" ? (l.anchor?.includes("#") ? "symbol anchor" : "file anchor") : l.kind);
/// Revisions and logicals of the current graph by id (see setGraph).
const revOf = (id) => state.byRev.get(id);
const logicalOf = (id) => state.byLogical.get(id);
function setGraph(graph) {
  state.raw = graph;
  if (!TEAM_COPIES.includes(state.teamCopies)) state.teamCopies = "merged";
  const g = teamView(graph);
  state.graph = g;
  state.byRev = new Map(g.revisions.map((r) => [r.id, r]));
  state.byLogical = new Map(g.logicals.map((l) => [l.id, l]));
}
/// The revision a link to `id` shows: itself, its local twin when its
/// team copy is merged, or null when team copies are hidden.
const shownRev = (id) => (state.byRev.has(id) ? id : state.folded.get(id) ?? null);
/// Team copies, as the viewer chose; the data is not changed.
/// - "shown": every mirror (`team/<id>/…`) as stored.
/// - "hidden": local records only; incidences on mirrors are dropped and
///   counted on their rule (`omitted`).
/// - "merged": a mirror whose head has a local twin (the server's `twin`:
///   the local record with the mirror's team path as key and the same
///   content, i.e. this store's own record published and imported back) is
///   folded into that record, and its edges go to it. A mirror whose head
///   differs from the local record, or that has none, stays.
function teamView(g) {
  state.folded = new Map();
  const mode = state.teamCopies;
  if (mode === "shown" || !g.logicals.some((l) => l.team)) return g;
  const revs = new Map(g.revisions.map((r) => [r.id, r]));
  const drop = new Set(), copyOf = new Map();
  for (const l of g.logicals) {
    if (!l.team) continue;
    if (mode === "hidden") { drop.add(l.id); continue; }
    const twin = revs.get(revs.get(l.head)?.twin);
    if (!twin || !twin.head) continue;
    drop.add(l.id);
    copyOf.set(twin.logical, l.key);
  }
  if (mode === "merged") {
    const heads = new Map(g.logicals.map((l) => [l.id, l.head]));
    for (const r of g.revisions) {
      if (!drop.has(r.logical)) continue;
      // An older mirror revision with no twin goes to the record itself.
      state.folded.set(r.id, r.twin ?? revs.get(heads.get(r.logical))?.twin);
    }
  }
  const gone = (id) => drop.has(revs.get(id)?.logical) && !state.folded.has(id);
  const to = (id) => state.folded.get(id) ?? id;
  const rules = [], seen = new Set();
  for (const rule of g.rules) {
    if (gone(rule.conclusion)) continue;
    const conclusion = to(rule.conclusion);
    const premises = rule.premises.filter((p) => !gone(p.rev)).map((p) => ({ ...p, rev: to(p.rev) }))
      .filter((p) => p.rev !== conclusion);
    const references = rule.references.filter((x) => !gone(x)).map(to).filter((x) => x !== conclusion);
    const omitted = rule.premises.filter((p) => gone(p.rev)).length + rule.references.filter(gone).length;
    if (rule.team && !premises.length && !references.length) continue;
    // A root rule that is the local rule over the same records, once.
    const sig = JSON.stringify([conclusion, rule.enabled, premises.map((p) => `${p.rev}${p.kind}`).sort(), [...references].sort()]);
    if (seen.has(sig)) continue;
    seen.add(sig);
    rules.push({ ...rule, conclusion, premises, references, omitted });
  }
  return {
    ...g,
    logicals: g.logicals.filter((l) => !drop.has(l.id)).map((l) => (copyOf.has(l.id) ? { ...l, teamCopy: copyOf.get(l.id) } : l)),
    revisions: g.revisions.filter((r) => !drop.has(r.logical)),
    rules,
    conflicts: (g.conflicts || []).filter((c) => !gone(c.target)).map((c) => ({ ...c, target: to(c.target) })),
    teamDropped: drop.size,
  };
}
const query = (extra) => `${extra}${extra.includes("?") ? "&" : "?"}branch=${state.branch}`;


// ---- rendering ---------------------------------------------------------

function render() {
  const g = state.graph;
  if (!g) return;
  const model = build(g);
  if (state.layout === "network") { renderNetwork(model); return; }
  // Layers: premises above their conclusions.
  stopSim();
  state.sim.sig = null;
  $("net").hidden = true;
  $("graph").hidden = false;
  const xy = layout(model);
  const vp = $("viewport");
  vp.replaceChildren();
  $("empty").hidden = model.nodes.size > 0;
  const q = state.search.trim().toLowerCase();
  const visible = (n) => !state.hidden.has(n.rev.class);
  const matches = (n) => !q || n.logical.key.toLowerCase().includes(q);
  const hl = state.highlight; // Set of node ids, or null
  const edgeLayer = el("g", {}, vp);
  for (const e of model.edges) {
    const a = xy.get(e.from), b = xy.get(e.to);
    if (!a || !b) continue;
    const na = model.nodes.get(e.from), nb = model.nodes.get(e.to);
    const dim = !visible(na) || !visible(nb) || (hl && !(hl.has(e.from) && hl.has(e.to))) || (q && !(matches(na) || matches(nb)));
    const cls = ["edge", e.kind, e.disabled ? "disabled" : "", dim ? "dim" : ""].join(" ");
    const grp = el("g", { class: cls }, edgeLayer);
    const x1 = a.x + NODE_W / 2, y1 = a.y + NODE_H, x2 = b.x + NODE_W / 2, y2 = b.y;
    const my = (y1 + y2) / 2;
    el("path", { d: `M${x1},${y1} C${x1},${my} ${x2},${my} ${x2},${y2 - 2}`, "marker-end": "url(#arrow)" }, grp);
    if (e.old !== null && e.old !== undefined) {
      const t = el("text", { x: (x1 + x2) / 2 + 4, y: my - 3 }, grp);
      t.textContent = `old r${e.old}`;
    }
    const title = el("title", {}, grp);
    title.textContent = `${e.kind}${e.disabled ? " (disabled)" : ""}${e.rule !== undefined ? ` · rule ${e.rule}` : ""}`;
  }
  const nodeLayer = el("g", {}, vp);
  for (const [id, n] of model.nodes) {
    const p = xy.get(id);
    const dim = !visible(n) || (hl && !hl.has(id)) || (q && !matches(n));
    const cls = ["node", n.rev.class, n.rev.base ? "base" : "", !n.rev.head ? "old" : "",
      state.selected === n.rev.id ? "selected" : "", q && matches(n) ? "hit" : "", dim ? "dim" : ""].join(" ");
    const grp = el("g", { class: cls, transform: `translate(${p.x},${p.y})` }, nodeLayer);
    el("rect", {
      width: NODE_W, height: NODE_H, rx: 7,
      fill: `var(--${n.rev.class}-bg)`, stroke: `var(--${n.rev.class})`,
    }, grp);
    // Path above, the last segment below: the part that tells records apart
    // is never the one cut.
    const [dir, leaf] = splitKey(label(n.logical));
    el("text", { x: 10, y: 16, class: "sub" }, grp).textContent = clip(dir, 38, true);
    el("text", { x: 10, y: 33 }, grp).textContent = clip(leaf, 34, false);
    el("text", { x: 10, y: 50, class: "sub" }, grp).textContent =
      `${kindLabel(n.logical)} · r${n.rev.id}${n.rev.head ? "" : " (old)"} · ${n.rev.summary.split("(")[0]}${n.rev.branch_only ? " · branch" : ""}${premiseNote(n.rev, true)}`;
    el("title", {}, grp).textContent = `${n.logical.key}\nr${n.rev.id} ${n.rev.summary}${n.rev.base ? " · base" : ""}${n.rev.abstract ? `\n\n${n.rev.abstract}` : ""}`;
    grp.addEventListener("click", (ev) => { ev.stopPropagation(); select(n.rev.id); });
    grp.addEventListener("mouseenter", () => hover(model, id, true));
    grp.addEventListener("mouseleave", () => hover(model, id, false));
  }
  applyView();
  if (!state.laidOut && model.nodes.size) { fit(xy); state.laidOut = true; }
}
// ---- network view (Obsidian-like) ----------------------------------------

/// What a network render draws, apart from styling: nodes with their labels
/// and status, and edges. Equal strings mean the scene built is current.
function structure(model) {
  const parts = [];
  for (const [id, n] of model.nodes) {
    const r = n.rev, l = n.logical;
    parts.push(`${id}|${r.id}|${r.class}|${r.base}|${r.head}|${r.summary}|${l.key}|${l.kind}|${l.anchor}|${r.abstract ?? ""}`);
  }
  for (const e of model.edges) parts.push(`${e.from}>${e.to}|${e.kind}|${e.disabled}|${e.rule}|${e.old}`);
  return parts.join("\n");
}
/// The network scene, drawn on a canvas (no element per node or edge: an
/// SVG of a few hundred dots spent most of each frame painting). `P` holds
/// the simulation's position objects, index-aligned with the arrays here.
const net = {
  n: 0, ids: [], nodes: [], P: [], R: null, hub: null, labels: [], titles: [],
  near: [], edges: [], groups: [], groupOf: null, colors: {}, hover: -1, raf: 0, ctx: null,
};
/// Colours from the stylesheet (light or dark).
function readColors() {
  const cs = getComputedStyle(document.documentElement);
  const v = (name) => cs.getPropertyValue(`--${name}`).trim();
  net.colors = Object.fromEntries(["bg", "fg", "muted", "line", "panel", "accent", ...CLASSES, ...KINDS].map((k) => [k, v(k)]));
}
/// Draws once on the next frame, unless the simulation already draws.
function requestDraw() {
  if (net.raf || state.sim.raf) return;
  net.raf = requestAnimationFrame(() => { net.raf = 0; drawNet(); });
}
/// A live force simulation drawn as dots: radius from the number of
/// neighbours, colour from the status, the name under the dot (only for
/// hubs, the selection and search hits when zoomed out). Positions persist
/// across refreshes, so only new nodes move in; dragging a node pulls its
/// neighbours along. Nothing is drawn while nothing changes. When only the
/// selection, search, filters or highlight changed, the scene is redrawn
/// as it is.
function renderNetwork(model) {
  const sim = state.sim;
  const cv = $("net");
  $("graph").hidden = true;
  cv.hidden = false;
  $("empty").hidden = model.nodes.size > 0;
  const ids = [...model.nodes.keys()];
  const sig = structure(model);
  if (sig === sim.sig) {
    if (!state.laidOut && ids.length) { fitNet(ids); state.laidOut = true; }
    requestDraw();
    return;
  }
  sim.sig = sig;
  readColors();
  const n = ids.length;
  const index = new Map(ids.map((id, i) => [id, i]));
  // Neighbours and degree.
  const near = ids.map(() => new Set());
  for (const e of model.edges) {
    const u = index.get(e.from), v = index.get(e.to);
    if (u === undefined || v === undefined || u === v) continue;
    near[u].add(v); near[v].add(u);
  }
  // Hubs: the best-connected nodes keep their names when zoomed out.
  const degrees = near.map((s) => s.size).sort((a, b) => b - a);
  const hubMin = Math.max(3, degrees[Math.min(degrees.length - 1, Math.max(5, Math.round(n * 0.12)))] ?? 3);
  // Start positions: kept ones stay; new ones near a placed neighbour, or
  // on the deterministic seed layout (spread out), computed only when some
  // new node needs it.
  let seed = null, fresh = 0;
  ids.forEach((id, i) => {
    if (sim.pos.has(id)) return;
    fresh++;
    const anchor = [...near[i]].map((m) => sim.pos.get(ids[m])).find(Boolean);
    const j = fresh * 2.39996;
    if (anchor) {
      sim.pos.set(id, { x: anchor.x + 40 * Math.cos(j), y: anchor.y + 40 * Math.sin(j), vx: 0, vy: 0 });
    } else {
      seed ||= layoutNetwork(model);
      const s0 = seed.get(id);
      sim.pos.set(id, { x: s0.x * 0.9, y: s0.y * 1.8, vx: 0, vy: 0 });
    }
  });
  for (const id of [...sim.pos.keys()]) if (!model.nodes.has(id)) sim.pos.delete(id);
  const nodes = ids.map((id) => model.nodes.get(id));
  const groupNames = nodes.map((node) => splitKey(label(node.logical))[0]);
  const groupIds = new Map();
  const G = Int32Array.from(groupNames, (g) => {
    if (!groupIds.has(g)) groupIds.set(g, groupIds.size);
    return groupIds.get(g);
  });
  const size = new Array(groupIds.size).fill(0);
  for (const g of G) size[g]++;
  Object.assign(net, {
    n, ids, nodes, near: near.map((s) => [...s]),
    P: ids.map((id) => sim.pos.get(id)),
    R: Float64Array.from(near, (s) => 6 + Math.sqrt(s.size) * 2.8),
    hub: Uint8Array.from(near, (s) => (s.size > hubMin ? 1 : 0)),
    labels: nodes.map((node) => clipMiddle(splitKey(label(node.logical))[1], 30)),
    titles: nodes.map((node) => `${node.logical.key}\n${kindLabel(node.logical)} · r${node.rev.id} ${node.rev.summary}${node.rev.base ? " · base" : ""}${node.rev.abstract ? `\n\n${node.rev.abstract}` : ""}`),
    groupOf: G,
    // Group names, at each group's centre (groups of three or more).
    groups: [...groupIds.keys()].map((name, g) => (name && size[g] >= 3 ? clip(name, 48, true) : null)),
    edges: model.edges
      .map((e) => ({ u: index.get(e.from), v: index.get(e.to), e }))
      .filter(({ u, v }) => u !== undefined && v !== undefined && u !== v),
    hover: -1,
  });

  // Forces, d3-style: velocity with decay, strength scaled by alpha.
  const P = net.P;
  const springs = [];
  const seen = new Set();
  for (const { u, v } of net.edges) {
    const key = u < v ? u * n + v : v * n + u;
    if (seen.has(key)) continue;
    seen.add(key);
    springs.push(u, v);
  }
  const X = new Float64Array(n), Y = new Float64Array(n), FX = new Float64Array(n), FY = new Float64Array(n);
  const CX = new Float64Array(groupIds.size), CY = new Float64Array(groupIds.size), CN = new Float64Array(groupIds.size);
  const step = () => {
    const a = sim.alpha;
    for (let i = 0; i < n; i++) { X[i] = P[i].x; Y[i] = P[i].y; }
    FX.fill(0); FY.fill(0);
    repel(X, Y, FX, FY, 4200 * a, 360000, 1);
    for (let i = 0; i < n; i++) { P[i].vx += FX[i]; P[i].vy += FY[i]; }
    for (let s = 0; s < springs.length; s += 2) {
      const pu = P[springs[s]], pv = P[springs[s + 1]];
      const ex = pv.x - pu.x, ey = pv.y - pu.y, d = Math.hypot(ex, ey) || 1;
      const f = ((d - 100) / d) * 0.05 * a;
      pu.vx += ex * f; pu.vy += ey * f; pv.vx -= ex * f; pv.vy -= ey * f;
    }
    CX.fill(0); CY.fill(0); CN.fill(0);
    for (let i = 0; i < n; i++) { CX[G[i]] += P[i].x; CY[G[i]] += P[i].y; CN[G[i]]++; }
    for (let i = 0; i < n; i++) {
      const g = G[i];
      P[i].vx += ((CX[g] / CN[g] - P[i].x) * 0.012 - P[i].x * 0.005) * a;
      P[i].vy += ((CY[g] / CN[g] - P[i].y) * 0.012 - P[i].y * 0.0085) * a;
    }
    for (const p of P) {
      if (p.fx !== undefined) { p.x = p.fx; p.y = p.fy; p.vx = p.vy = 0; continue; }
      p.vx *= 0.62; p.vy *= 0.62;
      p.x += p.vx; p.y += p.vy;
    }
    const target = sim.drag ? 0.25 : 0;
    sim.alpha += (target - sim.alpha) * 0.025;
  };
  sim.tick = () => {
    step();
    drawNet();
    if (sim.alpha < 0.004 && !sim.drag) { sim.raf = 0; return; }
    sim.raf = requestAnimationFrame(sim.tick);
  };
  // A new graph settles mostly out of sight, then moves in; a refresh
  // with a few new nodes just warms up.
  const first = fresh === n;
  sim.alpha = first ? 1 : fresh ? 0.35 : Math.max(sim.alpha, 0.05);
  if (first) for (let i = 0; i < 160; i++) step();
  if (!state.laidOut && n) { fitNet(ids); state.laidOut = true; }
  if (!sim.raf) sim.raf = requestAnimationFrame(sim.tick);
}
/// One frame of the network: edges batched by style (one stroke each),
/// arrowheads, dots, then names. Text keeps its size on screen at any zoom;
/// names show once the view is close enough to read them.
function drawNet() {
  const cv = $("net");
  if (cv.hidden) return;
  const dpr = window.devicePixelRatio || 1;
  const w = cv.clientWidth, h = cv.clientHeight;
  if (cv.width !== Math.round(w * dpr) || cv.height !== Math.round(h * dpr)) {
    cv.width = Math.round(w * dpr); cv.height = Math.round(h * dpr);
  }
  const c = (net.ctx ||= cv.getContext("2d"));
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.clearRect(0, 0, w, h);
  const { n, P, R, nodes, edges, colors: col } = net;
  if (!n) return;
  const { x: vx, y: vy, k } = state.view;
  const far = k < 0.55;
  const q = state.search.trim().toLowerCase();
  const hl = state.highlight;
  const matches = (i) => q && nodes[i].logical.key.toLowerCase().includes(q);
  const SX = new Float64Array(n), SY = new Float64Array(n), dim = new Uint8Array(n), hit = new Uint8Array(n);
  for (let i = 0; i < n; i++) {
    SX[i] = P[i].x * k + vx; SY[i] = P[i].y * k + vy;
    hit[i] = matches(i) ? 1 : 0;
    dim[i] = state.hidden.has(nodes[i].rev.class) || (hl && !hl.has(net.ids[i])) || (q && !hit[i]) ? 1 : 0;
  }
  // Hover: the node, its neighbours and their edges stand out.
  const hv = net.hover >= 0 && !hl && !state.sim.drag ? net.hover : -1;
  const close = new Uint8Array(n);
  if (hv >= 0) { close[hv] = 1; for (const m of net.near[hv]) close[m] = 1; }

  // Edges, one path per (style, emphasis).
  const buckets = new Map();
  for (const { u, v, e } of edges) {
    const a = hv >= 0 ? (u === hv || v === hv ? "near" : "faded")
      : ((state.hidden.has(nodes[u].rev.class) || state.hidden.has(nodes[v].rev.class) || (hl && !(hl.has(e.from) && hl.has(e.to))) || (q && !(hit[u] || hit[v]))) ? "dim" : "normal");
    const key = `${e.kind}|${e.disabled ? 1 : 0}|${a}`;
    let b = buckets.get(key);
    if (!b) buckets.set(key, (b = []));
    b.push(u, v);
  }
  const ALPHA = { normal: 0.32, dim: 0.06, faded: 0.05, near: 0.9 };
  c.lineCap = "butt";
  for (const [key, list] of buckets) {
    const [kind, disabled, a] = key.split("|");
    c.globalAlpha = ALPHA[a];
    c.strokeStyle = col[kind] || col.ref;
    c.lineWidth = a === "near" ? 1.6 : 1;
    c.setLineDash(disabled === "1" ? [1, 4] : kind === "sem" ? [4, 3] : kind === "ref" ? [2, 3] : []);
    c.beginPath();
    const heads = [];
    for (let j = 0; j < list.length; j += 2) {
      const u = list[j], v = list[j + 1];
      const dx = SX[v] - SX[u], dy = SY[v] - SY[u], d = Math.hypot(dx, dy) || 1;
      const ra = R[u] * k + 1, rb = R[v] * k + 3;
      if (d <= ra + rb) continue;
      const x1 = SX[u] + (dx / d) * ra, y1 = SY[u] + (dy / d) * ra, x2 = SX[v] - (dx / d) * rb, y2 = SY[v] - (dy / d) * rb;
      if (Math.max(x1, x2) < 0 || Math.min(x1, x2) > w || Math.max(y1, y2) < 0 || Math.min(y1, y2) > h) continue;
      c.moveTo(x1, y1); c.lineTo(x2, y2);
      heads.push(x2, y2, dx / d, dy / d);
    }
    c.stroke();
    // Arrowheads in the muted colour, 4.5 px, as the old SVG markers.
    if (heads.length) {
      c.setLineDash([]);
      c.fillStyle = col.muted;
      c.beginPath();
      for (let j = 0; j < heads.length; j += 4) {
        const [x, y, ux, uy] = [heads[j], heads[j + 1], heads[j + 2], heads[j + 3]];
        c.moveTo(x, y);
        c.lineTo(x - ux * 4.5 - uy * 2.25, y - uy * 4.5 + ux * 2.25);
        c.lineTo(x - ux * 4.5 + uy * 2.25, y - uy * 4.5 - ux * 2.25);
        c.closePath();
      }
      c.fill();
    }
  }
  c.setLineDash([]);
  // Dots.
  const alphaOf = (i) => (dim[i] ? 0.15 : hv >= 0 && !close[i] ? 0.18 : 1);
  for (let i = 0; i < n; i++) {
    const r = R[i] * k;
    if (SX[i] + r < -4 || SX[i] - r > w + 4 || SY[i] + r < -4 || SY[i] - r > h + 4) continue;
    const rev = nodes[i].rev, selected = state.selected === rev.id;
    c.globalAlpha = alphaOf(i);
    c.beginPath();
    c.arc(SX[i], SY[i], r, 0, 2 * Math.PI);
    c.fillStyle = col[rev.class];
    c.fill();
    if (selected) { c.strokeStyle = col.accent; c.lineWidth = 3.5; }
    else if (hit[i]) { c.strokeStyle = col.accent; c.lineWidth = 2.5; }
    else if (!rev.head) { c.strokeStyle = col.muted; c.lineWidth = 1.5; c.setLineDash([2, 2]); }
    else if (rev.base) { c.strokeStyle = col.fg; c.lineWidth = 2; c.globalAlpha *= 0.45; }
    else { c.strokeStyle = col.bg; c.lineWidth = 1.5; }
    c.stroke();
    c.setLineDash([]);
  }
  // Names under the dots; zoomed out, only hubs, the selection, search hits
  // and the hovered neighbourhood.
  const font = '13.5px -apple-system, system-ui, "Segoe UI", sans-serif';
  c.textAlign = "center";
  c.lineJoin = "round";
  c.lineWidth = 3.5;
  c.strokeStyle = col.bg;
  c.fillStyle = col.fg;
  for (let i = 0; i < n; i++) {
    const r = R[i] * k;
    const y = SY[i] + r + 16.9;
    if (SX[i] < -200 || SX[i] > w + 200 || y < -20 || y - r > h + 40) continue;
    const marked = state.selected === nodes[i].rev.id || hit[i] || (hv >= 0 && close[i]);
    let a = 0.9, bold = false;
    if (far) {
      if (marked) a = 0.95;
      else if (net.hub[i] && !(hv >= 0)) { a = 0.8; bold = true; }
      else continue;
    }
    c.globalAlpha = a * alphaOf(i);
    c.font = bold ? `600 ${font}` : font;
    c.strokeText(net.labels[i], SX[i], y);
    c.fillText(net.labels[i], SX[i], y);
  }
  // Group names above their clusters.
  const gn = net.groups.length;
  const gx = new Float64Array(gn), gtop = new Float64Array(gn).fill(Infinity), gc = new Float64Array(gn);
  for (let i = 0; i < n; i++) {
    const g = net.groupOf[i];
    if (!net.groups[g]) continue;
    gx[g] += P[i].x; gc[g]++; gtop[g] = Math.min(gtop[g], P[i].y);
  }
  c.globalAlpha = 0.55;
  c.fillStyle = col.muted;
  c.font = '600 16px -apple-system, system-ui, "Segoe UI", sans-serif';
  net.groups.forEach((name, g) => {
    if (name) c.fillText(name, (gx[g] / gc[g]) * k + vx, (gtop[g] - 26) * k + vy);
  });
  c.globalAlpha = 1;
}
/// The node under a pointer event, or -1: the nearest whose dot (plus a
/// few pixels) contains the point.
function netHit(ev) {
  if (!net.n) return -1;
  const at = toGraph(ev), k = state.view.k;
  let best = -1, bd = Infinity;
  for (let i = 0; i < net.n; i++) {
    const dx = net.P[i].x - at.x, dy = net.P[i].y - at.y, d2 = dx * dx + dy * dy;
    const rr = net.R[i] + 4 / k;
    if (d2 <= rr * rr && d2 < bd) { best = i; bd = d2; }
  }
  return best;
}
/// Pointer over the canvas: the hovered node (tooltip, highlight).
function hoverAt(ev) {
  const i = state.sim.drag ? net.hover : netHit(ev);
  if (i === net.hover) return;
  net.hover = i;
  const cv = $("net");
  cv.title = i >= 0 ? net.titles[i] : "";
  cv.classList.toggle("on-node", i >= 0);
  requestDraw();
}
function stopSim() {
  if (state.sim.raf) cancelAnimationFrame(state.sim.raf);
  state.sim.raf = 0;
}
function reheat(alpha) {
  const sim = state.sim;
  sim.alpha = Math.max(sim.alpha, alpha);
  if (!sim.raf && sim.tick) sim.raf = requestAnimationFrame(sim.tick);
}
/// The element the graph is drawn on: the canvas or the layered SVG.
const surface = () => (state.layout === "network" ? $("net") : $("graph"));
/// Graph coordinates of a pointer event.
function toGraph(ev) {
  const r = surface().getBoundingClientRect();
  return { x: (ev.clientX - r.left - state.view.x) / state.view.k, y: (ev.clientY - r.top - state.view.y) / state.view.k };
}
function startDrag(ev, id, rev) {
  ev.stopPropagation();
  ev.preventDefault();
  const p = state.sim.pos.get(id);
  const at = toGraph(ev);
  state.sim.drag = { id, rev, dx: p.x - at.x, dy: p.y - at.y, x0: ev.clientX, y0: ev.clientY, moved: false };
  p.fx = p.x; p.fy = p.y;
  reheat(0.12);
}
function moveDrag(ev) {
  const d = state.sim.drag;
  if (!d) return false;
  if (Math.abs(ev.clientX - d.x0) + Math.abs(ev.clientY - d.y0) > 3) d.moved = true;
  const at = toGraph(ev), p = state.sim.pos.get(d.id);
  p.fx = at.x + d.dx; p.fy = at.y + d.dy;
  reheat(0.12);
  return true;
}
function endDrag() {
  const d = state.sim.drag;
  if (!d) return false;
  const p = state.sim.pos.get(d.id);
  if (p) { delete p.fx; delete p.fy; }
  state.sim.drag = null;
  if (!d.moved) select(d.rev);
  return true;
}
function fitNet(ids) {
  const box = $("net").getBoundingClientRect();
  const P = ids.map((id) => state.sim.pos.get(id));
  const x0 = Math.min(...P.map((p) => p.x)) - 60, x1 = Math.max(...P.map((p) => p.x)) + 60;
  const y0 = Math.min(...P.map((p) => p.y)) - 40, y1 = Math.max(...P.map((p) => p.y)) + 40;
  const k = Math.min(1.6, Math.max(0.15, Math.min((box.width - 40) / (x1 - x0), (box.height - 100) / (y1 - y0))));
  state.view = { k, x: (box.width - (x1 - x0) * k) / 2 - x0 * k, y: 70 + (box.height - 100 - (y1 - y0) * k) / 2 - y0 * k };
  applyView();
}
function hover(model, id, on) {
  if (state.highlight) return;
  const near = new Set([id]);
  for (const e of model.edges) { if (e.from === id) near.add(e.to); if (e.to === id) near.add(e.from); }
  const ids = [...model.nodes.keys()];
  document.querySelectorAll("#viewport .node").forEach((n, i) => {
    n.classList.toggle("dim", on && !near.has(ids[i]));
  });
}
function fit(xy, W = NODE_W, H = NODE_H) {
  const svg = $("graph").getBoundingClientRect();
  const xs = [...xy.values()];
  const w = Math.max(...xs.map((p) => p.x)) + W, h = Math.max(...xs.map((p) => p.y)) + H;
  // A focused view stays readable (pan for the rest); the whole graph may
  // shrink further.
  const floor = state.focus && state.selected !== null ? 0.6 : 0.2;
  const k = Math.min(1.2, Math.max(floor, Math.min((svg.width - 60) / w, (svg.height - 110) / h)));
  state.view = { k, x: Math.max(20, (svg.width - w * k) / 2), y: 70 };
  applyView();
}
/// Pan and zoom: the layered SVG moves its viewport; the canvas redraws.
function applyView() {
  if (state.layout === "network") { requestDraw(); return; }
  const { x, y, k } = state.view;
  $("viewport").setAttribute("transform", `translate(${x},${y}) scale(${k})`);
}

// ---- detail panel ------------------------------------------------------

async function select(rev) {
  // A team copy that is merged opens its local record; a hidden one shows
  // the team copies again.
  if (shownRev(rev) === null && state.teamCopies === "hidden") setTeamCopies("shown");
  rev = shownRev(rev) ?? rev;
  if (!revOf(rev)) return;
  const moved = state.selected !== rev;
  state.selected = rev;
  state.highlight = null;
  if (state.tab === "graph" && state.focus && moved) state.laidOut = false;
  renderView();
  const g = state.graph;
  const r = revOf(rev);
  const l = logicalOf(r.logical);
  const box = $("detail");
  box.replaceChildren();
  $("panel").hidden = false;
  box.append(html("h2", "", label(l)));
  const badge = html("span", `badge ${r.class}`, r.summary);
  box.append(badge);
  const kv = html("dl", "kv");
  const row = (k, v) => { kv.append(html("dt", "", k)); const dd = html("dd"); dd.append(v); kv.append(dd); };
  row("kind", kindLabel(l));
  if (l.anchor) row("key", l.key);
  row("scope", l.lane);
  if (l.teamCopy) row("team copy", `${l.teamCopy} (same content, merged here)`);
  row("revision", `r${r.id}${r.head ? " (head)" : ` (head is r${l.head})`}`);
  if (r.branch_only) row("branch", "only on another branch (BRANCH_ONLY on main)");
  row("base", r.base ? "yes" : "no");
  row("layers", `L0 ${r.l0 ? "✓" : "✗"} · L1 ${r.l1 ? "✓" : "✗"} · L2 ${r.l2 ? "✓" : "✗"}`);
  row("support", String(r.support));
  if (r.premises) row("premises", r.premises.moved ? premiseNote(r, false) : `${r.premises.total} Sem premises, none changed`);
  box.append(kv);
  const actions = html("div", "list");
  const around = html("span", "link", "show in graph");
  around.addEventListener("click", () => { state.focus ||= 2; $("focus").value = String(state.focus); state.laidOut = false; setTab("graph"); });
  actions.append(around);
  box.append(actions);
  const conflicts = (g.conflicts || []).filter((c) => c.target === rev);
  if (conflicts.length) {
    box.append(html("h3", "", "Conflicts"));
    for (const c of conflicts) {
      box.append(html("div", c.open ? "conflict" : "muted",
        `artifact ${c.id} · ${c.open ? "open: gates this revision (CONFLICTED) until resolved" : "resolved"} · from a merge at epoch ${c.origin_epoch}`));
    }
  }
  let d;
  try { d = await getJSON(query(`/api/rev?id=${rev}`)); } catch (e) { showError(e); return; }
  if (state.selected !== rev) return;
  if (d.live && d.live.summary !== r.summary) {
    const live = html("div", "list");
    live.append(html("span", "muted", "now (ctxd):"), html("span", `badge ${d.live.class}`, d.live.summary));
    box.insertBefore(live, kv);
  }
  if (d.anchor) {
    const a = html("dl", "kv");
    const arow = (k, v) => { a.append(html("dt", "", k), html("dd", "", v)); };
    arow("anchors", d.anchor.type === "symbol" ? "a symbol" : "a file");
    arow("path", d.anchor.path);
    if (d.anchor.symbol) arow("symbol", d.anchor.symbol);
    if (d.anchor.language) arow("language", d.anchor.language);
    if (d.anchor.normaliser) arow("normaliser", d.anchor.normaliser);
    arow(d.anchor.oid ? "blob" : "hash", d.anchor.oid ?? d.anchor.hash);
    box.append(html("h3", "", "Observed code"), a);
  } else if (d.body === undefined) {
    box.append(html("h3", "", "Body"), html("pre", "muted", `(${d.body_error || "unavailable"})`));
  } else {
    // ADR 0016 §4: abstract and outline are the pure renderings ctx_read
    // serves; each says whether it left something out.
    const head = html("h3", "levels");
    head.append(document.createTextNode("Body "));
    const pre = html("pre");
    const show = (lv) => {
      state.level = lv;
      const r = lv === "full" ? { text: d.body, cut: false } : d.levels[lv];
      pre.textContent = r.text || "(empty)";
      for (const b of head.querySelectorAll("button")) b.classList.toggle("active", b.dataset.level === lv);
      cut.hidden = !r.cut;
    };
    for (const lv of ["abstract", "outline", "full"]) {
      const b = html("button", "seg", lv);
      b.type = "button";
      b.dataset.level = lv;
      b.addEventListener("click", () => show(lv));
      head.append(b);
    }
    const cut = html("div", "muted", "cut: the full body has more");
    box.append(head, pre, cut);
    show(d.levels ? state.level : "full");
    if (!d.levels) for (const b of head.querySelectorAll("button")) b.hidden = b.dataset.level !== "full";
  }
  const own = g.rules.filter((x) => x.conclusion === rev);
  box.append(html("h3", "", `Depends on (${own.length} rule${own.length === 1 ? "" : "s"})`));
  if (!own.length) box.append(html("div", "muted", r.base ? "base (no rule needed)" : "no rules"));
  for (const rule of own) {
    const line = html("div", "list");
    const hidden = rule.omitted ? ` · ${rule.omitted} on hidden team copies` : "";
    const note = (rule.premises.length || rule.omitted ? "" : " · references only, never supports") + hidden;
    const name = rule.team ? "team rule (at the root)" : `rule ${rule.id}`;
    line.append(html("span", "muted", `${name}${rule.enabled ? "" : " (disabled)"}${note}:`));
    for (const p of rule.premises) line.append(revLink(p.rev, `${p.kind} → `));
    for (const x of rule.references) line.append(revLink(x, "ref → "));
    box.append(line);
  }
  const uses = (rule) => rule.premises.find((p) => p.rev === rev)?.kind ?? (rule.references.includes(rev) ? "ref" : null);
  const dependents = g.rules.filter((x) => uses(x));
  box.append(html("h3", "", `Used by (${dependents.length})`));
  const used = html("div", "list");
  for (const rule of dependents) used.append(revLink(rule.conclusion, `${uses(rule)} ← `));
  box.append(used);
  const impact = [...d.impact.supported, ...d.impact.reference_only];
  box.append(html("h3", "", `Impact (${d.impact.supported.length} supported, ${d.impact.reference_only.length} reference only)`));
  const ib = html("span", "link", impact.length ? "highlight in graph" : "nothing depends on it");
  if (impact.length) ib.addEventListener("click", () => highlightRevs([rev, ...impact]));
  box.append(ib);
  box.append(html("h3", "", "Revisions"));
  const rl = html("div", "list");
  for (const x of d.revisions) rl.append(revLink(x, ""));
  box.append(rl);
  if (d.head_moves.length) {
    const hm = html("pre", "", d.head_moves.map((m) => `epoch ${m.epoch}: ${m.from === null ? "created" : `r${m.from}`} → r${m.to}`).join("\n"));
    box.append(hm);
  }
  box.append(html("h3", "", "Why"), whyTree(d.why));
  if (r.class !== "certified") box.append(html("h3", "", "Why not"), whyNotTree(d.why_not, own));
  box.append(html("h3", "", "Content hash"), html("pre", "", d.content));
}
/// The proof tree: how each revision holds (base or an active rule), its
/// premises below it.
function whyTree(p) {
  const ul = html("ul", "tree");
  const li = html("li");
  li.append(revLink(p.rev, ""));
  const how = p.witness === "base" ? "base" : p.witness === null ? "not supported" : `rule ${p.witness}`;
  li.append(html("span", "muted", ` — ${how}${p.active_rules.length > 1 ? ` (${p.active_rules.length} active rules)` : ""}`));
  if (p.references.length) {
    const refs = html("div", "list");
    refs.append(html("span", "muted", "references:"));
    for (const x of p.references) refs.append(revLink(x.rev, ""));
    li.append(refs);
  }
  for (const q of p.premises) li.append(whyTree(q));
  if (p.truncated) li.append(html("div", "muted", "… deeper premises not shown"));
  ul.append(li);
  return ul;
}
const FAILURE = {
  unsupported: (f) => ["premise ", f.premise, " is not supported"],
  not_current: (f) => ["cur premise ", f.premise, ` is no longer its head (now r${f.head})`],
  superseded_pinned: (f) => ["pinned ", f.premise, ` was superseded by r${f.head}`],
  semantic_head_unsupported: (f) => ["sem: the current head ", f.head, " does not hold"],
};
/// Each rule that could support the revision and why it does not.
function whyNotTree(w, own) {
  const ul = html("ul", "tree");
  if (!w.rules.length) ul.append(html("li", "muted", "no rules conclude it"));
  for (const f of w.rules) {
    const li = html("li");
    const rule = own.find((x) => x.id === f.rule);
    const refOnly = rule && !rule.premises.length;
    const gate = f.gate === "NotRequired" ? "" : ` · semantic ${f.gate}`;
    li.append(html("span", "", `rule ${f.rule}${f.disabled ? " (disabled)" : ""}${gate}`));
    if (refOnly) li.append(html("div", "muted", "references only: never supports its conclusion"));
    for (const i of f.incidences) {
      const [a, rev, b] = FAILURE[i.type](i);
      const line = html("div", "list");
      line.append(html("span", "", a), revLink(rev, ""), html("span", "", b));
      li.append(line);
    }
    if (!f.incidences.length && !f.disabled && !refOnly && f.gate === "NotRequired") li.append(html("div", "muted", "holds"));
    for (const u of f.upstream) li.append(whyNotTree(u, state.graph.rules.filter((x) => x.conclusion === u.rev)));
    ul.append(li);
  }
  return ul;
}
function revLink(rev, prefix) {
  const r = revOf(shownRev(rev) ?? rev) ?? state.raw?.revisions.find((x) => x.id === rev);
  const l = r && (logicalOf(r.logical) ?? state.raw?.logicals.find((x) => x.id === r.logical));
  const s = html("span", "link", `${prefix}${l ? label(l) : "?"}@r${r?.id ?? rev}`);
  s.title = r ? r.summary : "";
  s.addEventListener("click", () => select(rev));
  return s;
}
function highlightRevs(revs) {
  const set = new Set();
  for (const id of revs) {
    const rev = shownRev(id);
    const r = rev === null ? null : revOf(rev);
    if (!r) continue;
    set.add(state.allRevs ? `r${rev}` : `l${r.logical}`);
  }
  state.highlight = set;
  // A highlight is shown on the whole graph.
  state.focus = 0;
  $("focus").value = "0";
  state.laidOut = false;
  if (state.tab !== "graph") setTab("graph"); else render();
}

// ---- records -----------------------------------------------------------

const SEVERITY = { conflicted: 0, stale: 1, pending: 2, unavailable: 3, certified: 4 };
/// The heads shown under the current search and filters.
function shownHeads() {
  const g = state.graph;
  const q = state.search.trim().toLowerCase();
  const logicals = new Map(g.logicals.map((l) => [l.id, l]));
  return g.revisions
    .filter((r) => r.head && !state.hidden.has(r.class))
    .filter((r) => !state.changedSummaries || (r.premises && r.premises.moved))
    .map((r) => ({ r, l: logicals.get(r.logical) }))
    .filter(({ r, l }) => !q || l.key.toLowerCase().includes(q) || (r.abstract || "").toLowerCase().includes(q));
}
/// A tree of key path segments; chains of single-child groups are merged
/// so `quant-engine/research/` is one level, not two.
function pathTree(items) {
  const root = { name: "", path: "", groups: new Map(), leaves: [] };
  for (const it of items) {
    const parts = label(it.l).split("/");
    let node = root;
    for (const seg of parts.slice(0, -1)) {
      if (!node.groups.has(seg)) node.groups.set(seg, { name: seg, path: `${node.path}${seg}/`, groups: new Map(), leaves: [] });
      node = node.groups.get(seg);
    }
    node.leaves.push({ ...it, leaf: parts[parts.length - 1] });
  }
  const merge = (n) => {
    for (const [k, c] of [...n.groups]) {
      let m = c;
      while (!m.leaves.length && m.groups.size === 1) {
        const [only] = m.groups.values();
        m = { ...only, name: `${m.name}/${only.name}` };
      }
      merge(m);
      n.groups.set(k, m);
    }
    return n;
  };
  return merge(root);
}
function countTree(n) {
  const c = { total: n.leaves.length, bad: 0 };
  for (const x of n.leaves) if (x.r.class !== "certified") c.bad++;
  for (const g of n.groups.values()) { const s = countTree(g); c.total += s.total; c.bad += s.bad; }
  return c;
}
function renderRecords() {
  const g = state.graph;
  const box = $("records");
  box.replaceChildren();
  if (!g) return;
  renderRecent();
  const items = shownHeads();
  if (!items.length) {
    box.append(html("div", "muted pad", g.logicals.length ? "Nothing matches the search and filters." : "No records yet. Agents add them with ctx_record."));
    return;
  }
  const tree = pathTree(items);
  const searching = state.search.trim() !== "";
  const walk = (n, depth, parent) => {
    const groups = [...n.groups.values()].sort((a, b) => a.name.localeCompare(b.name));
    for (const grp of groups) {
      const c = countTree(grp);
      const open = searching || !state.collapsed.has(grp.path);
      const head = html("div", "group");
      head.style.paddingLeft = `${8 + depth * 16}px`;
      head.append(html("span", "caret", open ? "▾" : "▸"), html("span", "gname", `${grp.name}/`),
        html("span", "muted", ` ${c.total}`));
      if (c.bad) head.append(html("span", "badge stale small", `${c.bad} not certified`));
      head.addEventListener("click", () => { open ? state.collapsed.add(grp.path) : state.collapsed.delete(grp.path); renderRecords(); });
      parent.append(head);
      if (open) walk(grp, depth + 1, parent);
    }
    const leaves = [...n.leaves].sort((a, b) => SEVERITY[a.r.class] - SEVERITY[b.r.class] || a.leaf.localeCompare(b.leaf));
    for (const it of leaves) parent.append(recordRow(it, depth));
  };
  walk(tree, 0, box);
}
function recordRow({ r, l, leaf }, depth) {
  const row = html("div", `rec${state.selected === r.id ? " selected" : ""}`);
  row.style.paddingLeft = `${8 + depth * 16}px`;
  const top = html("div", "rec-top");
  const dot = html("span", "dot");
  dot.style.background = `var(--${r.class})`;
  top.append(dot, html("span", "rec-key", leaf), html("span", "tag", kindLabel(l)), html("span", "muted", `r${r.id}`));
  if (r.class !== "certified") top.append(html("span", `badge ${r.class} small`, r.summary));
  const note = premiseNote(r, false);
  if (note) top.append(html("span", "badge pending small", note));
  if (r.branch_only) top.append(html("span", "tag", "branch only"));
  if (l.teamCopy) {
    const t = html("span", "tag", "team");
    t.title = `published to the team as ${l.teamCopy} (same content; its copy is merged here)`;
    top.append(t);
  }
  row.append(top);
  if (r.abstract) row.append(html("div", "rec-abstract", r.abstract));
  row.title = l.key;
  row.addEventListener("click", () => select(r.id));
  return row;
}
/// The latest head moves on this branch, newest first.
function renderRecent() {
  const box = $("recent");
  box.replaceChildren();
  const t = state.timeline, g = state.graph;
  if (!t || !g) return;
  const logicals = new Map(g.logicals.map((l) => [l.id, l]));
  const moves = [];
  for (const c of t.commits) {
    if (c.branch !== state.branch) continue;
    for (const m of c.head_moves) moves.push({ ...m, epoch: c.epoch });
    if (moves.length >= 8) break;
  }
  if (!moves.length) return;
  box.append(html("h3", "", "Recently changed"));
  const list = html("div", "recent-list");
  for (const m of moves.slice(0, 8)) {
    const l = logicals.get(m.logical);
    const item = html("div", "recent-item");
    item.append(html("span", "muted", `epoch ${m.epoch}`), revLink(m.to, ""),
      html("span", "muted", m.from === null ? "created" : `from r${m.from}`));
    if (!l) continue;
    list.append(item);
  }
  box.append(list);
}

// ---- timeline ----------------------------------------------------------

function renderTimeline() {
  const t = state.timeline, g = state.graph;
  const box = $("timeline-view");
  box.replaceChildren();
  if (!t || !g) return;
  const keyOfLogical = (id) => { const l = logicalOf(id); return l ? label(l) : `logical ${id}`; };
  const here = $("timeline-here").checked;
  for (const c of t.commits) {
    if (here && c.branch !== state.branch) continue;
    const card = html("div", `commit${c.branch === state.branch ? "" : " elsewhere"}`);
    card.append(html("div", "meta", `epoch ${c.epoch} · ${c.branch_name} · ${c.channel} · principal ${c.principal} · ${c.status_diff.length} status change(s)`));
    const named = c.names.length ? ` (${c.names.join(", ")})` : "";
    card.append(html("div", "ops", c.ops.join(", ") + named));
    if (c.branch !== state.branch) { box.append(card); continue; }
    for (const m of c.head_moves) {
      const line = html("div", "list");
      line.append(html("span", "", `${keyOfLogical(m.logical)}: ${m.from === null ? "created" : `r${m.from}`} → `));
      line.append(revLink(m.to, ""));
      card.append(line);
    }
    if (c.status_diff.length) {
      const s = html("span", "link", "show changed nodes");
      s.addEventListener("click", () => highlightRevs(c.status_diff));
      card.append(s);
    }
    box.append(card);
  }
}

// ---- chrome ------------------------------------------------------------

function renderChrome() {
  const g = state.graph;
  const counts = Object.fromEntries(CLASSES.map((c) => [c, 0]));
  for (const r of g.revisions) if (r.head) counts[r.class]++;
  const open = (g.conflicts || []).filter((c) => c.open).length;
  $("epoch").textContent = `epoch ${g.epoch} · ${g.logicals.length} logicals · ${g.revisions.length} revisions · ${g.rules.length} rules${open ? ` · ${open} open conflict${open === 1 ? "" : "s"}` : ""}`;
  const sel = $("branch");
  sel.replaceChildren();
  for (const b of g.branches) {
    const o = html("option", "", `${b.name}${b.archived ? " (archived)" : ""}`);
    o.value = String(b.id);
    o.selected = b.id === state.branch;
    sel.append(o);
  }
  sel.hidden = g.branches.length < 2;
  // Status of the heads: the classes present are filters; when every head
  // is certified that is said once rather than as a row of zeros.
  const chips = $("chips");
  chips.replaceChildren();
  const heads = g.revisions.filter((r) => r.head).length;
  const chip = (cls, text, on, toggle) => {
    const c = html("button", `chip${on ? "" : " off"}`);
    c.type = "button";
    if (cls) { const dot = html("span", "dot"); dot.style.background = `var(--${cls})`; c.append(dot); }
    c.append(document.createTextNode(text));
    c.addEventListener("click", toggle);
    chips.append(c);
  };
  const flip = (c) => () => { state.hidden.has(c) ? state.hidden.delete(c) : state.hidden.add(c); renderChrome(); renderView(); };
  for (const c of CLASSES) {
    if (!counts[c] && !state.hidden.has(c)) continue;
    chip(c, `${c} ${counts[c]}`, !state.hidden.has(c), flip(c));
  }
  if (state.raw.logicals.some((l) => l.team)) {
    const s = html("select", "team-copies");
    s.title = "Team copies: records imported from the team (team/<id>/…). Merged: a copy identical to a local record with the same key is shown once, as that record.";
    for (const m of TEAM_COPIES) {
      const o = html("option", "", `team copies: ${m}`);
      o.value = m;
      o.selected = m === state.teamCopies;
      s.append(o);
    }
    s.addEventListener("change", () => setTeamCopies(s.value));
    chips.append(s);
    if (g.teamDropped) chips.append(html("span", "muted", `${g.teamDropped} ${state.teamCopies === "merged" ? "merged into local records" : "hidden"}`));
  }
  if (heads && counts.certified === heads) chips.append(html("span", "ok", "all heads certified"));
  const superseded = g.revisions.length - heads;
  if (superseded) chips.append(html("span", "muted", `${superseded} superseded revision${superseded === 1 ? "" : "s"}`));
  const changed = g.revisions.filter((r) => r.head && r.premises && r.premises.moved).length;
  if (changed || state.changedSummaries) {
    chip("pending", `summaries with changed premises ${changed}`, state.changedSummaries,
      () => { state.changedSummaries = !state.changedSummaries; renderChrome(); renderView(); });
  }
  const legend = $("legend");
  legend.replaceChildren();
  for (const k of KINDS) {
    const sw = html("span", "sw");
    sw.style.borderColor = `var(--${k})`;
    if (k === "sem") sw.style.borderTopStyle = "dashed";
    if (k === "ref") sw.style.borderTopStyle = "dotted";
    const item = html("div");
    item.append(sw, document.createTextNode(k));
    legend.append(item);
  }
  legend.append(html("div", "muted", "thick border = base"), html("div", "muted", "dashed box = old revision"),
    html("div", "muted", "ref: never changes status"), html("div", "muted", "purple = open conflict"));
}
function setTeamCopies(mode) {
  state.teamCopies = mode;
  store("teamCopies", mode);
  setGraph(state.raw);
  if (state.selected !== null) state.selected = shownRev(state.selected);
  state.laidOut = false;
  state.sim.sig = null;
  renderChrome();
  renderView();
}
function setTab(tab) {
  if (tab !== "graph") stopSim();
  state.tab = tab;
  for (const t of ["records", "graph", "timeline"]) $(`tab-${t}`).classList.toggle("active", tab === t);
  $("records-view").hidden = tab !== "records";
  $("graph-view").hidden = tab !== "graph";
  $("timeline-pane").hidden = tab !== "timeline";
  renderView();
}
/// Renders the active tab.
function renderView() {
  if (state.tab === "records") renderRecords();
  else if (state.tab === "graph") render();
  else renderTimeline();
}

async function refresh(force) {
  try {
    const { fingerprint } = await getJSON("/api/epoch");
    if (!force && fingerprint === state.fingerprint) return;
    const [graph, timeline] = await Promise.all([getJSON(query("/api/graph")), getJSON("/api/timeline?limit=300")]);
    state.fingerprint = fingerprint;
    setGraph(graph);
    state.timeline = timeline;
    showError(null);
    renderChrome();
    renderView();
    if (state.selected !== null && graph.revisions.some((r) => r.id === state.selected)) select(state.selected);
  } catch (e) {
    showError(e);
  }
}

function wire() {
  const svg = $("graph"), cv = $("net");
  let drag = null;
  const pan = (e, el) => { drag = { x: e.clientX, y: e.clientY, vx: state.view.x, vy: state.view.y, el }; el.classList.add("dragging"); };
  svg.addEventListener("mousedown", (e) => pan(e, svg));
  // On the canvas a press on a dot drags the node; elsewhere it pans.
  cv.addEventListener("mousedown", (e) => {
    const i = netHit(e);
    if (i >= 0) startDrag(e, net.ids[i], net.nodes[i].rev.id);
    else pan(e, cv);
  });
  cv.addEventListener("mousemove", (e) => { if (!drag) hoverAt(e); });
  cv.addEventListener("mouseleave", () => { if (net.hover >= 0 && !state.sim.drag) { net.hover = -1; cv.title = ""; cv.classList.remove("on-node"); requestDraw(); } });
  window.addEventListener("mouseup", () => { endDrag(); drag?.el.classList.remove("dragging"); drag = null; });
  window.addEventListener("mousemove", (e) => {
    if (moveDrag(e)) return;
    if (!drag) return;
    state.view.x = drag.vx + e.clientX - drag.x;
    state.view.y = drag.vy + e.clientY - drag.y;
    applyView();
  });
  for (const el of [svg, cv]) {
    el.addEventListener("wheel", (e) => {
      e.preventDefault();
      const r = el.getBoundingClientRect();
      const mx = e.clientX - r.left, my = e.clientY - r.top;
      const k = Math.min(3, Math.max(0.1, state.view.k * Math.exp(-e.deltaY * 0.0015)));
      state.view.x = mx - ((mx - state.view.x) * k) / state.view.k;
      state.view.y = my - ((my - state.view.y) * k) / state.view.k;
      state.view.k = k;
      applyView();
    }, { passive: false });
    el.addEventListener("click", () => { if (state.highlight) { state.highlight = null; render(); } });
  }
  // The canvas follows its box (window resize, the panel opening).
  new ResizeObserver(() => requestDraw()).observe(cv);
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => { readColors(); requestDraw(); });
  $("search").addEventListener("input", (e) => { state.search = e.target.value; renderView(); });
  $("all-revs").addEventListener("change", (e) => { state.allRevs = e.target.checked; state.laidOut = false; state.highlight = null; render(); });
  $("close").addEventListener("click", () => { $("panel").hidden = true; state.selected = null; renderView(); });
  $("tab-records").addEventListener("click", () => setTab("records"));
  $("tab-graph").addEventListener("click", () => setTab("graph"));
  $("focus").addEventListener("change", (e) => { state.focus = Number(e.target.value); state.laidOut = false; render(); });
  $("fit").addEventListener("click", () => { state.laidOut = false; render(); });
  $("layout").addEventListener("change", (e) => { state.layout = e.target.value; state.laidOut = false; render(); });
  $("fit").addEventListener("dblclick", () => { state.sim.pos.clear(); state.sim.sig = null; state.laidOut = false; render(); });
  $("tab-timeline").addEventListener("click", () => setTab("timeline"));
  $("timeline-here").addEventListener("change", renderTimeline);
  $("branch").addEventListener("change", (e) => {
    state.branch = Number(e.target.value);
    state.selected = null; state.highlight = null; state.laidOut = false;
    $("panel").hidden = true;
    refresh(true);
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { state.highlight = null; $("panel").hidden = true; state.selected = null; renderView(); }
  });
}

wire();
refresh(true);
setInterval(() => refresh(false), 2000);
