# DupeMap

Custom cross-machine duplicate file comparator based on hashes.

Explore and compare file-hash manifests from several machines, find duplicates, and (later
phases) export delete scripts. Windows desktop app in Rust. Done so far: the test-data generator,
the core library with command-line tools (Phases 0–1), the app shell (Phase 2), the compare
features: inspector, linked highlighting, filters and duplicate groups (Phase 3), the summary by
file type with editable categories (Phase 4), the saved session (Phase 5), and: scanning this PC
from the app, scan scripts for other computers, search by name or hash, and importing checksum
lists (`sha256sum` / `rclone hashsum` output).

## Layout

| path | what |
|---|---|
| `crates/core` | `dupemap-core`: manifest parsing and validation, SQLite storage, the in-memory duplicate index, synthetic data generator. All logic lives here and is unit-tested. |
| `crates/tools` | `dupemap-gen` (synthetic manifests), `dupemap-check` (import real manifests and print reports), `dupemap-scan` (the built-in scan from the command line), `dupemap-bench` (timings). |
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

* **Scan this PC** (on the start screen, and File → Scan this PC…): tick *This computer's
  drives*, *USB drives* and/or *Network drives* (each drive can be ticked on its own too) and
  DupeMap hashes their files one drive after another, the same as the export script would, and
  imports each drive's manifest as it finishes. Progress, speed and time left are shown; *Stop*
  keeps what was hashed and the next scan of that drive continues from there (also after a crash
  or power cut). Without administrator rights, folders only an administrator can read are
  skipped, so the window offers *Restart as administrator* (Windows asks first; DupeMap opens
  again with the window). Drive letters mapped in your normal session are not visible to a
  program run as administrator; DupeMap then reaches those network drives through their share and
  still writes their paths with the letter. Like the scripts it skips Windows, Program Files,
  ProgramData, AppData, the recycle bin and (unless you untick it) OneDrive / Google Drive folders,
  never reads online-only cloud files (that would download them), hashes files of 1 MB and up
  (adjustable), reads a spinning or USB disk one file at a time and an SSD with up to four, and
  keeps the computer awake. Manifests go to a `scans` folder beside the database, one per drive
  (`manifest-<PC>-<letter>.tsv`), with the scripts' source names (`<PC> D: (<label>)`), so a
  drive scanned here and by a script is one source.
* **Scan another computer** (start screen, File → Scan another computer…): choose Windows or
  Linux (TrueNAS SCALE, a Linux PC), the same tick boxes, and save the script. Run it there with
  administrator rights — on Windows right-click → *Run with PowerShell* (it asks for administrator
  rights itself), on Linux `sudo bash dupemap-scan-linux.sh` (or with `nohup` for a long run) —
  and import the manifest it writes. It continues where it stopped when run again. On Linux it
  scans each file system on its own (ZFS datasets, USB disks by their bus, NFS/SMB shares) and
  skips `.zfs` snapshots, the system folders of the system disk, TrueNAS's boot pool and system
  dataset, and app data.
* **Import**: File → Import manifests (Ctrl+O), or drop manifests, checksum lists, or a folder of
  them, anywhere on the window. Importing a file with the same name replaces the earlier import.
  An import report opens afterwards; alerts (non-SHA-256 hashes, malformed lines) stay visible in
  the status bar and as a ⚠ on the source's tab until you dismiss them (File → Manifests → Report).
* **Checksum lists** (`hash  path` per line, as `sha256sum` or `rclone hashsum sha256` write them,
  e.g. the OneDrive file from `rclone hashsum sha256 --download onedrive:`) are recognised and
  imported after a short question: which source the files are on (guessed from the file name:
  `onedrive-hashsum.txt` → the rclone remote `onedrive:`) and where its relative paths start. A
  size list (`size<TAB>path`, as `rclone lsf --format sp` writes it; `onedrive-sizes.tsv` next to
  the list is picked up by itself) fills in sizes. Without one, a file takes the size of a copy
  with the same SHA-256 elsewhere; a file with no such copy shows its size as `?` and counts as
  0 bytes. Re-importing the list uses the same answers.
