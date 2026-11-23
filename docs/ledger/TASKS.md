# Task ledger

| # | Piece | Status | Evidence |
| --- | --- | --- | --- |
| 0 | Repo setup: AGENTS.md, CI, ledgers | draft source | Commit on main; CI not yet run |
| 1 | Generic host-authenticated record in ANSF | compiled | ansf_operations.rs + recorder.rs; tests/recorder.rs (restart continuity, authority, settle, segments) pass on Windows and in CI windows-latest + ubuntu-latest (run 45211299380), commit a0fbddf |
| 2 | Snapshot store + before/after manifest | compiled | snapshot.rs + Recorder::record_changes; tests/snapshot.rs (scope check, limits, Git) pass on Windows (core.autocrlf=true) and CI windows+ubuntu (run 45212425343), commit d73a614; copy limits 1 GiB per snapshot, 256 MiB per file (owner-approved 2026-11-23) |
| 3 | File access with staged writes | compiled | files.rs + snapshot take_file/trash; tests/files.rs (staged write, stale, universal read/write, scope, trash/copy/move) pass on Windows and CI windows+ubuntu (run 45217048497), commit 25bf25d; document text extraction left to piece 10; working scope default pending owner approval |
| 4 | Origin tracking | compiled | origin.rs + FileAccess gate; tests/origin.rs (user runs, injection held / told once, send refused, external code held, origin propagates, everyday text not held, restart) pass on Windows and CI windows+ubuntu (run 45218543984), commit c9ff2b9; 24-char trace threshold and the untrusted-source list are proposals for the owner |
| 5 | Command runner | designed | — |
| 6 | Loop guard + restore-first | designed | — |
| 7 | Lu-driven memory injection | designed | — |
| 8 | Environment facts | designed | — |
| 9 | Model profile and routing | designed | — |
| 10 | Starter utilities on demand | designed | — |
| 11 | Resource management | designed | — |
| 12 | Hugging Face search + staged download | designed | — |
| 13 | Backends + engine updater | designed | — |
| B | First baseline (ported set builds + passes on Windows and Linux) | compiled | 5cc6af2: 55 passed, 0 failed, 0 ignored on owner PC, CI windows-latest and ubuntu-latest (run 45204512700) |
