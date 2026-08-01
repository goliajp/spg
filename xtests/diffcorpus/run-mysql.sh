#!/usr/bin/env bash
# MySQL 方言面 runner —— SPG 的 mysql-wire 对 MariaDB 11 oracle
set -uo pipefail
export PATH=/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH
HERE="$(cd "$(dirname "$0")" && pwd)"; OUT="$HERE/out"; mkdir -p "$OUT"
SPG() { docker exec -i spg-bench-mariadb mariadb -h host.docker.internal -P 26010 -u root --batch --raw -N --skip-ssl 2>&1 | grep -v '^WARNING'; }
MDB() { docker exec -i spg-bench-mariadb mariadb -h 127.0.0.1 -u root -pbench bench --batch --raw -N --skip-ssl 2>&1 | grep -v '^Warning'; }
for f in "$@"; do
  n="$(basename "$f" .sql)"
  SPG < "$f" > "$OUT/$n.mysql-spg" ; MDB < "$f" > "$OUT/$n.mysql-mdb"
  # 防假阳性:两侧都必须真的产出行,否则「IDENTICAL」只是同样失败
  for side in mysql-spg mysql-mdb; do
    if [ ! -s "$OUT/$n.$side" ] || grep -q 'OCI runtime\|command not found\|Can.t connect' "$OUT/$n.$side"; then
      printf '%-24s ⛔ %s 没有真正跑起来\n' "$n" "$side"; continue 2
    fi
  done
  if diff -u "$OUT/$n.mysql-mdb" "$OUT/$n.mysql-spg" > "$OUT/$n.mysql.diff" 2>&1; then
    printf '%-24s IDENTICAL\n' "$n"
  else
    printf '%-24s %s 行差异\n' "$n" "$(grep -cE '^[+-][^+-]' "$OUT/$n.mysql.diff")"
  fi
done
