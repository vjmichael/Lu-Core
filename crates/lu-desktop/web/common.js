import { parseChart, drawChart, drawTable } from "./charts.js";
import { mediaItem, embeddable, youtubeId, parseGallery, drawGallery } from "./media.js";
// Shared by Lu's windows: commands, events, theme, menus, dialogs, and a
// safe formatter for replies (text nodes only; model output is never HTML).

const T = window.__TAURI__;
export const call = (cmd, args = {}) => T.core.invoke(cmd, args);
export const on = (event, fn) => T.event.listen(event, (e) => fn(e.payload));

// ---------- theme: light, dark, or follow Windows ----------
const media = matchMedia("(prefers-color-scheme: dark)");
export function themeChoice() {
  try { return localStorage.getItem("lu-theme") || "light"; } catch { return "light"; }
}
export function applyTheme() {
  const c = themeChoice();
  const dark = c === "dark" || (c === "system" && media.matches);
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}
export function setTheme(c) {
  try { localStorage.setItem("lu-theme", c); } catch {}
  applyTheme();
}
applyTheme();
media.addEventListener("change", applyTheme);
addEventListener("storage", (e) => { if (e.key === "lu-theme") applyTheme(); });

// ---------- small DOM helpers ----------
export function el(tag, attrs = {}, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") n.className = v;
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
    else if (k === "text") n.textContent = v;
    else n.setAttribute(k, v === true ? "" : v);
  }
  for (const k of kids.flat()) if (k != null) n.append(k.nodeType ? k : document.createTextNode(String(k)));
  return n;
}
export const $ = (s, r = document) => r.querySelector(s);

export function bytes(n) {
  if (!n) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(u.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  return `${(n / 1024 ** i).toFixed(i >= 3 ? 1 : 0)} ${u[i]}`;
}
export function when(iso) {
  const d = new Date(iso.endsWith("Z") ? iso : iso + "Z");
  const days = (Date.now() - d) / 864e5;
  if (days < 1 && new Date().getDate() === d.getDate()) return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (days < 7) return d.toLocaleDateString([], { weekday: "short" });
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}

// ---------- menus, dialogs, notices ----------
let openMenu = null;
export function menu(x, y, items) {
  closeMenu();
  const m = el("div", { class: "menu" });
  for (const it of items) {
    if (it === "-") { m.append(el("hr")); continue; }
    m.append(el("button", { class: it.danger ? "danger" : null, onclick: () => { closeMenu(); it.run(); } }, it.label));
  }
  document.body.append(m);
  const r = m.getBoundingClientRect();
  m.style.left = Math.min(x, innerWidth - r.width - 8) + "px";
  m.style.top = Math.min(y, innerHeight - r.height - 8) + "px";
  openMenu = m;
  setTimeout(() => addEventListener("click", closeMenu, { once: true }), 0);
}
export function closeMenu() { if (openMenu) { openMenu.remove(); openMenu = null; } }
addEventListener("keydown", (e) => { if (e.key === "Escape") closeMenu(); });

function dialog(build) {
  return new Promise((resolve) => {
    const veil = el("div", { class: "veil" });
    const box = el("div", { class: "panel lit dialog" });
    const done = (v) => { veil.remove(); resolve(v); };
    build(box, done);
    veil.append(box);
    veil.addEventListener("mousedown", (e) => { if (e.target === veil) done(null); });
    box.addEventListener("keydown", (e) => { if (e.key === "Escape") done(null); });
    document.body.append(veil);
    (box.querySelector("input,button.primary,button.danger-main") || box.querySelector("button"))?.focus();
  });
}
export function confirmBox(title, text, yes = "Delete", danger = true) {
  return dialog((box, done) => {
    box.append(el("h3", { text: title }), el("p", { text }),
      el("div", { class: "row" },
        el("button", { class: "btn", onclick: () => done(false) }, "Cancel"),
        el("button", { class: danger ? "btn amber danger-main" : "btn primary", onclick: () => done(true) }, yes)));
  });
}
export function choiceBox(title, text, choices) {
  return dialog((box, done) => {
    box.append(el("h3", { text: title }), el("p", { text }),
      el("div", { class: "row" },
        el("button", { class: "btn", onclick: () => done(null) }, "Cancel"),
        ...choices.map((c) => el("button", { class: c.primary ? "btn amber danger-main" : "btn", onclick: () => done(c.value) }, c.label))));
  });
}
export function promptBox(title, value = "", placeholder = "", yes = "Save") {
  return dialog((box, done) => {
    const input = el("input", { class: "input", value, placeholder });
    input.addEventListener("keydown", (e) => { if (e.key === "Enter") done(input.value.trim() || null); });
    box.append(el("h3", { text: title }), input,
      el("div", { class: "row" },
        el("button", { class: "btn", onclick: () => done(null) }, "Cancel"),
        el("button", { class: "btn primary", onclick: () => done(input.value.trim() || null) }, yes)));
    setTimeout(() => { input.focus(); input.select(); }, 0);
  });
}
export function toast(text, ms = 4200) {
  const t = el("div", { class: "toast", text });
  document.body.append(t);
  setTimeout(() => t.remove(), ms);
}

// ---------- replies: a small Markdown subset, as DOM nodes ----------
function inline(text, parent) {
  // `code`, **bold**, *italic*, [links](https://…) and bare https:// links;
  // everything else is plain text.
  const re = /(`[^`\n]+`|\*\*[^*\n]+\*\*|\*[^*\n]+\*|\[[^\]\n]+\]\(https?:\/\/[^)\s]+\)|https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"\]])/g;
  let last = 0, m;
  while ((m = re.exec(text))) {
    if (m.index > last) parent.append(text.slice(last, m.index));
    const t = m[0];
    if (t.startsWith("[")) { const m = t.match(/^\[([^\]]+)\]\((.+)\)$/); parent.append(link(m[1], m[2])); }
    else if (/^https?:/.test(t)) parent.append(link(t, t));
    else if (t.startsWith("`")) parent.append(el("code", { text: t.slice(1, -1) }));
    else if (t.startsWith("**")) parent.append(el("strong", { text: t.slice(2, -2) }));
    else parent.append(el("em", { text: t.slice(1, -1) }));
    last = m.index + t.length;
  }
  if (last < text.length) parent.append(text.slice(last));
}
// Links open in the user's own browser, never inside Lu.
function link(text, url) {
  const a = el("a", { class: "link", href: url, title: url, text });
  a.addEventListener("click", (e) => { e.preventDefault(); call("open_link", { url }).catch((err) => toast(String(err))); });
  return a;
}
// "![caption](src)" lines (one or more images on a line, or a bare YouTube /
// media address on its own line): the items to embed, or null.
function mediaLine(line) {
  const t = line.trim();
  const imgs = [...t.matchAll(/!\[([^\]]*)\]\(([^)\s]+)\)/g)];
  if (imgs.length && !t.replace(/!\[[^\]]*\]\([^)\s]+\)/g, "").trim()) return imgs.map((m) => ({ src: m[2], caption: m[1] }));
  if (/^(https?:\/\/|[a-z]:[\\/])\S+$/i.test(t) && (youtubeId(t) || embeddable(t))) return [{ src: t }];
  return null;
}

