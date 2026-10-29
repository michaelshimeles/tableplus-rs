# Design decisions

Why tableplus-rs is built the way it is, and what was traded away.

## GUI toolkit: egui (eframe)

The goal is a TablePlus-like app that starts instantly, stays small in memory, and scrolls through
huge result sets without stutter — on macOS first, but buildable and testable on a headless Linux
box. Options considered:

| Option | Why not (for this app) |
|---|---|
| Tauri / Electron | A web view is exactly the overhead TablePlus is loved for avoiding: 100+ MB per renderer process, slower cold start, and a big DOM table needs its own virtualization anyway. |
| Native AppKit via `objc2` | The most native look, but macOS-only — nothing could be run or screenshotted on the Linux machine this was developed on — and several times the code for every view. |
| gpui (Zed) | Fast and Mac-focused, but its API is still moving with Zed and documentation outside the Zed codebase is thin. |
| iced | Elm-style retained architecture is pleasant, but multi-line code editing with custom highlighting/completion and a custom-painted grid were less mature than egui's equivalents. |
| Slint / Qt | A separate UI language or a C++ toolchain, plus licensing questions for distribution. |

**egui** won on the things that matter most here:

- **The grid.** Immediate mode makes a virtualized grid natural: each frame paints only the rows and
  columns inside the viewport straight onto the painter (text galleys, backgrounds, borders), with
  one interaction rect for the body. A 100,000-row result costs the same per frame as a 30-row one.
- **Startup and footprint.** One statically linked binary, first frame in about half a second even
  on a software-rendered VPS, and a heap of a few tens of MB.
