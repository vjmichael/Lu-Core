# Toolchain ledger (owner's Windows PC)

Checked 2026-11-23. Nothing was installed; everything required was already present.

| Tool | Found | Installed by Lu-Core work |
| --- | --- | --- |
| Rust (rustup 1.29.3, rustc/cargo 1.96.0) | stable-x86_64-pc-windows-msvc, target x86_64-pc-windows-msvc | No |
| MSVC build tools | VS Build Tools 2022, VS Build Tools 2026, VS Community 2026; MSVC 14.51.36231 (`cl` not on PATH; normal, cargo finds it via vswhere) | No |
| Windows SDK | 10.0.26100.0 | No |
| WebView2 runtime | 157.0.4341.60 | No |
| Node / npm | 24.18.0 / 11.16.0 | No |
| LLVM / clang | 23.1.5 (C:\Program Files\LLVM) | No |
| Git | 2.56.0.windows.4 | No |
| Protobuf compiler | libprotoc 36.2 | No |
| GitHub CLI | 2.103.2 | No |
| NSIS 3.13 (makensis, C:\Program Files (x86)\NSIS; not on PATH) | Found 2026-11-24 when winget install NSIS.NSIS reported it already installed; used by scripts/build-installer.ps1 | No |