export function renderReply(text, into) {
  into.textContent = "";
  const lines = text.split("\n");
  let i = 0, list = null, para = [];
  const flush = () => {
    if (para.length) { const p = el("p"); inline(para.join("\n"), p); into.append(p); para = []; }
  };
  while (i < lines.length) {
    const line = lines[i];
    if (line.startsWith("```")) {
      flush(); list = null;
      const lang = line.slice(3).trim().toLowerCase();
      const code = [];
      i++;
      while (i < lines.length && !lines[i].startsWith("```")) code.push(lines[i++]);
      const closed = i < lines.length;
      i++;
      if (lang === "gallery") {
        if (!closed) { into.append(el("div", { class: "chart chart-wait", text: "Putting pictures together…" })); continue; }
        try { into.append(drawGallery(parseGallery(code.join("\n")))); }
        catch (e) { into.append(el("div", { class: "note-line", text: `This gallery could not be shown: ${e.message}.` })); }
        continue;
      }
      // Chart data in a plain or json block counts too (models drop the word).
      let isChart = lang === "chart";
      if (!isChart && closed && (lang === "" || lang === "json")) {
        try { const o = JSON.parse(code.join("\n")); isChart = !!o && typeof o === "object" && !Array.isArray(o) && Array.isArray(o.series) && ["bar", "line", "area", "pie", "scatter"].includes(String(o.type).toLowerCase()); } catch {}
      }
      if (isChart) {
        // Still streaming: a placeholder, not half-written JSON.
        if (!closed) { into.append(el("div", { class: "chart chart-wait", text: "Drawing a chart…" })); continue; }
        try { into.append(drawChart(parseChart(code.join("\n")))); }
        catch (e) {
          into.append(el("div", { class: "note-line", text: `This chart could not be drawn: ${e.message}.` }));
          into.append(el("pre", {}, el("code", { text: code.join("\n") })));
        }
        continue;
      }
      into.append(el("pre", {}, el("code", { text: code.join("\n") })));
      continue;
    }
    // A markdown table: a header row, a |---| row, then rows.
    if (/^\s*\|.*\|\s*$/.test(line) && /^\s*\|?\s*:?-{2,}/.test(lines[i + 1] || "")) {
      flush(); list = null;
      const rows = [];
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) rows.push(lines[i++]);
      const t = drawTable(rows);
      t.paint(inline);
      into.append(t.node);
      continue;
    }
    // Pictures, music and video on their own lines; several on one line make a row.
    const media = mediaLine(line);
    if (media) {
      flush(); list = null;
      const items = [...media];
      into.append(items.length === 1 ? mediaItem(items[0]) : drawGallery({ title: "", layout: "row", columns: items.length, items }));
      i++;
      continue;
    }
    const h = line.match(/^(#{1,3})\s+(.*)/);
    const li = line.match(/^\s*([-*]|\d+\.)\s+(.*)/);
    if (h) { flush(); list = null; const n = el("h" + h[1].length); inline(h[2], n); into.append(n); }
    else if (li) {
      flush();
      const ordered = /\d/.test(li[1]);
      if (!list || (list.tagName === "OL") !== ordered) { list = el(ordered ? "ol" : "ul"); into.append(list); }
      const item = el("li"); inline(li[2], item); list.append(item);
    } else if (!line.trim()) { flush(); list = null; }
    else { list = null; para.push(line); }
    i++;
  }
  flush();
}