- **Cross-platform with one code path.** The same UI runs on macOS (wgpu → Metal) and Linux (OpenGL
  via glow, which also works under Xvfb's software renderer). Platform specifics are small and
  `cfg`-gated: the Keychain, the `muda` menu bar, and the full-size titlebar.
- **Good text editing.** `TextEdit` with a custom layouter gives a code editor with syntax
  highlighting; the completion popup, gutter and key handling are layered on top.

The cost is that egui widgets don't look like AppKit controls out of the box. That is addressed
with a custom theme (two palettes, 5 px radii, muted borders, macOS-like spacing), custom-painted
segmented controls/chips/icons, bundled fonts, a native menu bar, and a full-size content view so
the toolbar is the title bar — like TablePlus itself. Accessibility comes through AccessKit.

## Fonts: Inter and JetBrains Mono, bundled

SF Pro is the "native" choice, but Apple's licence forbids redistributing it, and loading it from
the system would make layout differ between machines (and fail on Linux). Inter is metrically very
close to SF for UI text; JetBrains Mono is used for data cells and SQL so columns of numbers line
up. Both are SIL OFL and embedded in the binary (licences in `crates/app/assets/fonts`).

## Database drivers: native async crates, text values

`sqlx` was considered and rejected: it is designed around typed decoding into Rust structs, while a
database GUI must show *any* column type — enums, arrays, ranges, intervals, geometric types, JSON,
user-defined types — exactly as the server prints it. So:

- **PostgreSQL**: `tokio-postgres`. A statement is prepared to learn column names and types, then
  run through the simple-query protocol so every value arrives as the server's own text rendering.
  Rows are streamed as they arrive, and cancellation uses the protocol's `CancelToken`.
- **MySQL / MariaDB**: `mysql_async` over the text protocol, for the same reason. Cancellation opens a
  second connection and issues `KILL QUERY`.
- **SQLite**: `rusqlite` with the bundled library (no system SQLite version skew), run on a blocking
  thread; `column_decltype` supplies declared types; cancellation uses `sqlite3_interrupt`.

Values are kept in a small `Value` enum (null, bool, signed/unsigned ints, float, exact decimals
kept verbatim, text, bytes) so
the grid can right-align numbers, show `NULL` distinctly and generate correctly quoted literals.

## Editing model

- Edits are **staged** in memory and shown inline (modified / inserted / deleted), then turned into
  `UPDATE`/`INSERT`/`DELETE` statements that the user can review, and committed in **one
  transaction** — all or nothing.
- Rows are identified by their primary key. Tables without one are still editable: the row is
  matched on all of its original values and limited to a single physical row (`ctid` in
  PostgreSQL, `rowid` in SQLite, `LIMIT 1` in MySQL), as TablePlus does.
- Paging uses `LIMIT/OFFSET` with the primary key appended to the `ORDER BY` as a tiebreaker, so
  rows don't jump between pages after an edit or when sorting by a column with duplicates.

## Streaming exports

Exporting a whole table runs **one** query and streams rows to the file in batches of 5,000
(`Connection::query_batches`). Paging the export with `OFFSET` would be both O(n²) on the server and
inconsistent: without a total order, `OFFSET` pages can overlap — an early version exported
duplicate rows from the 2,000,000-row table exactly this way. Memory stays flat regardless of table
size.

## Concurrency

All database work runs on a multi-threaded Tokio runtime. The UI spawns a task per request and
keeps a one-shot receiver; tabs poll their pending results each frame and the task requests a
repaint when it finishes. The UI thread never waits on the network, and long queries can be
cancelled from the toolbar.

## Secrets

On macOS passwords go to the **Keychain** (`keyring`). On other platforms there is no universally
available secret store — the Secret Service needs a D-Bus session and an unlocked keyring, which
headless servers and CI don't have — so passwords are kept in `secrets.json` in the config
directory with `0600` permissions. Connections can also be set to ask for the password every time.

## SSH and TLS

- **SSH** uses `russh`, a pure-Rust implementation: no libssh2/OpenSSL C dependency for the tunnel,
  and async-native. Host keys are checked against `~/.ssh/known_hosts`: a key that contradicts a
  recorded one is refused (possible man-in-the-middle). Hosts that aren't listed yet are accepted
  without being written to the file — the app never modifies the user's SSH configuration.
- **TLS** uses `native-tls`, i.e. Security.framework on macOS and OpenSSL on Linux, so the system
  trust store and its updates are used. Modes mirror libpq (`disable` … `verify-full`).

## Query plans

EXPLAIN output differs by engine and version: MySQL prints a text tree or one of two JSON shapes,
MariaDB and PostgreSQL print JSON, and SQLite's `EXPLAIN QUERY PLAN` returns rows without numbers.
A separate EXPLAIN lab captured 387 outputs from 18 servers to pin these down.
[docs/query-plans.md](docs/query-plans.md) describes each format and how times and bars are
derived.

- **One model.** Every parser fills the same tree of steps, and inclusive time, self time and the
  estimate ratio are derived in core, so the views never branch on the engine. Parsers recognize
  their format from the result's shape, so a hand-typed `EXPLAIN` gets the plan view too.
- **Which statement to send** is a table of rules by engine, mode and minimum version. MySQL 8.4
  and later explain through JSON format version 2 rather than TREE text, because it keeps tables,
  indexes and conditions as separate fields at full precision. That costs a statement that sets
  the format and one that puts the session's own setting back.
- **Explain Analyze never writes.** It shows the estimated plan, with a note, for any statement but
  a query and where the server can't measure (MySQL before 8.0.18, SQLite before 3.42). The rest
  runs read-only, because a query can still write through a function such as `nextval`. A session
  with its own transaction open is refused, because the read-only setting can't reach it. The
  driver prepares the plan request first, so the server refuses a second statement the tokenizer
  missed. `Connection::explain` holds the connection from the setup to the reset, so another tab's
  statement never lands in between, and a failed reset is shown in Messages.
- **The statement's time is the longest any step reports**, not the root's: MySQL 8.0.18 can report
  microseconds for a limit above a union that took milliseconds to materialize.

## Explain Analyze on SQLite

SQLite has no `EXPLAIN ANALYZE`, but once a statement has run, `sqlite3_stmt_scanstatus_v2` reports
the loops, rows visited, estimate and counter ticks of each `EXPLAIN QUERY PLAN` step.

- **Build flag.** The API exists only when SQLite is compiled with `SQLITE_ENABLE_STMT_SCANSTATUS`.
  `.cargo/config.toml` passes it to every cargo command in the repository, and a build without it
  fails to link. The connection's check of the compile option only covers a library compiled
  without collection, where Explain Analyze falls back to the estimate.
- **Collection only while measuring.** With collection on, SQLite reads the counter around every
  instruction: a self-join of `order_details` ran about 4% slower (7.5 ms against 7.2 ms on an M5
  Max). A connection turns it off when it opens and on only around the measured statement.
- **A second write guard.** The driver refuses any statement that `sqlite3_stmt_readonly` reports
  as writing, so one the request rules misread still never runs.
- **Times are shares of ticks.** SQLite doesn't time steps. A step's time is its share of the
  statement's ticks, scaled to the time spent in `sqlite3_step`, and the plan's format says so.

## Scope cuts

- **Redis and MongoDB** (stretch goals) were not implemented. They aren't relational: they need a key
  browser and a document viewer rather than the grid/structure/SQL model everything else shares, and
  the time went into making the four SQL engines solid instead.
- No SSH agent or jump hosts, no client TLS certificates, and query results (as opposed to tables)
  are read-only.

## Verification

Everything is tested against real servers: `docker/compose.yml` runs PostgreSQL 17, MySQL 8.4,
MariaDB 11.4 and an SSH bastion with seed data (including a 2,000,000-row table and a table with
every supported column type). Integration tests in `crates/core/tests` cover introspection, type
rendering, edits, `ALTER` generation, import, streaming, cancellation, TLS and tunnelling; CI runs
them on every push. The README screenshots are generated by `scripts/screenshots.sh`, which drives
the release build under Xvfb.
