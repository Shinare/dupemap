# DupeMap

Custom cross-machine duplicate file comparator based on hashes.

Explore and compare file-hash manifests from several machines, find duplicates, and (later
phases) export delete scripts. Windows desktop app in Rust. Done so far: the test-data generator,
the core library with three command-line tools (Phases 0–1) and the app shell (Phase 2).

## Layout

| path | what |
|---|---|
| `crates/core` | `dupemap-core`: manifest parsing and validation, SQLite storage, the in-memory duplicate index, synthetic data generator. All logic lives here and is unit-tested. |
| `crates/tools` | `dupemap-gen` (synthetic manifests), `dupemap-check` (import real manifests and print reports), `dupemap-bench` (timings). |
| `crates/app` | `DupeMap.exe`, the desktop app (egui). |

## Build (Windows)

1. Install Rust with rustup from <https://rustup.rs> and choose the default MSVC toolchain
   (it installs the Visual Studio C++ build tools if they are missing; SQLite is compiled
   from source, so a C compiler is needed).
2. In this folder:

```
cargo test --release
cargo build --release
```

Binaries end up in `target\release\`.

## The app

```
target\release\DupeMap.exe
```

* **Import**: File → Import manifests (Ctrl+O), or drop `manifest-*.tsv` files, or a folder of
  them, anywhere on the window. Importing a file with the same name replaces the earlier import.
  An import report opens afterwards; alerts (non-SHA-256 hashes, malformed lines) stay visible in
  the status bar and as a ⚠ on the source's tab until you dismiss them (File → Manifests → Report).
* **Tabs**: every tab shows all sources or one source (the Source box above the table). Add tabs
  with `+` or Ctrl+T, drag a tab to the side of another to split the view, close with ×.
* **Columns**: *Size* — the bar is the share of the parent folder, its colour the absolute size
  (darker blue = smaller, log scale 1 MB → 100 GB). *Files* — files in the folder. *Copies* — for a
  file, every copy anywhere including itself (pill colour: 2, 3, 4–5, 6+); for a folder, how many of
  its files have another copy. *Elsewhere* — for a file, copies in **other** sources; for a folder,
  the share of its bytes that also exist outside it (✔ all = every file has a copy outside).
  Unique files are dimmed. Sort by Name or Size by clicking the header; drag header edges to resize.
* **Keyboard**: ↑ ↓ PgUp PgDn Home End move, → expands, ← collapses or goes to the parent,
  Enter toggles, Ctrl+C copies the path.
* **Right-click** a folder or file: copy path or name, show in Explorer (for paths on this PC),
  expand or collapse everything below, **ignore this folder**.
* **Ignored folders** (File → Ignored folders): folders matching a rule are left out completely —
  not shown, not counted, and their files never count as copies. Windows system folders, Program
  Files, ProgramData, `$Recycle.Bin`-style folders, AppData and ZFS `.zfs` snapshot folders are
  ignored out of the box; `node_modules`, `.git` and `__pycache__` are offered switched off. Add
  your own folders or patterns, for every source or just one: `D:\Games\Steam`, `?:\Temp` (any
  drive), `**\node_modules` (anywhere), `/mnt/tank/.recycle`, `gdrive:/Trash`. Matching ignores
  case. Changes apply when you press Apply (the index is rebuilt; nothing is re-imported).

It draws with OpenGL 3, which every GPU driver of the last decade provides. In a virtual machine
or remote session without GPU drivers, copy `opengl32.dll` and `libgallium_wgl.dll` from a
[Mesa for Windows](https://github.com/pal1000/mesa-dist-win/releases) build (x64 folder) next to
`DupeMap.exe`; that is how the CI runner, which has no GPU, runs it.

The database lives in `%LOCALAPPDATA%\DupeMap\dupemap.db`; `DupeMap.exe --db D:\other.db` uses
another one. `DupeMap.exe --db C:\tmp\demo.db --demo 1000000` fills a separate database with
generated demo data. The app reads manifests only; it has no network access and never deletes
anything.

## Try it on your manifests (command line)

```
target\release\dupemap-check C:\Hashes\manifest-*.tsv
target\release\dupemap-check --db C:\Hashes\dupemap.db C:\Hashes\manifest-*.tsv
```

It prints, per manifest, the rows imported and any alerts (non-SHA-256 hashes, malformed lines
with line numbers and samples, duplicate listings, overlaps with other manifests), then a
per-source summary and the biggest duplicate groups with every location. Exit code 1 means at
least one manifest raised an alert. With `--db` the database is kept; re-running with a
manifest of the same file name replaces the earlier import.

## Manifest rules

* Format: UTF-8 (BOM allowed; UTF-16 is decoded too), tab-separated, header
  `hash<TAB>size<TAB>host<TAB>path`, CRLF or LF.
* Hashes must be 64 hex characters (SHA-256). `sha256:<hex>` is accepted. Anything else
  (`quickxor:`, `md5:`, unprefixed MD5/SHA-1...) is **flagged**: imported, shown with an alert,
  and only ever matched against rows with the same algorithm.
* Lines with missing columns, a non-numeric size, an empty hash/host/path, or a path without a
  file name are **malformed**: skipped, counted and sampled with their line numbers.
* Each distinct `host` value is a source. Paths are split at their anchor: `D:\`, `\\server\share\`,
  `/`, or an rclone remote such as `gdrive:`. Windows names compare case-insensitively.
* Linux names that are not valid UTF-8 are shown with `�` and their exact bytes are kept.

## How duplicates are counted

* Two files are copies when algorithm, digest and size are all equal.
* A path listed twice is one file, never two copies: the same path twice in one manifest
  (overlapping scan folders) keeps one listing; the same path in two manifests (e.g. a USB disk
  scanned by both Windows scripts) keeps one listing, preferring SHA-256, then the newer scan
  (manifest file date), then the later import. Removing a manifest restores what it hid.
  Cloud remotes are the exception: Google Drive allows identical names, so same-path rows there
  stay separate files.
* For every folder, under any filter: total size, file count, files with another copy anywhere,
  and the bytes/files whose content also exists **outside that folder**. "Fully duplicated
  elsewhere" means every file in the folder has a copy outside it.

## Tests and benchmarks

```
cargo test --release                          # unit tests + brute-force cross-checks
target\release\dupemap-gen --out C:\tmp\gen --rows 1000000
target\release\dupemap-bench --rows 1000000
```

To check real manifests end to end (every path reproduced exactly, copy counts equal a plain
group-by of hash and size), point the optional test at a folder of them; nothing is committed:

```
set DUPEMAP_REAL_MANIFESTS=C:\Hashes
cargo test --release --test real_manifests -- --nocapture
```

`dupemap-gen` writes eight manifests that mimic three PCs, USB drives (scanned twice), a TrueNAS
box with two pools, Google Drive and OneDrive, with awkward names and deliberate bad lines, plus
`expected.json` describing what an import should report.
