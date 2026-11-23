# Lu Execution Layer — Design Spec

> **Reading note.** "The repo", "existing code" and "the repo's design" in this document mean the old Lu-AI-GUI repository, which is a reference only. New code lives in Lu-Core. See the brief, section "Two repositories".


Nov 22, 2026 · @Michael

## Purpose

The execution layer gives any model Lu loads the smallest surface that still lets it do real work: a short list of basic tools (see Basic tools and routing). The owner's own test is the evidence. Qwen produced a Word document with nothing but a command line and filesystem access once the extra toolkit was switched off. Too many tools confuse a model and cost performance, so Lu adds nothing to this surface by default. The browser and the workspaces (NVP, Lu IDE, Cognitum) plug in later and are not part of it.

The layer is AI-facing: its job is to make that small surface reliable, so the model's own intelligence does the work and Lu makes the work safe. It does this in four ways:

- **Authority**, so the user's instruction decides what may happen, without a sandbox and without nagging.
- **Staging and snapshots**, so a bad edit never reaches the real file.
- **Verification**, so a claimed success is checked, never trusted.
- **A journal**, so every effect is recorded and a stronger model can pick up a failed job.

This is the inner layer of the owner's framework, C = f(K, I, A, F). The CLI and filesystem are I, the user's instruction is A, and verification and recovery are F. The ANSF memory spec covers the outer layer.

## Basic tools and routing

The default surface is five basic tools, built into Lu and not supplied by an external server. The owner hit the reason with Jan: a toolkit running as an MCP server timed out on a long video analysis, because the call crossed an IPC bridge and blocked until the job finished. Built-in tools remove that hop. Any job expected to run long returns a task ID at once and finishes in the background. The five are:

1. **Run commands.**
2. **Filesystem:** list, inspect, copy, move and delete to a recoverable trash.
3. **Universal read:** text files as they are. For document formats the existing reader extracts the text, and for other binary files Lu returns the type, the size and a hex preview.
4. **Universal write:** the model gives the content as text, with the path and the extension it chooses, and Lu writes it as is. Binary content can be given as base64 or hex, which Lu decodes. Every write is staged and verified. Format-specific writers are speed shortcuts and are never required.
5. **Web search:** read-only.

Images are not handled by the main model. Lu routes them to a multimodal model.

**Deterministic work belongs to Lu.** The model decides what should happen, and Lu does the how: hashing, archives, format conversion, searching, indexing and verification. These run on the CPU, cost no tokens and leave the GPU free for the model. The pattern is the same as a calculator or code-interpreter tool, with Lu choosing the cheapest suitable processor for each job.

**Hard reasoning goes to Cognitum.** Cognitum is a set of deterministic engines: exact arithmetic, symbolic math, planning, formal logic, optimization, probability, physics and causal reasoning. The model stays the orchestrator and hands this work off. Exact computation is better done by an engine than by any model, so the handoff is the default for it. The per-model profile decides only where Cognitum's softer reasoning help is worth offering.

**Background tasks are Lu's job.** The model starts a task and carries on with something else. Lu tracks the task, journals it, keeps it across restarts, and tells the model when it finishes, with the result and whether it was verified. The model never polls, which saves its context and tokens.

## Shortcuts: utilities on demand

Writing code and running commands takes time and tokens, and a weaker model gets them wrong more often. A utility, such as creating a Word document, reading an archive or editing a CSV, replaces a page of generated code with one structured, checked call. The owner's other finding still holds: too many tools confuse a model. So the surface has three tiers:

1. **Always on:** the five basic tools above.
2. **Utilities, disclosed on demand.** Lu shows a utility to a model only when the task calls for it or the model's profile says it needs the help. A strong model such as Qwen 3.8 27B may never see most of them, and a small model sees more. This is the K(α, N, T) layer at work.
3. **Learned shortcuts.** When a sequence of commands has worked and been verified several times, ANSF's verified-procedure memory can promote it to a one-call shortcut, recorded with its prerequisites and the versions it was tested on.

