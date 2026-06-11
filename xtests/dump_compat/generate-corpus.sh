#!/usr/bin/env bash
# generate-corpus.sh — re-generate the PG / MySQL / MariaDB dump
# fixtures from real container instances. Idempotent — overwrites
# each <dialect>/<app>/schema.sql in place.
#
# Run when:
#   - bumping postgres/mysql/mariadb container versions
#   - adding a new <app> schema seed under fixtures/
#   - adding a new dialect
#
# Requires: docker.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

# v7.22 (round-13 T2/T3) — container versions are parametrized and
# track current upstream: PG 18 changed several pg_dump emission
# shapes (\restrict wrappers, named inline NOT NULLs) that the
# PG 15 corpus structurally couldn't produce, which is exactly how
# the round-13 gaps stayed invisible. Override via env when probing
# a different major.
PG_IMAGE="${PG_IMAGE:-postgres:18}"
MYSQL_IMAGE="${MYSQL_IMAGE:-mysql:8.4}"
MARIADB_IMAGE="${MARIADB_IMAGE:-mariadb:11.4}"

# ---- shared seed fixtures (per-app, dialect-agnostic-ish SQL) ----

# minimal: single table + index, exercises SET preamble + COMMENT
SEED_MINIMAL_PG=$(cat <<'EOF'
CREATE TABLE posts (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    tags TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_posts_created ON posts(created_at DESC);
COMMENT ON TABLE posts IS 'Blog posts';
COMMENT ON COLUMN posts.title IS 'Post title';
EOF
)

SEED_MINIMAL_MYSQL=$(cat <<'EOF'
CREATE TABLE posts (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_posts_created (created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='Blog posts';
EOF
)

# blog: posts + comments + tags (FK shapes typical to a CMS)
SEED_BLOG_PG=$(cat <<'EOF'
CREATE TABLE authors (
    id BIGSERIAL PRIMARY KEY,
    handle TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE posts (
    id BIGSERIAL PRIMARY KEY,
    author_id BIGINT NOT NULL REFERENCES authors(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    published_at TIMESTAMPTZ,
    is_draft BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE INDEX idx_posts_author ON posts(author_id);
CREATE INDEX idx_posts_published ON posts(published_at DESC) WHERE published_at IS NOT NULL;
CREATE TABLE comments (
    id BIGSERIAL PRIMARY KEY,
    post_id BIGINT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    author_id BIGINT REFERENCES authors(id) ON DELETE SET NULL,
    body TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_comments_post ON comments(post_id);
EOF
)

SEED_BLOG_MYSQL=$(cat <<'EOF'
CREATE TABLE authors (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    handle VARCHAR(64) NOT NULL UNIQUE,
    email VARCHAR(255) NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE TABLE posts (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    author_id BIGINT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    published_at TIMESTAMP NULL,
    is_draft BOOLEAN NOT NULL DEFAULT TRUE,
    KEY idx_posts_author (author_id),
    CONSTRAINT fk_posts_author FOREIGN KEY (author_id) REFERENCES authors(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE TABLE comments (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    post_id BIGINT NOT NULL,
    author_id BIGINT NULL,
    body TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    KEY idx_comments_post (post_id),
    CONSTRAINT fk_comments_post FOREIGN KEY (post_id) REFERENCES posts(id) ON DELETE CASCADE,
    CONSTRAINT fk_comments_author FOREIGN KEY (author_id) REFERENCES authors(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
EOF
)

# forum: hierarchical + composite UNIQUE + partial index
SEED_FORUM_PG=$(cat <<'EOF'
CREATE TABLE forums (
    id BIGSERIAL PRIMARY KEY,
    parent_id BIGINT REFERENCES forums(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    sort_order INT NOT NULL DEFAULT 0,
    UNIQUE(parent_id, slug)
);
CREATE INDEX idx_forums_parent ON forums(parent_id);
CREATE TABLE topics (
    id BIGSERIAL PRIMARY KEY,
    forum_id BIGINT NOT NULL REFERENCES forums(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    pinned BOOLEAN NOT NULL DEFAULT FALSE,
    locked BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_reply_at TIMESTAMPTZ
);
CREATE INDEX idx_topics_forum ON topics(forum_id, pinned DESC, last_reply_at DESC NULLS LAST);
CREATE INDEX idx_topics_active ON topics(last_reply_at DESC) WHERE locked = FALSE;
EOF
)

SEED_FORUM_MYSQL=$(cat <<'EOF'
CREATE TABLE forums (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    parent_id BIGINT NULL,
    name VARCHAR(255) NOT NULL,
    slug VARCHAR(128) NOT NULL,
    sort_order INT NOT NULL DEFAULT 0,
    UNIQUE KEY uq_forum_slug (parent_id, slug),
    KEY idx_forums_parent (parent_id),
    CONSTRAINT fk_forums_parent FOREIGN KEY (parent_id) REFERENCES forums(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE TABLE topics (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    forum_id BIGINT NOT NULL,
    title VARCHAR(512) NOT NULL,
    pinned BOOLEAN NOT NULL DEFAULT FALSE,
    locked BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_reply_at TIMESTAMP NULL,
    KEY idx_topics_forum (forum_id),
    CONSTRAINT fk_topics_forum FOREIGN KEY (forum_id) REFERENCES forums(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
EOF
)

dump_pg() {
    local app="$1"
    local upper=$(echo "$app" | tr a-z A-Z)
    local seed_var="SEED_${upper}_PG"
    local seed="${!seed_var}"
    local outfile="$HERE/pg/$app/schema.sql"
    local container="dump-corpus-pg-$app"
    docker rm -f "$container" >/dev/null 2>&1 || true
    docker run -d --name "$container" \
        -e POSTGRES_USER=u -e POSTGRES_PASSWORD=p -e POSTGRES_DB=app \
        "$PG_IMAGE" >/dev/null
    # Wait for ready
    # postgres containers initdb then RESTART before POSTGRES_DB
    # exists; pg_isready answers during phase one. Probe the target
    # database with a real query instead (same fix as the mysql 8.4
    # readiness race).
    for i in $(seq 1 60); do
        if docker exec -e PGPASSWORD=p "$container" psql -U u -d app -c 'SELECT 1' >/dev/null 2>&1; then break; fi
        sleep 1
    done
    echo "$seed" | docker exec -i -e PGPASSWORD=p "$container" psql -U u -d app -v ON_ERROR_STOP=1 >/dev/null
    docker exec "$container" pg_dump -U u -d app --schema-only --no-owner --no-acl > "$outfile"
    docker rm -f "$container" >/dev/null
    echo "wrote $outfile ($(wc -l < "$outfile") lines)"
}

# v7.15.0 — full pg_dump WITH data (no --schema-only). pg_dump's
# default emits `COPY t (col, col) FROM stdin;` for every table
# with rows, so this fixture exercises the pgwire COPY FROM STDIN
# path that --schema-only had been sidestepping.
dump_pg_with_data() {
    local app="$1"
    local upper=$(echo "$app" | tr a-z A-Z)
    local seed_var="SEED_${upper}_PG"
    local seed="${!seed_var}"
    local data_var="SEED_${upper}_PG_DATA"
    local data="${!data_var:-}"
    local outfile="$HERE/pg/${app}-with-data/schema.sql"
    mkdir -p "$(dirname "$outfile")"
    local container="dump-corpus-pg-${app}-data"
    docker rm -f "$container" >/dev/null 2>&1 || true
    docker run -d --name "$container" \
        -e POSTGRES_USER=u -e POSTGRES_PASSWORD=p -e POSTGRES_DB=app \
        "$PG_IMAGE" >/dev/null
    # postgres containers initdb then RESTART before POSTGRES_DB
    # exists; pg_isready answers during phase one. Probe the target
    # database with a real query instead (same fix as the mysql 8.4
    # readiness race).
    for i in $(seq 1 60); do
        if docker exec -e PGPASSWORD=p "$container" psql -U u -d app -c 'SELECT 1' >/dev/null 2>&1; then break; fi
        sleep 1
    done
    echo "$seed" | docker exec -i -e PGPASSWORD=p "$container" psql -U u -d app -v ON_ERROR_STOP=1 >/dev/null
    if [[ -n "$data" ]]; then
        echo "$data" | docker exec -i -e PGPASSWORD=p "$container" psql -U u -d app -v ON_ERROR_STOP=1 >/dev/null
    fi
    docker exec "$container" pg_dump -U u -d app --no-owner --no-acl > "$outfile"
    docker rm -f "$container" >/dev/null
    echo "wrote $outfile ($(wc -l < "$outfile") lines)"
}

# Seed data for the with-data dumps. PG-side INSERTs that produce
# rows for the same schemas the --schema-only seeds define.
SEED_MINIMAL_PG_DATA=$(cat <<'EOF'
INSERT INTO posts (title, body, tags) VALUES
    ('Hello, world', 'first post', ARRAY['intro','hello']),
    ('Second post', 'body two', ARRAY['follow-up']),
    ('Tab\thandling', 'COPY round-trip must escape \t \n \\ safely', ARRAY[]::TEXT[]),
    ('Quotes ''matter''', 'and so do backslashes \\', ARRAY['edge-cases']);
EOF
)

dump_mysql() {
    local app="$1"
    local image="$2"   # $MYSQL_IMAGE or $MARIADB_IMAGE
    local target_dir="$3"  # mysql or mariadb
    # mariadb 11+ dropped the mysql/mysqldump compat names.
    local client="mysql" dumper="mysqldump"
    if [[ "$target_dir" == "mariadb" ]]; then client="mariadb"; dumper="mariadb-dump"; fi
    local upper=$(echo "$app" | tr a-z A-Z)
    local seed_var="SEED_${upper}_MYSQL"
    local seed="${!seed_var}"
    local outfile="$HERE/$target_dir/$app/schema.sql"
    local container="dump-corpus-${target_dir}-$app"
    docker rm -f "$container" >/dev/null 2>&1 || true
    docker run -d --name "$container" \
        -e MYSQL_ROOT_PASSWORD=p -e MYSQL_DATABASE=app \
        "$image" >/dev/null
    # Wait for ready
    # mysql 8.4+ restarts once during first-boot init; `mysqladmin
    # ping` answers DURING the init phase, so probe with a real
    # query against the target db instead.
    for i in $(seq 1 90); do
        if docker exec "$container" "$client" -uroot -pp -e 'SELECT 1' app >/dev/null 2>&1; then break; fi
        sleep 1
    done
    sleep 1
    echo "$seed" | docker exec -i "$container" "$client" -uroot -pp app
    docker exec "$container" "$dumper" -uroot -pp --no-data --skip-comments app > "$outfile" 2>/dev/null
    docker rm -f "$container" >/dev/null
    echo "wrote $outfile ($(wc -l < "$outfile") lines)"
}


# v7.22 (round-13) — `rich`: a PG-only app whose schema forces every
# round-13 shape OUT OF pg_dump 18 itself: named CHECK constraints,
# UNIQUE NULLS NOT DISTINCT, identity columns, enum types, pg_trgm
# GIN indexes (schema-qualified opclass in the dump), partial
# indexes. NOT NULL columns automatically dump as named inline
# constraints on PG 18. Vector shapes live in the e2e suite instead
# (the stock postgres image has no pgvector).
SEED_RICH_PG=$(cat <<'EOF'
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE TYPE key_kind AS ENUM ('pgp', 'smime');
CREATE TABLE groups (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name TEXT NOT NULL,
    domain TEXT,
    CONSTRAINT groups_name_len_check CHECK (char_length(name) > 0)
);
ALTER TABLE groups ADD CONSTRAINT groups_name_domain_key UNIQUE NULLS NOT DISTINCT (name, domain);
CREATE TABLE encryption_keys (
    id BIGSERIAL PRIMARY KEY,
    group_id BIGINT REFERENCES groups(id) ON DELETE CASCADE,
    key_type key_kind NOT NULL,
    clean_text TEXT
);
CREATE INDEX idx_keys_trgm ON encryption_keys USING gin (clean_text gin_trgm_ops) WHERE clean_text IS NOT NULL;
EOF
)

SEED_RICH_PG_DATA=$(cat <<'EOF'
INSERT INTO groups (name, domain) VALUES ('staff', NULL), ('ops', 'a.com');
INSERT INTO encryption_keys (group_id, key_type, clean_text) VALUES
    (1, 'pgp', 'semi;colon and tab\t inside'),
    (2, 'smime', NULL);
-- v7.23 (round-14) — a 1 MiB body: real mail dumps carry thousands
-- of >64KiB TEXT cells; this row pins the storage codec + COPY
-- paths through the gate.
INSERT INTO encryption_keys (group_id, key_type, clean_text)
    VALUES (1, 'pgp', repeat('Z', 1048576));
EOF
)

# v7.22 (T3) — full-data mysqldump/mariadb-dump fixtures: the data
# sections exercise LOCK TABLES / DISABLE KEYS conditional comments
# and multi-row INSERT packing that --no-data structurally omits.
SEED_MINIMAL_MYSQL_DATA=$(cat <<'EOF'
INSERT INTO posts (title, body, tags) VALUES
    ('Hello, world', 'first post', 'intro,hello'),
    ('Quotes ''matter''', 'and; semicolons too', 'edge-cases');
EOF
)

dump_mysql_with_data() {
    local app="$1"
    local image="$2"
    local target_dir="$3"
    local client="mysql" dumper="mysqldump"
    if [[ "$target_dir" == "mariadb" ]]; then client="mariadb"; dumper="mariadb-dump"; fi
    local upper=$(echo "$app" | tr a-z A-Z)
    local seed_var="SEED_${upper}_MYSQL"
    local seed="${!seed_var}"
    local data_var="SEED_${upper}_MYSQL_DATA"
    local data="${!data_var:-}"
    local outfile="$HERE/$target_dir/${app}-with-data/schema.sql"
    mkdir -p "$(dirname "$outfile")"
    local container="dump-corpus-${target_dir}-${app}-data"
    docker rm -f "$container" >/dev/null 2>&1 || true
    docker run -d --name "$container" \
        -e MYSQL_ROOT_PASSWORD=p -e MYSQL_DATABASE=app \
        "$image" >/dev/null
    # mysql 8.4+ restarts once during first-boot init; `mysqladmin
    # ping` answers DURING the init phase, so probe with a real
    # query against the target db instead.
    for i in $(seq 1 90); do
        if docker exec "$container" "$client" -uroot -pp -e 'SELECT 1' app >/dev/null 2>&1; then break; fi
        sleep 1
    done
    sleep 1
    echo "$seed" | docker exec -i "$container" "$client" -uroot -pp app
    if [[ -n "$data" ]]; then
        echo "$data" | docker exec -i "$container" "$client" -uroot -pp app
    fi
    docker exec "$container" "$dumper" -uroot -pp --skip-comments app > "$outfile" 2>/dev/null
    docker rm -f "$container" >/dev/null
    echo "wrote $outfile ($(wc -l < "$outfile") lines)"
}

mkdir -p "$HERE"/{pg,mysql,mariadb}/{minimal,blog,forum}
mkdir -p "$HERE"/pg/{minimal-with-data,rich,rich-with-data}
mkdir -p "$HERE"/{mysql,mariadb}/minimal-with-data

for app in minimal blog forum; do
    dump_pg "$app"
    dump_mysql "$app" "$MYSQL_IMAGE" mysql
    dump_mysql "$app" "$MARIADB_IMAGE" mariadb
done

# v7.15.0 — one full-data fixture exercises the COPY FROM STDIN
# path. Skipped for MySQL/MariaDB since mysqldump emits INSERT
# statements by default (not COPY).
dump_pg_with_data minimal
dump_pg rich
dump_pg_with_data rich
dump_mysql_with_data minimal "$MYSQL_IMAGE" mysql
dump_mysql_with_data minimal "$MARIADB_IMAGE" mariadb

echo "corpus regenerated"
