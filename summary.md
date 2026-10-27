### Windows build: success

- Commit: 66ff1ac1888637449f1f5b8a6fdc04173ac9acd1
- Toolchain: rustc 1.99.1 (58cf8f9c3 2026-10-13)
- Steps: test **success**, build **success**, smoke test **success**, app screenshots **success**
- Screenshots: branch `ci-shots/pr-6`
- Tests: 141 passed, 0 failed

<details><summary>Benchmark, 1M generated rows</summary>


| step | result |
|---|---|
| generate manifests | 1.59 s |
| rows generated | 999966 |
| manifest bytes | 149 MB |
| import all manifests | 9.58 s |
| import rate | 104385 rows/s |
| peak RSS during import | 151 MB |
| database size | 140 MB |
| open database + build index | 820.6 ms |
|   build stages | sources 0 ms, folders 110 ms, files 474 ms, contents 147 ms, ordering 21 ms, copies 49 ms, summaries 18 ms |
| index: files / folders / contents / dup groups | 842009 / 46950 / 279721 / 218552 |
| index memory (estimate) | 44 MB |
| RSS after index build / peak during build | 139 MB / 168 MB |
| first screen: all sources + each source root | 0.0 ms |
| list largest folder (14603 files, by name) | 66.9 ms |
| same folder sorted by size | 21.1 ms |
| folder stats, no filter (uncached) | 7.5 ms |
| folder stats, filter unique only | 7.8 ms |
| folder stats, filter not present in first source | 13.2 ms |
| new filter (has copies in other sources): all folder stats | 17.6 ms |
| list with cached filter | 0.0 ms |
| duplicate groups: top 100 of 218552 | 2.0 ms |
| inspector: locations of top group (3 copies) | 0.3 ms |
| highlight folders for a selection (11 folders) | 0.0 ms |
| duplicate groups: a page of 200 halfway down (200 rows) | 1.6 ms |
| duplicate groups sorted by size of each copy: first page (sorts every group) | 25.2 ms |
| same order, another page (cached) | 0.0 ms |
| duplicate groups across sources: count and extra bytes (218147 groups) | 2.9 ms |
| inspector: where a folder's copies are, slowest source or drive | 28.5 ms (50145 files) |
| summary by file type, all sources (38 extensions) | 6.7 ms |
| summary by file type, biggest source (380983 files) | 2.2 ms |
| search names, no match: 0 hits | 241.5 ms |
| search names, no match (again): 0 hits | 251.6 ms |
| search names, a word: 8463 hits | 202.2 ms |
| search names with Polish letters: 8899 hits | 182.6 ms |
| search names, pattern *.mkv: 10000 hits+ | 144.2 ms |
| search a full SHA-256: 5 hits | 182.6 ms |
| search the first 8 digits of a hash: 5 hits | 301.2 ms |
| remove manifest-PC2-usb (192256 rows, 142111 restored) | 1.63 s |
| re-import it (overlaps manifest-PC2) | 3.24 s |

</details>
