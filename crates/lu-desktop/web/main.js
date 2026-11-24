import { call, on, el, $, menu, confirmBox, choiceBox, promptBox, toast, renderReply, when, setTheme, themeChoice, bytes } from "./common.js";

const state = {
  folders: [], chats: [], open: new Set(JSON.parse(localStorage.getItem("lu-open-folders") || "[]")),
  current: null, status: { state: "unloaded" }, models: [],
  // Answers being written, per conversation: { text, items: [{kind, text, ok, folder}], body, waiting }.
  streams: new Map(),
  // What is typed and attached, per conversation ("new" before the first message).
  drafts: new Map(), attach: new Map(),
  // Messages typed while Lu was still answering, sent in order afterwards.
  queued: new Map(),
};
const tree = $("#tree"), messages = $("#messages"), input = $("#input"), sendBtn = $("#send");

// ---------------------------------------------------------------- sidebar

async function refresh() {
  const lib = await call("library");
  state.folders = lib.folders;
  state.chats = lib.chats;
  if ($("#search").value.trim()) return runSearch();
  drawTree();
}

function chatItem(c) {
  const row = el("button", {
    class: "item" + (c.id === state.current ? " active" : ""),
    draggable: "true",
    title: c.title,
    onclick: () => openChat(c.id),
    oncontextmenu: (e) => { e.preventDefault(); chatMenu(c, e.clientX, e.clientY); },
    ondragstart: (e) => { e.dataTransfer.setData("text/lu-chat", c.id); e.dataTransfer.effectAllowed = "move"; },
  },
    el("span", { class: "name", text: c.title }),
    el("span", { class: "faint", style: "font-size:12px", text: when(c.updated) }),
    el("span", { class: "more", title: "More", onclick: (e) => { e.stopPropagation(); chatMenu(c, e.clientX, e.clientY); } }, "⋯"));
  return row;
}

function dropZone(node, folder) {
  node.addEventListener("dragover", (e) => { if (e.dataTransfer.types.includes("text/lu-chat")) { e.preventDefault(); node.classList.add("drop-target"); } });
  node.addEventListener("dragleave", () => node.classList.remove("drop-target"));
  node.addEventListener("drop", async (e) => {
    e.preventDefault(); node.classList.remove("drop-target");
    const id = e.dataTransfer.getData("text/lu-chat");
    if (!id) return;
    await call("move_chat", { id, folder });
    if (folder != null) { state.open.add(folder); saveOpen(); }
    refresh();
  });
}
const saveOpen = () => localStorage.setItem("lu-open-folders", JSON.stringify([...state.open]));

function drawTree() {
  tree.textContent = "";
  for (const f of state.folders) {
    const open = state.open.has(f.id);
    const chats = state.chats.filter((c) => c.folder === f.id);
    const head = el("button", {
      class: "item folder" + (open ? " open" : ""),
      onclick: () => { open ? state.open.delete(f.id) : state.open.add(f.id); saveOpen(); drawTree(); },
      oncontextmenu: (e) => { e.preventDefault(); folderMenu(f, e.clientX, e.clientY); },
    },
      el("span", { class: "chev" }, "›"), el("span", { class: "name", text: f.name }),
      el("span", { class: "faint", style: "font-size:12px", text: chats.length || "" }),
      el("span", { class: "more", onclick: (e) => { e.stopPropagation(); folderMenu(f, e.clientX, e.clientY); } }, "⋯"));
    dropZone(head, f.id);
    tree.append(head);
    if (open) tree.append(el("div", { class: "nest" }, chats.length ? chats.map(chatItem) : el("div", { class: "faint", style: "padding:4px 12px;font-size:12px", text: "Empty. Drag conversations here." })));
  }
  const loose = state.chats.filter((c) => c.folder == null);
  const label = el("div", { class: "group-title" }, el("span", { class: "label", text: "Conversations" }));
  dropZone(label, null);
  tree.append(label);
  const list = el("div", {}, loose.map(chatItem));
  dropZone(list, null);
  tree.append(list);
  if (!state.chats.length) tree.append(el("div", { class: "faint", style: "padding:8px 12px", text: "No conversations yet." }));
}

