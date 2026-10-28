
| step | result |
|---|---|
| generate manifests | 1.62 s |
| rows generated | 999966 |
| manifest bytes | 149 MB |
| import all manifests | 6.75 s |
| import rate | 148249 rows/s |
| peak RSS during import | 152 MB |
| database size | 140 MB |
| open database + build index | 435.9 ms |
|   build stages | sources 0 ms, folders 57 ms, files 240 ms, contents 88 ms, ordering 14 ms, copies 26 ms, summaries 10 ms |
| index: files / folders / contents / dup groups | 842009 / 46950 / 279721 / 218552 |
| index memory (estimate) | 46 MB |
| RSS after index build / peak during build | 141 MB / 170 MB |
| first screen: all sources + each source root | 0.0 ms |
| list largest folder (14603 files, by name) | 29.7 ms |
| same folder sorted by size | 10.5 ms |
| folder stats, no filter (uncached) | 3.6 ms |
| folder stats, filter unique only | 3.8 ms |
| folder stats, filter not present in first source | 6.2 ms |
| new filter (has copies in other sources): all folder stats | 8.1 ms |
| list with cached filter | 0.0 ms |
| duplicate groups: top 100 of 218552 | 1.1 ms |
| inspector: locations of top group (3 copies) | 0.2 ms |
| highlight folders for a selection (11 folders) | 0.0 ms |
| duplicate groups: a page of 200 halfway down (200 rows) | 1.0 ms |
| duplicate groups sorted by size of each copy: first page (sorts every group) | 13.7 ms |
| same order, another page (cached) | 0.0 ms |
| duplicate groups across sources: count and extra bytes (218147 groups) | 1.9 ms |
| inspector: where a folder's copies are, slowest source or drive | 12.3 ms (380983 files) |
| summary by file type, all sources (38 extensions) | 3.8 ms |
| summary by file type, biggest source (380983 files) | 1.0 ms |
| search names, no match: 0 hits | 133.5 ms |
| search names, no match (again): 0 hits | 118.3 ms |
| search names, a word: 8463 hits | 142.4 ms |
| search names with Polish letters: 8899 hits | 124.7 ms |
| search names, pattern *.mkv: 10000 hits+ | 92.6 ms |
| search a full SHA-256: 5 hits | 0.5 ms |
| search the first 8 digits of a hash: 5 hits | 0.5 ms |
| treemap of all sources (8525 boxes) | 7.8 ms |
| treemap of the biggest source (6292 boxes) | 3.4 ms |
| remove manifest-PC2-usb (192256 rows, 142111 restored) | 1.54 s |
| re-import it (overlaps manifest-PC2) | 1.93 s |
