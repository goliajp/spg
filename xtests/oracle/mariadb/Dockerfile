# MariaDB 11 oracle image — v7.38 differential harness.
#
# Kept separate from MySQL even though wire protocol overlaps:
# MariaDB has diverged on JSON, sequences, system-versioned tables,
# and a handful of date functions. The differential corpus needs to
# catch those drifts.
FROM mariadb:11
COPY initdb/*.sql /docker-entrypoint-initdb.d/
COPY server.cnf /etc/mysql/conf.d/oracle.cnf