function chatMenu(c, x, y) {
  menu(x, y, [
    { label: "Rename", run: async () => { const t = await promptBox("Rename conversation", c.title); if (t) { await call("rename_chat", { id: c.id, title: t }); refresh(); if (c.id === state.current) $("#title").textContent = t; } } },
    { label: "Move to…", run: () => moveMenu(c, x, y) },
    "-",
    { label: "Delete", danger: true, run: async () => {
      if (!(await confirmBox("Delete this conversation?", `“${c.title}” will move to the trash. You can restore it from Manage.`))) return;
      await call("delete_chat", { id: c.id });
      if (c.id === state.current) { state.current = null; showEmpty(); }
      refresh();
    } },
  ]);
}
function moveMenu(c, x, y) {
  setTimeout(() => menu(x, y, [
    { label: "No folder", run: async () => { await call("move_chat", { id: c.id, folder: null }); refresh(); } },
    ...state.folders.map((f) => ({ label: f.name, run: async () => { await call("move_chat", { id: c.id, folder: f.id }); refresh(); } })),
    "-",
    { label: "New folder…", run: async () => { const n = await promptBox("New folder", "", "Folder name", "Create"); if (n) { const id = await call("new_folder", { name: n }); await call("move_chat", { id: c.id, folder: id }); state.open.add(id); saveOpen(); refresh(); } } },
  ]), 0);
}
function folderMenu(f, x, y) {
  menu(x, y, [
    { label: "New conversation here", run: () => newChat(f.id) },
    { label: "Rename", run: async () => { const n = await promptBox("Rename folder", f.name); if (n) { await call("rename_folder", { id: f.id, name: n }); refresh(); } } },
    "-",
    { label: "Delete folder", danger: true, run: async () => {
      const count = state.chats.filter((c) => c.folder === f.id).length;
      if (!count) { if (await confirmBox("Delete this folder?", `“${f.name}” is empty.`)) { await call("delete_folder", { id: f.id, withChats: false }); refresh(); } return; }
      const pick = await choiceBox("Delete this folder?", `“${f.name}” has ${count} conversation${count > 1 ? "s" : ""}.`, [
        { label: "Keep conversations", value: "keep" },
        { label: "Delete them too", value: "all", primary: true },
      ]);
      if (!pick) return;
      await call("delete_folder", { id: f.id, withChats: pick === "all" });
      if (pick === "all" && state.chats.some((c) => c.folder === f.id && c.id === state.current)) { state.current = null; showEmpty(); }
      refresh();
    } },
  ]);
}

let searchTimer = null;
$("#search").addEventListener("input", () => { clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 180); });
async function runSearch() {
  const q = $("#search").value.trim();
  if (!q) return drawTree();
  const hits = await call("search_chats", { query: q });
  tree.textContent = "";
  tree.append(el("div", { class: "group-title" }, el("span", { class: "label", text: hits.length ? "Results" : "No matches" })));
  for (const h of hits) {
    const snip = el("span", { class: "snippet" });
    // The match is wrapped in [ ] by the search; show it highlighted.
    for (const part of h.snippet.split(/(\[[^\]]*\])/)) {
      snip.append(part.startsWith("[") && part.endsWith("]") ? el("mark", { text: part.slice(1, -1) }) : part);
    }
    tree.append(el("button", { class: "item hit" + (h.id === state.current ? " active" : ""), onclick: () => openChat(h.id) },
      el("span", { class: "name" }, el("span", { text: h.title }), snip)));
  }
}

// ---------------------------------------------------------------- conversation

function showEmpty() {
  $("#title").textContent = "New conversation";
  messages.textContent = "";
  messages.append(el("div", { class: "empty", style: "height:60vh" }, el("div", {},
    el("h2", { text: "What can I help with?" }),
    el("div", { text: state.status.state === "loaded" ? `${state.status.model} is ready.` : "Choose a model at the top right to begin." }))));
}

