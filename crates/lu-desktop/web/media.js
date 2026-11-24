// Pictures, music and video in replies, and galleries of them.
//
// - Files on this PC show at once (through Tauri's asset protocol).
// - Anything from the web waits for a click: a reply cannot make Lu fetch
//   an address by itself (an address can carry data out).
// - YouTube links play in a privacy-enhanced player, after a click.
const T = window.__TAURI__;

const IMG = /\.(png|jpe?g|gif|webp|bmp|svg|avif)$/i;
const AUD = /\.(mp3|wav|ogg|oga|flac|m4a|aac|opus)$/i;
const VID = /\.(mp4|webm|mov|m4v|ogv)$/i;

const make = (tag, attrs = {}, ...kids) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "text") n.textContent = v;
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
    else n.setAttribute(k, v === true ? "" : v);
  }
  for (const k of kids.flat()) if (k != null) n.append(k);
  return n;
};

export function youtubeId(url) {
  try {
    const u = new URL(url);
    const host = u.hostname.replace(/^www\.|^m\./, "");
    if (host === "youtu.be") return u.pathname.slice(1).split("/")[0] || null;
    if (host === "youtube.com" || host === "music.youtube.com") {
      if (u.pathname === "/watch") return u.searchParams.get("v");
      const m = u.pathname.match(/^\/(shorts|embed|live)\/([\w-]{6,})/);
      if (m) return m[2];
    }
  } catch {}
  return null;
}
const isLocal = (src) => /^[a-z]:[\\/]/i.test(src) || src.startsWith("file:") || src.startsWith("/") || src.startsWith("\\\\");
const localPath = (src) => (src.startsWith("file:") ? decodeURIComponent(src.replace(/^file:\/*/i, "").replace(/^([a-z]):/i, "$1:")) : src);
const kindOf = (src) => {
  const path = src.split(/[?#]/)[0];
  if (youtubeId(src)) return "youtube";
  if (AUD.test(path)) return "audio";
  if (VID.test(path)) return "video";
  if (IMG.test(path)) return "image";
  return isLocal(src) ? "file" : "image";
};

/** Whether a markdown target is something to embed (vs. a plain link). */
export function embeddable(src) {
  return youtubeId(src) != null || IMG.test(src.split(/[?#]/)[0]) || AUD.test(src.split(/[?#]/)[0]) || VID.test(src.split(/[?#]/)[0]);
}

/** One embedded item: { src, caption }. */
export function mediaItem({ src, caption }) {
  src = String(src || "").trim();
  caption = caption ? String(caption).slice(0, 200) : "";
  const fig = make("figure", { class: "media" });
  const kind = kindOf(src);
  const cap = caption ? make("figcaption", { text: caption }) : null;
  const show = (url) => {
    fig.textContent = "";
    let node;
    if (kind === "youtube") {
      node = make("iframe", { src: `https://www.youtube-nocookie.com/embed/${encodeURIComponent(youtubeId(src))}`, allow: "autoplay; encrypted-media; picture-in-picture; fullscreen", allowfullscreen: true, referrerpolicy: "strict-origin-when-cross-origin", title: caption || "Video" });
    } else if (kind === "audio") {
      node = make("audio", { controls: true, preload: "metadata", src: url });
    } else if (kind === "video") {
      node = make("video", { controls: true, preload: "metadata", src: url });
    } else if (kind === "file") {
      node = make("div", { class: "note-line", text: `Not a picture, song or video: ${src}` });
    } else {
      node = make("img", { src: url, alt: caption, loading: "lazy" });
      node.addEventListener("click", () => openViewer(url, caption));
      node.addEventListener("error", () => { fig.textContent = ""; fig.append(make("div", { class: "note-line", text: `This picture could not be shown: ${src}` })); });
    }
    node.classList.add("media-" + kind);
    fig.append(node);
    if (cap) fig.append(cap);
  };
  if (isLocal(src)) {
    if (kind === "file") { show(""); return fig; }
    try { show(T.core.convertFileSrc(localPath(src))); }
    catch { fig.append(make("div", { class: "note-line", text: src })); }
    return fig;
  }
  if (!/^https?:\/\//i.test(src)) { fig.append(make("div", { class: "note-line", text: `Not shown: ${src}` })); return fig; }
  // From the web: wait for the user, unless they chose to show web media at once.
  let auto = false;
  try { auto = localStorage.getItem("lu-web-media") === "auto"; } catch {}
  if (auto) { show(src); return fig; }
  let host = "";
  try { host = new URL(src).hostname; } catch {}
  const what = { youtube: "Play video from YouTube", audio: `Play audio from ${host}`, video: `Play video from ${host}`, image: `Show picture from ${host}` }[kind];
  fig.classList.add("media-wait");
  fig.append(make("button", { class: "media-load", onclick: () => show(src), title: src }, make("span", { class: "media-icon", text: kind === "image" ? "🖼" : kind === "audio" ? "♪" : "▶" }), make("span", { text: what })));
  if (cap) fig.append(cap);
  return fig;
}

/** A gallery: { layout: "grid" | "row" | "column", columns, items: [{src, caption}] }. */
export function parseGallery(text) {
  let g;
  try { g = JSON.parse(text); } catch { throw new Error("the gallery is not valid JSON"); }
  const items = (Array.isArray(g) ? g : g?.items || []).slice(0, 60).filter((x) => x && (typeof x === "string" || x.src)).map((x) => (typeof x === "string" ? { src: x } : x));
  if (!items.length) throw new Error("the gallery has no items");
  const layout = ["grid", "row", "column"].includes(g?.layout) ? g.layout : "grid";
  const columns = Math.min(Math.max(parseInt(g?.columns) || Math.min(items.length, 3), 1), 6);
  return { title: g?.title ? String(g.title).slice(0, 160) : "", layout, columns, items };
}
export function drawGallery(g) {
  const box = make("div", { class: "gallery gallery-" + g.layout });
  if (g.title) box.append(make("div", { class: "chart-title", text: g.title }));
  const grid = make("div", { class: "gallery-items", style: g.layout === "grid" ? `grid-template-columns: repeat(${g.columns}, minmax(0, 1fr))` : "" });
  for (const it of g.items) grid.append(mediaItem(it));
  box.append(grid);
  return box;
}

// A larger view of a picture; click or Esc closes it.
function openViewer(url, caption) {
  const shade = make("div", { class: "viewer", onclick: () => shade.remove() }, make("img", { src: url, alt: caption || "" }), caption ? make("div", { class: "viewer-cap", text: caption }) : null);
  const esc = (e) => { if (e.key === "Escape") { shade.remove(); removeEventListener("keydown", esc); } };
  addEventListener("keydown", esc);
  document.body.append(shade);
}
