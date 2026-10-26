# DupeMap

Custom cross-machine duplicate file comparator based on hashes.

Explore and compare file-hash manifests from several machines, find duplicates, and (later
phases) export delete scripts. Windows desktop app in Rust; this repository currently contains
Phases 0 and 1: the test-data generator and the core library, plus three command-line tools.

## Layout

| path | what |
|---|---|
| `crates/core` | `dupemap-core`: manifest parsing and validation, SQLite storage, the in-memory duplicate index, synthetic data generator. All logic lives here and is unit-tested. |
| `crates/tools` | `dupemap-gen` (synthetic manifests), `dupemap-check` (import real manifests and print reports), `dupemap-bench` (timings). |
| `crates/app` | Phase 2+: the egui GUI. |

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

## Try it on your manifests

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

`dupemap-gen` writes eight manifests that mimic three PCs, USB drives (scanned twice), a TrueNAS
box with two pools, Google Drive and OneDrive, with awkward names and deliberate bad lines, plus
`expected.json` describing what an import should report.
