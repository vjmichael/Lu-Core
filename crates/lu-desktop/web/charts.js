// Charts the model asks for in a ```chart block, drawn by Lu as SVG.
// The model only supplies data (JSON); nothing it writes is run or
// inserted as markup. All text goes in as text nodes.
//
// {"type": "bar" | "line" | "area" | "pie" | "scatter",
//  "title": "…", "x": "axis label", "y": "axis label",
//  "labels": ["Jan", "Feb", …],                  // bar, line, area, pie
//  "series": [{"name": "Sales", "data": [3, 5, …]}]}   // scatter: data [[x, y], …]

const NS = "http://www.w3.org/2000/svg";
const svg = (tag, attrs = {}, text) => {
  const n = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
  if (text != null) n.textContent = String(text);
  return n;
};
const div = (cls, text) => {
  const n = document.createElement("div");
  if (cls) n.className = cls;
  if (text != null) n.textContent = String(text);
  return n;
};
const color = (i) => `var(--c${(i % 6) + 1})`;
const fmt = (v) => {
  if (typeof v !== "number" || !isFinite(v)) return String(v);
  const a = Math.abs(v);
  if (a >= 1e9) return +(v / 1e9).toFixed(1) + "B";
  if (a >= 1e6) return +(v / 1e6).toFixed(1) + "M";
  if (a >= 1e4) return +(v / 1e3).toFixed(1) + "k";
  return Number.isInteger(v) ? v.toLocaleString() : +v.toFixed(2) + "";
};

/** Check the spec; returns a cleaned copy or throws a plain message. */
export function parseChart(text) {
  let s;
  try { s = JSON.parse(text); } catch { throw new Error("the chart data is not valid JSON"); }
  if (!s || typeof s !== "object") throw new Error("the chart data is empty");
  const type = String(s.type || "bar").toLowerCase();
  if (!["bar", "line", "area", "pie", "scatter"].includes(type)) throw new Error(`unknown chart type “${type}”`);
  const series = (Array.isArray(s.series) ? s.series : s.data ? [{ name: s.y || "", data: s.data }] : []).slice(0, 12).map((x, i) => ({
    name: String(x?.name ?? `Series ${i + 1}`).slice(0, 80),
    data: (Array.isArray(x?.data) ? x.data : []).slice(0, 2000),
  }));
  if (!series.length || !series.some((x) => x.data.length)) throw new Error("the chart has no data");
  const labels = (Array.isArray(s.labels) ? s.labels : []).slice(0, 2000).map((l) => String(l).slice(0, 60));
  return { type, title: s.title ? String(s.title).slice(0, 160) : "", x: s.x ? String(s.x).slice(0, 80) : "", y: s.y ? String(s.y).slice(0, 80) : "", labels, series };
}

/** A chart element for a cleaned spec. */
export function drawChart(spec) {
  const box = div("chart");
  if (spec.title) box.append(div("chart-title", spec.title));
  const hidden = new Set();
  const stage = div("chart-stage");
  const tip = div("chart-tip hidden");
  box.append(stage, tip);
  const legend = div("chart-legend");
  const redraw = () => {
    stage.textContent = "";
    stage.append(spec.type === "pie" ? pie(spec, tip) : spec.type === "scatter" ? scatter(spec, hidden, tip) : cartesian(spec, hidden, tip));
  };
  // Legend: click a series to hide or show it.
  const names = spec.type === "pie" ? spec.labels.length ? spec.labels : spec.series[0].data.map((_, i) => `#${i + 1}`) : spec.series.map((s) => s.name);
  if (names.length > 1 || spec.type === "pie") {
    names.forEach((n, i) => {
      const item = document.createElement("button");
      item.className = "chart-key";
      item.append(Object.assign(document.createElement("i"), { style: `background:${color(i)}` }), document.createTextNode(n));
      if (spec.type !== "pie") item.onclick = () => { hidden.has(i) ? hidden.delete(i) : hidden.add(i); item.classList.toggle("off", hidden.has(i)); redraw(); };
      legend.append(item);
    });
    box.append(legend);
  }
  // A plain table of the numbers, for reading exact values.
  const more = document.createElement("details");
  more.className = "chart-data";
  more.append(Object.assign(document.createElement("summary"), { textContent: "Data" }), dataTable(spec));
  box.append(more);
  redraw();
  return box;
}

const W = 640, H = 300, L = 56, R = 16, T = 14, B = 44;

