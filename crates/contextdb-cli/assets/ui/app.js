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

const state = {
  graph: null, timeline: null, fingerprint: null,
  selected: null, allRevs: false, search: "", hidden: new Set(),
  view: { x: 40, y: 40, k: 1 }, highlight: null, tab: "records", laidOut: false,
  branch: 0, focus: 0, collapsed: new Set(), changedSummaries: false, level: "abstract",
  layout: "network",
  // Network layout: the live simulation, kept across refreshes.
  sim: { pos: new Map(), alpha: 0, raf: 0, drag: null, tick: null, dots: null, edges: [] },
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
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
        const ex = x[i] - x[j], ey = y[i] - y[j];
        const d2 = ex * ex + ey * ey + 0.01;
        // Repulsion only nearby (grid variant): unconnected nodes would
        // otherwise drift far out and shrink everything else on fit.
        if (d2 > CUTOFF2) continue;
        const f = (L * L) / d2;
        dx[i] += ex * f; dy[i] += ey * f; dx[j] -= ex * f; dy[j] -= ey * f;
      }
    }
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
  for (let pass = 0; pass < 40; pass++) {
    let moved = false;
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
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
const query = (extra) => `${extra}${extra.includes("?") ? "&" : "?"}branch=${state.branch}`;


// ---- rendering ---------------------------------------------------------

function render() {
  const g = state.graph;
  if (!g) return;
  const model = build(g);
  const net = state.layout === "network";
  if (net) { renderNetwork(model); return; }
  stopSim();
  $("graph").classList.remove("net", "far", "hovering");
  const xy = net ? layoutNetwork(model) : layout(model);
  const W = net ? NET_W : NODE_W, H = net ? NET_H : NODE_H;
  const vp = $("viewport");
  vp.replaceChildren();
  $("graph").classList.toggle("net", net);
  $("empty").hidden = model.nodes.size > 0;
  const q = state.search.trim().toLowerCase();
  const visible = (n) => !state.hidden.has(n.rev.class);
  const matches = (n) => !q || n.logical.key.toLowerCase().includes(q);
  const hl = state.highlight; // Set of node ids, or null

  if (net) {
    // Each key directory's name above its cluster.
    const top = new Map();
    for (const p of xy.values()) {
      const t = top.get(p.group);
      if (!t) top.set(p.group, { x0: p.x, x1: p.x + W, y: p.y });
      else { t.x0 = Math.min(t.x0, p.x); t.x1 = Math.max(t.x1, p.x + W); t.y = Math.min(t.y, p.y); }
    }
    // Larger groups first; a label that would overlap one already placed
    // moves up, and single-node groups get none (the node says enough).
    const size = new Map();
    for (const p of xy.values()) size.set(p.group, (size.get(p.group) || 0) + 1);
    const placed = [];
    const groupLayer = el("g", {}, vp);
    for (const [name, t] of [...top].sort((a, b) => size.get(b[0]) - size.get(a[0]))) {
      if (!name || size.get(name) < 2) continue;
      const text = clip(name, 48, true), w = text.length * 7.4, cx = (t.x0 + t.x1) / 2;
      let y = t.y - 10;
      for (let tries = 0; tries < 12 && placed.some((b) => Math.abs(b.x - cx) < (b.w + w) / 2 && Math.abs(b.y - y) < 16); tries++) y -= 16;
      placed.push({ x: cx, y, w });
      el("text", { class: "group-label", x: cx, y, "text-anchor": "middle" }, groupLayer).textContent = text;
    }
  }
  const edgeLayer = el("g", {}, vp);
  for (const e of model.edges) {
    const a = xy.get(e.from), b = xy.get(e.to);
    if (!a || !b) continue;
    const na = model.nodes.get(e.from), nb = model.nodes.get(e.to);
    const dim = !visible(na) || !visible(nb) || (hl && !(hl.has(e.from) && hl.has(e.to))) || (q && !(matches(na) || matches(nb)));
    const cls = ["edge", e.kind, e.disabled ? "disabled" : "", dim ? "dim" : ""].join(" ");
    const grp = el("g", { class: cls }, edgeLayer);
    if (net) {
      // Straight, from box edge to box edge.
      const ax = a.x + W / 2, ay = a.y + H / 2, bx = b.x + W / 2, by = b.y + H / 2;
      const [x1, y1] = boxExit(ax, ay, bx - ax, by - ay, W / 2, H / 2);
      const [x2, y2] = boxExit(bx, by, ax - bx, ay - by, W / 2 + 2, H / 2 + 2);
      el("path", { d: `M${x1},${y1} L${x2},${y2}`, "marker-end": "url(#arrow)" }, grp);
      const title = el("title", {}, grp);
      title.textContent = `${e.kind}${e.disabled ? " (disabled)" : ""}${e.rule !== undefined ? ` · rule ${e.rule}` : ""}${e.old != null ? ` · old r${e.old}` : ""}`;
      continue;
    }
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
      width: W, height: H, rx: 7,
      fill: `var(--${n.rev.class}-bg)`, stroke: `var(--${n.rev.class})`,
    }, grp);
    // Path above, the last segment below: the part that tells records apart
    // is never the one cut.
    const [dir, leaf] = splitKey(label(n.logical));
    if (net) {
      // Compact: the last key segment; the cluster label names the path.
      el("text", { x: 9, y: 18 }, grp).textContent = clipMiddle(leaf, 25);
      el("title", {}, grp).textContent = `${n.logical.key}\n${kindLabel(n.logical)} · r${n.rev.id} ${n.rev.summary}${n.rev.base ? " · base" : ""}${n.rev.abstract ? `\n\n${n.rev.abstract}` : ""}`;
      grp.addEventListener("click", (ev) => { ev.stopPropagation(); select(n.rev.id); });
      grp.addEventListener("mouseenter", () => hover(model, id, true));
      grp.addEventListener("mouseleave", () => hover(model, id, false));
      continue;
    }
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
  if (!state.laidOut && model.nodes.size) { fit(xy, W, H); state.laidOut = true; }
}
// ---- network view (Obsidian-like) ----------------------------------------

