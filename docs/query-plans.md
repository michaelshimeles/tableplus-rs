# Query plans

How tableplus-rs asks each engine for a plan and reads the answer into one model (`crates/core/src/plan/`). [DECISIONS.md](../DECISIONS.md) says why it works this way. The rules below come from the EXPLAIN lab (`tableplus-rs-explain-lab`), which captured 387 outputs from MySQL 5.6 to 9.7, MariaDB 10.6 to 11.8, PostgreSQL 12 to 18 and SQLite 3.53.

## Statements per engine

| Server | Explain | Explain Analyze |
|---|---|---|
| MySQL before 8.0.18 | `EXPLAIN FORMAT=JSON`, version 1 | the estimate, with a note |
| MySQL 8.0.18 to 8.3 | `EXPLAIN FORMAT=TREE` | `EXPLAIN ANALYZE`, TREE text |
| MySQL 8.4 and later | `EXPLAIN FORMAT=JSON`, version 2 | `EXPLAIN ANALYZE FORMAT=JSON`, version 2 |
| MariaDB | `EXPLAIN FORMAT=JSON` | `ANALYZE FORMAT=JSON` |
| PostgreSQL | `EXPLAIN (FORMAT JSON)` | `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` |
| SQLite before 3.42 | `EXPLAIN QUERY PLAN` | the estimate, with a note |
| SQLite 3.42 and later | `EXPLAIN QUERY PLAN` | the statement itself, measured through `sqlite3_stmt_scanstatus_v2` |

On MySQL 8.4 and later, a setup statement saves the session's `explain_json_format_version` in `@tableplus_explain_json_format` and sets it to 2, and a reset statement puts it back.

Explain Analyze runs the statement read-only, so a function that writes, such as `nextval`, fails:

- **PostgreSQL** runs it in `BEGIN READ ONLY` and rolls back, which also undoes writes to temporary tables.
- **MySQL and MariaDB** turn on `transaction_read_only` (MariaDB before 11.1: `tx_read_only`) and turn off `autocommit`, so the statement opens a read-only transaction. `ROLLBACK AND NO CHAIN NO RELEASE` then undoes writes to InnoDB temporary tables, whatever `completion_type` says, and both settings are put back. `START TRANSACTION` isn't used because it would release `LOCK TABLES` locks.

Neither reaches a transaction the session already has open, so with one open, Explain Analyze asks the user to commit or roll back.

Explain Analyze doesn't block effects that outlast the statement:

- PostgreSQL's temporary sequences and session advisory locks.
- MySQL and MariaDB user variables, `GET_LOCK` locks, and writes to non-transactional temporary tables (MEMORY, MyISAM, Aria, ARCHIVE, CSV).
- A pending `SET TRANSACTION` on MySQL and MariaDB, which the statement uses up and no variable shows.

Explain overwrites a session's own user variables `@tableplus_explain_read_only`, `@tableplus_explain_autocommit`, `@tableplus_explain_json_format` and `@tableplus_explain_sql`, and its prepared statement `tableplus_explain`.

`Connection::explain` runs the setup, the plan request and the reset while it holds the connection, so no other statement on that connection runs in between. The reset runs even when the plan request fails, and a failed reset statement is reported with the plan.

Explain Analyze shows the estimate instead of running anything but a query. The check reads the statement's keywords: only a `SELECT`, `TABLE` or `VALUES` statement runs, after `WITH` clauses that are queries themselves, and never a `SELECT … INTO` or `TABLE … INTO`. On SQLite the driver also refuses a statement that `sqlite3_stmt_readonly` reports as writing.

The check has to read the statement the way the server does:

