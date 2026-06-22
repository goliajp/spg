# MySQL 8 oracle image — v7.38 differential harness.
#
# 8.4 LTS is the long-lived MySQL line; `--server-id` defaults are
# fine for our single-node container. `my.cnf` flips replication +
# durability knobs to favour fast tmpfs-only smoke runs.
FROM mysql:8.4
COPY initdb/*.sql /docker-entrypoint-initdb.d/
COPY my.cnf /etc/mysql/conf.d/oracle.cnf
