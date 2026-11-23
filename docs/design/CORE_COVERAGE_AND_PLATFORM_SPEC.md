# Lu — Core Coverage Map and Platform Spec

> **Reading note.** "The repo", "existing code" and "the repo's design" in this document mean the old Lu-AI-GUI repository, which is a reference only. New code lives in Lu-Core. See the brief, section "Two repositories".


This lines up every core architecture piece in the Lu repo's design (central design, v1.0 reconciliation, v2.0 parking lot) against the three specs we have written, and specifies the platform pieces that were still missing: resource management, model search, inference backends, and the workspace adapter.

## How to read this

Status words follow the repo's own ladder: designed, draft source, compiled, integrated, real-model exercised, installed accepted. Everything below is read from file names, sizes and design docs, not from line-by-line code review, and nothing here is Windows-tested.

## Two kinds of DLC

You call everything that plugs into Lu a DLC. There are two kinds, and they connect differently.

- **DLC CE (core extension):** adds to what Lu core itself can do. Cognitum is the example: a mathematical engine with four parts (SSM, KAN, HDC and the deterministic tools). It supplies capabilities Lu's model can hand work to, and it has no screen of its own.
- **DLC workshop:** adds to what you can do with Lu. NVP (cinematography) and the IDE (coding) are the examples. Each has its own screen, its own project state and its own effects, and uses Lu's model and memory.

Your two primary projects are Lu core and the IDE. You do a lot of vibe coding, so a proper coding space is easier for you than coding through Lu core. That makes the IDE the first workshop to plan for, and Lu core must run with none of these connected.

The repo's own code uses the same split: a core extension host in Lu core (the ce\_ modules, for in-process capabilities like Cognitum) and the workspace adapter (for separate apps like the IDE and NVP).

## Coverage map

| Core piece (repo design) | Covered by | Repo today | Action |
| --- | --- | --- | --- |
| ANSF immutable conversation storage, books, library, revisions (central §4.1, V1-03) | ANSF spec | \~8,000 lines, cut over to SQLite in M2 | Reuse |
| Memory retrieval into the model, belief revision, continuity (§4.2, V1-08) | ANSF spec | Source-ID retrieval and belief code exist; model exposure was the open item | Rework: Lu-driven injection |
| Context change monitoring (§4.3) | ANSF spec (goal and scope changes re-check origin) | Goal records and clarification exist | Reuse; fold into origin tracking |
| Observe and Learn (§4.3) | Not covered | Off by default, capture worker crate exists | Leave off; later plug-in |
| Authority, permission, access (§2, V1-04) | Execution-layer spec | Per-effect grants and conversation authority | Rework: origin tracking replaces per-effect prompts |
| Native tools: files, commands, read, write, search (§3.3, V1-06) | Execution-layer spec | Many per-task readers and tools | Rework: basic tools plus utilities on demand |
| Effect journal and recovery states (§3.3, V1-07) | Execution-layer spec | Journal with PREPARED to UNCERTAIN exists | Reuse; add snapshots |
| Loop guard and bounded repair (model boundary layer) | Execution-layer spec | generation\_loop\_guard exists | Rework: restore first |
| Process supervision, Job Object cleanup (§3.1, V1-05) | Execution-layer spec | Job crate and background process modules; descendant cleanup unproven | Reuse; owner test needed |
| Model boundary reliability: strict JSON, one normalization, two regenerations (§8.4) | Not covered | Present in repo | Reuse as is; keep under the runner |
| Cognitum handoff | Execution-layer spec | Optional provider, not wired | Handoff default for exact work |
| Model profile K(α,N,T) | ANSF spec | Not in repo | New |
| Resource management (central §5, V1-05) | This doc, part A | Small modules: host\_resource\_authority 143 lines, native\_resource\_pressure 92, warm\_model\_residency 81 | Build out |
| Hugging Face model search and download (P03) | This doc, part B | Catalog, transport and staging, about 700 lines, review-only by design | Reuse; add install flow |
| Inference backends CPU, Vulkan, ROCm (§3.2, V1-02) | This doc, part C | Selection contract names all four; CPU and Vulkan builds pinned; ROCm only in the contract and tests | Add ROCm and an LM Studio style engine updater |
| Model import, GGUF and MMProj admission (V1-02) | This doc, part C | model\_import, gguf\_preflight, mmproj\_admission, model\_admission exist | Reuse |
| Workspace IPC adapter (v2.0 parking lot) | This doc, part D | Draft PR #244: lu.workspace.v0 contract, framing, Linux and Windows transports; not on main | Reuse PR #244; timing is your call |
| Desktop GUI, streaming, queues, attachments (§6, V1-09) | Not covered | Tauri and React shell exists; alpha accepted | Out of scope for Opus's first pass; keep Jan or LM Studio look |
| Packaging, NSIS, pinned engines (§7, V1-01, V1-10) | Not covered | Preview build script exists | Later, owner tests |
| Durable agents and task scheduler (§5, V1-07) | Partly (background tasks) | agent\_host\_scheduler, agent\_tool\_loop exist | Review after build order step 5 |
| Lu IDE, NVP, Cognitum workspaces | Plug-ins via part D | Post-core | Out of scope |