const dkey = () => state.current || "new";
function saveDraft() { state.drafts.set(dkey(), input.value); }
function restoreDraft() { input.value = state.drafts.get(dkey()) || ""; autosize(); drawAttach(); }
function attachments() { if (!state.attach.has(dkey())) state.attach.set(dkey(), []); return state.attach.get(dkey()); }
function drawAttach() {
  const box = $("#attach");
  box.textContent = "";
  const list = attachments();
  for (const p of list) {
    box.append(el("span", { class: "file-chip", title: p }, fileIcon(p), el("span", { class: "n", text: baseName(p) }),
      el("button", { class: "x", title: "Remove", onclick: () => { list.splice(list.indexOf(p), 1); drawAttach(); } }, "×")));
  }
  box.classList.toggle("hidden", !list.length);
}
const baseName = (p) => p.split(/[\\/]/).pop();
const fileIcon = (p) => el("span", { text: /\.(png|jpe?g|webp|gif|bmp)$/i.test(p) ? "🖼" : "📄" });

async function openChat(id) {
  if (state.current !== id) { saveDraft(); state.current = id; restoreDraft(); }
  const chat = state.chats.find((c) => c.id === id);
  $("#title").textContent = chat ? chat.title : "Conversation";
  const list = await call("chat_messages", { id });
  if (state.current !== id) return;
  messages.textContent = "";
  for (const m of list) addMessage(m.role, m.text);
  let s = state.streams.get(id);
  if (!s) {
    const live = await call("chat_live", { id });
    if (live.writing || live.waiting) { s = { text: live.text || "", items: [], waiting: live.waiting }; state.streams.set(id, s); }
  }
  if (s) {
    // Waiting: the message is not saved yet, so show it from memory.
    if (s.waiting && s.user && list.at(-1)?.text !== s.user) addMessage("user", s.user);
    s.body = addMessage("lu", "");
    s.body.classList.add("caret");
    for (const it of s.items) drawItem(s, it);
    s.body._raw = s.text;
    if (s.waiting) s.body.append(waitingNote());
    else renderReply(s.text, s.body);
  }
  if (!list.length && !s) showEmpty();
  scrollDown(true);
  setBusy();
  drawTreeSelection();
  input.focus();
}
function drawTreeSelection() {
  for (const n of tree.querySelectorAll(".item:not(.folder)")) n.classList.remove("active");
  $("#search").value.trim() ? runSearch() : drawTree();
}

// A user message is stored as its text plus one "[Attached] path" line per file.
function splitAttached(text) {
  const files = [], lines = [];
  for (const l of text.split("\n")) { const m = l.match(/^\[Attached\] (.+)$/); m ? files.push(m[1]) : lines.push(l); }
  return { text: lines.join("\n").trim(), files };
}
function addMessage(role, text) {
  if (messages.querySelector(".empty")) messages.textContent = "";
  const body = el("div", { class: "body" });
  body._raw = text;
  const m = el("div", { class: "msg " + (role === "user" ? "user" : "lu") });
  if (role === "user") {
    const { text: t, files } = splitAttached(text);
    if (t) { body.textContent = t; m.append(body); }
    if (files.length) m.append(el("div", { class: "attachments" }, files.map((p) => el("span", { class: "file-chip", title: p }, fileIcon(p), el("span", { class: "n", text: baseName(p) })))));
  } else {
    renderReply(text, body);
    m.append(body);
  }
  const copy = el("button", { title: "Copy", onclick: async () => {
    const raw = role === "user" ? splitAttached(body._raw).text : body._raw;
    try { await navigator.clipboard.writeText(raw); toast("Copied."); } catch { toast("Could not copy."); }
  } }, "Copy");
  const tools = el("div", { class: "msg-tools" }, copy);
  if (role !== "user") tools.append(el("button", { title: "Ask again", onclick: () => retry(m) }, "Retry"));
  m.append(tools);
  messages.append(m);
  return body;
}
// Send the user message before this answer again.
function retry(answer) {
  if (state.current && state.streams.has(state.current)) return toast("Lu is still answering in this conversation.");
  let prev = answer.previousElementSibling;
  while (prev && !prev.classList.contains("user")) prev = prev.previousElementSibling;
  const body = prev?.querySelector(".body") || prev;
  const raw = prev ? (prev.querySelector(".body")?._raw ?? null) : null;
  const src = raw ?? (prev ? prev.textContent : null);
  if (!src) return toast("Nothing to ask again.");
  const { text, files } = splitAttached(src);
  sendText(text, files);
  void body;
}
function scrollDown(force) {
  const t = $("#thread");
  if (force || t.scrollHeight - t.scrollTop - t.clientHeight < 160) t.scrollTop = t.scrollHeight;
}

