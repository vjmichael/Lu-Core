# Task ledger

| # | Piece | Status | Evidence |
| --- | --- | --- | --- |
| 0 | Repo setup: AGENTS.md, CI, ledgers | draft source | Commit on main; CI not yet run |
| 1 | Generic host-authenticated record in ANSF | compiled | ansf_operations.rs + recorder.rs; tests/recorder.rs (restart continuity, authority, settle, segments) pass on Windows and in CI windows-latest + ubuntu-latest (run 45211299380), commit a0fbddf |
| 2 | Snapshot store + before/after manifest | compiled | snapshot.rs + Recorder::record_changes; tests/snapshot.rs (scope check, limits, Git) pass on Windows (core.autocrlf=true) and CI windows+ubuntu (run 45212425343), commit d73a614; copy limits 1 GiB per snapshot, 256 MiB per file (owner-approved 2026-11-23) |
| 3 | File access with staged writes | compiled | files.rs + snapshot take_file/trash; tests/files.rs (staged write, stale, universal read/write, scope, trash/copy/move) pass on Windows and CI windows+ubuntu (run 45217048497), commit 25bf25d; document text extraction left to piece 10; working scope default pending owner approval |
| 4 | Origin tracking | compiled | origin.rs + FileAccess gate. Owner rules 2026-11-23: anything from outside the PC is untrusted; no run/install/delete/send in a turn after untrusted content was read. tests/origin.rs pass on Windows (incl. Mark of the Web) and CI windows+ubuntu (run 45220877829), commit 15cd365 |
| 5 | Command runner | compiled | runner.rs + process_tree.rs; tests/runner.rs (record + changes, timeout no orphan, leftover cleanup, cancel, output trim, background report/stop, held by gate, crash reconcile) pass on Windows (Job Object); Linux process groups pass in CI ubuntu (run 45223583351), commit 9ce52ab; setsid escape needs cgroups (not built) |
| 6 | Loop guard + restore-first | compiled | loop_guard.rs (stream guard ported from generation_loop_guard.rs, tool-loop guard, repair cap with restore-first recovery persisted in SQLite); tests/loop_guard.rs pass on Windows and CI windows+ubuntu (run 45224645874), commit 8e7a983; repair cap 3 owner-approved 2026-11-23 |
| 7 | Lu-driven memory injection | compiled | memory.rs: bounded, sourced, hidden block (goal, open questions, unknown outcomes, env facts slot, recent commands, relevant older messages), answer saved by Lu, injections logged; tests/memory.rs pass on Windows and CI windows+ubuntu (run 45225956469), commit 9f6cab7; not yet given to a real model |
| 8 | Environment facts | designed | — |
| 9 | Model profile and routing | designed | — |
| 10 | Starter utilities on demand | designed | — |
| 11 | Resource management | designed | — |
| 12 | Hugging Face search + staged download | designed | — |
| 13 | Backends + engine updater | designed | — |
| B | First baseline (ported set builds + passes on Windows and Linux) | compiled | 5cc6af2: 55 passed, 0 failed, 0 ignored on owner PC, CI windows-latest and ubuntu-latest (run 45204512700) |
