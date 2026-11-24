import { call, on, el, $, bytes, toast, confirmBox } from "./common.js";

const page = $("#page");
let tab = location.hash.slice(1) || "mine";
let selected = null;
const downloads = new Map(); // "repo/first file" -> {done,total,file,error,finished}
const dkey = (repo, first) => `${repo}/${first}`;
const installs = new Map();  // engine id -> {stage,done,total,error,finished}

function show(t) {
  tab = t;
  for (const b of document.querySelectorAll(".tab")) b.classList.toggle("on", b.dataset.tab === t);
  ({ mine, discover, imports, engines, downloads: history })[t === "import" ? "imports" : t]();
}
for (const b of document.querySelectorAll(".tab")) b.addEventListener("click", () => show(b.dataset.tab));
on("open-tab", (t) => show(t));

function head(title, ...right) {
  return el("div", { class: "head" }, el("h2", { text: title }), ...right);
}

// ---------------------------------------------------------------- status line
function drawStatus(st) {
  $("#dot").className = "dot " + ({ loaded: "loaded", loading: "loading", error: "error" }[st.state] || "");
  $("#status").textContent = st.state === "loaded" ? `${st.model} · ${st.backend}` : st.state === "loading" ? `Loading ${st.model}…` : "No model loaded";
}
on("model-status", (st) => { drawStatus(st); if (tab === "mine") mine(); });

// ---------------------------------------------------------------- my models
async function mine() {
  const list = await call("models");
  const st = await call("model_status");
  drawStatus(st);
  if (!selected && list.length) selected = list[0].id;
  page.textContent = "";
  page.append(head("My models", el("button", { class: "btn", onclick: () => show("import") }, "Import"), el("button", { class: "btn", onclick: () => show("discover") }, "Find models")));
  if (!list.length) {
    page.append(el("div", { class: "empty" }, el("div", {}, el("h2", { text: "No models yet" }), el("div", { text: "Import one you already have, or find one on Hugging Face." }))));
    return;
  }
  const left = el("div", { class: "list scroll" });
  for (const m of list) {
    const loaded = st.state === "loaded" && st.model_id === m.id;
    left.append(el("div", { class: "card" + (m.id === selected ? " on" : ""), onclick: () => { selected = m.id; mine(); } },
      el("div", { class: "t" }, loaded ? el("span", { class: "dot loaded", style: "display:inline-block;margin-right:8px" }) : null, m.name),
      el("div", { class: "s", text: `${bytes(m.bytes)}${m.architecture ? " · " + m.architecture : ""}${m.exists ? "" : " · file missing"}` })));
  }
  const m = list.find((x) => x.id === selected) || list[0];
  page.append(el("div", { class: "split" }, left, detail(m, st)));
}