* **Search** (View → Search, Ctrl+F): files and folders in every source (or one) by name — part
  of a name, case and accents ignored (`plaza` finds `Plaża`), or a whole-name pattern with `*`
  and `?` (`*.mkv`, `IMG_20??.jpg`) — or files by hash: paste a SHA-256 (or type its first
  digits; `md5:…` for other hashes) to find every copy. Results sort by name, folder, source,
  size or copies; select one to see it in the inspector, double-click to show it in a tab.
  About a second for 5 million files.
* **Tabs**: every tab shows all sources or one source (the Source box above the table). Add tabs
  with `+` or Ctrl+T, drag a tab to the side of another to split the view, close with ×.
* **Columns**: *Size* — the bar is the share of the parent folder, its colour the absolute size
  (darker blue = smaller, log scale 1 MB → 100 GB). *Files* — files in the folder. *Copies* — for a
  file, every copy anywhere including itself (pill colour: 2, 3, 4–5, 6+); for a folder, how many of
  its files have another copy. *Elsewhere* — for a file, copies in **other** sources; for a folder,
  the share of its bytes that also exist outside it (✔ all = every file has a copy outside).
  Unique files are dimmed. Click any column header to sort by it (again to reverse; numbers sort
  largest first): Copies sorts folders by how many of their files have a copy, Elsewhere by the
  share of their bytes also found outside them. Drag header edges to resize.
  A narrow tab (split views, small screens) drops columns to keep the names readable: Files first,
  then Copies, then Elsewhere; the inspector and tooltips still show everything.
* **Keyboard**: ↑ ↓ PgUp PgDn Home End move, → expands, ← collapses or goes to the parent,
  Enter toggles, Ctrl+C copies the path. Ctrl+F searches, Ctrl+G opens the duplicate groups,
  Ctrl+U the summary by file type, Ctrl+I shows or hides the inspector.
* **Right-click** a folder or file: copy path or name, show in Explorer (for paths on this PC),
  expand or collapse everything below, **ignore this folder**, *Show every copy* / *Where are the
  copies?* (opens the inspector).
* **Inspector** (the panel on the right; View → Show/Hide inspector). Select a file and it lists
  **every copy**, grouped by source, with its path; *Show* opens that copy in a tab (it expands
  the folders and scrolls to it), next to *Copy path* and *Show in Explorer* for files on this PC.
  No copy is called the original: which ones to keep is your decision. Select a folder and it
  shows how much of it has a copy outside it, which sources hold those copies, and in each source
  the folder holding (nearly) all of them, with *Show* to jump there.
* **Linked highlighting**: while a file is selected, every copy of it is shaded orange in every
  tab, and the folders leading to those copies get a fainter shade with a thin orange mark, so
  collapsed folders show where to look. While a folder is selected, the folders holding its
  copies are marked the same way.
* **Filter** (button above each table; every tab has its own): *Unique*, *With copies*, *With a
  copy in another source*, *With no copy in another source* (only on its own machine or drive),
  *Missing from* a chosen source (what that source lacks), and *SHA-256 only* / *Not SHA-256
  (flagged)*. Folder sizes and counts then add up the matching files only. ✕ clears the filter.
* **Duplicate groups** (View → Duplicate groups): every set of identical files, the most space in
  extra copies first (extra copies = every copy beyond one). Narrow it to groups with a copy on a
  chosen source, groups within one source or across sources, 3+ copies, or flagged hashes.
  Click a column header to sort by size of each copy, copies, sources or extra copies. Selecting a
  group lists its copies in the inspector; double-click or Enter opens the first copy in a tab.
* **Summary by file type** (View → Summary by file type, Ctrl+U, or right-click a source): the
  total size of a source, or of all of them, split three ways — *Elsewhere* (the file has a copy
  in another source; solid orange), *Same source* (it has copies, but only within its own source;
  striped orange) and *Unique* (the only copy anywhere; grey). Then the same split by source (click
  one to summarise it), by category (click a category to see its extensions) and by extension.
  Every column sorts.
