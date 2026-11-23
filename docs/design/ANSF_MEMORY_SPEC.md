# Lu ANSF Memory — Design Spec

> **Reading note.** "The repo", "existing code" and "the repo's design" in this document mean the old Lu-AI-GUI repository, which is a reference only. New code lives in Lu-Core. See the brief, section "Two repositories".


Nov 22, 2026 · @Michael

## Purpose

ANSF gives any model Lu loads a memory that outlasts the chat, the model and the context window, so the user never has to start over. It is AI-facing: it is built for the model to use, and the user benefits through the model.

The deficiencies it targets are ones the owner has watched happen, locally and in the cloud:

- **History limit.** Even large cloud platforms force a new conversation when the context fills, and context is lost.
- **Forgotten environment.** A fresh chat claims there is no installer or toolchain, although the user already supplied everything needed to build an installer, a Windows PE environment, or a Windows or Debian QEMU test setup.
- **Model switches.** When a weaker model (Flint) corrupted a file with a bad line edit and kept making it worse, the owner had to switch to Qwen 3.8 27B to repair it. The second model should inherit the full story of what was tried, with nothing re-explained.
- **Models that don't use memory tools.** Qwen 3.6 had to be told to save and to read memory. Qwen 3.8 does both unprompted. Lu must give the same continuity to both.

## Framework view

The framework nests three layers, and the design follows the nesting. The outer layer, ch⊕α(t), is the human and the AI working together over time; ANSF is how Lu carries that collaboration across time. The middle layer, K = K(α, N, T), is the model's own capability, which depends on its architecture, its scale and the task; this is why Lu keeps a profile of each model. The inner layer, C = f(K, I, A, F), is practical capability: the model's knowledge together with interfaces, authority and feedback. The owner should correct any misreading. ANSF's main contribution is to K and F, and it must never leak into A.

| Factor | What ANSF contributes |
| --- | --- |
| K: knowledge and reasoning | Relevant history, facts and procedures are put in front of the model at each step, so a smaller or newer model reasons as if it had been there all along. |
| I: interfaces | The model reaches memory through the same small surface as everything else, or Lu does it for the model. No new tool is required. |
| A: authority | None. Memory is context, never permission. A past approval, a remembered path or a recalled instruction cannot authorize an effect. |
| F: feedback and recovery | Every command, result, file change and uncertain effect is recorded, so Lu can verify, restore and hand work to another model. |
| ch⊕α(t): human and AI together | The user's goals, corrections and answers are stored as the user's own words, and clarification questions are never asked twice. This is where a vague request or half-finished AI work would otherwise score -1. |

**Model profile (the K(α, N, T) layer).** Lu keeps a record for each model and quantization it runs: what the model handled and what it failed at, by kind of task, and whether it uses memory tools unprompted. Lu uses the profile to choose between Lu-driven and model-driven memory, to decide how much to inject, and to suggest a stronger model when a job is beyond the current one. It is built from the journal's verified outcomes, never from what the model says about itself. The profile also records the delta: for each kind of task, whether an augmentation plug-in such as Cognitum improved the result for that model. The owner's hypothesis is that small models gain the most from augmentation because their reasoning is weak, while larger models reason better but keep the weaknesses of their architecture, which may become more visible with scale. Lu does not assume this; the profile measures it per model and per task, and Lu offers augmentation only where the record shows it helps.

## Three memory horizons

ANSF holds three kinds of memory with different lifetimes and different checks.

| Horizon | Holds | Lifetime | Key rule |
| --- | --- | --- | --- |
| Short-term (working) | The current goal, the user's constraints, open questions, the files and tools in play, the plan so far | The current task; rebuilt from the record, never trusted from a summary | Comes only from the user's own turns and the host's observations |
| Long-term | Past conversations kept as immutable entries, the user's stated preferences, facts about the machine (installed tools, installers, versions), verified procedures | Permanent, but each fact carries its source and when it was last verified | Re-check an environment fact before relying on it; a contradicted fact is marked, never silently kept |
| Operational | Every command run, every file touched, the state of each effect (prepared, executing, applied, failed, uncertain), snapshot references | Permanent journal | Written by the host, never by the model; an uncertain effect is never replayed blindly |

The history limit disappears because the context window becomes a cache. The full record lives on disk and is retrieved by relevance, or directly by source ID when the model already knows what it needs.

## What gets recorded