function detail(m, st) {
  const s = { ...m.settings };
  const num = (v) => (v === "" || v == null ? null : Number(v));
  const field = (label, node) => [el("label", { text: label }), node];
  const ctx = el("input", { class: "input", type: "number", min: 512, step: 1024, value: s.context });
  const layers = el("input", { class: "input", type: "number", min: 0, placeholder: "All that fit", value: s.gpu_layers ?? "" });
  const backend = el("select", { class: "input" }, ...["auto", "rocm", "vulkan", "cpu"].map((b) => el("option", { value: b, selected: s.backend === b }, { auto: "Automatic", rocm: "ROCm", vulkan: "Vulkan", cpu: "CPU only" }[b])));
  const temp = el("input", { class: "input", type: "number", min: 0, max: 2, step: 0.05, placeholder: "Model default", value: s.temperature ?? "" });
  const topp = el("input", { class: "input", type: "number", min: 0, max: 1, step: 0.05, placeholder: "Model default", value: s.top_p ?? "" });
  const prompt = el("textarea", { class: "input", placeholder: "Lu’s default instructions" });
  prompt.value = s.system_prompt || "";
  const loaded = st.state === "loaded" && st.model_id === m.id;
  const save = async () => {
    try {
      await call("save_model_settings", { id: m.id, settings: { context: num(ctx.value) || 16384, gpu_layers: num(layers.value), backend: backend.value, temperature: num(temp.value), top_p: num(topp.value), system_prompt: prompt.value.trim() || null } });
      toast(loaded ? "Saved. Reload the model to use the new settings." : "Saved.");
    } catch (e) { toast(String(e)); }
  };
  return el("div", { class: "detail scroll" },
    el("div", { style: "display:flex;align-items:center;gap:12px;margin-bottom:6px" },
      el("h2", { style: "margin:0;font-size:18px;font-weight:600;flex:1", text: m.name }),
      loaded ? el("button", { class: "btn", onclick: () => call("unload_model") }, "Unload")
             : el("button", { class: "btn primary", disabled: !m.exists, onclick: () => call("load_model", { id: m.id }) }, "Load")),
    el("div", { class: "chips", style: "margin-bottom:22px" },
      el("span", { class: "chip", text: bytes(m.bytes) }),
      m.architecture ? el("span", { class: "chip", text: m.architecture }) : null,
      m.trained_context ? el("span", { class: "chip", text: `trained for ${m.trained_context.toLocaleString()} tokens` }) : null,
      el("span", { class: "chip", text: m.source === "downloaded" ? `from ${m.repo}` : "imported" }),
      m.projector ? el("span", { class: "chip", title: "The image support file loads with the model automatically.", text: "sees images" }) : null),
    el("div", { class: "label", style: "margin-bottom:10px", text: "Settings" }),
    el("div", { class: "form" },
      ...field("Context length", ctx),
      ...field("GPU layers", layers),
      ...field("Engine", backend),
      ...field("Temperature", temp),
      ...field("Top-p", topp),
      ...field("Instructions", prompt)),
    el("div", { style: "display:flex;gap:8px;margin-top:20px" },
      el("button", { class: "btn primary", onclick: save }, "Save"),
      el("span", { style: "flex:1" }),
      m.source === "downloaded" ? el("button", { class: "btn danger", onclick: async () => {
        if (!(await confirmBox("Delete the model file?", `${m.name} (${bytes(m.bytes)}) will be erased from disk${m.projector ? ", with its image support file" : ""}. This cannot be undone; you can download it again later.`, "Delete file"))) return;
        try { const freed = await call("delete_model", { id: m.id }); toast(`Deleted. ${bytes(freed)} freed.`); selected = null; mine(); } catch (e) { toast(String(e), 7000); }
      } }, "Delete file") : null,
      el("button", { class: "btn ghost danger", onclick: async () => {
        if (await confirmBox("Remove from the list?", `${m.name} leaves Lu’s list. The file itself stays on disk.`, "Remove")) { await call("remove_model", { id: m.id }); selected = null; mine(); }
      } }, "Remove from list")),
    el("div", { class: "faint", style: "margin-top:16px;font-size:12px;word-break:break-all", text: m.path }));
}

// ---------------------------------------------------------------- discover (Hugging Face)
let lastQuery = "";
async function discover(results) {
  page.textContent = "";
  const q = el("input", { class: "input", placeholder: "Search models on Hugging Face", value: lastQuery, style: "max-width:420px" });
  const go = async () => {
    lastQuery = q.value.trim();
    if (!lastQuery) return;
    body.textContent = "";
    body.append(el("div", { class: "muted", style: "padding:20px", text: "Searching…" }));
    try { discover(await call("hf_search", { query: lastQuery })); } catch (e) { body.textContent = ""; body.append(el("div", { class: "notice", style: "margin:20px", text: String(e) })); }
  };
  q.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); });
  page.append(head("Discover · Hugging Face", q, el("button", { class: "btn primary", onclick: go }, "Search")));
  const body = el("div", { class: "split" });
  page.append(body);
  const list = el("div", { class: "list scroll" });
  const right = el("div", { class: "detail scroll" }, el("div", { class: "muted", text: results ? "Pick a model to see its files." : "Models come from huggingface.co. Search by name, for example “Qwen3.8” or “gemma 4”." }));
  body.append(list, right);
  for (const r of results || []) {
    list.append(el("div", { class: "card", onclick: (e) => { for (const c of list.children) c.classList.remove("on"); e.currentTarget.classList.add("on"); files(r, right); } },
      el("div", { class: "t", text: r.repo }),
      el("div", { class: "s", text: `Hugging Face · ${r.downloads.toLocaleString()} downloads · ${r.license}${r.unavailable ? " · not available" : ""}` })));
  }
  if (results && !results.length) list.append(el("div", { class: "muted", style: "padding:12px", text: "Nothing found." }));
}

