# PG18 oracle image — v7.38 differential harness.
#
# Stock `postgres:18-alpine` plus our deterministic-output config and
# initdb hooks. Init scripts only run on first start of an empty data
# dir; with `tmpfs:/var/lib/postgresql/data` that means every
# container boot.
FROM postgres:18-alpine
COPY initdb/*.sql /docker-entrypoint-initdb.d/
COPY postgresql.conf /etc/postgresql/postgresql.conf
CMD ["postgres", "-c", "config_file=/etc/postgresql/postgresql.conf"]