A utility earns its place only if it measurably beats the command line on time, correctness or safety, the rule already set in the repo's execution-layer philosophy. Every utility goes through the same origin check, snapshot and verification as any other action. Progressive tool disclosure, where tools are revealed as the task needs them, is the standard pattern here; Claude Code and some MCP clients use it.

## Starter utility set

The repo registers about 45 model-facing tools. I sorted them from their names and the repo's own tool audit; I have not run each one, and none of the Office tools has been tested on Windows.

| Utility | Existing tools | Why it earns its place |
| --- | --- | --- |
| Read documents | `lu_reader_read` | A plain file read cannot open a PDF, EPUB or Word file, and OCR needs a library |
| Create Word, PowerPoint and Excel files | `lu_docx_create`, `lu_pptx_create`, `lu_spreadsheet_create` | The owner's Qwen test produced a zero-byte spreadsheet before it rebuilt the file by hand; a checked tool avoids that |
| List and extract archives | `lu_archive_list`, `lu_zip_extract`, `lu_iso_extract`, `lu_linux_archive_extract` | Archive command-line tools differ between Windows and Linux |

**Used by Lu, not called by the model:** `lu_artifact_inspect` checks that a created file really opens and has the expected structure, which is the verification this spec requires. `lu_check_env` finds installed tools for the environment facts in the ANSF spec. `lu_validate_schema` guards the JSON boundary.

**Not exposed to the model:**

- The `lu_reviewed_*` tools are per-task memory readers, replaced by generic recording.
- The `lu_browser_*` and `lu_web_*` tools wait for the later browser plug-in.
- `lu_office_create_revision` and the batch revision tool are advanced editing.
- The `lu_background_*` tools become the backend of the command runner. Git is also left out, because the model runs it through the command line.

The repo's own audit says the background process supervisor is host-planned and not a model-facing runner, so the command-line runner is new work built on top of it.

## Authority

The user's instruction is the authority. When the user tells the model to do something, Lu lets it do that thing and the steps it plainly implies, without stopping to ask again. If the model follows what the user told it, the responsibility is the user's.

The owner's principle is that access is not permission, and permission is not authority:

- **Access** is what the OS account can do. Lu runs as the user, so access is broad.
- **Permission** is a grant someone gave. A past grant or a remembered approval never carries over.
- **Authority** is the host's decision that this specific action traces back to this user's instruction. Only Lu's host decides it, never the model.

The guard is **origin**, not a per-command question. Every action the model proposes carries where it came from:

1. **The user's instruction**, or a step the instruction plainly implies. It runs.
2. **External content**: a web page, a downloaded or opened file, a command's output, a recalled memory. It has no authority. The action is held, and Lu tells the user what content asked for it. This is the defense against prompt injection.

Most existing agent tools (Claude Code, Cline, Open Interpreter) either ask before each command or run inside a sandbox. Lu does neither by default. It tracks where the action came from and spends the user's attention only on actions the user did not ask for.

## Kinds of effect

Lu cannot reliably read a free-text command and know what it will do, so it handles each kind of effect by how it is protected and recovered, not by parsing the command.

| Kind of effect | Examples | What Lu does |
| --- | --- | --- |
| Read | Open or list files, system information | Runs freely and is journaled |
| Write in the working scope | Edit or create files in the project or the folders the user named | Staged write, snapshot, check, journal |
| Write outside the working scope | System folders, other drives, other users' files | Held unless the user's instruction named the place |
| Delete | Remove files or folders | Goes to a recoverable trash with a snapshot reference, never a permanent delete by default |
| Install or change the system | Install a package, change settings | Runs if the user asked; journaled, and the environment facts are updated |
| Network read | Download a page or file | Read-only. The result is external content and cannot trigger anything |
| Network send | Upload, post, send a message | Runs only if the user's instruction asked for it; never from an action that came from external content |
| Run a program or script | Any executable | Runs as the user; code that came from outside gets the extra handling below |
| Long-running process | A server, a build, a watcher | Owned and cleaned up by Lu's process control |