The execution layer is a CLI plus filesystem access, so Lu cannot know in advance what a command means. Recording is therefore generic: it works for any command and does not depend on a per-tool reader (the existing code has such readers for only two tasks, a Git HEAD read and media facts).

- **Human turns**, marked as host-authenticated. Imported or recalled text is history and never counts as a new user turn.
- **Model output**, including streaming that was cancelled partway.
- **Every command**, its working folder, its full output and its exit status.
- **Every file change**: the path, a hash before and after, and a reference to the pre-edit snapshot.
- **Environment facts** the model or host discovers (a tool, an installer, a version), each with its source and a last-verified time.
- **Decisions and corrections**: the user's answers to clarifying questions, changes of goal, and rejected attempts.
- **Failures**: what was tried, what broke and what restored it, so no model repeats the same mistake.

## How memory reaches the model

Two paths read and write the same store, so the user sees the same continuity whichever model is loaded.

1. **Lu-driven (default for any model).** Before each answer, Lu retrieves what is relevant and places a bounded, labelled block in the prompt: the current goal, relevant past entries with their source IDs, and environment facts with their verification status. After each turn, Lu saves the exchange and its effects itself. This is the path for a model that was never trained on memory tools, such as Qwen 3.6.
2. **Model-driven (for models that use tools well).** A model like Qwen 3.8 that saves and recalls on its own can call memory directly, and can fetch an exact entry by source ID without searching.

Lu observes which path a model needs. If a model does not use memory tools unprompted, Lu keeps the Lu-driven path switched on for it; if it does, Lu's injection can shrink to what the model did not ask for. Retrieved content is always labelled as history or data. It is never presented as an instruction, and text recalled from files, web pages or old chats cannot become a command.

Longer term, the recorded sessions are the raw material for training a model on how to use Lu, with verified outcomes marking which sessions went well. This spec only makes sure the data is captured in a clean, reusable form.

## Storage and communication

- **Storage is SQLite** with real relational columns and indexes: one book per conversation, plus a library index across books. Entries are immutable; edits are new records that point back to the original.
- **Protobuf is the inner language.** Lu core modules, workers and inference engines talk to each other in typed Protobuf messages. Memory requests and results between them use the same contract.
- **JSON appears only at the edges:** the model boundary, the desktop UI, external APIs and one-time import of legacy chat files. New conversations create no JSONL journal.
- **Source IDs are locators, not permission.** A known ID opens one entry directly, with no search, but only after the host checks the active conversation and the book.
- **Large files stay outside the database.** Memory stores a reference and an integrity hash, and the host checks the file is still there and unchanged before it is used.
- **Writes are transactional and crash-safe.** The event is committed to its book first; derived indexes are repaired at startup if they fell behind, and a truncated book is refused, never silently accepted.

## Rules

