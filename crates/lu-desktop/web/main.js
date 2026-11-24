import { call, on, el, $, menu, confirmBox, choiceBox, promptBox, toast, renderReply, when, setTheme, themeChoice, bytes } from "./common.js";

const state = {
  folders: [], chats: [], open: new Set(JSON.parse(localStorage.getItem("lu-open-folders") || "[]")),
  current: null, status: { state: "unloaded" }, streaming: null, models: [],
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

async function openChat(id) {
  state.current = id;
  const chat = state.chats.find((c) => c.id === id);
  $("#title").textContent = chat ? chat.title : "Conversation";
  const list = await call("chat_messages", { id });
  messages.textContent = "";
  for (const m of list) addMessage(m.role, m.text);
  if (!list.length) showEmpty();
  scrollDown(true);
  drawTreeSelection();
  input.focus();
}
function drawTreeSelection() {
  for (const n of tree.querySelectorAll(".item:not(.folder)")) n.classList.remove("active");
  $("#search").value.trim() ? runSearch() : drawTree();
}

function addMessage(role, text) {
  if (messages.querySelector(".empty")) messages.textContent = "";
  const body = el("div", { class: "body" });
  if (role === "user") body.textContent = text; else renderReply(text, body);
  const m = el("div", { class: "msg " + (role === "user" ? "user" : "lu") }, body);
  messages.append(m);
  return body;
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
  if (!text || state.streaming) return;
  if (state.status.state !== "loaded") { toast("Load a model first: use the model button at the top right."); return; }
  if (!state.current) {
    state.current = await call("new_chat", { folder: null });
    messages.textContent = "";
  }
  input.value = ""; autosize();
  addMessage("user", text);
  const body = addMessage("lu", "");
  body.classList.add("caret");
  state.streaming = { chat: state.current, body, text: "", actions: new Map() };
  setBusy(true);
  scrollDown(true);
  try { await call("send_message", { chat: state.current, text }); }
  catch (e) { finishStream(); toast(String(e)); }
}

function finishStream() {
  if (state.streaming) state.streaming.body.classList.remove("caret");
  state.streaming = null;
  setBusy(false);
}
function setBusy(b) {
  sendBtn.disabled = b;
  $("#hint").textContent = b ? "Lu is answering…" : "Enter to send · Shift+Enter for a new line";
}

on("chat-event", (ev) => {
  const s = state.streaming;
  if (!s || ev.chat !== s.chat) return;
  if (ev.type === "text") {
    s.text += ev.text;
    renderReply(s.text, s.body);
    scrollDown();
  } else if (ev.type === "action") {
    const chip = el("div", { class: "action running", text: ev.text });
    s.body.parentElement.before(chip);
    s.actions.set(ev.text, chip);
  } else if (ev.type === "action-done") {
    const chip = s.actions.get(ev.text);
    if (chip) chip.className = "action " + (ev.ok ? "done" : "failed");
  } else if (ev.type === "held") {
    s.body.parentElement.before(el("div", { class: "notice", text: ev.text }));
  } else if (ev.type === "error") {
    s.body.textContent = "";
    s.body.append(el("div", { class: "notice", text: ev.text }));
    finishStream();
  } else if (ev.type === "finished") {
    const bits = [];
    if (ev.stopped) bits.push("Stopped: " + ev.stopped);
    if (ev.speed) bits.push(`${ev.speed.toFixed(1)} tokens/s`);
    if (bits.length) s.body.parentElement.after(el("div", { class: "meta", text: bits.join(" · ") }));
    finishStream();
  }
});
on("library-changed", () => refresh().then(() => {
  const c = state.chats.find((x) => x.id === state.current);
  if (c) $("#title").textContent = c.title;
}));

// ---------------------------------------------------------------- composer

function autosize() { input.style.height = "auto"; input.style.height = Math.min(input.scrollHeight, 220) + "px"; }
input.addEventListener("input", autosize);
input.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); } });
sendBtn.addEventListener("click", send);
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
  if (st.state === "error" && st.message) toast(st.message, 8000);
  if (messages.querySelector(".empty")) showEmpty();
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

$("#settings").addEventListener("click", async (e) => {
  const r = e.currentTarget.getBoundingClientRect();
  const s = await call("app_settings");
  const cur = themeChoice();
  menu(r.left, r.top - 190, [
    { label: (cur === "light" ? "● " : "") + "Day", run: () => setTheme("light") },
    { label: (cur === "dark" ? "● " : "") + "Night", run: () => setTheme("dark") },
    { label: (cur === "system" ? "● " : "") + "Follow Windows", run: () => setTheme("system") },
    "-",
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
