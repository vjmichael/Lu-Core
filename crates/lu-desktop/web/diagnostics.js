import { call, on, el, $, toast } from "./common.js";

// Diagnostics: read-only checks, Lu's tool self-test, and benchmarks.
// Nothing here runs by itself; each button is the user's own start.
const page = $("#page");
let tab = "checks";
let last = { checks: null, bench: null };

function show(t) {
  tab = t;
  for (const b of document.querySelectorAll(".tab")) b.classList.toggle("on", b.dataset.tab === t);
  ({ checks, bench })[t]();
}
for (const b of document.querySelectorAll(".tab")) b.addEventListener("click", () => show(b.dataset.tab));

const head = (title, ...right) => el("div", { class: "head" }, el("h2", { text: title }), ...right);
const mark = { ok: "✓", problem: "!", "not enough evidence": "?" };

function findingList(findings) {
  const box = el("div", { class: "findings" });
  const order = { problem: 0, "not enough evidence": 1, ok: 2 };
  for (const f of [...findings].sort((a, b) => order[a.severity] - order[b.severity])) {
    const more = el("div", { class: "finding-more hidden" },
      f.cause ? el("div", {}, el("b", { text: "Cause: " }), f.cause) : null,
      f.fix ? el("div", {}, el("b", { text: "What to do: " }), f.fix) : null,
      f.evidence?.length ? el("ul", {}, f.evidence.map((e) => el("li", { text: e }))) : null,
      f.private?.length ? el("details", {}, el("summary", { text: "Details on this PC only" }), el("ul", {}, f.private.map((e) => el("li", { class: "mono", text: e })))) : null);
    const row = el("div", { class: "finding sev-" + f.severity.replace(/ /g, "-"), onclick: () => more.classList.toggle("hidden") },
      el("span", { class: "sev", text: mark[f.severity] || "?" }),
      el("div", { class: "f-body" }, el("div", { text: f.summary }), el("div", { class: "faint", style: "font-size:12px", text: f.check })));
    box.append(row, more);
  }
  return box;
}

function runButtons(result) {
  return el("button", { class: "btn ghost", onclick: async () => {
    try { const p = await call("dx_export", { run: result.run }); toast("Report saved: " + p, 7000); } catch (e) { toast(String(e)); }
  } }, "Export report");
}

async function checks() {
  page.textContent = "";
  const run = (cmd, label) => el("button", { class: "btn" + (cmd === "dx_check" ? " primary" : ""), onclick: async (e) => {
    e.currentTarget.disabled = true; $("#status").textContent = label + "…";
    try { last.checks = { title: label, ...(await call(cmd)) }; } catch (err) { toast(String(err)); }
    $("#status").textContent = ""; checks();
  } }, label);
  page.append(head("Checks", run("dx_check", "Check Lu"), run("dx_self_test", "Test Lu’s tools")));
  const body = el("div", { class: "detail scroll", style: "padding:18px 22px" });
  page.append(body);
  body.append(el("div", { class: "muted", style: "margin-bottom:14px", text: "“Check Lu” reads Lu’s own records and changes nothing. “Test Lu’s tools” writes, edits, restores, unpacks and runs a command in a scratch folder, then removes it; your files are not touched." }));
  if (!last.checks) return;
  const r = last.checks;
  const problems = r.findings.filter((f) => f.severity !== "ok").length;
  body.append(el("div", { class: "row-between" }, el("h3", { text: `${r.title}: ${problems ? problems + " to look at" : "all good"}` }), runButtons(r)), findingList(r.findings));
}

async function bench() {
  page.textContent = "";
  const st = await call("model_status");
  const start = (kind, label) => el("button", { class: "btn" + (kind === "memory" ? " primary" : ""), disabled: st.state !== "loaded" || st.busy, onclick: async () => {
    try { await call("dx_benchmark", { kind }); last.bench = { running: kind }; $("#status").textContent = label + " running…"; bench(); } catch (e) { toast(String(e)); }
  } }, label);
  page.append(head("Benchmarks", start("speed", "Speed and accuracy"), start("memory", "Memory (three arms)")));
  const body = el("div", { class: "detail scroll", style: "padding:18px 22px" });
  page.append(body);
  body.append(el("div", { class: "muted", style: "margin-bottom:14px", text: st.state === "loaded"
    ? `On ${st.model} (${st.backend}). Speed: eight fixed questions with exact answers. Memory: the same questions asked with no earlier conversation, with the recent messages only, and with Lu’s memory, to see what memory adds.`
    : "Load a model first; benchmarks run on the loaded model." }));
  const b = last.bench;
  if (!b) return;
  if (b.running) { body.append(el("div", { class: "muted", text: "Running… this takes a minute or two. You can keep using Lu." })); return; }
  if (b.error) { body.append(el("div", { class: "notice", text: b.error })); return; }
  const table = el("table", { class: "grid" });
  if (b.kind === "memory") {
    const names = { none: "No memory", recent: "Recent messages only", memory: "Lu’s memory" };
    body.append(el("div", { class: "row-between" }, el("h3", { text: `Memory on ${b.model}` }), runButtons(b)));
    body.append(el("div", { class: "bars" }, b.arms.map((a) => el("div", { class: "bar-row" },
      el("span", { class: "bar-label", text: names[a.arm] || a.arm }),
      el("span", { class: "bar" }, el("i", { style: `width:${a.total ? (a.correct / a.total) * 100 : 0}%` })),
      el("span", { class: "bar-num", text: `${a.correct} of ${a.total}` })))));
    table.append(el("tr", {}, el("th", { text: "Question" }), ...["none", "recent", "memory"].map((a) => el("th", { text: names[a] }))));
    for (const task of [...new Set(b.results.map((r) => r.task))]) {
      table.append(el("tr", {}, el("td", { text: task.replace(/^mem-/, "") }), ...["none", "recent", "memory"].map((a) => {
        const r = b.results.find((x) => x.task === task && x.arm === a);
        return el("td", { title: r?.answer || "", text: r?.correct === true ? "✓ right" : r?.correct === false ? "✗ wrong" : "–" });
      })));
    }
  } else {
    body.append(el("div", { class: "row-between" }, el("h3", { text: `${b.correct} of ${b.total} right on ${b.model}${b.speed ? ` · ${b.speed.toFixed(1)} tokens/s` : ""}` }), runButtons(b)));
    table.append(el("tr", {}, el("th", { text: "Question" }), el("th", { text: "Kind" }), el("th", { text: "Result" }), el("th", { text: "Speed" })));
    for (const r of b.results) table.append(el("tr", {}, el("td", { text: r.task }), el("td", { class: "muted", text: r.kind }), el("td", { title: r.answer, text: r.correct === true ? "✓ right" : r.correct === false ? "✗ wrong" : "not judged" }), el("td", { class: "muted", text: r.speed ? r.speed.toFixed(1) + " t/s" : "" })));
  }
  body.append(el("div", { class: "faint", style: "margin:14px 0 6px;font-size:12px", text: "Hover a result to see the model’s answer. Answers are kept unchanged in Lu’s DX records." }), table);
}

on("dx-event", (ev) => { last.bench = ev; $("#status").textContent = ""; if (tab === "bench") bench(); if (!ev.error) toast("Benchmark finished."); });
on("model-status", () => { if (tab === "bench" && !last.bench?.running) bench(); });
show("checks");