1. **Memory is not authority.** No remembered approval, path, credential or tool revives a permission. Only a host-authenticated instruction from the user authorizes an effect, and it covers what the user asked for, so Lu does not re-ask about steps the instruction already includes. An action that traces back to external content (a web page, a file, a command's output) instead of the user's instruction does not inherit the user's authority.
2. **Stage, verify, then apply.** Lu, not the model, enforces this for every file write: the new content is written to a temporary file, checked, and only then swapped into place atomically. A model that already works this way is fine; one that does not is protected anyway.
3. **Snapshot before any effect.** The original is preserved before a change, and the snapshot reference is journaled with the effect.
4. **Restore, don't patch.** When a check fails, the first move is to restore the snapshot, not to let the model repair a damaged file.
5. **Repair cap.** Repeated failed repairs are handled by Lu's existing loop guard, the tool that stops token looping and tool looping. It stops the model, reverts to the last good state, then asks the user or offers a stronger model. This spec only requires that the guard counts failed repairs of the same job and triggers the restore.
6. **Check every edit.** Lu confirms what actually changed (size, hashes, that only intended lines moved) and never accepts the model's statement that it worked.
7. **Uncertain means uncertain.** An interrupted effect is marked uncertain and is reconciled before anything is retried.
8. **Facts expire.** An environment fact is re-verified before an effect depends on it; a fact contradicted by a later observation is marked as such.

## Model hand-offs

The record belongs to Lu, not to the model, so changing models mid-job is a normal operation. When the user (or Lu, after the repair cap) switches models, the new model receives a case file built from the record:

1. The user's goal and constraints, in the user's own words.
2. What was tried, in order, with each result and each failure.
3. The current state of every touched file, checked against its snapshot. If a file is damaged, it is restored first, so the new model starts from a known good state.
4. Open questions and anything the user already answered.

The new model's work goes through the same staging, snapshots and checks as the old one's. Nothing about the hand-off lowers the checks.

## Reuse versus rewrite

This is based on the repo's documents and file names, plus a unit-test run on Linux (634 passed, 0 failed, 28 skipped). I have not read the code line by line, so Opus should confirm each call below against the source before relying on it.

| Area | Existing code | Call |
| --- | --- | --- |
| Conversation books and entries | `ansf.rs`, `ansf_live_chat.rs`, `ansf_revisions.rs` | Reuse as the base, if Opus's read confirms the schema fits the record in this spec |
| Library index and direct source IDs | `ansf_library.rs`, `ansf_source_ids.rs` | Reuse |
| Goals and clarifications | `ansf_goal_records.rs`, `ansf_goals.rs` | Reuse |
| Effect and model-turn journals, authority checks | `journal.rs`, `model_turn_journal.rs`, `conversation_authority.rs` | Reuse; these carry the memory-is-not-authority rule |
| Atomic file writes and edit safety | `native_atomic_write.rs`, `native_edit_existing.rs`, `edit_safety.rs` | Reuse as the staging step, extended to cover every write the CLI makes |
| Loop guard | `generation_loop_guard.rs` | Reuse as the basis of the repair cap |
| Beliefs, procedures, semantic search | `ansf_belief_revision.rs`, `ansf_verified_procedures.rs`, `ansf_semantic.rs` | Keep; extend after the core record works |
| Per-tool memory readers | Memory is wired to only two tasks (Git HEAD, media facts) | Do not extend; replace with generic recording |
| **Missing today** | Generic recording of any command and its result; the Lu-driven injection path; a snapshot store; automatic recording of environment facts (a manual ledger of installers and SDK packages already exists in \`operational\_asset\_ledger.rs\`) | New work |

The documents say the memory-to-model link (retrieval handed to a live model) is still open, so the injection path is the largest piece of new work.

## Acceptance tests

Each item is marked with where it can be tested. Anything not run is reported as not tested, never assumed to pass.

| Test | Passing means | Where |
| --- | --- | --- |
| Restart continuity | After a restart, a new turn gets the same goal, history and open questions | Linux |
| Crash mid-write | A kill during a file edit leaves the original intact and the effect marked uncertain | Linux |
| Staged write | A deliberately corrupting edit is caught before it replaces the file | Linux |
| Restore, don't patch, and the repair cap | After repeated failed repairs, the file is restored and the model stopped | Linux |
| Memory is not authority | A recalled old approval is refused for a new effect | Linux |
| Source-ID access | An ID from another conversation is denied; one from this conversation opens directly | Linux |
| Bounded injection | The memory block stays within its budget and every item carries source and status | Linux |
| Model switch | A second model resumes a failed job from the case file with no re-explaining | Linux with a stand-in model; real models on the owner's machine |
| Untrained versus trained model | Qwen 3.6 gets memory through Lu-driven mode; Qwen 3.8 uses it on its own; the user sees the same continuity | Owner's machine, R9700 |
| Toolchain amnesia | A fresh chat knows the installers and tools already supplied and re-checks them | Owner's machine |
| Long session | Days of use without lost context or a forced new chat | Owner's machine |

## Open questions

Decided by the owner:

- The repair cap belongs to the existing loop guard (see Rules).
- Lu's injected memory block is hidden from the chat.
- The user cannot see Lu's memories today. Viewing and editing them is a later feature, not part of this spec.
- The user is responsible for what they tell the model to do. Lu's guards target outside threats: instructions hidden in external content, and running untested code.

Still open:

- [ ] **Secrets and deletion.** Entries are immutable, so deleting a secret needs a redaction that erases the content and keeps only a marker. Lu should not try to decide what a secret is in general. It could recognize obvious key formats and ask once whether the model should forget them. Owner to confirm.
- [ ] **KaNT.** The K(α, N, T) layer is now reflected as a per-model profile (see Framework view). Owner to confirm the profile covers what it should.
- [ ] **Untested or downloaded code.** Whether it runs in an isolated environment, and who decides, is settled in the execution-layer spec.
