# Lu DX — Diagnostics and Benchmarks (draft for owner review)

> **Status: approved by the owner on Nov 23, 2026; steps 1, 2 (except the tool self-test) and 3 (speed and task kinds) are draft source in `crates/lu-core/src/dx.rs`.** Written from the owner's request in chat and from what the old Lu-AI-GUI repository records of the earlier DX work. The original DX-00 to DX-04 specification (jan_alt, Oct 29) no longer exists (the owner deleted that fork); its decisions are reconstructed from the summaries in Lu-AI-GUI `docs/LU_CORE_DESIGN_PARITY_CP13_20261116.md`. Owner decisions: exports go to a `dx` folder in Lu's data folder; benchmarks may use Cognitum as a checker when it is connected; Lu ships the ROCm libraries it needs.

## Purpose

DX is how Lu explains its own problems and measures itself. When something goes wrong (a model will not load, an answer is slow, a tool call never returned), DX says what happened, where it happened, and what can be done about it, in plain words, with the evidence behind it. When asked, it also runs benchmarks: how fast and how well a model, a backend or Lu's memory performs on this machine.

The first real case is the one that prompted this document. The pinned ROCm build of llama.cpp loaded a 27B model and answered at 2 tokens per second. Lu reported a pass. Task Manager showed the GPU at 0%. Reading the build showed that its GPU module needs `hipblas.dll` and `rocblas.dll`, which the build does not include and the PC does not have, so llama.cpp silently fell back to the CPU. A DX check would have said that in one line, before the model was ever loaded.

## Rules

These carry over from the earlier DX work and the owner's Oct 28 instruction.

1. **Read-only.** DX observes, explains and measures. It never changes Lu's state, never repairs, installs, deletes or restarts anything, and never grants permission. A suggested fix is text for the user; carrying it out is a separate action that goes through the origin gate like any other.
2. **Passive by default, active only on request.** Reading Lu's own records runs any time and costs almost nothing. Anything that does work (a benchmark, loading a model, a self-test, a probe that starts a process) is off by default and runs only when the user starts it, each time. No background benchmark, no listener, no remote access.
3. **Private by default.** A DX report contains no prompts, model output, file contents, file paths, tokens or keys. It names things by kind and short identifier ("model qwen3.8-27b Q4_K_M", "engine rocm b12651"). The user can open a private detail view on their own machine; an export is sanitized unless the user chooses otherwise for that export.
4. **Honest evidence.** DX never invents a cause. Missing, lost or contradictory evidence is reported as "not enough evidence", with what is missing. A dropped event is counted, not hidden. A stale or late record never turns into a success. (Earlier DX-07, DX-08, DX-10, DX-14.)
5. **Does not interfere.** DX reads; it does not slow down a generation, hold a lock that a tool needs, or change timing. Its own records are bounded in size.
6. **Not authority.** Like memory, a diagnostic finding is information. A finding that says "ROCm needs the HIP SDK" does not install the HIP SDK.

## What DX reads (passive)

Lu-Core already writes almost all the evidence DX needs, in SQLite:

| Source | What it tells DX |
| --- | --- |
| Effect journal | Every action: prepared, running, applied, failed, uncertain |
| ANSF operations | Commands started and finished, file changes, effects left uncertain by a crash |
| Origin gate | Actions held or refused, and which outside content caused it |
| Resource journal | Measured RAM, VRAM and disk; refusals, pressure, leases, evictions |
| Engine registry and backend results | Which engine builds are installed and active; each model's load time, speed and failures per backend |
| Model profile | Verified outcomes per model and task kind; memory path in use |
| Environment facts | Installed tools, their versions, re-check results, contradictions |
| Model registry (Hugging Face) | What was downloaded, from where, its hash and GGUF check |
| Engine logs | The tail of `llama-server`'s own log for a load (kept locally, never exported raw) |

DX adds one thing: a **correlation ID** that the host gives each user turn, so the records of one turn can be followed across these stores. It is minted by Lu, never by a model, and is not a permission (earlier DX-00).

## Checks (passive and on request)

Each check returns a finding: **ok**, **problem** (with cause, evidence and a suggested fix), or **not enough evidence** (with what is missing).

### Passive checks (always available, read-only)

- **Engines:** each installed build's version, devices it reported at install, and whether it is active. A GPU build with no GPU device is a problem.
- **Last load:** for the model last loaded, the backend, layers offloaded to the GPU, load time, tokens per second, and whether that speed is in line with the model's earlier record on this machine.
- **Unknown outcomes:** effects left uncertain by a crash or timeout, oldest first.
- **Held actions:** what is waiting for the user's approval, and why.
- **Resources:** current pressure, reserves (provisional or owner-set), active GPU leases, recent refusals.
- **Memory:** for the current chat, how much of the memory block was used, what was left out, and whether the model is on the Lu-driven or model-driven path.
- **Environment:** facts that are contradicted or stale.

### Active checks (only when the user starts them)

