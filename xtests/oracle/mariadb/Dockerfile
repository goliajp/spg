# MariaDB oracle image — v7.38 differential harness.
#
# Kept separate from MySQL even though wire protocol overlaps:
# MariaDB has diverged on JSON, sequences, system-versioned tables,
# and a handful of date functions. The differential corpus needs to
# catch those drifts.
# D13: `lts`/`latest` both resolve to 12.3.2 on landing day (2026-08-17).
FROM mariadb:12.3.2
COPY initdb/*.sql /docker-entrypoint-initdb.d/
COPY server.cnf /etc/mysql/conf.d/oracle.cnf
