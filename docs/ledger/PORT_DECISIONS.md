# Port-or-rewrite decisions

Source: Lu-AI-GUI @ 3d637f7 (2026-11-22), shallow copy in a temp folder outside this clone.
Method: read each module's header, schema and public API, and traced its `crate::` dependencies. Not a full line-by-line review; each port gets a full read when it is ported.

## First port set (the baseline)

These modules are dependency-closed. They need no Tauri, no desktop and no tool registry.

| Module (old path `src-tauri/lu-core/src/`) | Lines | In-crate deps | Decision | Why |
| --- | --- | --- | --- | --- |
| `ansf.rs` | 587 | artifact_links, goal_records, goals, live_chat, semantic | Port | SQLite books/chapters/entries/edits plus FTS5 already match the spec. Roles lack `command`/`file_change`, so the generic record (piece 1) goes in a new table beside it, not by widening this one |
| `ansf_live_chat.rs` | 810 | ansf | Port | Typed event rows with host/legacy provenance and verified user-turn IDs = the host-authenticated human turn |
| `ansf_revisions.rs` | 263 | ansf | Port | Append-only edits |
| `ansf_source_ids.rs` | 209 | ansf | Port | Direct source-ID access |
| `ansf_library.rs` | 405 | none | Port | Cross-book index |
| `ansf_goals.rs`, `ansf_goal_records.rs` | 274 + 166 | ansf | Port | Goals and clarifications |
| `ansf_artifact_links.rs` | 255 | ansf | Port | Reference + hash for large files (spec: "large files stay outside the database") |
| `ansf_semantic.rs` | 341 | ansf, source_ids | Port | `ansf.rs` imports it; cheaper to port than to cut out |
| `journal.rs` | 499 | contracts | Port | SQLite effect journal with PREPARED→EXECUTING→APPLIED/FAILED/UNCERTAIN and restart reconciliation, as the spec requires |
| `contracts.rs` | 137 | none | Port | Shared types for the journal |
| `model_turn_journal.rs` | 362 | model_json_boundary | Port with model_json_boundary | Model-boundary audit; the coverage map says keep it as is |

About 4,300 lines plus their tests. Baseline = these build and their ported tests pass on Windows and Linux.

## Calls in the specs that the source contradicts

| Spec call | What the source shows | Revised decision |
| --- | --- | --- |
| `generation_loop_guard.rs`: "reuse as the basis of the repair cap" | 89 lines. It only watches one token stream for runaway bytes and repetition. No tool-loop counting, no failed-repair counting, no restore hook | **Rewrite** for piece 6: keep its stream check, and add per-job failed-repair counting that triggers snapshot restore. The spec's "existing setting" for the cap doesn't exist; I propose 3 failed repairs of the same job (pending owner) |
| `native_atomic_write.rs`: "reuse as the staged-write step" | 137 lines, **create-new only** (fails if the file exists). No replace, no pre-swap snapshot, no content check | **Rewrite** for piece 3: temp file in the same folder, verify, snapshot original, atomic replace. Keep its create-new path |
| `durable.rs`: "reuse" | Hard-codes a check for `native_archive_tree_tool::TOOL_ID`; replays from Protobuf receipts | **Rework** later: remove the tool-specific branch. Not in the first set |
| `continuity.rs` | Stores JSON payloads (10 serde_json uses) | **Rework** when it is needed: typed columns, no JSON inside SQLite |
| `conversation_authority.rs`: "rework" | Confirmed: per-task capability grants; the header itself says it was never wired in | **Rewrite** for piece 4 as origin tracking; port only its user-turn verification idea |
| `native_edit_existing.rs`: "reuse" | 1,803 lines, tied to `windows_file_guard` and `wire` (the old tool registry) | **Rewrite** inside piece 3; too coupled to the old tool surface to port cheaply |
| `native_background_process.rs`: "reuse" | 2,300 lines, depends on `tasks` (1,922) and observe/egress modules | **Rewrite** the runner for piece 5; port only `lu-win-worker-job`'s Job Object code (1,136-line `lib.rs`) |

## Defaults proposed (pending owner approval)

- **Snapshot size cap (no Git):** copy files that are likely to change, up to 256 MiB per command and 64 MiB per single file. Past the cap, record a manifest (path, size, hash) only and tell the user that undo isn't possible.
- **Repair cap:** 3 failed repairs of the same job, then restore and stop.

## Old-test evidence (Windows, owner's PC, 2026-11-23)

`cargo test --lib` filtered to ansf, journal, native_atomic_write, generation_loop_guard, model_turn_journal, conversation_authority, native_trash_custody: **49 passed, 0 failed, 4 ignored**. The 4 ignored tests are outside the first set (ffprobe, git HEAD, Python process). They need real external programs and were not run. This is the old code's behaviour, the reference that ported copies must match. It is not evidence for Lu-Core itself.

## Baseline recorded (2026-11-23)

Commit 5cc6af2 ports the first set unchanged and adds 6 of the old integration test files (the ones that use only ported modules). 55 tests pass, 0 fail and 0 are ignored, on the owner's PC and in CI on windows-latest and ubuntu-latest (run 45204512700). Status: **compiled**. Not desktop integrated, not real-model exercised.
Old tests left out because they use modules outside the set: `desktop_owner_recovery.rs` (desktop_owner, read_approval_ledger), plus the continuity, conversation_authority and trash tests.

## Review of the ported set (2026-11-23, commit 55e038f)

Every ported file was read in full. Four bugs were fixed and out-of-scope code removed (see the commit message). 4,697 → 4,099 lines; 55 tests pass on Windows and Linux CI.

Left as is, for later pieces:
- `ansf_semantic`: search returns an error once a book holds more than 8,192 indexed chunks. That's a hard limit for a long chat; it needs a nearest-neighbour index before semantic recall is used for real.
- `model_json_boundary`: the code-fence repair only accepts LF line endings, so a fenced reply with CRLF fails to parse. Small; fix when the model boundary is wired.
- Wiring: nothing calls these modules yet. The ANSF book, the effect journal and the model audit are three separate SQLite files with no shared caller. That caller is piece 1.

## Piece 10a: document reader (rewrite, not port)

Old: Lu-AI-GUI src-tauri/lu-core/src/native_reader.rs, native_reader_pdf.rs, native_reader_docx.rs, native_reader_office.rs, native_reader_odf.rs, native_reader_epub.rs, native_reader_rtf.rs (about 2,700 lines). Reviewed native_reader.rs and native_reader_pdf.rs line by line.

Decision: rewrite as `reader.rs` on vetted crates. The old PDF adapter hand-parsed PDF and refused files with object streams (`/ObjStm`), multi-stream `/Contents`, or any non-ASCII glyph bytes (font maps), which covers most PDFs written by Word, LibreOffice or a browser; it also capped files at 12 MiB and 128 pages. New: `pdf-extract` 0.12 (lopdf) for PDF, `calamine` 0.38 for xlsx/xls/xlsb/ods, `zip` 9 + `quick-xml` 0.42 for docx/pptx/odt/odp/epub, own RTF parser. Parsers run under `catch_unwind`; zipped parts are read with a 64 MiB inflate cap (zip bombs); documents over 64 MiB are refused.

Not covered, and reported as such: OCR for scanned PDFs, encrypted PDFs and password-protected workbooks, legacy binary .doc/.ppt. Not ported: the old XLSX range selector (`range:Sheet!A1:B9`); sheets are read whole.