- **Engine dependency check:** reads a build's required libraries from its files (no execution) and reports which are missing and where they normally come from. For a ROCm build this is the `hipblas.dll` / `rocblas.dll` check from the case above, and whether AMD's HIP SDK is present (`HIP_PATH`).
- **Device check:** runs `llama-server --list-devices` for a build (starts a process, so it is active).
- **Load check:** loads a chosen model on a chosen backend, confirms where the layers went (offload line and measured VRAM), and unloads it.
- **Tool self-test:** runs Lu's own basic tools on a scratch folder (write, read, edit, snapshot, restore, archive) and confirms each result. Never touches the user's files.

## Benchmarks (only when the user starts them)

Benchmarks are active checks that measure. Rules from the earlier ANSF benchmark work apply: the scoring is fixed before the run, raw outputs are kept unchanged, and a later re-scoring is labelled as such and never replaces the original.

- **Speed:** load time, prompt and generation tokens per second, per model, quantization and backend, with the machine's measured state at the time. Results go into the per-model backend record and the model profile.
- **Backend comparison:** the same model on CPU, Vulkan and ROCm, so Lu's backend order is based on measurement on this machine.
- **Memory (three arms):** the same questions answered with no memory, with recent history only, and with Lu's memory, to measure what memory adds for a given model (the ANSF spec's untrained-versus-trained test builds on this).
- **Task kinds:** small fixed task sets for the model profile's first kinds (scheduling and ordering, multi-step math, reasoning puzzles), each scored by a checker, never by the model's own claim.

Benchmark results are evidence for the model profile only when they come from a checker, like any other outcome.

## Output

- **In the app:** a plain-language "Check Diagnostics" view. Each finding is one or two sentences: what is wrong, what it affects, what to do. Technical detail and the private view are one step further, on the user's request.
- **For the model:** a short notice when a finding affects the current task ("the ROCm engine cannot use the GPU on this machine; using Vulkan"). Data, never instructions.
- **Export:** a sanitized report (Protobuf inside Lu; JSON or text at the export boundary) the owner can share. Its version, the correlation IDs, counts of lost evidence, and nothing private.
- **Storage:** DX's own findings and benchmark runs in SQLite, immutable, bounded.

## How this maps to the earlier DX items

| Earlier item | Here |
| --- | --- |
| DX-00 typed bounded trace, host-minted correlation, loss counters | Correlation ID per turn; findings and exports typed and bounded; losses counted |
| DX-01, DX-02 startup, turn, tool dispatch, cancellation, recovery | Read from the effect journal and ANSF operations Lu-Core already writes |
| DX-03 localization with dropped evidence, plain and technical export | Findings with cause and evidence; "not enough evidence" when it is missing |
| DX-04 privacy, bounded overhead, noninterference; active checks separately approved | Rules 2, 3 and 5 |
| DX-06 plain-language Check Diagnostics | Output, in the app |
| DX-07, DX-08, DX-10, DX-14 fail closed on contradictory, late or missing evidence | Rule 4 |
| DX-09, DX-12 cancellation tied to its task; signalled is not terminated | Read from the runner's records; a cancel without a confirmed exit is "uncertain" |
| DX-IPC (read-only local socket) | Not in this phase; no listener. Revisit with the workspace adapter |
| Lu-to-Lu assistance | Not in this phase |

## Build order (proposed)

1. Passive checks over the existing stores, findings in SQLite, plain-language text. Includes the engine and last-load checks.
2. Active checks: engine dependency check (fixes the ROCm case's reporting), device check, load check, tool self-test.
3. Benchmarks: speed and backend comparison first, then memory three-arm and task kinds.
4. Sanitized export.

## Acceptance tests

| Test | Passing means | Where |
| --- | --- | --- |
| Read-only | Running every passive check and every report leaves every store byte-identical | Linux and Windows |
| Silent CPU fallback | A GPU build that would run on the CPU is reported as a problem with the missing libraries named | Owner's machine (ROCm build), plus a test double in CI |
| Not enough evidence | With a record deleted or contradicted, DX says what is missing and names no cause | Linux |
| Privacy | A report built from a chat containing a secret-looking string, a path and a prompt contains none of them | Linux |
| Off by default | No active check or benchmark runs without a user start; nothing listens on a socket | Linux and Windows |
| Benchmark integrity | Re-scoring keeps the original scores and raw outputs | Linux |

## Open questions

- [x] **Confirm the reconstruction.** Owner: go ahead (Nov 23).
- [ ] **Benchmark sets.** Which fixed task sets to start with; the owner's Cognitum tests are the default source for the first task kinds.
- [x] **Export destination.** A `dx` folder in Lu's data folder; nothing is sent anywhere.
- [x] **ROCm path.** Lu ships what it needs: a ROCm runtime pack (hipBLAS, rocBLAS and their kernels) built from AMD's pinned ROCm tarball, so any user runs out of the box.
- [ ] **Benchmark sets.** A starter set of eight exact-answer tasks is built in; Cognitum can check them when connected.
