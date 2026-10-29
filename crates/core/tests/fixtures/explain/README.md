# EXPLAIN fixtures

Plan output from server versions the Docker stack in `docker/compose.yml` doesn't run, read by `tests/plan_fixtures.rs`. Each file is the server's output unchanged, apart from surrounding whitespace. The SQLite file holds the `EXPLAIN QUERY PLAN` rows as tab-separated columns under a header.

They were captured from the Northwind sample database by the EXPLAIN lab (`tableplus-rs-explain-lab`). The plans hold table and column names, row estimates and the constants of the lab's queries, but no rows of data. Northwind is distributed under the Microsoft Public License (Ms-PL), from https://github.com/pthom/northwind_psql.

| file | server | statement |
|---|---|---|
| `mysql-5.7-dml.json` | MySQL 5.7.44 | `EXPLAIN FORMAT=JSON` of an UPDATE with an IN and an EXISTS subquery |
| `mysql-8.0.46-modern-analyze.txt` | MySQL 8.0.46 | `EXPLAIN ANALYZE` of a recursive CTE joined to window functions |
| `mysql-9.7-dml-json-v2.json` | MySQL 9.7.2 | `EXPLAIN FORMAT=JSON` version 2 of the UPDATE |
| `mariadb-10.6-dml.json` | MariaDB 10.6.28 | `EXPLAIN FORMAT=JSON` of the UPDATE |
| `postgres-12-dml.json` | PostgreSQL 12.22 | `EXPLAIN (FORMAT JSON)` of the UPDATE |
| `sqlite-3.53-modern-query-plan.tsv` | SQLite 3.53.2 | `EXPLAIN QUERY PLAN` of the recursive CTE query |