- **One statement.** Explain refuses text that splits into more than one statement. The driver also prepares the plan request before running it, and the server refuses to prepare a second statement the tokenizer missed.
- **Comments.** MySQL starts a `--` comment only before an ASCII space or control character, so `1 --1` is arithmetic there, and PostgreSQL nests block comments. Explain Analyze refuses MySQL's `/*! … */` comments, whose text the server runs as part of the statement.
- **Characters.** The tokenizer reads every non-ASCII character, a no-break space included, as part of an identifier, as the servers do in UTF-8. Other character sets, such as GBK, can split the bytes differently, which the prepare step catches. On MySQL and MariaDB the text reaches `PREPARE` as a hex literal, so the server reads it in the session's character set and `sql_mode`. A plan request larger than `max_allowed_packet` is refused before anything is sent.
- **SQLite PRAGMAs** are never explained, because SQLite applies some of them while preparing the statement, even under `EXPLAIN QUERY PLAN`.

## Formats

- **MySQL TREE** is one text cell with a line per iterator, nested by four spaces of indentation. Numbers follow in groups, `(cost=0.525 rows=91)` and `(actual time=0.00275..0.0141 rows=91 loops=1)`, and small times can use exponent notation (`312e-6`). `-> Hash` and some others carry no numbers.
- **MySQL JSON version 1** nests query blocks, the operations around them (ordering, grouping, windowing) and the tables they read. It has estimates only, and costs from 5.7 on. A table's estimate is the rows it examines per scan, at its own read and evaluation cost; its `prefix_cost` belongs to the join it ends.
- **MySQL JSON version 2** nests the same iterators as TREE through `inputs`, with actuals under `EXPLAIN ANALYZE`. A `heading` such as `Hash` becomes a node of its own, as TREE prints it on a line of its own. On every pair the lab captured, version 2 lists the same steps as TREE, and it keeps the table, schema, used columns, index, lookup and filter conditions and join algorithm as separate fields, at full precision where TREE rounds to three digits.
- **MariaDB JSON** holds a `query_block`. From 10.11 on a join is a `nested_loop` array; in 10.6 it is a run of repeated `table` keys in one object, so objects are read with duplicate keys kept, in order.
- **PostgreSQL JSON** is an array with one object per statement. Each has a `Plan` tree, whose children sit in `Plans`, beside plan-wide values such as `Planning Time` and `Triggers`. Below a `Gather`, each worker and the leader counts as a loop, so a step's total time is divided by the processes that ran it. Plans parse up to 256 JSON levels deep, enough for a 100-table join.
- **SQLite's query plan** is one row per step, with `id`, `parent`, `notused` and `detail`. It has no estimates and no actuals.
- **SQLite's scan status** is what the driver returns for a measured run: a row for the statement, with the rows it returned and the time spent in `sqlite3_step`, then one per plan step with SQLite's `NLOOP`, `NVISIT`, `EST` and `NCYCLE`.

