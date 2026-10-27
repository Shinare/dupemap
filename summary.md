### Windows build: success

- Commit: 5235ff720fa74a48f4ff7ea5519c812874ca4c68
- Toolchain: rustc 1.99.1 (58cf8f9c3 2026-10-13)
- Steps: test **success**, build **success**, smoke test **success**, app screenshots **success**
- Screenshots: branch `ci-shots/pr-7`
- Tests: 151 passed, 0 failed

<details><summary>Benchmark, 1M generated rows</summary>


| step | result |
|---|---|
| generate manifests | 1.21 s |
| rows generated | 999966 |
| manifest bytes | 149 MB |
| import all manifests | 13.03 s |
| import rate | 76732 rows/s |
| peak RSS during import | 152 MB |
| database size | 142 MB |
| open database + build index | 1.48 s |
|   build stages | sources 0 ms, folders 150 ms, files 1015 ms, contents 202 ms, ordering 29 ms, copies 61 ms, summaries 18 ms |
| index: files / folders / contents / dup groups | 842009 / 46950 / 276850 / 218805 |
| index memory (estimate) | 44 MB |
| RSS after index build / peak during build | 141 MB / 169 MB |
| first screen: all sources + each source root | 0.0 ms |
| list largest folder (14603 files, by name) | 127.2 ms |
| same folder sorted by size | 32.6 ms |
| folder stats, no filter (uncached) | 12.7 ms |
| folder stats, filter unique only | 12.5 ms |
| folder stats, filter not present in first source | 19.8 ms |
| new filter (has copies in other sources): all folder stats | 26.1 ms |
| list with cached filter | 0.0 ms |
| duplicate groups: top 100 of 218805 | 2.1 ms |
| inspector: locations of top group (3 copies) | 0.3 ms |
| highlight folders for a selection (11 folders) | 0.0 ms |
| duplicate groups: a page of 200 halfway down (200 rows) | 3.7 ms |
| duplicate groups sorted by size of each copy: first page (sorts every group) | 35.1 ms |
| same order, another page (cached) | 0.0 ms |
| duplicate groups across sources: count and extra bytes (218762 groups) | 5.9 ms |
| inspector: where a folder's copies are, slowest source or drive | 28.2 ms (380983 files) |
| summary by file type, all sources (38 extensions) | 8.6 ms |
| summary by file type, biggest source (380983 files) | 4.5 ms |
| search names, no match: 0 hits | 407.4 ms |
| search names, no match (again): 0 hits | 407.7 ms |
| search names, a word: 8463 hits | 605.1 ms |
| search names with Polish letters: 8899 hits | 446.0 ms |
| search names, pattern *.mkv: 10000 hits+ | 279.0 ms |
| search a full SHA-256: 5 hits | 397.3 ms |
| search the first 8 digits of a hash: 5 hits | 326.1 ms |
| remove manifest-PC2-usb (192256 rows, 0 restored) | 1.49 s |
| re-import it (overlaps manifest-PC2) | 4.09 s |

</details>
