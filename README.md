# tableplus-rs

A fast, native database GUI in the spirit of [TablePlus](https://tableplus.com), written in Rust.
One ~30 MB binary, no Electron, no web view: PostgreSQL, MySQL, MariaDB and SQLite, with SSH
tunnels, TLS, a virtualized data grid with staged editing, a structure editor that generates
`ALTER` statements, and a SQL editor with schema-aware autocomplete.

macOS is the primary target (Keychain, native menu bar, `.app`/DMG bundle); it also builds and
runs on Linux, which is where the screenshots below were taken (Xvfb, software rendering).

![Data grid with staged edits](docs/screenshots/table-editing.png)

## Features

**Connections**
- Saved connections with name, colour tag and environment label (local/development/testing/staging/production), grouped and searchable on the welcome screen
- PostgreSQL, MySQL 8, MariaDB, SQLite
- Passwords in the macOS Keychain (a `0600` file elsewhere); optional "ask every time"
- SSH tunnels (password or private key, with passphrase); host keys are checked against `~/.ssh/known_hosts` and a changed key is refused
- TLS: disable / prefer / require / verify-ca / verify-full, optional CA certificate
- Paste a URL (`postgres://…`, `mysql://…`, `mariadb://…`, `sqlite:///path`) to fill the form; **Test** before saving
- Several sessions open at once, switched from the toolbar

**Browsing**
- Sidebar of schemas → tables, views, materialized views, functions, procedures, with item counts and fuzzy search
- `⌘K` / `⌘P` quick open across every object in every schema
- Definitions of views and routines (`Show definition`), copy names, open in a new query

**Data grid**
- Custom-painted, virtualized grid: only visible cells are laid out, so a page of 1,000 rows or a 100,000-row query result scrolls smoothly; sticky header and row numbers, resizable columns, keyboard navigation and multi-row selection
- Server-side pagination (100–20,000 rows per page) with exact row counts, sorting (shift-click for multi-column; the primary key is appended as a tiebreaker so paging stays stable) and a filter bar with `=, <>, <, >, <=, >=, contains, begins/ends with, IN, IS NULL, BETWEEN` and raw SQL conditions
- Inline editing: double-click or Enter/F2, Tab to the next cell; set NULL / DEFAULT / empty string; add, duplicate and delete rows
- Edits are **staged** (yellow = modified, green = inserted, red = deleted), reviewed as SQL, and committed in one transaction with `⌘S` — or discarded
- Copy cells, or rows as TSV / CSV / JSON / `INSERT` statements; filter by a cell's value

**Structure**
- Columns (name, type, nullability, default, comment), indexes and constraints
- Edit columns, add or drop columns and indexes; the generated `ALTER TABLE` / `CREATE INDEX` / `COMMENT` statements are previewed before they run (and can be opened in the SQL editor instead)

**SQL editor**
- Syntax highlighting for each dialect, line numbers
- Autocomplete for keywords, functions, schemas, tables and columns — alias-aware (`u.` lists the columns of `users u`), and quiet inside comments and strings
- Run the statement under the cursor or the selection (`⌘↩`), or everything (`⌘⇧↩`); one result tab per statement plus a Messages tab with timings, affected rows and errors pointing at the failing statement
- Explain (`⌘E`) shows the planner's estimates without running the statement; Explain Analyze (`⌘⇧E`) runs it and measures each step, for MySQL, MariaDB, PostgreSQL and SQLite. Plans open as a collapsible tree with estimated and actual rows, loops and time bars, a Workbench-style graph coloured by how each table is read, and a flame chart; a detail panel lists every value the server reported. A hand-typed `EXPLAIN` gets the same view. Explain Analyze shows the estimated plan instead of running a statement that writes (`INSERT`, `UPDATE`, `DELETE`, a data-changing CTE, `SELECT … INTO`), and runs the rest read-only, so a query that writes through a function such as `nextval` fails instead of changing data. With a transaction already open, Explain Analyze asks you to commit or roll back first.
- Query history per connection, searchable, reopenable
- Results are capped (100,000 rows by default) and can be cancelled

**Import / export**
- Export a whole table (streamed from the server — memory stays flat), the current page, or any query result as CSV (delimiter, header), JSON or SQL `INSERT`s
- Import CSV (header names matched to columns, empty → NULL, batched inserts in one transaction) or run a `.sql` script

**Look and feel**
- Dark and light themes (or follow the system), Inter UI font and JetBrains Mono for data and SQL
- Native macOS menu bar, full-size content view with the toolbar as the title bar, app icon, `.app` bundle and DMG

## Screenshots

| | |
|---|---|
| ![Welcome](docs/screenshots/welcome.png) Saved connections | ![Commit preview](docs/screenshots/commit-preview.png) Reviewing staged changes before `⌘S` |
| ![Two million rows](docs/screenshots/large-table.png) Filtering and sorting a 2,000,000-row table | ![Autocomplete](docs/screenshots/sql-autocomplete.png) Alias-aware column completion |
| ![Results](docs/screenshots/sql-results.png) Run all: one result tab per statement | ![Quick open](docs/screenshots/quick-open.png) `⌘K` quick open |
| ![Structure](docs/screenshots/structure-preview.png) Structure edits become reviewed `ALTER` statements | ![Connection dialog](docs/screenshots/connection-dialog.png) SSH tunnel + successful Test (light theme) |
| ![MySQL, light](docs/screenshots/light-mysql.png) MySQL session next to a PostgreSQL one | ![Explain Analyze](docs/screenshots/explain-tree.png) `⌘⇧E` Explain Analyze on MySQL: estimated and actual rows per step, with a badge on a 72,306× miss |
| ![Plan graph](docs/screenshots/explain-graph.png) The same plan as a graph, with a branch folded by double-clicking its step | ![Flame chart](docs/screenshots/explain-flame.png) Flame chart: width is time, and the details say how far the estimate missed |

All screenshots are produced by [`scripts/screenshots.sh`](scripts/screenshots.sh), which drives the
release build against the dockerized databases with `xdotool`.

## Building

Requires Rust 1.85+ (edition 2024).

```sh
cargo build --release          # binary: target/release/tableplus-rs
cargo run --release
```

`.cargo/config.toml` sets `LIBSQLITE3_FLAGS=SQLITE_ENABLE_STMT_SCANSTATUS` for every cargo command
run inside the repository, and the build needs it: the app links `sqlite3_stmt_scanstatus_v2`,
which the bundled SQLite only has with that flag. Cargo reads that file from the working
directory, so a build started outside the repository (`cargo install --git`, or `--manifest-path`
from elsewhere) fails to link, as does one with `LIBSQLITE3_FLAGS` already set in the environment
without the flag.

**macOS** — build a signed `.app` and a DMG in `dist/` (ad-hoc signature unless `SIGN_IDENTITY` is set):

```sh
scripts/bundle-macos.sh                 # native architecture
UNIVERSAL=1 scripts/bundle-macos.sh     # arm64 + x86_64
```

**Linux** — needs the usual windowing/GL headers and OpenSSL:

```sh
sudo apt-get install libxkbcommon-dev libgl1-mesa-dev libwayland-dev libssl-dev
```

Environment variables:

| Variable | Effect |
|---|---|
| `TABLEPLUS_RS_HOME` | Config directory (default: `~/Library/Application Support/tableplus-rs` on macOS, `~/.config/tableplus-rs` on Linux) |
| `TABLEPLUS_RENDERER=glow\|wgpu` | Force a renderer (default: wgpu on macOS, OpenGL elsewhere) |
| `RUST_LOG` | Log filter, e.g. `tableplus=debug` |

## Trying it with the demo databases

`docker/compose.yml` starts PostgreSQL 17, MySQL 8.4, MariaDB 11 and an SSH bastion, each seeded with
the same shop schema (users, products, orders, 50,000 orders, 2,000,000 analytics events, every
column type the drivers handle). `scripts/demo-home.sh` writes a config directory with saved
connections to all of them plus a generated SQLite file:

```sh
docker compose -f docker/compose.yml up -d --wait
scripts/demo-home.sh /tmp/tp-demo
TABLEPLUS_RS_HOME=/tmp/tp-demo cargo run --release
```

| Connection | Address | User / password |
|---|---|---|
| PostgreSQL | `127.0.0.1:54329/shop` | `tp` / `tp_pass` |
| MySQL 8 | `127.0.0.1:33069/shop` | `tp` / `tp_pass` |
| MariaDB | `127.0.0.1:33070/shop` | `tp` / `tp_pass` |
| SSH bastion | `127.0.0.1:22229`, user `tunnel`, key `docker/sshd/test_key` | reaches `postgres:5432` |
| SQLite | `testdata/shop.sqlite` | |

## Keyboard shortcuts

`⌘` is Ctrl on Linux and Windows.

| Shortcut | Action |
|---|---|
| `⌘K`, `⌘P` | Quick open table / view / function |
| `⌘N` | New connection |
| `⌘T` | New SQL query tab |
| `⌘W` | Close tab |
| `⌘⇧]` / `⌘⇧[`, `Ctrl+Tab` | Next / previous tab |
| `⌘↩` | Run current statement (or selection) |
| `⌘⇧↩` | Run all statements |
| `⌘E` | Explain the current statement (estimated plan) |
| `⌘⇧E` | Explain Analyze (runs the statement and measures its plan) |
| `←` / `→`, `↑` / `↓`, `Space` | In a plan: collapse / expand, move, toggle |
| `⌘S` | Commit staged changes |
| `⌘R` | Reload |
| `⌘F` | Filter rows |
| `⌘,` | Preferences (macOS menu) |
| `Ctrl+Space` | Show completions |
| `↩` / `F2`, `Tab`, `Esc` | Edit cell, next cell, cancel edit |
| `⌫` / `Delete` | Mark selected rows for deletion |
| `⌘C` | Copy cell; with several rows selected, copy rows as TSV |

## Testing

```sh
cargo test --workspace                                   # unit tests; DB tests skip if unreachable
docker compose -f docker/compose.yml up -d --wait
TABLEPLUS_REQUIRE_DB=1 cargo test --workspace            # integration tests must connect
```

The integration tests (`crates/core/tests/`) run against the real servers: introspection of
schemas/tables/views/routines/indexes/constraints, every column type round-tripping through the
grid's text representation, staged edits committed and rolled back, `ALTER` generation applied to
real tables, CSV import, streaming 250,000 rows, query cancellation, TLS, and connecting through the
SSH bastion, plus Explain and Explain Analyze on each engine. `plan_fixtures.rs` parses plans from
the server versions the stack doesn't run, kept in `crates/core/tests/fixtures/explain/`. CI
(`.github/workflows/ci.yml`) runs rustfmt, clippy with `-D warnings` and the full suite against the
docker databases on Linux, and builds, unit-tests and bundles the app on macOS.

## Performance

Measured on the Linux VPS used for development (release build, Xvfb with Mesa **software**
rendering, so GPU-backed macOS numbers will be better), against the docker databases:

| | |
|---|---|
| Launch to first frame | ~0.5 s |
| Memory at the welcome screen | ~130 MB RSS, of which ~64 MB is Mesa's software rasterizer (`libgallium`/LLVM); ~26 MB heap |
| Browsing the 2,000,000-row table, 300 rows per page | ~138 MB RSS |
| `select * from analytics.events limit 100000` | 127 ms to fetch and display; ~165 MB RSS, flat while scrolling |
| Streaming 250,000 rows through `query_batches` (debug build) | PostgreSQL 0.85 s, MySQL 1.24 s |
| Release binary | ~30 MB (stripped, LTO) |

## Architecture

```
crates/core   tableplus-core — no UI code
  driver/       Connection trait + PostgreSQL (tokio-postgres), MySQL/MariaDB (mysql_async),
                SQLite (rusqlite, on a blocking thread); introspection, text-protocol results,
                transactions, cancellation, streaming query_batches
  tunnel.rs     SSH port forwarding with russh, known_hosts check
  sqlgen.rs     SELECT/COUNT with filters, sorting and paging; UPDATE/INSERT/DELETE for staged edits
  alter.rs      structure diff → ALTER TABLE / CREATE INDEX / COMMENT per dialect
  sqltext.rs    lexer shared by highlighting, statement splitting and completion
  plan/         query plans: one tree model for every engine, the EXPLAIN statement per engine
                and version, and parsers for MySQL TREE and JSON, MariaDB and PostgreSQL JSON,
                and SQLite's query plan and measured scan status
  completion.rs context-aware autocomplete (tables, aliases, columns, keywords, functions)
  export.rs     CSV / JSON / INSERT writers, CSV → INSERT import
  config.rs, store.rs, secrets.rs   saved connections, history, Keychain / 0600 secrets file
crates/app    the egui application (binary: tableplus-rs)
  app.rs        window chrome, sessions, global shortcuts, async task plumbing
  grid.rs       virtualized data grid;  staged.rs  staged edits
  table_tab.rs  data + structure tabs;  structure.rs  structure editor
  query_tab.rs  SQL editor + result tabs;  editor.rs  code editor with highlighting and completion
  plan_view.rs  plan pane: tree with time bars, graph (plan_graph.rs), flame chart (plan_flame.rs)
                and a detail panel
  sidebar.rs, quick_open.rs, welcome.rs, conn_dialog.rs, export_dialog.rs, theme.rs, …
```

Database work runs on a Tokio runtime; the UI thread never blocks. Each request is a small task
whose result is polled by the tab that issued it, which then requests a repaint. Why egui and the
other trade-offs are explained in [DECISIONS.md](DECISIONS.md).

## Not implemented

- Redis and MongoDB (listed as stretch goals) are not supported.
- SSH agent authentication and jump hosts; client TLS certificates.
- Editing query results (only table views are editable — they have a known primary key).
- The macOS build is compiled and unit-tested in CI and its platform code was type-checked locally, but
  the screenshots and interactive testing above were done on Linux.
