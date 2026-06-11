# SPG dump-compat report

Generated 2026-06-11T02:55:55Z against SPG `7.22.0`.

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