function ticks(min, max, n = 5) {
  if (min === max) { min -= 1; max += 1; }
  const step0 = (max - min) / n, mag = 10 ** Math.floor(Math.log10(step0));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= step0) || step0;
  const lo = Math.floor(min / step) * step, hi = Math.ceil(max / step) * step;
  const out = [];
  for (let v = lo; v <= hi + step / 2; v += step) out.push(+v.toFixed(10));
  return out;
}
function frame(g, yt, y, xLabel, yLabel) {
  for (const v of yt) {
    g.append(svg("line", { x1: L, x2: W - R, y1: y(v), y2: y(v), class: "grid" }));
    g.append(svg("text", { x: L - 8, y: y(v) + 4, "text-anchor": "end", class: "tick" }, fmt(v)));
  }
  if (xLabel) g.append(svg("text", { x: (L + W - R) / 2, y: H - 4, "text-anchor": "middle", class: "axis" }, xLabel));
  if (yLabel) g.append(svg("text", { x: 12, y: (T + H - B) / 2, "text-anchor": "middle", transform: `rotate(-90 12 ${(T + H - B) / 2})`, class: "axis" }, yLabel));
}
function showTip(tip, evt, lines) {
  tip.textContent = "";
  lines.forEach((l, i) => tip.append(div(i ? "" : "h", l)));
  tip.classList.remove("hidden");
  const r = tip.parentElement.getBoundingClientRect();
  tip.style.left = Math.min(evt.clientX - r.left + 12, r.width - tip.offsetWidth - 4) + "px";
  tip.style.top = evt.clientY - r.top + 12 + "px";
}
const hideTip = (tip) => tip.classList.add("hidden");
const num = (v) => (typeof v === "number" ? v : parseFloat(v));

function cartesian(spec, hidden, tip) {
  const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" });
  const shown = spec.series.map((s, i) => ({ ...s, i })).filter((s) => !hidden.has(s.i));
  const n = Math.max(spec.labels.length, ...spec.series.map((s) => s.data.length));
  const vals = shown.flatMap((s) => s.data.map(num)).filter(isFinite);
  const yt = ticks(Math.min(0, ...vals), Math.max(0, ...vals, 1));
  const y = (v) => T + (H - T - B) * (1 - (v - yt[0]) / (yt.at(-1) - yt[0]));
  const band = (W - L - R) / Math.max(n, 1);
  const xc = (j) => L + band * (j + 0.5);
  frame(root, yt, y, spec.x, spec.y);
  const every = Math.ceil(n / 12);
  for (let j = 0; j < n; j += every) root.append(svg("text", { x: xc(j), y: H - B + 16, "text-anchor": "middle", class: "tick" }, (spec.labels[j] ?? j + 1).toString().slice(0, 14)));
  if (spec.type === "bar") {
    const w = (band * 0.78) / Math.max(shown.length, 1);
    shown.forEach((s, k) => s.data.forEach((v, j) => {
      v = num(v); if (!isFinite(v)) return;
      const r = svg("rect", { x: xc(j) - band * 0.39 + k * w, y: Math.min(y(v), y(0)), width: Math.max(w - 2, 1), height: Math.max(Math.abs(y(v) - y(0)), 1), rx: 3, fill: color(s.i), class: "mark" });
      r.onmousemove = (e) => showTip(tip, e, [spec.labels[j] ?? `#${j + 1}`, `${s.name}: ${fmt(v)}`]);
      r.onmouseleave = () => hideTip(tip);
      root.append(r);
    }));
  } else {
    for (const s of shown) {
      const pts = s.data.map((v, j) => [xc(j), y(num(v)), num(v), j]).filter((p) => isFinite(p[2]));
      if (!pts.length) continue;
      const d = pts.map((p, k) => (k ? "L" : "M") + p[0].toFixed(1) + " " + p[1].toFixed(1)).join(" ");
      if (spec.type === "area") root.append(svg("path", { d: `${d} L${pts.at(-1)[0]} ${y(Math.max(0, yt[0]))} L${pts[0][0]} ${y(Math.max(0, yt[0]))} Z`, fill: color(s.i), class: "area" }));
      root.append(svg("path", { d, fill: "none", stroke: color(s.i), "stroke-width": 2.25, "stroke-linejoin": "round", "stroke-linecap": "round" }));
      for (const [px, py, v, j] of pts) {
        const c = svg("circle", { cx: px, cy: py, r: pts.length > 60 ? 2 : 3.5, fill: color(s.i), class: "mark" });
        c.onmousemove = (e) => showTip(tip, e, [spec.labels[j] ?? `#${j + 1}`, `${s.name}: ${fmt(v)}`]);
        c.onmouseleave = () => hideTip(tip);
        root.append(c);
      }
    }
  }
  return root;
}

function scatter(spec, hidden, tip) {
  const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" });
  const shown = spec.series.map((s, i) => ({ ...s, i })).filter((s) => !hidden.has(s.i));
  const pts = shown.flatMap((s) => s.data.filter((p) => Array.isArray(p)).map((p) => [num(p[0]), num(p[1]), s])).filter((p) => isFinite(p[0]) && isFinite(p[1]));
  const xt = ticks(Math.min(...pts.map((p) => p[0]), 0), Math.max(...pts.map((p) => p[0]), 1));
  const yt = ticks(Math.min(...pts.map((p) => p[1]), 0), Math.max(...pts.map((p) => p[1]), 1));
  const x = (v) => L + (W - L - R) * ((v - xt[0]) / (xt.at(-1) - xt[0]));
  const y = (v) => T + (H - T - B) * (1 - (v - yt[0]) / (yt.at(-1) - yt[0]));
  frame(root, yt, y, spec.x, spec.y);
  for (const v of xt) root.append(svg("text", { x: x(v), y: H - B + 16, "text-anchor": "middle", class: "tick" }, fmt(v)));
  for (const [px, py, s] of pts) {
    const c = svg("circle", { cx: x(px), cy: y(py), r: 4, fill: color(s.i), class: "mark dot" });
    c.onmousemove = (e) => showTip(tip, e, [s.name, `${spec.x || "x"}: ${fmt(px)}`, `${spec.y || "y"}: ${fmt(py)}`]);
    c.onmouseleave = () => hideTip(tip);
    root.append(c);
  }
  return root;
}

