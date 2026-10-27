### Windows build: success

- Commit: e34ea03bb391fe90ad5f23087336934f507300a1
- Toolchain: rustc 1.99.1 (58cf8f9c3 2026-10-13)
- Steps: test **success**, build **success**, exe icon and version **success**, smoke test **success**, app screenshots **success**
- DupeMap.exe: description: DupeMap; file version: 0.1.0; product: DupeMap 0.1.0; original name: DupeMap.exe; icons: 1
- Screenshots: branch `ci-shots/pr-8`
- Tests: 153 passed, 0 failed

<details><summary>Benchmark, 1M generated rows</summary>


| step | result |
|---|---|
| generate manifests | 1.07 s |
| rows generated | 999966 |
| manifest bytes | 149 MB |
| import all manifests | 7.24 s |
| import rate | 138165 rows/s |
| peak RSS during import | 151 MB |
| database size | 140 MB |
| open database + build index | 438.9 ms |
|   build stages | sources 0 ms, folders 52 ms, files 240 ms, contents 95 ms, ordering 15 ms, copies 26 ms, summaries 10 ms |
| index: files / folders / contents / dup groups | 842009 / 46950 / 279721 / 218552 |
| index memory (estimate) | 46 MB |
| RSS after index build / peak during build | 141 MB / 170 MB |
| first screen: all sources + each source root | 0.0 ms |
| list largest folder (14603 files, by name) | 30.0 ms |
| same folder sorted by size | 11.2 ms |
| folder stats, no filter (uncached) | 3.8 ms |
| folder stats, filter unique only | 3.7 ms |
| folder stats, filter not present in first source | 6.2 ms |
| new filter (has copies in other sources): all folder stats | 8.1 ms |
| list with cached filter | 0.0 ms |
| duplicate groups: top 100 of 218552 | 1.2 ms |
| inspector: locations of top group (3 copies) | 0.2 ms |
| highlight folders for a selection (11 folders) | 0.0 ms |
| duplicate groups: a page of 200 halfway down (200 rows) | 0.7 ms |
| duplicate groups sorted by size of each copy: first page (sorts every group) | 13.9 ms |
| same order, another page (cached) | 0.0 ms |
| duplicate groups across sources: count and extra bytes (218147 groups) | 2.9 ms |
| inspector: where a folder's copies are, slowest source or drive | 15.0 ms (169730 files) |
| summary by file type, all sources (38 extensions) | 3.8 ms |
| summary by file type, biggest source (380983 files) | 1.0 ms |
| search names, no match: 0 hits | 134.0 ms |
| search names, no match (again): 0 hits | 116.0 ms |
| search names, a word: 8463 hits | 139.9 ms |
| search names with Polish letters: 8899 hits | 122.8 ms |
| search names, pattern *.mkv: 10000 hits+ | 93.6 ms |
| search a full SHA-256: 5 hits | 0.5 ms |
| search the first 8 digits of a hash: 5 hits | 0.5 ms |
| remove manifest-PC2-usb (192256 rows, 142111 restored) | 1.54 s |
| re-import it (overlaps manifest-PC2) | 1.94 s |

</details>
