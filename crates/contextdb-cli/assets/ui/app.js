// ctx ui front end: a layered view of the justification DAG (premises above
// their conclusions), coloured by derived state, refreshed when the op_log
// changes. Read-only: every request is a GET.
"use strict";

const SVGNS = "http://www.w3.org/2000/svg";
const NODE_W = 196, NODE_H = 46, GAP_X = 36, GAP_Y = 78;
const CLASSES = ["certified", "stale", "pending", "conflicted", "unavailable"];
const KINDS = ["pin", "cur", "sem", "ref"];

const state = {
  graph: null, timeline: null, fingerprint: null,
  selected: null, allRevs: false, search: "", hidden: new Set(),
  view: { x: 40, y: 40, k: 1 }, highlight: null, tab: "graph", laidOut: false,
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

async function getJSON(url) {
  const r = await fetch(url, { cache: "no-store" });
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
    for (const p of rule.premises) {
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
  return { nodes, edges };
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
const keyOf = (model, id) => model.nodes.get(id)?.logical?.key || "";

// ---- rendering ---------------------------------------------------------

function render() {
  const g = state.graph;
  if (!g) return;
  const model = build(g);
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
    const key = n.logical.key.length > 28 ? n.logical.key.slice(0, 27) + "…" : n.logical.key;
    el("text", { x: 10, y: 19 }, grp).textContent = key;
    el("text", { x: 10, y: 36, class: "sub" }, grp).textContent =
      `${n.logical.kind} · r${n.rev.id}${n.rev.head ? "" : " (old)"} · ${n.rev.summary.split("(")[0]}`;
    el("title", {}, grp).textContent = `${n.logical.key}\nr${n.rev.id} ${n.rev.summary}${n.rev.base ? " · base" : ""}`;
    grp.addEventListener("click", (ev) => { ev.stopPropagation(); select(n.rev.id); });
    grp.addEventListener("mouseenter", () => hover(model, id, true));
    grp.addEventListener("mouseleave", () => hover(model, id, false));
  }
  applyView();
  if (!state.laidOut && model.nodes.size) { fit(xy); state.laidOut = true; }
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
function fit(xy) {
  const svg = $("graph").getBoundingClientRect();
  const xs = [...xy.values()];
  const w = Math.max(...xs.map((p) => p.x)) + NODE_W, h = Math.max(...xs.map((p) => p.y)) + NODE_H;
  const k = Math.min(1.2, Math.max(0.2, Math.min((svg.width - 60) / w, (svg.height - 60) / h)));
  state.view = { k, x: (svg.width - w * k) / 2, y: 30 };
  applyView();
}
function applyView() {
  const { x, y, k } = state.view;
  $("viewport").setAttribute("transform", `translate(${x},${y}) scale(${k})`);
}

// ---- detail panel ------------------------------------------------------

async function select(rev) {
  state.selected = rev;
  state.highlight = null;
  render();
  const g = state.graph;
  const r = g.revisions.find((x) => x.id === rev);
  const l = g.logicals.find((x) => x.id === r.logical);
  const box = $("detail");
  box.replaceChildren();
  $("panel").hidden = false;
  box.append(html("h2", "", l.key));
  const badge = html("span", `badge ${r.class}`, r.summary);
  box.append(badge);
  const kv = html("dl", "kv");
  const row = (k, v) => { kv.append(html("dt", "", k)); const dd = html("dd"); dd.append(v); kv.append(dd); };
  row("kind", l.kind);
  row("scope", l.lane);
  row("revision", `r${r.id}${r.head ? " (head)" : ` (head is r${l.head})`}`);
  row("base", r.base ? "yes" : "no");
  row("layers", `L0 ${r.l0 ? "✓" : "✗"} · L1 ${r.l1 ? "✓" : "✗"} · L2 ${r.l2 ? "✓" : "✗"}`);
  row("support", String(r.support));
  box.append(kv);
  let d;
  try { d = await getJSON(`/api/rev?id=${rev}`); } catch (e) { showError(e); return; }
  if (state.selected !== rev) return;
  box.append(html("h3", "", "Body"), html("pre", d.body === undefined ? "muted" : "", d.body ?? `(${d.body_error || "unavailable"})`));
  const premises = g.rules.filter((x) => x.conclusion === rev);
  box.append(html("h3", "", `Depends on (${premises.length} rule${premises.length === 1 ? "" : "s"})`));
  if (!premises.length) box.append(html("div", "muted", r.base ? "base (no rule needed)" : "no rules"));
  for (const rule of premises) {
    const line = html("div", "list");
    line.append(html("span", "muted", `rule ${rule.id}${rule.enabled ? "" : " (disabled)"}:`));
    for (const p of rule.premises) line.append(revLink(p.rev, `${p.kind} → `));
    box.append(line);
  }
  const dependents = g.rules.filter((x) => x.premises.some((p) => p.rev === rev));
  box.append(html("h3", "", `Used by (${dependents.length})`));
  const used = html("div", "list");
  for (const rule of dependents) used.append(revLink(rule.conclusion, `${rule.premises.find((p) => p.rev === rev).kind} ← `));
  box.append(used);
  const impact = [...d.impact.supported, ...d.impact.reference_only];
  box.append(html("h3", "", `Impact (${impact.length})`));
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
  box.append(html("h3", "", "Why"), html("pre", "", d.why));
  if (r.class !== "certified") box.append(html("h3", "", "Why not"), html("pre", "", d.why_not));
  box.append(html("h3", "", "Content hash"), html("pre", "", d.content));
}
function revLink(rev, prefix) {
  const g = state.graph;
  const r = g.revisions.find((x) => x.id === rev);
  const l = r && g.logicals.find((x) => x.id === r.logical);
  const s = html("span", "link", `${prefix}${l ? l.key : "?"}@r${rev}`);
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
  render();
}

// ---- timeline ----------------------------------------------------------

function renderTimeline() {
  const t = state.timeline, g = state.graph;
  const box = $("timeline-view");
  box.replaceChildren();
  if (!t || !g) return;
  const keyOfLogical = (id) => g.logicals.find((l) => l.id === id)?.key ?? `logical ${id}`;
  for (const c of t.commits) {
    const card = html("div", "commit");
    card.append(html("div", "meta", `epoch ${c.epoch} · ${c.channel} · principal ${c.principal} · ${c.status_diff.length} status change(s)`));
    card.append(html("div", "ops", c.ops.join(", ")));
    for (const m of c.head_moves) {
      const line = html("div", "list");
      line.append(html("span", "", `${keyOfLogical(m.logical)}: ${m.from === null ? "created" : `r${m.from}`} → `));
      line.append(revLink(m.to, ""));
      card.append(line);
    }
    if (c.status_diff.length) {
      const s = html("span", "link", "show changed nodes");
      s.addEventListener("click", () => { setTab("graph"); highlightRevs(c.status_diff); });
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
  $("epoch").textContent = `epoch ${g.epoch} · ${g.logicals.length} logicals · ${g.revisions.length} revisions · ${g.rules.length} rules`;
  const chips = $("chips");
  chips.replaceChildren();
  for (const c of CLASSES) {
    const chip = html("button", `chip${state.hidden.has(c) ? " off" : ""}`);
    chip.type = "button";
    const dot = html("span", "dot");
    dot.style.background = `var(--${c})`;
    chip.append(dot, document.createTextNode(`${c} ${counts[c]}`));
    chip.addEventListener("click", () => { state.hidden.has(c) ? state.hidden.delete(c) : state.hidden.add(c); renderChrome(); render(); });
    chips.append(chip);
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
  legend.append(html("div", "muted", "thick border = base"), html("div", "muted", "dashed box = old revision"));
}
function setTab(tab) {
  state.tab = tab;
  $("tab-graph").classList.toggle("active", tab === "graph");
  $("tab-timeline").classList.toggle("active", tab === "timeline");
  $("graph-view").hidden = tab !== "graph";
  $("timeline-view").hidden = tab !== "timeline";
  if (tab === "timeline") renderTimeline();
}

async function refresh(force) {
  try {
    const { fingerprint } = await getJSON("/api/epoch");
    if (!force && fingerprint === state.fingerprint) return;
    const [graph, timeline] = await Promise.all([getJSON("/api/graph"), getJSON("/api/timeline?limit=300")]);
    state.fingerprint = fingerprint;
    state.graph = graph;
    state.timeline = timeline;
    showError(null);
    renderChrome();
    render();
    if (state.tab === "timeline") renderTimeline();
    if (state.selected !== null && graph.revisions.some((r) => r.id === state.selected)) select(state.selected);
  } catch (e) {
    showError(e);
  }
}

function wire() {
  const svg = $("graph");
  let drag = null;
  svg.addEventListener("mousedown", (e) => { drag = { x: e.clientX, y: e.clientY, vx: state.view.x, vy: state.view.y }; svg.classList.add("dragging"); });
  window.addEventListener("mouseup", () => { drag = null; svg.classList.remove("dragging"); });
  window.addEventListener("mousemove", (e) => {
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
  }, { passive: false });
  svg.addEventListener("click", () => { if (state.highlight) { state.highlight = null; render(); } });
  $("search").addEventListener("input", (e) => { state.search = e.target.value; render(); });
  $("all-revs").addEventListener("change", (e) => { state.allRevs = e.target.checked; state.laidOut = false; state.highlight = null; render(); });
  $("close").addEventListener("click", () => { $("panel").hidden = true; state.selected = null; render(); });
  $("tab-graph").addEventListener("click", () => setTab("graph"));
  $("tab-timeline").addEventListener("click", () => setTab("timeline"));
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { state.highlight = null; $("panel").hidden = true; state.selected = null; render(); }
  });
}

wire();
refresh(true);
setInterval(() => refresh(false), 2000);