function pie(spec, tip) {
  const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" });
  const data = spec.series[0].data.map(num).map((v) => (isFinite(v) && v > 0 ? v : 0));
  const total = data.reduce((a, b) => a + b, 0) || 1;
  const cx = W / 2, cy = H / 2, r = H / 2 - 16, r0 = r * 0.58;
  let a = -Math.PI / 2;
  data.forEach((v, i) => {
    if (!v) return;
    const b = a + (v / total) * Math.PI * 2;
    const big = b - a > Math.PI ? 1 : 0;
    const p = (ang, rad) => `${cx + rad * Math.cos(ang)} ${cy + rad * Math.sin(ang)}`;
    const d = v === total
      ? `M${p(0, r)} A${r} ${r} 0 1 1 ${p(Math.PI, r)} A${r} ${r} 0 1 1 ${p(0, r)} M${p(0, r0)} A${r0} ${r0} 0 1 0 ${p(Math.PI, r0)} A${r0} ${r0} 0 1 0 ${p(0, r0)} Z`
      : `M${p(a, r)} A${r} ${r} 0 ${big} 1 ${p(b, r)} L${p(b, r0)} A${r0} ${r0} 0 ${big} 0 ${p(a, r0)} Z`;
    const seg = svg("path", { d, fill: color(i), class: "mark slice", "fill-rule": "evenodd" });
    const label = spec.labels[i] ?? `#${i + 1}`;
    seg.onmousemove = (e) => showTip(tip, e, [label, `${fmt(v)} · ${((v / total) * 100).toFixed(1)}%`]);
    seg.onmouseleave = () => hideTip(tip);
    root.append(seg);
    a = b;
  });
  root.append(svg("text", { x: cx, y: cy + 6, "text-anchor": "middle", class: "total" }, fmt(total)));
  return root;
}

function dataTable(spec) {
  const t = document.createElement("table");
  t.className = "grid";
  const row = (cells, head) => {
    const tr = document.createElement("tr");
    for (const c of cells) tr.append(Object.assign(document.createElement(head ? "th" : "td"), { textContent: c }));
    return tr;
  };
  if (spec.type === "scatter") {
    t.append(row(["Series", spec.x || "x", spec.y || "y"], true));
    for (const s of spec.series) for (const p of s.data) if (Array.isArray(p)) t.append(row([s.name, fmt(num(p[0])), fmt(num(p[1]))]));
  } else {
    t.append(row(["", ...spec.series.map((s) => s.name)], true));
    const n = Math.max(spec.labels.length, ...spec.series.map((s) => s.data.length));
    for (let j = 0; j < Math.min(n, 500); j++) t.append(row([spec.labels[j] ?? `#${j + 1}`, ...spec.series.map((s) => fmt(num(s.data[j])))]));
  }
  return t;
}

/** A markdown table as a sortable table (click a header to sort). */
export function drawTable(rows) {
  const split = (l) => l.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
  const head = split(rows[0]);
  const body = rows.slice(2).map(split);
  const wrap = div("md-table");
  const t = document.createElement("table");
  t.className = "grid";
  wrap.append(t);
  let sortCol = -1, dir = 1;
  const paint = (inline) => {
    t.textContent = "";
    const tr = document.createElement("tr");
    head.forEach((h, i) => {
      const th = document.createElement("th");
      inline(h, th);
      if (i === sortCol) th.append(dir > 0 ? " ▲" : " ▼");
      th.onclick = () => { dir = sortCol === i ? -dir : 1; sortCol = i; paint(inline); };
      tr.append(th);
    });
    t.append(tr);
    const rowsSorted = sortCol < 0 ? body : [...body].sort((a, b) => {
      const x = a[sortCol] ?? "", y = b[sortCol] ?? "";
      const nx = parseFloat(x.replace(/[,$%]/g, "")), ny = parseFloat(y.replace(/[,$%]/g, ""));
      return (isFinite(nx) && isFinite(ny) ? nx - ny : x.localeCompare(y)) * dir;
    });
    for (const r of rowsSorted) {
      const tr2 = document.createElement("tr");
      head.forEach((_, i) => { const td = document.createElement("td"); inline(r[i] ?? "", td); tr2.append(td); });
      t.append(tr2);
    }
  };
  return { node: wrap, paint };
}
