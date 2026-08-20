# SPG dump-compat report

Generated 2026-08-20T23:08:42Z against SPG `local-build`.

| Dialect | App | Status | Stmts pass/total | First error |
|---|---|---|---:|---|
| pg | blog | PASS | 35/35 |  |
| pg | forum | PASS | 29/29 |  |
| pg | minimal-with-data | PASS | 22/22 |  |
| pg | minimal | PASS | 21/21 |  |
| pg | rich-with-data | PASS | 29/29 |  |
| pg | rich | PASS | 27/27 |  |
| mysql | blog | PASS | 33/33 |  |
| mysql | forum | PASS | 28/28 |  |
| mysql | minimal-with-data | SKIP(wire) | - | mysql data via psql is a transport mismatch; covered by import pass |
| mysql | minimal | PASS | 23/23 |  |
| mariadb | blog | PASS | 34/34 |  |
| mariadb | forum | PASS | 29/29 |  |
| mariadb | minimal-with-data | SKIP(wire) | - | mysql data via psql is a transport mismatch; covered by import pass |
| mariadb | minimal | PASS | 24/24 |  |

## Embed import pass (`spg import`)

| Dialect | App | Import |
|---|---|---|
| pg | blog | PASS |
| pg | forum | PASS |
| pg | minimal-with-data | PASS |
| pg | minimal | PASS |
| pg | rich-with-data | PASS |
| pg | rich | PASS |
| mysql | blog | PASS |
| mysql | forum | PASS |
| mysql | minimal-with-data | PASS |
| mysql | minimal | PASS |
| mariadb | blog | PASS |
| mariadb | forum | PASS |
| mariadb | minimal-with-data | PASS |
| mariadb | minimal | PASS |
