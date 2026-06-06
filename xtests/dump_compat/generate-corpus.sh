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
        postgres:15 >/dev/null
    # Wait for ready
    for i in $(seq 1 30); do
        if docker exec "$container" pg_isready -U u >/dev/null 2>&1; then break; fi
        sleep 1
    done
    echo "$seed" | docker exec -i -e PGPASSWORD=p "$container" psql -U u -d app -v ON_ERROR_STOP=1 >/dev/null
    docker exec "$container" pg_dump -U u -d app --schema-only --no-owner --no-acl > "$outfile"
    docker rm -f "$container" >/dev/null
    echo "wrote $outfile ($(wc -l < "$outfile") lines)"
}

dump_mysql() {
    local app="$1"
    local image="$2"   # mysql:8 or mariadb:10.11
    local target_dir="$3"  # mysql or mariadb
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
    for i in $(seq 1 60); do
        if docker exec "$container" mysqladmin ping -uroot -pp --silent >/dev/null 2>&1; then break; fi
        sleep 1
    done
    sleep 2
    echo "$seed" | docker exec -i "$container" mysql -uroot -pp app
    docker exec "$container" mysqldump -uroot -pp --no-data --skip-comments app > "$outfile" 2>/dev/null
    docker rm -f "$container" >/dev/null
    echo "wrote $outfile ($(wc -l < "$outfile") lines)"
}

mkdir -p "$HERE"/{pg,mysql,mariadb}/{minimal,blog,forum}

for app in minimal blog forum; do
    dump_pg "$app"
    dump_mysql "$app" mysql:8.0 mysql
    dump_mysql "$app" mariadb:10.11 mariadb
done

echo "corpus regenerated"