async function newChat(folder = null) {
  const id = await call("new_chat", { folder });
  if (folder != null) { state.open.add(folder); saveOpen(); }
  await refresh();
  await openChat(id);
}

async function send() {
  const text = input.value.trim();
  const files = [...attachments()];
  if (!text && !files.length) return;
  if (state.status.state !== "loaded") { toast("Load a model first: use the model button at the top right."); return; }
  input.value = ""; autosize();
  attachments().length = 0; drawAttach();
  state.drafts.delete(dkey());
  // Still answering here: queue it; it goes when the answer is done.
  if (state.current && state.streams.has(state.current)) {
    const chat = state.current;
    addMessage("user", composed(text, files));
    const bubble = messages.lastElementChild;
    const chip = el("div", { class: "queued-chip" }, "Queued · sends after this answer ", el("button", { class: "x", title: "Remove", onclick: () => {
      const q = state.queued.get(chat) || [];
      const i = q.findIndex((x) => x.bubble === bubble);
      if (i >= 0) q.splice(i, 1);
      bubble.remove();
    } }, "×"));
    bubble.append(chip);
    if (!state.queued.has(chat)) state.queued.set(chat, []);
    state.queued.get(chat).push({ text, files, bubble, chip });
    scrollDown(true);
    return;
  }
  await sendText(text, files);
}
const composed = (text, files) => (files.length ? `${text}\n${files.map((f) => `\n[Attached] ${f}`).join("")}` : text);
async function sendText(text, files, queued = null) {
  if (!state.current && !queued) {
    state.current = await call("new_chat", { folder: null });
    messages.textContent = "";
  }
  const chat = queued?.chat || state.current;
  const visible = chat === state.current;
  if (queued) queued.chip?.remove();
  else addMessage("user", composed(text, files));
  const body = visible ? addMessage("lu", "") : null;
  body?.classList.add("caret");
  state.streams.set(chat, { text: "", items: [], body: body || undefined, waiting: false, user: text });
  setBusy();
  if (visible) scrollDown(true);
  try { await call("send_message", { chat, text, attachments: files, reasoning: reasoningChoice() }); }
  catch (e) { endStream(chat); toast(String(e)); }
}

const waitingNote = () => el("div", { class: "faint wait", text: "Waiting for the other conversation to finish…" });
function endStream(chat) {
  const s = state.streams.get(chat);
  if (s?.body) s.body.classList.remove("caret");
  state.streams.delete(chat);
  setBusy();
  // The next queued message of this conversation goes now.
  const next = state.queued.get(chat)?.shift();
  if (next) setTimeout(() => sendText(next.text, next.files, { chat, chip: next.chip }), 50);
}
function setBusy() {
  const b = !!state.current && state.streams.has(state.current);
  sendBtn.textContent = b ? "Stop" : "Send";
  sendBtn.classList.toggle("danger", b);
  $("#hint").textContent = b ? "Lu is answering… Enter queues your message · Esc or Stop stops the answer" : "Enter to send · Shift+Enter for a new line · drop files to attach";
}
function stopCurrent() { if (state.current && state.streams.has(state.current)) call("stop_chat", { chat: state.current }); }
const shown = (chat, s) => chat === state.current && s.body?.isConnected;

