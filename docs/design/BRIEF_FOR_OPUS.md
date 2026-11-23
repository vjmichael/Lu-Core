# Lu — Handoff Brief for Opus 5.5

Nov 23, 2026 · @Michael

This brief lives in the new repository, **Lu-Core**, under `docs/design/`. That repository is where all new code goes. The old repository, **Lu-AI-GUI**, is a reference only (see "Two repositories").

## What you are building

Lu is a local AI desktop that looks and behaves like Jan or LM Studio from the outside. Underneath, it carries the owner's design: a small set of built-in tools, a durable memory that gives the feeling of an endless chat, and checks that make a model's work trustworthy. The goal is for Lu to feel as capable as a cloud AI model. Lu core must run on its own. The workspaces (NVP for cinematography, Lu IDE for coding, Cognitum for reasoning engines) plug in later and are never required.

The owner calls everything that plugs into Lu a DLC, in two kinds. A **DLC CE (core extension)** extends what Lu core can do; Cognitum is one, a mathematical engine with four parts (SSM, KAN, HDC and deterministic tools). A **DLC workshop** extends what the user can do with Lu; NVP and the IDE are workshops. The owner's two primary projects are Lu core and Lu IDE. Do not build any DLC now, and do not build the workspace adapter (it exists as draft source on PR #244; leave it alone). Nothing you build may block either kind from plugging in later.

The design is read through the owner's framework, and the nesting is the rule above all others:

- **Outer, ch⊕α(t):** the human and the AI working together over time. ANSF memory belongs here.
- **Middle, K(α, N, T):** the model's capability, which depends on its architecture, scale and task. The per-model profile belongs here.
- **Inner, C = f(K, I, A, F):** practical capability from knowledge, interfaces, authority and feedback. The execution layer belongs here.

If a build decision cannot be traced to one of these layers, raise it before building it. The owner's own tests show the cost of getting this wrong: a model that gives half-finished work, or an unclear request, scores -1 in the framework.

## What to read first

Four documents travel with this brief, in `docs/design/` of Lu-Core: `ANSF_MEMORY_SPEC.md`, `EXECUTION_LAYER_SPEC.md`, `CORE_COVERAGE_AND_PLATFORM_SPEC.md`, and this brief (`BRIEF_FOR_OPUS.md`). The specs are the requirements; the coverage map shows how every core piece of the old design lines up. From the old repository (Lu-AI-GUI), also read `AGENTS.md`, `docs/LU_CENTRAL_DESIGN.md`, `docs/LU_AI_EXECUTION_LAYER_PHILOSOPHY_20261107.md`, `docs/LU_HUMAN_CENTERED_AI_MEMORY_FIRST_20261107.md` and `docs/LU_M2_ANSF_CONVERSATION_CUTOVER_20261107.md`, without cloning it onto the owner's PC (see below).

In these documents, "the repo", "existing code" and "the repo's design" mean the old Lu-AI-GUI repository, unless a sentence says Lu-Core.

## Two repositories

