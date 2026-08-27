# MariaDB oracle image — v7.38 differential harness.
#
# Kept separate from MySQL even though wire protocol overlaps:
# MariaDB has diverged on JSON, sequences, system-versioned tables,
# and a handful of date functions. The differential corpus needs to
# catch those drifts.
# D13: pinned exactly. 12.3.3 is current as of 2026-08-27.
# The `COPY initdb/*.sql` that used to be here outlived the directory --
# the last script in it was deleted as redundant and this line was not,
# so the image could not be rebuilt from its own Dockerfile for days.
# Nothing noticed, because `docker compose up` reuses a built image.
FROM mariadb:12.3.3
COPY server.cnf /etc/mysql/conf.d/oracle.cnf
