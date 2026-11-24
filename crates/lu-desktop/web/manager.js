import { call, on, el, $, when, toast, confirmBox, promptBox, menu } from "./common.js";

const page = $("#page");
let tab = "all";
const picked = new Set();
let filter = "";

function show(t) {
  tab = t;
  for (const b of document.querySelectorAll(".tab")) b.classList.toggle("on", b.dataset.tab === t);
  ({ all, folders, trash })[t]();
}
for (const b of document.querySelectorAll(".tab")) b.addEventListener("click", () => show(b.dataset.tab));
on("library-changed", () => { if (tab !== "trash") show(tab); });

function head(title, ...right) {
  return el("div", { class: "head" }, el("h2", { text: title }), ...right);
}

async function all() {
  const lib = await call("library");
  const folderName = new Map(lib.folders.map((f) => [f.id, f.name]));
  const chats = lib.chats.filter((c) => !filter || c.title.toLowerCase().includes(filter.toLowerCase()));
  for (const id of [...picked]) if (!lib.chats.some((c) => c.id === id)) picked.delete(id);
  page.textContent = "";
  const search = el("input", { class: "input", placeholder: "Filter by title", value: filter, style: "max-width:260px" });
  search.addEventListener("input", () => { filter = search.value; all(); setTimeout(() => { const s = page.querySelector("input"); s.focus(); s.setSelectionRange(s.value.length, s.value.length); }, 0); });
  const moveBtn = el("button", { class: "btn", disabled: !picked.size, onclick: (e) => {
    const r = e.currentTarget.getBoundingClientRect();
    menu(r.left, r.bottom + 6, [
      { label: "No folder", run: () => moveAll(null) },
      ...lib.folders.map((f) => ({ label: f.name, run: () => moveAll(f.id) })),
      "-",
      { label: "New folder…", run: async () => { const n = await promptBox("New folder", "", "Folder name", "Create"); if (n) moveAll(await call("new_folder", { name: n })); } },
    ]);
  } }, `Move ${picked.size || ""} to…`);
  const delBtn = el("button", { class: "btn danger", disabled: !picked.size, onclick: async () => {
    if (!(await confirmBox(`Delete ${picked.size} conversation${picked.size > 1 ? "s" : ""}?`, "They will move to the trash, where you can restore them."))) return;
    for (const id of picked) await call("delete_chat", { id });
    picked.clear(); all();
  } }, "Delete");
  page.append(head("Conversations", search, moveBtn, delBtn));
  const allBox = el("input", { type: "checkbox", checked: chats.length > 0 && chats.every((c) => picked.has(c.id)), onchange: (e) => { for (const c of chats) e.target.checked ? picked.add(c.id) : picked.delete(c.id); all(); } });
  const table = el("table", { class: "grid" }, el("tr", {}, el("th", { style: "width:36px" }, allBox), el("th", { text: "Title" }), el("th", { text: "Folder" }), el("th", { text: "Last used" })));
  for (const c of chats) {
    table.append(el("tr", {},
      el("td", {}, el("input", { type: "checkbox", checked: picked.has(c.id), onchange: (e) => { e.target.checked ? picked.add(c.id) : picked.delete(c.id); all(); } })),
      el("td", { text: c.title }),
      el("td", { class: "muted", text: c.folder != null ? folderName.get(c.folder) || "" : "—" }),
      el("td", { class: "muted", text: when(c.updated) })));
  }
  page.append(el("div", { class: "scroll", style: "flex:1;min-height:0;padding:0 6px" }, table));
  if (!chats.length) page.append(el("div", { class: "muted", style: "padding:20px", text: "No conversations." }));
}
async function moveAll(folder) {
  for (const id of picked) await call("move_chat", { id, folder });
  toast(`Moved ${picked.size}.`);
  picked.clear(); all();
}

async function folders() {
  const lib = await call("library");
  page.textContent = "";
  page.append(head("Folders", el("button", { class: "btn primary", onclick: async () => { const n = await promptBox("New folder", "", "Folder name", "Create"); if (n) { await call("new_folder", { name: n }); folders(); } } }, "New folder")));
  const table = el("table", { class: "grid" }, el("tr", {}, el("th", { text: "Folder" }), el("th", { text: "Conversations" }), el("th")));
  for (const f of lib.folders) {
    const n = lib.chats.filter((c) => c.folder === f.id).length;
    table.append(el("tr", {}, el("td", { text: f.name }), el("td", { class: "muted", text: n }),
      el("td", { style: "text-align:right" },
        el("button", { class: "btn ghost", onclick: async () => { const nn = await promptBox("Rename folder", f.name); if (nn) { await call("rename_folder", { id: f.id, name: nn }); folders(); } } }, "Rename"),
        el("button", { class: "btn ghost danger", onclick: async () => {
          if (!(await confirmBox("Delete this folder?", n ? `Its ${n} conversation${n > 1 ? "s" : ""} will stay, outside any folder.` : "It is empty."))) return;
          await call("delete_folder", { id: f.id, withChats: false }); folders();
        } }, "Delete"))));
  }
  page.append(el("div", { class: "scroll", style: "flex:1;min-height:0;padding:0 6px" }, table));
  if (!lib.folders.length) page.append(el("div", { class: "muted", style: "padding:20px", text: "No folders yet." }));
}

async function trash() {
  const items = await call("trash");
  page.textContent = "";
  page.append(head("Trash"));
  const table = el("table", { class: "grid" }, el("tr", {}, el("th", { text: "Conversation" }), el("th", { text: "Deleted" }), el("th")));
  for (const t of items) {
    table.append(el("tr", {}, el("td", { text: t.title }), el("td", { class: "muted", text: when(t.deleted) }),
      el("td", { style: "text-align:right" },
        el("button", { class: "btn ghost", onclick: async () => { await call("restore_chat", { trashId: t.id }); toast("Restored."); trash(); } }, "Restore"),
        el("button", { class: "btn ghost danger", onclick: async () => {
          if (!(await confirmBox("Delete forever?", `“${t.title}” will be erased and cannot be restored.`, "Delete forever"))) return;
          await call("erase_chat", { trashId: t.id }); trash();
        } }, "Delete forever"))));
  }
  page.append(el("div", { class: "scroll", style: "flex:1;min-height:0;padding:0 6px" }, table));
  if (!items.length) page.append(el("div", { class: "muted", style: "padding:20px", text: "The trash is empty." }));
}

show("all");