function drawItem(s, it) {
  if (!s.body?.isConnected) return;
  const row = s.body.parentElement;
  if (it.kind === "action") {
    it.node = el("div", { class: "action " + (it.ok == null ? "running" : it.ok ? "done" : "failed"), text: it.text });
    row.before(it.node);
  } else if (it.kind === "note") {
    row.before(el("div", { class: "note-line", text: it.text }));
  } else if (it.kind === "approval") {
    drawApproval(row, it);
  } else if (it.kind === "held") {
    const n = el("div", { class: "notice" }, el("div", { text: it.text }));
    if (it.folder) {
      const btn = el("button", { class: "btn primary", style: "margin-top:8px", onclick: async () => {
        btn.disabled = true;
        try {
          await call("allow_folder", { path: it.folder });
          btn.textContent = "Allowed";
          toast("Lu may now work in " + it.folder + ". Ask again to continue.");
        } catch (e) { btn.disabled = false; toast(String(e)); }
      } }, "Allow this folder");
      n.append(btn);
    }
    row.before(n);
  }
}

function drawApproval(row, it) {
  const card = el("div", { class: "approval" });
  it.node = card;
  row.before(card);
  paintApproval(it);
}
function paintApproval(it) {
  const card = it.node;
  if (!card?.isConnected) return;
  card.textContent = "";
  if (it.folder) card.append(el("div", { class: "q", text: `${it.what} needs a folder Lu is not allowed to work in yet:` }), el("div", { class: "f", text: it.folder }));
  else card.append(el("div", { class: "q", text: `${it.what} came after Lu read outside content. Check it before allowing:` }), el("div", { class: "f", text: it.detail || "" }), it.reason ? el("div", { class: "faint", style: "font-size:12px;margin:-4px 0 10px", text: it.reason }) : null);
  if (it.answer) {
    const said = { once: "Allowed for this answer.", always: "Allowed. Lu will remember this folder.", deny: "Not allowed. Lu stopped.", timeout: "No answer in time. Lu stopped; send a new message to try again.", stopped: "Stopped." }[it.answer] || it.answer;
    card.append(el("div", { class: "muted", text: said }));
    return;
  }
  const answer = (choice) => call("answer_approval", { id: it.id, choice }).catch((e) => toast(String(e)));
  const left = Math.max(0, Math.ceil((it.deadline - Date.now()) / 1000));
  card.append(el("div", { class: "btns" },
    el("button", { class: "btn primary", onclick: () => answer("once") }, "Allow"),
    it.folder ? el("button", { class: "btn", onclick: () => answer("always") }, "Always allow this folder") : null,
    el("button", { class: "btn ghost", onclick: () => answer("deny") }, "Don’t allow"),
    el("span", { class: "t", text: `Waiting ${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}` })));
}
setInterval(() => { for (const s of state.streams.values()) for (const it of s.items) if (it.kind === "approval" && !it.answer) paintApproval(it); }, 1000);

on("chat-event", (ev) => {
  let s = state.streams.get(ev.chat);
  if (!s) {
    if (ev.type === "finished" || ev.type === "error") return;
    s = { text: "", items: [], waiting: false };
    state.streams.set(ev.chat, s);
  }
  const vis = shown(ev.chat, s);
  if (ev.type === "queued") {
    s.waiting = true;
    if (vis) { s.body.textContent = ""; s.body.append(waitingNote()); }
  } else if (ev.type === "started") {
    s.waiting = false;
    if (vis) s.body.textContent = "";
  } else if (ev.type === "text") {
    s.text += ev.text;
    if (s.body) s.body._raw = s.text;
    if (vis) { renderReply(s.text, s.body); scrollDown(); }
  } else if (ev.type === "note") {
    const it = { kind: "note", text: ev.text };
    s.items.push(it);
    if (vis) drawItem(s, it);
  } else if (ev.type === "approval") {
    const it = { kind: "approval", id: ev.id, what: ev.what, folder: ev.folder, detail: ev.detail, reason: ev.reason, deadline: Date.now() + ev.seconds * 1000 };
    s.items.push(it);
    if (vis) { drawItem(s, it); scrollDown(true); }
    else toast("Lu is waiting for your approval in another conversation.", 6000);
  } else if (ev.type === "approval-done") {
    const it = s.items.find((x) => x.id === ev.id);
    if (it) { it.answer = ev.answer; paintApproval(it); }
  } else if (ev.type === "action") {
    const it = { kind: "action", text: ev.text, ok: null };
    s.items.push(it);
    if (vis) drawItem(s, it);
  } else if (ev.type === "action-done") {
    const it = [...s.items].reverse().find((x) => x.kind === "action" && x.text === ev.text && x.ok == null);
    if (it) { it.ok = ev.ok; if (it.node) it.node.className = "action " + (ev.ok ? "done" : "failed"); }
  } else if (ev.type === "held") {
    const it = { kind: "held", text: ev.text, folder: ev.folder };
    s.items.push(it);
    if (vis) drawItem(s, it);
  } else if (ev.type === "error") {
    if (vis) { s.body.textContent = ""; s.body.append(el("div", { class: "notice", text: ev.text })); }
    else toast(ev.text);
    endStream(ev.chat);
  } else if (ev.type === "finished") {
    const bits = [];
    if (ev.stopped) bits.push("Stopped: " + ev.stopped);
    if (ev.speed) bits.push(`${ev.speed.toFixed(1)} tokens/s`);
    if (vis && bits.length) s.body.parentElement.after(el("div", { class: "meta", text: bits.join(" · ") }));
    endStream(ev.chat);
  }
});