A parser claims a result by its shape, not by which button ran it. A Run result opens as a plan only when its statement asks for one (`EXPLAIN`, `DESCRIBE`, or MariaDB's `ANALYZE` and `SHOW EXPLAIN`), so a query that returns plan-shaped JSON stays rows. A format without a parser shows the server's raw text. Three look-alikes stay rows: `DESCRIBE orders` on MySQL and MariaDB lists the table's columns, MariaDB's `ANALYZE TABLE` updates statistics, and SQLite's `EXPLAIN` without `QUERY PLAN` lists bytecode.

## Estimates and times

A step has an optional estimate (rows per loop, cost) and optional actuals (rows per loop, loops, and per-loop time to the first and the last row). The first-row time is optional because MariaDB reports durations only. Inclusive time is the last-row time times the loops. Self time is the inclusive time minus the children's, never below zero, and passes through a child the server didn't time, such as MySQL's `-> Hash`.

MySQL prints `rows=0`, often with `cost=0`, on steps it doesn't estimate: temporary tables, and the sort and limit above them. That reads as no estimate and is never flagged as a misestimate. A zero elsewhere is a real estimate, such as PostgreSQL's for an UPDATE without RETURNING.

The statement's time is the longest any step reports, counting an untimed step as the steps below it. In one capture MySQL 8.0.18 reported 48 µs for the limit above a union whose materialization took 12.3 ms. Summing the steps below instead overcounts: a dependent subquery's 4 ms already sits inside the branch that calls it, and the sum came to 18.6 ms. The summary, the tree's bar scale and the flame chart's shares all use this one number.

### MariaDB

Every MariaDB time is a total over all of a step's loops. The [ANALYZE FORMAT=JSON documentation](https://mariadb.com/kb/en/analyze-format-json/) calls `r_total_time_ms` how much time "in total" was spent, and the [timing trackers](https://github.com/MariaDB/server/blob/main/sql/sql_analyze_stmt.h) add up cycles without dividing by the loop count. What a step's time covers differs by step:

- A query block's `r_total_time_ms` spans the whole block, so it covers everything below it.
- A table's `r_table_time_ms` (storage engine calls) plus `r_other_time_ms` (the gap after each call, such as checking its condition) is its own work. A derived table or subquery it materializes is timed by its own query block, so that block's time is added on top.
- A sort's `r_total_time_ms` covers the table it reads directly, but not a temporary table: the join fills that before the sort starts, so the join's time is added on top.

Each step's total over its loops, children included, is divided by its `r_loops`. Because `r_other_time_ms` is billed by gaps between timers, the parts of a block can add up to slightly more than the block's own total, and the block's self time then stays at zero.

### SQLite

SQLite doesn't time steps. It reads a hardware counter around each VDBE instruction (the time-stamp counter on x86, the virtual timer on ARM, both at a fixed rate), calls the difference cycles, and sums them per step. A step's time is its share of the statement's cycles, scaled to the statement's measured time. The format reads "SQLite scan status (times from CPU cycles)", and a `timing` attribute gives the totals. A step's rows per loop are `NVISIT / NLOOP`.

What a step's counters cover depends on the step:

- A loop (`SCAN` or `SEARCH`) counts the instructions that use its own table or index cursor, so a join's loops don't overlap.
- A subquery counts its code range, which holds its children's code, so its time includes theirs.
- A co-routine reports nothing. The `SCAN` that reads it counts the co-routine's code range instead, which would count that time twice under their common parent. The scan's loops, rows and cycles therefore move to the co-routine, and the scan keeps its loops and rows, untimed.
- `MATERIALIZE` counts its code range but no loops, so it carries no actuals. Its children keep their times.

Some instructions still count for two steps, and nothing in the counters says which. A correlated subquery reads the outer loop's columns inside its own range. A sort's range covers the code that fills and empties it, which can compute select-list columns. In the lab's `portable` query, `USE TEMP B-TREE FOR ORDER BY` reports about 190,000 cycles, of which about 167,000 belong to `CORRELATED SCALAR SUBQUERY 1`. The parent's self time then stays at zero.

`EST` is an estimate only for `SCAN` and `SEARCH`; SQLite reports 1 for every other step, which therefore gets none. A measured run also reports building an automatic index as a step of its own (`CREATE AUTOMATIC INDEX ON t(col)`), which `EXPLAIN QUERY PLAN` never prints. It stays in the plan, because its cost is real. The plan's root is always the statement.

## Cost bars

Cost bars in an estimated plan span the steps below a step when those cost more than the step counts. MySQL leaves every temporary table's cost out of the steps that read it, so on the 9.7 union query the limit costs 2.85 while its two branches cost 757 and 31,389. Scaled to the costliest step, nearly every bar was empty, the limit's included. The solid part of a bar is the server's own number, and a paler part covers what the steps below cost, so the top steps fill and every branch keeps its true share. PostgreSQL's costs already include the steps below, so its bars don't change.

Two other rules were tried. Scaling each step to the top of its own cost region made both union branches full, which hid a 20 times difference between them. Bars on table reads alone, scaled to the costliest read, compared a nested loop's per-loop lookup costs with whole scans. A log scale was ruled out earlier because it overstates small branches.

## Tests

`crates/core/tests/postgres.rs`, `mysql.rs` and `sqlite.rs` explain and analyze statements on the Docker stack and the SQLite seed. `sqlite_analyze.rs` checks measured runs against `EXPLAIN QUERY PLAN` and the statement's rows. `plan_fixtures.rs` parses six captured plans from server versions the stack doesn't run, kept in `crates/core/tests/fixtures/explain/`.
