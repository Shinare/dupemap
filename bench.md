
| step | result |
|---|---|
| generate manifests | 1.46 s |
| rows generated | 999966 |
| manifest bytes | 149 MB |
| import all manifests | 6.98 s |
| import rate | 143215 rows/s |
| peak RSS during import | 152 MB |
| database size | 142 MB |
| open database + build index | 551.0 ms |
|   build stages | sources 0 ms, folders 68 ms, files 301 ms, contents 118 ms, ordering 17 ms, copies 31 ms, summaries 14 ms |
| index: files / folders / contents / dup groups | 842009 / 46950 / 276850 / 218805 |
| index memory (estimate) | 46 MB |
| RSS after index build / peak during build | 143 MB / 172 MB |
| first screen: all sources + each source root | 0.0 ms |
| list largest folder (14603 files, by name) | 41.3 ms |
| same folder sorted by size | 14.2 ms |
| folder stats, no filter (uncached) | 4.6 ms |
| folder stats, filter unique only | 4.4 ms |
| folder stats, filter not present in first source | 7.7 ms |
| new filter (has copies in other sources): all folder stats | 11.1 ms |
| list with cached filter | 0.0 ms |
| duplicate groups: top 100 of 218805 | 1.6 ms |
| inspector: locations of top group (3 copies) | 0.3 ms |
| highlight folders for a selection (11 folders) | 0.0 ms |
| duplicate groups: a page of 200 halfway down (200 rows) | 1.3 ms |
| duplicate groups sorted by size of each copy: first page (sorts every group) | 22.9 ms |
| same order, another page (cached) | 0.0 ms |
| duplicate groups across sources: count and extra bytes (218762 groups) | 3.7 ms |
| inspector: where a folder's copies are, slowest source or drive | 17.4 ms (380983 files) |
| summary by file type, all sources (38 extensions) | 7.1 ms |
| summary by file type, biggest source (380983 files) | 1.9 ms |
| search names, no match: 0 hits | 176.0 ms |
| search names, no match (again): 0 hits | 155.2 ms |
| search names, a word: 8463 hits | 179.9 ms |
| search names with Polish letters: 8899 hits | 159.1 ms |
| search names, pattern *.mkv: 10000 hits+ | 147.7 ms |
| search a full SHA-256: 5 hits | 0.6 ms |
| search the first 8 digits of a hash: 5 hits | 0.6 ms |
| treemap of all sources (8525 boxes) | 10.1 ms |
| treemap of the biggest source (6292 boxes) | 4.7 ms |
| remove manifest-PC2-usb (192256 rows, 0 restored) | 676.7 ms |
| re-import it (overlaps manifest-PC2) | 1.87 s |
