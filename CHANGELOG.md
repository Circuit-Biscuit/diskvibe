# Changelog

## [0.1.0]

First release.

- **Explorer** — folder tree, breadcrumb with back / forward / up, sortable table with multi-column sort, right-click actions, recycle-bin delete.
- **One filter language everywhere** — `size > 100mb and (ext in (mp4, mkv) or name ~ setup) and not hidden`, with a no-typing condition builder and quick chips. The same filter drives the Explorer, Treemap, Charts, Duplicates and Reports.
- **Treemap and charts** — click a tile to zoom in; click a slice or bar to list exactly those files.
- **Duplicates** — matched by size, then BLAKE3 content hash; keep newest / oldest / shortest path.
- **Cleanup** — system temp locations, plus cache and junk detection inside a scan (`node_modules`, Rust `target`, Unity `Library`, app caches, crash dumps, leftovers).
- **Advisor** — disk overview with SSD / HDD type and a speed test, pagefile and hibernation advice, and "move these videos off your SSD" suggestions.
- **Organize** — sort a folder's loose files into Videos, Images, Archives, Installers… with preview and undo.
- **Reports** — 14 optional sections in TXT, Markdown, HTML, CSV or JSON.
- **Saved scans** — every scan is saved and the latest reopens on launch.
- **CLI and MCP server** in the same binary, sharing every tool with the app.
- Windows build signed with a self-signed Circuit Biscuit certificate.