The working scope is the folders the user named for the task or opened as the project. The owner should confirm how scope is defined (see open questions).

## Untrusted content and untested code

The owner's two biggest security concerns are outside prompt injection and running untested code outside a safe environment.

**External content is data.** Web pages, downloaded files, repository text and command output can inform the model but never instruct it. Reading the internet is read-only: a fetch returns text or saves a file marked as external, and nothing runs because it was fetched.

**Origin travels with the content.** A file downloaded from the web carries an external mark. A command that runs that file, or is built from text taken from it, is treated as coming from external content and is held, even if the model proposed it in good faith.

**Untested code has two cases:**

- **Code the model just wrote because the user asked for it.** It runs, with a snapshot first. The user asked, and the responsibility is theirs.
- **Code that came from outside** (a download, a web snippet, a cloned repository, an installer). It is held, and Lu offers to run it in an isolated environment. The isolation is optional and only for this case. The owner dislikes sandboxing the model's normal work, and this keeps that stance. Candidates are Windows Sandbox, a Hyper-V or QEMU virtual machine, or WSL2. The repo already has a design for an isolated KVM virtual machine.

**What this does not solve.** Prompt injection cannot be fully prevented. Lu limits the damage with origin tracking, no network sending from external-origin actions, snapshots before writes, and a journal that shows what happened.

## Process control

Every command Lu runs for the model is an owned process tree, so nothing is left running by accident.

- **Owned tree.** On Windows the command runs inside a Job Object, so its children and grandchildren end with it. On Linux it runs in its own process group or control group. The repo already has a Windows job crate for this.
- **Limits.** Each command has a timeout, a cancel that kills the whole tree, and a cap on how much output goes back to the model.
- **Bounded output.** The model gets a trimmed result with a clear note when output was cut. The full output is stored in the record so nothing is lost.
- **Complete result.** Every result carries the exit status, the working folder, the duration and whether it timed out or was cancelled.
- **Startup reconciliation.** After a crash or restart, Lu finds processes it started and did not finish, ends them if they are still running and marks their effects uncertain.
- **Background processes.** A server or a long build is listed, checked and stopped by Lu, and never left unowned. Lu also tells the model when a background task finishes, so the model never has to poll.

The repo's own acceptance notes list descendant cleanup on a physical Windows machine as unproven, so it is a required test on the owner's machine.

## Verification and recovery

The two basic tools that change things are the **command line** and **file access**. They protect work differently, and the difference is the hard part of this design.

**File access (Lu controls the whole write).** A write goes to a temporary file in the same folder, is checked, and is then swapped into place atomically. Before the swap Lu snapshots the original. The check confirms the size, the hashes, and that only the intended lines changed. This is the owner's temp-file-first rule, enforced by Lu so the model does not have to remember it.

**Command line (Lu cannot see inside the command).** A command such as a script or a `sed` edit can change files Lu never sees being written. Lu's protection here is before and after:

- Before a command, Lu takes a cheap record of the working scope: a manifest of paths, sizes and hashes, plus copies of files that are likely to change.
- After the command, Lu compares and reports what changed. A changed file that fails its check can be restored from the snapshot.
- If the scope is a Git repository, Git is the snapshot. For a large folder with no Git, Lu records a manifest only and tells the user it can detect a change but cannot always undo it. The right size limits for copying are a decision for Opus to propose and the owner to approve.

**Recovery rules** follow the ANSF memory spec. A failed check means restore the snapshot first, never repair a damaged file in place. Repeated failed repairs of the same job go to Lu's existing loop guard, which stops the model, restores the last good state and then asks the user or offers a stronger model. An effect cut off partway is marked uncertain and is reconciled before anything is retried. The model's statement that something worked is never accepted as the result.

## Reuse versus rewrite

This is based on file names, sizes and the repo's documents, plus a Linux unit-test run (634 passed, 0 failed, 28 skipped). I have not read the code line by line, so Opus should confirm each call against the source.