/// A live force simulation drawn as dots: radius from the number of
/// neighbours, colour from the status, the name under the dot (faded out
/// when zoomed out). Positions persist across refreshes, so only new nodes
/// move in; dragging a node pulls its neighbours along; at rest every dot
/// floats a little (a CSS animation, no simulation cost).
function renderNetwork(model) {
  const sim = state.sim;
  const svg = $("graph"), vp = $("viewport");
  vp.replaceChildren();
  svg.classList.add("net");
  $("empty").hidden = model.nodes.size > 0;
  const q = state.search.trim().toLowerCase();
  const visible = (n) => !state.hidden.has(n.rev.class);
  const matches = (n) => !q || n.logical.key.toLowerCase().includes(q);
  const hl = state.highlight;
  const ids = [...model.nodes.keys()];
  // Neighbours and degree.
  const near = new Map(ids.map((id) => [id, new Set()]));
  for (const e of model.edges) { near.get(e.from)?.add(e.to); near.get(e.to)?.add(e.from); }
  const radius = (id) => 6 + Math.sqrt(near.get(id).size) * 2.8;
  // Hubs: the best-connected nodes keep their names when zoomed out.
  const degrees = ids.map((id) => near.get(id).size).sort((a, b) => b - a);
  const hubMin = Math.max(3, degrees[Math.min(degrees.length - 1, Math.max(5, Math.round(ids.length * 0.12)))] ?? 3);
  // Start positions: kept ones stay; new ones near a placed neighbour, or
  // on the deterministic seed layout (spread out).
  const seed = layoutNetwork(model);
  let fresh = 0;
  for (const id of ids) {
    if (sim.pos.has(id)) continue;
    fresh++;
    const anchor = [...near.get(id)].map((n) => sim.pos.get(n)).find(Boolean);
    const s0 = seed.get(id);
    const j = fresh * 2.39996;
    sim.pos.set(id, anchor
      ? { x: anchor.x + 40 * Math.cos(j), y: anchor.y + 40 * Math.sin(j), vx: 0, vy: 0 }
      : { x: s0.x * 0.9, y: s0.y * 1.8, vx: 0, vy: 0 });
  }
  for (const id of [...sim.pos.keys()]) if (!model.nodes.has(id)) sim.pos.delete(id);
  const groupOf = new Map(ids.map((id) => [id, splitKey(label(model.nodes.get(id).logical))[0]]));
  const groupSize = new Map();
  for (const g of groupOf.values()) groupSize.set(g, (groupSize.get(g) || 0) + 1);

  // Edges first (under the dots).
  const edgeLayer = el("g", {}, vp);
  const edges = [];
  for (const e of model.edges) {
    const na = model.nodes.get(e.from), nb = model.nodes.get(e.to);
    const dim = !visible(na) || !visible(nb) || (hl && !(hl.has(e.from) && hl.has(e.to))) || (q && !(matches(na) || matches(nb)));
    const line = el("line", { class: ["edge-line", e.kind, e.disabled ? "disabled" : "", dim ? "dim" : ""].join(" "), "marker-end": "url(#arrow-net)" }, edgeLayer);
    const t = el("title", {}, line);
    t.textContent = `${e.kind}${e.disabled ? " (disabled)" : ""}${e.rule !== undefined ? ` · rule ${e.rule}` : ""}${e.old != null ? ` · old r${e.old}` : ""}`;
    edges.push({ line, from: e.from, to: e.to });
  }
  // Group names, at each group's centre (groups of three or more).
  const groupLayer = el("g", {}, vp);
  const groupEls = new Map();
  for (const [g, n] of groupSize) {
    if (!g || n < 3) continue;
    const t = el("text", { class: "group-label", "text-anchor": "middle" }, groupLayer);
    t.textContent = clip(g, 48, true);
    groupEls.set(g, t);
  }
  // Dots.
  const nodeLayer = el("g", {}, vp);
  const dots = new Map();
  let k = 0;
  for (const [id, n] of model.nodes) {
    const dim = !visible(n) || (hl && !hl.has(id)) || (q && !matches(n));
    const cls = ["node", "dot", near.get(id).size > hubMin ? "hub" : "", n.rev.class, n.rev.base ? "base" : "", !n.rev.head ? "old" : "",
      state.selected === n.rev.id ? "selected" : "", q && matches(n) ? "hit" : "", dim ? "dim" : ""].join(" ");
    const outer = el("g", { class: cls }, nodeLayer);
    // The float: an inner group drifts a few pixels, each on its own phase.
    // (A class per phase: the page's CSP allows no inline styles.)
    const bob = el("g", { class: `bob p${k++ % 6}` }, outer);
    const r = radius(id);
    el("circle", { r, fill: `var(--${n.rev.class})` }, bob);
    const t = el("text", { y: r, dy: "1.25em", "text-anchor": "middle" }, bob);
    t.textContent = clipMiddle(splitKey(label(n.logical))[1], 30);
    el("title", {}, outer).textContent = `${n.logical.key}\n${kindLabel(n.logical)} · r${n.rev.id} ${n.rev.summary}${n.rev.base ? " · base" : ""}${n.rev.abstract ? `\n\n${n.rev.abstract}` : ""}`;
    outer.addEventListener("mousedown", (ev) => startDrag(ev, id, n.rev.id));
    outer.addEventListener("mouseenter", () => hoverNet(id, near, true));
    outer.addEventListener("mouseleave", () => hoverNet(id, near, false));
    dots.set(id, { el: outer, r });
  }

  sim.dots = dots;
  sim.edges = edges;
  const draw = () => {
    for (const [id, d] of dots) {
      const p = sim.pos.get(id);
      d.el.setAttribute("transform", `translate(${p.x.toFixed(1)},${p.y.toFixed(1)})`);
    }
    for (const e of edges) {
      const a = sim.pos.get(e.from), b = sim.pos.get(e.to);
      const dx = b.x - a.x, dy = b.y - a.y, d = Math.hypot(dx, dy) || 1;
      const ra = dots.get(e.from).r + 1, rb = dots.get(e.to).r + 3;
      e.line.setAttribute("x1", (a.x + (dx / d) * ra).toFixed(1));
      e.line.setAttribute("y1", (a.y + (dy / d) * ra).toFixed(1));
      e.line.setAttribute("x2", (b.x - (dx / d) * rb).toFixed(1));
      e.line.setAttribute("y2", (b.y - (dy / d) * rb).toFixed(1));
    }
    const c = new Map();
    for (const id of ids) {
      const g = groupOf.get(id);
      if (!groupEls.has(g)) continue;
      const p = sim.pos.get(id), t = c.get(g) || { x: 0, y: 0, top: Infinity, n: 0 };
      t.x += p.x; t.n++; t.top = Math.min(t.top, p.y);
      c.set(g, t);
    }
    for (const [g, t] of c) {
      const el2 = groupEls.get(g);
      el2.setAttribute("x", (t.x / t.n).toFixed(1));
      el2.setAttribute("y", (t.top - 26).toFixed(1));
    }
  };

  // Forces, d3-style: velocity with decay, strength scaled by alpha.
  const springs = [];
  const seen = new Set();
  for (const e of model.edges) {
    const key = e.from < e.to ? `${e.from}|${e.to}` : `${e.to}|${e.from}`;
    if (e.from === e.to || seen.has(key)) continue;
    seen.add(key);
    springs.push([e.from, e.to]);
  }
  const P = ids.map((id) => sim.pos.get(id));
  const G = ids.map((id) => groupOf.get(id));
  const step = () => {
    const a = sim.alpha;
    const n = P.length;
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
        const ex = P[i].x - P[j].x, ey = P[i].y - P[j].y;
        const d2 = ex * ex + ey * ey + 1;
        if (d2 > 360000) continue;
        const f = (4200 * a) / d2;
        P[i].vx += ex * f; P[i].vy += ey * f; P[j].vx -= ex * f; P[j].vy -= ey * f;
      }
    }
    for (const [u, v] of springs) {
      const pu = sim.pos.get(u), pv = sim.pos.get(v);
      const ex = pv.x - pu.x, ey = pv.y - pu.y, d = Math.hypot(ex, ey) || 1;
      const f = ((d - 100) / d) * 0.05 * a;
      pu.vx += ex * f; pu.vy += ey * f; pv.vx -= ex * f; pv.vy -= ey * f;
    }
    const cx = new Map();
    for (let i = 0; i < n; i++) {
      const t = cx.get(G[i]) || { x: 0, y: 0, n: 0 };
      t.x += P[i].x; t.y += P[i].y; t.n++;
      cx.set(G[i], t);
    }
    for (let i = 0; i < n; i++) {
      const t = cx.get(G[i]);
      P[i].vx += ((t.x / t.n - P[i].x) * 0.012 - P[i].x * 0.005) * a;
      P[i].vy += ((t.y / t.n - P[i].y) * 0.012 - P[i].y * 0.0085) * a;
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
    draw();
    if (sim.alpha < 0.004 && !sim.drag) { sim.raf = 0; return; }
    sim.raf = requestAnimationFrame(sim.tick);
  };
  // A new graph settles mostly out of sight, then floats in; a refresh
  // with a few new nodes just warms up.
  const first = fresh === ids.length;
  sim.alpha = first ? 1 : fresh ? 0.35 : Math.max(sim.alpha, 0.05);
  if (first) for (let i = 0; i < 160; i++) step();
  draw();
  applyView();
  updateZoomClass();
  if (!state.laidOut && ids.length) { fitNet(ids, dots); state.laidOut = true; }
  if (!sim.raf) sim.raf = requestAnimationFrame(sim.tick);
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
/// Graph coordinates of a pointer event.
function toGraph(ev) {
  const r = $("graph").getBoundingClientRect();
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
function hoverNet(id, near, on) {
  const sim = state.sim;
  if (state.highlight || sim.drag || !sim.dots) return;
  $("graph").classList.toggle("hovering", on);
  for (const d of sim.dots.values()) d.el.classList.remove("near");
  for (const e of sim.edges) e.line.classList.remove("near");
  if (!on) return;
  for (const n of [id, ...near.get(id)]) sim.dots.get(n)?.el.classList.add("near");
  for (const e of sim.edges) if (e.from === id || e.to === id) e.line.classList.add("near");
}
/// Labels show once the view is close enough to read them.
/// Text keeps its size on screen at any zoom (the --inv factor; set through
/// the CSSOM, which the page's CSP allows).
function updateZoomClass() {
  const svg = $("graph");
  svg.classList.toggle("far", state.view.k < 0.55);
  svg.style.setProperty("--inv", (1 / Math.max(state.view.k, 0.25)).toFixed(3));
}
function fitNet(ids, dots) {
  const svg = $("graph").getBoundingClientRect();
  const P = ids.map((id) => state.sim.pos.get(id));
  const x0 = Math.min(...P.map((p) => p.x)) - 60, x1 = Math.max(...P.map((p) => p.x)) + 60;
  const y0 = Math.min(...P.map((p) => p.y)) - 40, y1 = Math.max(...P.map((p) => p.y)) + 40;
  const k = Math.min(1.6, Math.max(0.15, Math.min((svg.width - 40) / (x1 - x0), (svg.height - 100) / (y1 - y0))));
  state.view = { k, x: (svg.width - (x1 - x0) * k) / 2 - x0 * k, y: 70 + (svg.height - 100 - (y1 - y0) * k) / 2 - y0 * k };
  applyView();
  updateZoomClass();
  void dots;
}
function hover(model, id, on) {
  if (state.highlight) return;
  const near = new Set([id]);
  for (const e of model.edges) { if (e.from === id) near.add(e.to); if (e.to === id) near.add(e.from); }
  document.querySelectorAll("#viewport .node").forEach((n, i) => {
    const nid = [...model.nodes.keys()][i];
    n.classList.toggle("dim", on && !near.has(nid));
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
function applyView() {
  const { x, y, k } = state.view;
  $("viewport").setAttribute("transform", `translate(${x},${y}) scale(${k})`);
}

// ---- detail panel ------------------------------------------------------

async function select(rev) {
  const moved = state.selected !== rev;
  state.selected = rev;
  state.highlight = null;
  if (state.tab === "graph" && state.focus && moved) state.laidOut = false;
  renderView();
  const g = state.graph;
  const r = g.revisions.find((x) => x.id === rev);
  const l = g.logicals.find((x) => x.id === r.logical);
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
    const note = rule.premises.length ? "" : " · references only, never supports";
    line.append(html("span", "muted", `rule ${rule.id}${rule.enabled ? "" : " (disabled)"}${note}:`));
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
  const g = state.graph;
  const r = g.revisions.find((x) => x.id === rev);
  const l = r && g.logicals.find((x) => x.id === r.logical);
  const s = html("span", "link", `${prefix}${l ? label(l) : "?"}@r${rev}`);
  s.title = r ? r.summary : "";
  s.addEventListener("click", () => select(rev));
  return s;
}
function highlightRevs(revs) {
  const set = new Set();
  for (const rev of revs) {
    const r = state.graph.revisions.find((x) => x.id === rev);
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
  const keyOfLogical = (id) => { const l = g.logicals.find((x) => x.id === id); return l ? label(l) : `logical ${id}`; };
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
    state.graph = graph;
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
  const svg = $("graph");
  let drag = null;
  svg.addEventListener("mousedown", (e) => { drag = { x: e.clientX, y: e.clientY, vx: state.view.x, vy: state.view.y }; svg.classList.add("dragging"); });
  window.addEventListener("mouseup", () => { endDrag(); drag = null; svg.classList.remove("dragging"); });
  window.addEventListener("mousemove", (e) => {
    if (moveDrag(e)) return;
    if (!drag) return;
    state.view.x = drag.vx + e.clientX - drag.x;
    state.view.y = drag.vy + e.clientY - drag.y;
    applyView();
  });
  svg.addEventListener("wheel", (e) => {
    e.preventDefault();
    const r = svg.getBoundingClientRect();
    const mx = e.clientX - r.left, my = e.clientY - r.top;
    const k = Math.min(3, Math.max(0.1, state.view.k * Math.exp(-e.deltaY * 0.0015)));
    state.view.x = mx - ((mx - state.view.x) * k) / state.view.k;
    state.view.y = my - ((my - state.view.y) * k) / state.view.k;
    state.view.k = k;
    applyView();
    updateZoomClass();
  }, { passive: false });
  svg.addEventListener("click", () => { if (state.highlight) { state.highlight = null; render(); } });
  $("search").addEventListener("input", (e) => { state.search = e.target.value; renderView(); });
  $("all-revs").addEventListener("change", (e) => { state.allRevs = e.target.checked; state.laidOut = false; state.highlight = null; render(); });
  $("close").addEventListener("click", () => { $("panel").hidden = true; state.selected = null; renderView(); });
  $("tab-records").addEventListener("click", () => setTab("records"));
  $("tab-graph").addEventListener("click", () => setTab("graph"));
  $("focus").addEventListener("change", (e) => { state.focus = Number(e.target.value); state.laidOut = false; render(); });
  $("fit").addEventListener("click", () => { state.laidOut = false; render(); });
  $("layout").addEventListener("change", (e) => { state.layout = e.target.value; state.laidOut = false; render(); });
  $("fit").addEventListener("dblclick", () => { state.sim.pos.clear(); state.laidOut = false; render(); });
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
