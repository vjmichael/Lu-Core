# AGENTS.md — Lu-Core

Rules for any agent (human or AI) working in this repository. The design lives in `docs/design/`; start with `BRIEF_FOR_OPUS.md`.

## Branching
- **Main only.** Commit directly to `main`. No branches, no pull requests.
- If a branch is ever unavoidable, reconcile it into `main` when finished and delete it.
- Commit small and often. CI (`.github/workflows/ci.yml`) is for verification only; it never gates or merges.

## Old repository
- Lu-AI-GUI is a reference only. Never clone it into this folder or anywhere permanent on the owner's PC.
- Fetch only files being ported, into a temporary folder outside this clone; delete it afterwards.
- Each ported file's commit message names the old file it came from.

## Engineering rules
- Inner communication: typed Protobuf. Storage: SQLite with relational columns. JSON only at the model, UI, external and legacy-import boundaries. No new JSONL journals.
- Basic tools are built in: no MCP server, no Docker, no mandatory sandbox.
- Windows is primary; everything must also build and pass tests on Linux.
- Lu core must not depend on NVP, Lu IDE or Cognitum.
- Dev builds use an isolated identity and data folder; never touch the owner's real Lu chats, memory or models.
- Never commit secrets, keys, tokens or credentials.
- Complete implementations, not skeletons. If something cannot be completed, say what is missing.

## Status and reporting
- Status levels: designed, draft source, compiled, desktop integrated, real-model exercised, installed accepted, released.
- Never write "done" without the level and the evidence. Disclose every skipped test.
- Only the owner can mark anything "installed accepted".
- Toolchain installs are recorded in `docs/ledger/TOOLCHAIN.md`; tasks in `docs/ledger/TASKS.md`.