// Files dropped on the window: their paths go into the message, which
// also allows Lu to work in their folders for that message.
T_drop();
function T_drop() {
  const w = window.__TAURI__?.webviewWindow?.getCurrentWebviewWindow?.();
  if (!w?.onDragDropEvent) return;
  const comp = document.querySelector(".composer");
  w.onDragDropEvent((e) => {
    const p = e.payload;
    if (p.type === "enter" || p.type === "over") comp.classList.add("drop-target");
    else comp.classList.remove("drop-target");
    if (p.type === "drop" && p.paths?.length) {
      const list = attachments();
      for (const f of p.paths) if (!list.includes(f)) list.push(f);
      drawAttach(); input.focus();
    }
  });
}
on("library-changed", () => refresh().then(() => {
  const c = state.chats.find((x) => x.id === state.current);
  if (c) $("#title").textContent = c.title;
}));

// ---------------------------------------------------------------- composer

function autosize() { input.style.height = "auto"; input.style.height = Math.min(input.scrollHeight, 220) + "px"; }
input.addEventListener("input", autosize);
input.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); } });
sendBtn.addEventListener("click", () => (state.current && state.streams.has(state.current) ? stopCurrent() : send()));
addEventListener("keydown", (e) => { if (e.key === "Escape") stopCurrent(); });

// Reasoning: shown only for models whose template takes it.
const reasonLabel = { off: "Thinking off", on: "Thinking on", minimal: "Reasoning: Minimal", low: "Reasoning: Low", medium: "Reasoning: Medium", high: "Reasoning: High", xhigh: "Reasoning: Extra high", max: "Reasoning: Max" };
function reasoningChoice() {
  const levels = state.status.reasoning || [];
  if (!levels.length) return null;
  let v = null;
  try { v = localStorage.getItem("lu-reasoning"); } catch {}
  return levels.includes(v) ? v : levels.includes("medium") ? "medium" : levels.includes("on") ? "on" : levels[0];
}
function drawReasoning() {
  const b = $("#reasoning");
  const v = reasoningChoice();
  b.classList.toggle("hidden", !v);
  if (v) b.textContent = reasonLabel[v] || v;
}
$("#reasoning").addEventListener("click", (e) => {
  const r = e.currentTarget.getBoundingClientRect();
  const cur = reasoningChoice();
  menu(r.left, r.top - 40 * (state.status.reasoning.length + 1), state.status.reasoning.map((l) => ({
    label: (l === cur ? "● " : "") + (reasonLabel[l] || l),
    run: () => { try { localStorage.setItem("lu-reasoning", l); } catch {} drawReasoning(); },
  })));
});
$("#new-chat").addEventListener("click", () => newChat());
$("#new-folder").addEventListener("click", async () => {
  const n = await promptBox("New folder", "", "Folder name", "Create");
  if (n) { const id = await call("new_folder", { name: n }); state.open.add(id); saveOpen(); refresh(); }
});
addEventListener("keydown", (e) => { if (e.ctrlKey && e.key.toLowerCase() === "n") { e.preventDefault(); newChat(); } });
$("#open-models").addEventListener("click", () => call("open_window", { kind: "models" }));
$("#open-manager").addEventListener("click", () => call("open_window", { kind: "manager" }));
$("#title").addEventListener("dblclick", async () => {
  if (!state.current) return;
  const c = state.chats.find((x) => x.id === state.current);
  const t = await promptBox("Rename conversation", c?.title || "");
  if (t) { await call("rename_chat", { id: state.current, title: t }); refresh(); $("#title").textContent = t; }
});