Three pieces had no home in our specs: the model boundary layer, the GUI, and packaging. The first is already in the repo and only needs protecting. The other two are real work but not what we are building first.

## A. Resource management

Purpose: keep the machine usable while models, tools and plug-ins compete for memory and the GPU.

- Lu measures free RAM, VRAM and disk before loading a model or starting a heavy job, and refuses or queues when there is not enough. Numbers come from measurement, never guesses.
- One owner for model load and unload. Plug-ins such as NVP ask Lu to unload the model, do their work, and ask for a reload. Lu arbitrates; no plug-in unloads on its own.
- Heavy deterministic work runs on the CPU so the GPU stays with the model.
- Pressure events (low memory, a worker over budget) go to the journal and to the model as short notices.
- The repo's own note says the thresholds need a separate human-reviewed reconciliation. We take the existing three modules as the base and set thresholds with you after real measurements.

## B. Hugging Face model search

- Search by name, show license, size, quantization files and hashes. Results are leads only: Lu never downloads or runs anything from a result without your say-so.
- Download into the model folder through the staged-write path: temp file, verify hash, then swap in. Resume on failure.
- Gated or private repos are shown as not available; no token handling in the first version.
- After download, the existing import and admission checks run (GGUF preflight, MMProj pairing).

## C. Inference backends

- Three engine builds side by side: CPU, Vulkan, ROCm. Lu probes the machine and picks automatically, or you force one per model.
- Each model remembers which backend worked and how fast. That feeds the model profile.
- ROCm is in, decided. You already run ROCm llama.cpp in LM Studio on the R9700, so Lu builds the ROCm path as a normal backend, with no gate on a hardware check. If a particular machine cannot use it, Lu says so plainly and falls back.
- Backend updates work like LM Studio's: Lu lists the installed engine builds and their versions, offers newer ones, downloads into a staging area, verifies the hash against the pinned list, then switches over. The old build stays so you can roll back in one click. A failed update never replaces a working engine.
- You can pin a model to a specific engine version, since a new build can break a model that worked.
- Backend crashes are recoverable: Lu unloads, reports in plain words, and offers the next backend.

## D. Workspace adapter

- The adapter already exists as source: the `lu.workspace.v0` contract and transport on draft PR #244 (not on main), with docs LU\_IPC\_01A\_B\_V0\_CONTRACT and LU\_IPC\_01D\_R1\_NATIVE\_TRANSPORT. We reuse it; we do not redesign it.
- What it provides: one Protobuf envelope with length-prefixed frames (256 KiB cap), a hello and challenge handshake, Linux Unix sockets with kernel peer credentials, Windows named pipes with owner-only access and process checks, streaming with credits and backpressure, cancel, and model leases.
- The repo's later decision replaces my earlier note: Windows named pipes and Linux Unix sockets, no TCP, no MCP, no Docker. The Nov 3 note that said Unix sockets only on both is superseded.
- Who connects and what each needs, from the contract's own map: Lu IDE (project tree, diffs, protected build and test, uses Lu's model and memory), Cognitum (math and reasoning operations, pure compute, no workspace), NVP (production data, render jobs, asks Lu to unload and reload the model for RAM and VRAM). Each keeps its own state and its own effects; Lu stays the only model host.
- Connected is not trusted. A hello is only a claim. Every call still passes Lu's origin tracking, so a plug-in acts for you only when your instruction started it, and no message can mint a grant.
- Known gaps in the source: Windows signer and package checks are candidates, not accepted; pure-compute effect (Cognitum) has no bridge yet, so the adapter refuses it; no installed test of any plug-in.
- The repo ledger says plug-in integration comes after standalone v1.0. Core must run with no plug-in connected.

## Build order additions

The ten-piece order in the Opus brief stays. These come after piece 10, except where noted.

1. Resource measurement and model load arbitration (can start alongside piece 5, since the command runner needs the same measurements).
2. Hugging Face search and staged download.
3. Backend selection with per-model memory of what worked, ROCm, and the engine updater with rollback.
4. Workspace adapter from PR #244, with Cognitum as the test client. Decided: this waits until the standalone core is built.

## Acceptance tests

On Linux, by Opus: resource refusal when memory is short, download resume and hash failure, backend fallback order, engine update with a bad hash refused and rollback to the old build.

On your machine: R9700 load on Vulkan and on ROCm, an engine update and rollback, a real model unload and reload around a heavy job, a real download of a small GGUF.

## Open questions

- Decided: the workspace adapter waits until the standalone core is built. Open for later: whether Opus starts from the PR #244 branch or only its contract.
- Memory thresholds: set after measuring on your machine.
- Which engine builds the updater lists: only the pinned ones in the repo, or LM Studio style newest-upstream builds with a hash check? Default: pinned list first.
