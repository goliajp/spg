# MySQL oracle image — v7.38 differential harness.
#
# 9.7 LTS is the long-lived MySQL line; `--server-id` defaults are
# fine for our single-node container. `my.cnf` flips replication +
# durability knobs to favour fast tmpfs-only smoke runs.
# D13: `lts` alias resolved mechanically on landing day (2026-08-17)
# -> 9.7.2 (the 26.x year-line is `innovation`; the oracle rides LTS).
FROM mysql:9.7.2
COPY my.cnf /etc/mysql/conf.d/oracle.cnf