async function files(r, into) {
  into.textContent = "";
  into.append(el("div", { class: "muted", text: "Loading files…" }));
  let info;
  try { info = await call("hf_files", { repo: r.repo }); } catch (e) { into.textContent = ""; into.append(el("div", { class: "notice", text: String(e) })); return; }
  into.textContent = "";
  into.append(el("h2", { style: "margin:0 0 4px;font-size:17px;font-weight:600", text: info.repo }), el("div", { class: "muted", style: "margin-bottom:16px", text: `On Hugging Face · huggingface.co/${info.repo} · License: ${info.license}` }));
  if (info.unavailable) { into.append(el("div", { class: "notice", text: "This model needs a Hugging Face account to download, which Lu does not support yet." })); return; }
  const proj = info.projectors[0];
  const withVision = proj ? el("input", { type: "checkbox", id: "vision" }) : null;
  if (proj) into.append(el("label", { style: "display:flex;gap:8px;align-items:center;margin-bottom:12px" }, withVision, `Also get image support (${bytes(proj.bytes)})`));
  const table = el("table", { class: "grid" }, el("tr", {}, el("th", { text: "Version" }), el("th", { text: "Size" }), el("th")));
  for (const c of info.choices) {
    const key = dkey(info.repo, c.files[0].path);
    const btn = el("button", { class: "btn dl", onclick: async () => {
      btn.disabled = true; btn.textContent = "Starting…";
      try {
        await call("hf_download", { repo: info.repo, revision: info.revision, files: c.files, projector: withVision?.checked ? proj : null });
      } catch (e) { btn.disabled = false; btn.textContent = "Download"; toast(String(e)); }
    } }, "Download");
    btn.dataset.key = key;
    drawButton(btn);
    table.append(el("tr", {}, el("td", { text: c.quant || c.label }), el("td", { text: bytes(c.bytes) }), el("td", { style: "text-align:right" }, btn)));
  }
  into.append(table);
}

function drawButton(btn) {
  const d = downloads.get(btn.dataset.key);
  const running = d && !d.finished && !d.error;
  btn.disabled = !!running || !!d?.finished;
  btn.textContent = running ? `Downloading ${d.total ? Math.floor((d.done / d.total) * 100) : 0}%` : d?.finished ? "Downloaded" : d?.error ? "Try again" : "Download";
}

// Active transfers stay visible at the top of the window, on every tab.
function drawStrip() {
  const strip = $("#strip");
  strip.textContent = "";
  const rows = [];
  for (const [k, d] of downloads) if (!d.finished && !d.error) rows.push([`Downloading ${d.repo} · ${d.file || ""}`, d.done, d.total, k]);
  for (const [id, j] of installs) if (!j.finished && !j.error && j.stage) rows.push([`Installing ${({ rocm: "ROCm", vulkan: "Vulkan", cpu: "CPU" })[id] || id} · ${j.stage}`, j.done, j.total, id]);
  for (const [label, done, total] of rows) {
    const pct = total ? (done / total) * 100 : 0;
    strip.append(el("div", { class: "row" }, el("span", { text: label }), el("span", { class: "muted", text: `${total > 1 ? bytes(done) + " of " + bytes(total) + " · " : ""}${Math.floor(pct)}%` }),
      el("div", { class: "strip-bar" }, el("i", { style: `width:${Math.max(pct, 1)}%` }))));
  }
  strip.classList.toggle("hidden", !rows.length);
}

on("download-event", (ev) => {
  const k = dkey(ev.repo, ev.first || ev.file);
  const d = downloads.get(k) || { repo: ev.repo };
  if (ev.started) { delete d.error; delete d.finished; }
  Object.assign(d, ev);
  downloads.set(k, d);
  for (const b of document.querySelectorAll("button.dl")) if (b.dataset.key === k) drawButton(b);
  drawStrip();
  if (ev.finished) toast(`${ev.repo} is ready in My models.`);
  if (ev.error) toast(`Download stopped: ${ev.error}`, 8000);
  if ((ev.finished || ev.error || ev.started) && tab === "downloads") history();
});

// ---------------------------------------------------------------- downloads
async function history() {
  const rows = await call("download_history");
  page.textContent = "";
  page.append(head("Downloads", el("button", { class: "btn ghost", onclick: async () => { await call("clear_download_history"); history(); } }, "Clear history")));
  const table = el("table", { class: "grid" }, el("tr", {}, el("th", { text: "Model" }), el("th", { text: "Size" }), el("th", { text: "Started" }), el("th", { text: "Result" })));
  const label = { running: "Downloading…", finished: "Finished", failed: "Stopped" };
  for (const r of rows) {
    table.append(el("tr", {},
      el("td", {}, el("div", { text: r.repo }), el("div", { class: "faint", style: "font-size:12px", text: r.files })),
      el("td", { text: bytes(r.bytes) }),
      el("td", { class: "muted", text: new Date(r.started.replace(" ", "T") + "Z").toLocaleString() }),
      el("td", { class: r.outcome === "failed" ? "danger" : "muted", title: r.message || "", text: label[r.outcome] + (r.message ? ` · ${r.message}` : "") })));
  }
  page.append(el("div", { class: "scroll", style: "flex:1;min-height:0;padding:0 6px" }, table));
  if (!rows.length) page.append(el("div", { class: "muted", style: "padding:20px", text: "Nothing downloaded yet." }));
}