* **File type categories** (View → File type categories…, or *Categories…* in a summary): a
  category is a list of extensions. Rename, reorder, add or remove categories, edit their
  extensions, or pick from the largest extensions that are in no category yet ("Other").
  Right-click an extension in a summary to count it under another category. Saved in the database;
  *Restore defaults* brings back the built-in list (video, audio, images, documents, archives, disk
  images & backups, software, development, databases, game data).
* **Manifests** (File → Manifests): *Open folder* opens the folder holding a manifest file.
* **Ignored folders** (File → Ignored folders): folders matching a rule are left out completely —
  not shown, not counted, and their files never count as copies. Windows system folders, Program
  Files, ProgramData, `$Recycle.Bin`-style folders, AppData and ZFS `.zfs` snapshot folders are
  ignored out of the box; `node_modules`, `.git` and `__pycache__` are offered switched off. Add
  your own folders or patterns, for every source or just one: `D:\Games\Steam`, `?:\Temp` (any
  drive), `**\node_modules` (anywhere), `/mnt/tank/.recycle`, `gdrive:/Trash`. Matching ignores
  case. Changes apply when you press Apply (the index is rebuilt; nothing is re-imported).

* **Your session is kept.** The app saves what you had open, about a second after every change, and
  restores it at the next start: the tabs and how they are split, per tab the source, sort,
  filters, expanded folders, selected row and scroll position, the inspector (open or not, its
  width and what it shows) and the window (size, position, maximised). It is a small file beside
  the database (`dupemap.session.json`, with a `.bak` copy of the previous one), written to a
  temporary file and swapped in, so a crash or power cut leaves the last good one. Folders and
  files are remembered by database id, so re-importing does not lose your place; anything that no
  longer exists is skipped. View → Reset layout starts again with one tab; `--reset-session`
  deletes the saved session at startup (for example if a window was left on a monitor you
  unplugged).

It draws with OpenGL 3, which every GPU driver of the last decade provides. In a virtual machine
or remote session without GPU drivers, copy `opengl32.dll` and `libgallium_wgl.dll` from a
[Mesa for Windows](https://github.com/pal1000/mesa-dist-win/releases) build (x64 folder) next to
`DupeMap.exe`; that is how the CI runner, which has no GPU, runs it.

The database lives in `%LOCALAPPDATA%\DupeMap\dupemap.db`; `DupeMap.exe --db D:\other.db` uses
another one. `DupeMap.exe --db C:\tmp\demo.db --demo 1000000` fills a separate database with
generated demo data, and `--window 1024x768` opens the window at that size (to try small
screens; it overrides the saved window size); `--scan` opens *Scan this PC* at start. The app
only reads files (manifests, and the drives you scan); it has no network access and never deletes
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
manifest of the same file name replaces the earlier import. Checksum lists are recognised;
`--source onedrive: --base onedrive: --sizes onedrive-sizes.tsv` say where they belong (otherwise
guessed from the file name).

The built-in scan from the command line:

```
target\release\dupemap-scan --list                    # drives and how they are classed
target\release\dupemap-scan --out C:\Hashes D: usb     # D: and every USB drive
target\release\dupemap-scan --folder E:\Photos --host "PC2 E: (Photos)" --out C:\Hashes
```

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

* Two files are copies when algorithm, digest and size are all equal. A file from a checksum
  list has no size: it is a copy of the files with the same algorithm and digest, and takes
  their size.
* A path listed twice is one file, never two copies: the same path twice in one manifest
  (overlapping scan folders) keeps one listing; the same path in two manifests (e.g. a USB disk
  scanned by both Windows scripts) keeps one listing, preferring SHA-256, then a listing with a
  size (over a checksum list's), then the newer scan (manifest file date), then the later import.
  Removing a manifest restores what it hid.
  Cloud remotes are the exception: Google Drive allows identical names, so same-path rows there
  stay separate files.
* For every folder, under any filter: total size, file count, files with another copy anywhere,
  and the bytes/files whose content also exists **outside that folder**. "Fully duplicated
  elsewhere" means every file in the folder has a copy outside it.
* "Where the copies are" (inspector, for a folder): per source, the bytes of the folder's files
  that have a copy there (outside the folder itself; a file can count for several sources), and
  the deepest folder of that source holding copies of at least 90% of those bytes.

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