// ---------------------------------------------------------------- model status and picker

function drawStatus(st) {
  state.status = st;
  const dot = $("#model-dot");
  dot.className = "dot " + ({ loaded: "loaded", loading: "loading", error: "error" }[st.state] || "");
  $("#model-name").textContent =
    st.state === "loaded" ? st.model :
    st.state === "loading" ? `Loading ${st.model}…` :
    st.state === "error" ? "Couldn’t load model" : "No model loaded";
  $("#model-backend").textContent = st.state === "loaded" ? st.backend || "" : "";
  const bar = $("#loadbar");
  bar.classList.toggle("hidden", st.state !== "loading");
  bar.firstElementChild.style.width = Math.round((st.progress || 0) * 100) + "%";
  if (st.state === "error" && st.message && st.can_force && st.model_id != null && !st._asked) {
    st._asked = true;
    choiceBox("Not enough free memory", st.message, [{ label: "Load anyway", value: "force" }, { label: "Don’t load", value: "no", primary: true }])
      .then((v) => { if (v === "force") call("load_model", { id: st.model_id, force: true }); });
  } else if (st.state === "error" && st.message) toast(st.message, 8000);
  if (messages.querySelector(".empty")) showEmpty();
  drawReasoning();
}
on("model-status", drawStatus);

$("#model-pill").addEventListener("click", async (e) => {
  const r = e.currentTarget.getBoundingClientRect();
  const models = await call("models");
  const items = models.filter((m) => m.exists).map((m) => ({
    label: (state.status.model_id === m.id && state.status.state === "loaded" ? "● " : "") + `${m.name}  ·  ${bytes(m.bytes)}`,
    run: () => call("load_model", { id: m.id }),
  }));
  if (!items.length) items.push({ label: "No models yet — add one…", run: () => call("open_window", { kind: "models", tab: "import" }) });
  items.push("-");
  if (state.status.state === "loaded") items.push({ label: "Unload model", run: () => call("unload_model") });
  items.push({ label: "Manage models…", run: () => call("open_window", { kind: "models" }) });
  menu(r.right - 300, r.bottom + 6, items);
});

// ---------------------------------------------------------------- settings

function webMediaAuto() { try { return localStorage.getItem("lu-web-media") === "auto"; } catch { return false; } }

$("#settings").addEventListener("click", async (e) => {
  const r = e.currentTarget.getBoundingClientRect();
  const s = await call("app_settings");
  const cur = themeChoice();
  menu(r.left, r.top - 275, [
    { label: (cur === "light" ? "● " : "") + "Day", run: () => setTheme("light") },
    { label: (cur === "dark" ? "● " : "") + "Night", run: () => setTheme("dark") },
    { label: (cur === "system" ? "● " : "") + "Follow Windows", run: () => setTheme("system") },
    "-",
    { label: (webMediaAuto() ? "● " : "") + "Show web pictures and videos at once", run: () => { try { localStorage.setItem("lu-web-media", webMediaAuto() ? "click" : "auto"); } catch {} toast(webMediaAuto() ? "Web pictures and videos now show at once." : "Web pictures and videos now wait for a click."); } },
    "-",
    { label: "Diagnostics…", run: () => call("open_window", { kind: "diagnostics" }) },
    { label: "Work folder…", run: async () => {
      const p = await promptBox("Work folder", s.workspace, "Folder Lu may work in");
      if (p) { try { await call("set_workspace", { path: p }); toast("Lu will work in " + p); } catch (err) { toast(String(err)); } }
    } },
  ]);
});

// ---------------------------------------------------------------- start

(async () => {
  drawStatus(await call("model_status"));
  await refresh();
  showEmpty();
  input.focus();
})();