// ---------------------------------------------------------------- import
async function imports() {
  page.textContent = "";
  const path = el("input", { class: "input", placeholder: "Paste the path of a .gguf file", style: "max-width:520px" });
  const add = async (p) => {
    try { await call("import_model", { path: p }); toast("Added to My models."); show("mine"); } catch (e) { toast(String(e), 7000); }
  };
  path.addEventListener("keydown", (e) => { if (e.key === "Enter" && path.value.trim()) add(path.value); });
  page.append(head("Import a model", path, el("button", { class: "btn primary", onclick: () => path.value.trim() && add(path.value) }, "Add")));
  const body = el("div", { class: "detail scroll", style: "padding:18px 22px" });
  page.append(body);
  body.append(el("div", { class: "muted", style: "margin-bottom:14px", text: "Lu uses the file where it is; nothing is copied. Models in LM Studio’s folder:" }));
  const mine = new Set((await call("models")).map((m) => m.path.toLowerCase()));
  const found = await call("scan_models", {});
  if (!found.length) { body.append(el("div", { class: "faint", text: "None found in LM Studio’s folder." })); return; }
  const table = el("table", { class: "grid" }, el("tr", {}, el("th", { text: "File" }), el("th", { text: "Size" }), el("th")));
  for (const f of found) {
    const have = mine.has(f.path.toLowerCase());
    table.append(el("tr", {}, el("td", { title: f.path, text: f.name }), el("td", { text: bytes(f.bytes) }),
      el("td", { style: "text-align:right" }, have ? el("span", { class: "faint", text: "Added" }) : el("button", { class: "btn", onclick: () => add(f.path) }, "Add"))));
  }
  body.append(table);
}

// ---------------------------------------------------------------- engines
async function engines() {
  const info = await call("engines");
  page.textContent = "";
  page.append(head("Engines"));
  const body = el("div", { class: "detail scroll", style: "padding:18px 22px" });
  page.append(body);
  if (info.gpus.length) body.append(el("div", { class: "muted", style: "margin-bottom:16px", text: "Graphics card: " + info.gpus.map((g) => `${g.name} (${Math.round(g.total / 1024)} GB)`).join(", ") }));
  const about = {
    rocm: "AMD’s compute stack. Fastest at reading long prompts on Radeon cards.",
    vulkan: "Works on any modern graphics card. Often a little faster at writing replies.",
    cpu: "Runs on the processor only. Slow, but always works.",
  };
  for (const e of info.engines) {
    const job = installs.get(e.id);
    const busy = job && !job.finished && !job.error;
    const status = e.installed ? `Installed · ${e.installed}` : "Not installed";
    const row = el("div", { class: "engine" },
      el("div", { class: "info" }, el("b", { text: e.name }), el("div", { class: "muted", text: about[e.id] }),
        el("div", { class: "faint", style: "font-size:12px;margin-top:4px", text: status + (e.needs_pack && !e.installed ? " · includes AMD support files (about 2.3 GB to download)" : e.size && !e.installed ? ` · ${bytes(e.size)}` : "") }),
        busy ? el("div", { class: "progress-line" }, el("div", { class: "faint", style: "font-size:12px", text: job.stage ? `${job.stage} ${job.total ? Math.round(job.done / job.total * 100) + "%" : ""}` : "Starting…" }), el("div", { class: "strip-bar" }, el("i", { style: `width:${job.total ? (job.done / job.total) * 100 : 3}%` }))) : null),
      e.versions.length > 1 ? el("button", { class: "btn ghost", onclick: async () => { try { const v = await call("rollback_engine", { variant: e.id }); toast(`${e.name} is back on ${v}.`); engines(); } catch (x) { toast(String(x)); } } }, "Use previous version") : null,
      e.installed && e.available === e.installed ? el("span", { class: "faint", text: "Up to date" })
        : el("button", { class: "btn primary", disabled: busy, onclick: async () => { installs.set(e.id, {}); engines(); try { await call("install_engine", { variant: e.id }); } catch (x) { installs.set(e.id, { error: String(x) }); toast(String(x)); engines(); } } }, busy ? "Installing…" : e.installed ? "Update" : "Install"));
    body.append(row);
  }
}
on("engine-event", (ev) => {
  const j = installs.get(ev.id) || {};
  Object.assign(j, ev);
  installs.set(ev.id, j);
  if (ev.finished) toast("Engine installed.");
  if (ev.error) toast(`Install stopped: ${ev.error}`, 9000);
  drawStrip();
  if (tab === "engines") engines();
});

// What is already running (started before this window opened).
(async () => {
  for (const t of await call("transfers")) {
    if (t.kind === "download") downloads.set(dkey(t.repo, t.first || t.file), { ...t });
    else if (t.kind === "engine") installs.set(t.id, { ...t });
  }
  drawStrip();
  show(tab);
})();