- **Lu-Core** (https://github.com/vjmichael/Lu-Core, public, main only) is the new, clean repository. All new code, tests, CI and documents go here.
- **Lu-AI-GUI** (https://github.com/vjmichael/Lu-AI-GUI) is the old repository. It stays untouched on GitHub as a reference. **The owner does not want the old codebase on their PC.** Never clone it into the build folder or anywhere permanent. Read individual files from GitHub, or take a shallow copy into a temporary folder outside the Lu-Core clone and delete it when you finish.
- **Porting replaces "reuse".** Where the specs say reuse, copy only the modules you need into Lu-Core and adapt them. Bring no old branches, no history, no ledgers and none of the old document pile. Say in the commit message which old file each ported file came from.
- **Decide per module: port or rewrite.** The old core is about 107,000 lines of tightly connected modules, and one file can pull in dozens of others. Take dependency-closed sets, and rewrite a module when porting it costs more than writing it fresh. Report each decision and why.
- **The old tests are evidence, not the baseline.** The old repository's Linux run was 634 passed, 0 failed, 28 skipped. Use its tests inside your temporary copy to check that a ported module still behaves the same, then write Lu-Core's own tests.

The specs deliberately change some things the old repository says. Where they conflict, the specs win, because they carry the owner's latest decisions:

| Topic | The repo says | The specs say |
| --- | --- | --- |
| Shell access | No universal ambient shell | A command line is a basic tool, under origin-based authority and without a sandbox by default |
| Permission | Per-effect grants (`ce_permissions.rs`) | Authority comes from the user's instruction and where each action came from; Lu does not re-ask |
| Tools | About 45 model-facing tools | Five basic tools built in, with utilities disclosed on demand |
| Memory | Readers wired for two tasks only (Git HEAD, media facts) | Generic recording of every command, result and file change |
| Branching | The old `AGENTS.md` prefers one focused pull request | The owner wants commits to main only; Lu-Core's own `AGENTS.md` says so |
| Status | Alpha 2 is the accepted baseline; v1.0 is not released | Lu-Core starts at designed; nothing in it counts as accepted until it passes the owner's installed tests |

## Recommended build order

This order follows the dependencies. Opus may refine it, but should say so in the report.

| # | Piece | Depends on | First test |
| --- | --- | --- | --- |
| 1 | Generic record of commands, results and file changes in ANSF, host-authenticated | None | Restart continuity |
| 2 | Snapshot store and before and after manifest, with Git as the snapshot when the scope is a repository | 1 | Command scope check |
| 3 | File access with staged writes and verification (universal write and read) | 2 | Staged write |
| 4 | Origin tracking on every proposed action, replacing per-effect grants | 1 | Prompt injection |
| 5 | Command runner with owned process tree, timeout, cancel, output cap and background tasks that report back | 1, 2, 4 | Timeout and cancel |
| 6 | Loop guard connected to restore-first recovery | 2, 3 | Loop guard |
| 7 | Lu-driven memory injection (hidden) and automatic saving | 1 | Bounded injection |
| 8 | Automatic environment facts, re-checked before use | 1, 5 | Toolchain amnesia |
| 9 | Model profile and routing: images to a multimodal model, exact work to Cognitum | 1, 7 | Untrained versus trained model |
| 10 | Starter utilities disclosed on demand | 9 | Minimal surface |

Pieces 1 to 6 are the foundation of both specs. Build and test them before 7 to 10.

After piece 10, build the platform pieces from the coverage map. Piece 11 can start alongside piece 5, because the command runner needs the same measurements.

- **11. Resource management:** measure RAM, VRAM and disk before loading a model or starting a heavy job; one owner for model load and unload; pressure notices to the journal and the model. Thresholds come from real measurement with the owner.
- **12. Hugging Face search and staged download:** leads only, staged write with hash check and resume, then the existing import and admission checks.
- **13. Backends and engine updater:** CPU, Vulkan and ROCm builds side by side; automatic pick or a forced pick per model; each model remembers what worked. Add an LM Studio style engine updater: list installed builds, offer newer ones from the pinned list, download to staging, verify the hash, switch over, keep the old build for one-click rollback, allow pinning a model to an engine version.

## Working on the owner's PC

The owner runs you in the Claude desktop app's Code tab on their Windows PC, in a folder for the Lu-Core clone, so they can test directly. You start with no memory of the design conversation; these documents are all you have. Day one:

1. Clone Lu-Core into a new folder the owner names, read `docs/design/`, and say back in three lines what you are building.
2. Check the Windows toolchain before anything else: Rust with the MSVC target, Node, the Tauri prerequisites including WebView2, MSVC build tools, LLVM, Git and the Protobuf compiler. The old repository's build scripts and its Windows acceptance documents show what it expected. Report what is missing, install it with the standard installers, and record each install in a ledger file in Lu-Core so it is never asked about twice.
3. Work only inside the Lu-Core clone. Never point a test build at the owner's real Lu chats, memory or model folders; give the new app its own isolated development identity and data folder.
4. Do not clone the old repository onto the PC. Fetch only the files you port, into a temporary folder outside the clone, and delete it afterwards.
5. First commit to main: `AGENTS.md` with the main-only rule from this brief, plus a GitHub Actions workflow that builds and tests on Windows and Linux for verification only. Commit small and often to main.
6. The first baseline is the first ported set of modules building and passing on Windows and on Linux. Record it before adding anything else.
7. The owner starts you in Manual permission mode and may loosen it. Ask before installing system software and before anything outside the clone folder.
8. Send the first report in the standard format.

## Rules of engagement

- **Main only.** Commit to main. No branches and no pull requests; CI is for verification only. If a branch is ever unavoidable, reconcile it to main when finished.
- **Port first, rewrite where cheaper.** The specs mark each old module as reuse, rework or replace. Those calls came from file names and documents, not a line-by-line read. Check each against the source, and say in the report where one is wrong.
- **Protobuf and SQLite.** Inner communication between modules, workers and engines is typed Protobuf. Storage is SQLite with relational columns. JSON appears only at the model, UI, external and legacy-import boundaries. Create no new JSONL journals.
- **Do not port** the per-task memory readers or the `lu_reviewed_*` tools. Port a per-format tool only when the starter utility set calls for it.
- **Built in, not external.** The basic tools run inside Lu. No MCP server, no Docker and no mandatory sandbox. Windows is the primary platform, and the code must also build and pass its tests on Linux.
- **Complete, not minimal.** The owner wants full implementations. If something cannot be completed, say what is missing rather than shipping a skeleton.
- **Independence.** Lu core must not depend on NVP, Lu IDE or Cognitum.
- **Secrets stay out.** Never commit keys, tokens or credentials.

## Defaults for open questions

The owner has not answered these yet. Use these defaults so work is not blocked, build them so they are easy to change, and list each in the report as pending the owner's approval.

| Question | Default |
| --- | --- |
| Working scope | The project folder plus any folder the user's instruction names. Reads outside it are free; writes outside it are held |
| An action that came from external content | Hold it, tell the user once what content asked for it, and log it |
| Steps an instruction plainly implies | A package from a trusted package manager counts as the user's instruction; code from other sources is held |
| Isolation for external code | Build the option to run it in isolation, but do not pick an environment; the owner chooses |
| Snapshot size limits | Git when the scope is a repository; otherwise copy up to a size cap Opus proposes; beyond it, record a manifest only and tell the user |
| Secrets | No automatic classification. Provide a redaction that erases content and leaves a marker. Offer once to forget obvious key formats |
| Repair cap | The loop guard's existing setting |
| First model-profile task kinds | Scheduling and ordering, multi-step math and reasoning puzzles, from the owner's Cognitum tests |
| Viewing memories | Not in this phase; the injected memory block is hidden |

More defaults, decided by the owner or following from their decisions:

- **Workspace adapter:** decided, waits until the standalone core is built.
- **ROCm:** decided, built as a normal backend; the owner already runs ROCm llama.cpp in LM Studio on the R9700.
- **Engine updater source:** pinned builds first. Offering newest upstream builds, hash-checked, is pending the owner's approval.
- **Memory thresholds for resource management:** set after measuring on the owner's machine, not guessed.

## How to report

The owner skims reports and reads them later, so keep them short. Every report has the same three parts:

1. **What failed**, with the cause if known.
2. **The next task.**
3. **The remaining tasks.**

Keep a task ledger as you work, so the owner and the work cannot drift apart. State the status of each piece with one of the repo's levels: designed, draft source, compiled, desktop integrated, real-model exercised, installed accepted, released. Never write "done" without the level and the evidence.

Disclose every skipped test. Mark each result as verified or untested, and keep uncertainty visible instead of smoothing it over. Be frank and direct, without flattery, and pair any criticism of the specs with a way forward.

## Definition of done

A piece is done at the "compiled" and tested level when every test marked Linux in the two specs passes on Linux and in CI. That does not make it accepted. A passing unit suite is not proof of a working installed Windows app or a real model turn.

Only the owner can run these on a Windows machine with the R9700:

- Descendant process cleanup with Windows Job Objects.
- Qwen 3.6 against Qwen 3.8 memory behavior, and the minimal tool surface with Qwen 3.8.
- A long session over days.
- The toolchain amnesia test in a fresh chat.
- A model load on the R9700 with the Vulkan and ROCm engines, and a model forced onto each.
- An engine update, a refused bad-hash update, and a rollback.
- A real small-model download from Hugging Face, including an interrupted and resumed one.
- A model unload and reload around a heavy job.

For each of these, write a PowerShell test script in the style of the old repository's physical-test scripts (read them from GitHub), and report the result as NOT\_TESTED until the owner reports it. Never mark a piece "installed accepted" on your own.