The main change: the repo exposes many specialized tools to the model. The target is two. Existing modules can stay as internal services, but by default the model sees only the command line and file access.

| Area | Existing code | Call |
| --- | --- | --- |
| Process control | `native_background_process.rs` and its control, status, scheduler and result modules; the `lu-win-worker-job` crate | Reuse as the owned-process-tree layer; it is host-planned today, so the model-facing command runner is new work on top |
| Atomic writes and edit safety | `native_atomic_write.rs`, `native_edit_existing.rs`, `edit_safety.rs` | Reuse as the staged-write step for file access |
| Trash and recoverable delete | `native_file_trash_tool.rs`, `native_trash_custody.rs` | Reuse |
| Journals and durability | `journal.rs`, `durable.rs`, `model_turn_journal.rs` | Reuse; the effect journal is the record the ANSF spec needs |
| Loop guard | `generation_loop_guard.rs` | Reuse; connect it to the restore-first recovery |
| Environment discovery | `native_environment_discovery.rs`, `native_capability_inventory.rs` | Reuse; feeds the environment facts in the ANSF spec |
| Authority and permissions | `conversation_authority.rs`, `ce_permissions.rs`, `host_resource_authority.rs` | Rework: the repo grants permission per effect, while this spec decides authority by the origin of each action |
| Per-format and per-app tools | `native_docx_*`, `native_csv_edit.rs`, `native_archive_*`, `native_app_*` and similar | Keep as utilities that Lu discloses on demand (see Shortcuts); do not extend |
| Browser | `browser_*` modules | Out of scope here; a later plug-in |
| **Missing today** | Origin tracking on every proposed action; a command runner with before and after snapshots; a working-scope definition | New work |

## Acceptance tests

Each item says where it can be tested. A test that has not run is reported as not tested, never as passed.

| Test | Passing means | Where |
| --- | --- | --- |
| User instruction runs | "Create this file" runs with no confirmation prompt | Linux |
| Prompt injection | A fetched page that says to delete files produces a held action that does not run, and the user is told which content asked | Linux, stand-in model |
| No sending from external origin | An action that came from external content cannot upload, post or send | Linux |
| Staged write | A deliberately corrupting edit is caught and the original file is untouched | Linux |
| Command scope check | A command that changes files is reported; a changed file that fails its check is restored | Linux |
| Delete is recoverable | A deleted file can be restored from the trash | Linux |
| Timeout and cancel | The whole process tree ends and no orphan remains | Linux; Windows on the owner's machine |
| Crash mid-command | After a restart the leftover process is ended and the effect is marked uncertain | Linux |
| Output cap | Large output is trimmed with a clear note and the full output is stored | Linux |
| External code held | A downloaded script does not run until the user decides | Linux |
| Loop guard | Repeated failed repairs trigger a restore and stop the model | Linux |
| Minimal surface | Qwen 3.8 completes a real task, such as making a Word document, with only the command line and file access | Owner's machine, R9700 |
| Windows cleanup | Descendant processes end with the Job Object | Owner's machine |

## Open questions

- [ ] **Working scope.** Is it the project folder plus any folder the user's instruction names? Should reads outside it be free or held?
- [ ] **Held actions.** When an action from external content is held, should Lu ask the user once, or drop it and log it?
- [ ] **"Plainly implied" steps.** An instruction like "install X" implies downloading code from outside. Should a trusted package manager count as the user's instruction, with other sources held?
- [ ] **Isolation for external code.** Which option first on Windows? Windows Sandbox needs a Pro or Enterprise edition, so WSL2 or a virtual machine may be needed on Home.
- [ ] **Snapshot size limits.** When does Lu copy a folder, rely on Git, or only record a manifest? Opus proposes values and the owner approves them.

## At a glance

&#91;embedded content: one command's path · 1 origin check, 1 result check\]

A held action stops at the origin check. A failed check restores the snapshot, and the loop guard decides whether the model may try again.
