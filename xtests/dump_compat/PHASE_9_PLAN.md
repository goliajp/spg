# Phase 9 — Dump-compat corpus expansion to 13/13 + gate upgrade

The v7.17 epic plan called for adding three more dialect-shaped
dumps and lifting the release gate from 9/9 to 13/13. Current
state: 9/9 corpus entries (3 dialects × 3 apps: minimal / blog /
forum). Phase 9 adds:

* **9.1 WordPress 5.x MySQL dump** — a real `wp_*` schema with
  the canonical Posts / Postmeta / Options / Users / Usermeta
  tables, `utf8mb4_unicode_ci` collation throughout. Source:
  spin up a WordPress 5.x docker image, install the
  Twenty-Twenty-Three theme (or any default), `mysqldump
  wordpress > xtests/dump_compat/mysql/wordpress/schema.sql`.

* **9.2 Rails 6+ PG schema.rb → SQL** — a real Rails 6 / 7
  migrations-converted PG dump. Source: `bin/rails db:schema:dump`
  in a fresh `rails new --database=postgresql` app with the
  default Active Storage / Action Text / Action Mailbox
  generators, then `pg_dump --schema-only` the resulting
  database.

* **9.3 Django 4.x migrations → SQL** — Django's `manage.py
  sqlmigrate` output for a stock `django-admin startproject` +
  `python manage.py startapp users` setup with auth.User
  customisations. Source: PG `pg_dump --schema-only`.

* **9.4 release gate upgrade** — lift `xtests/dump_compat/run.sh`
  pass threshold from 9/9 to 13/13. Required so the four-gate
  ship check (workspace tests + sqllogictest + mailrs + dump-
  compat) needs the expanded corpus to be green.

These three corpora can't be reasonably hand-written — they
need to come from real container outputs to be representative.
The autonomous tick that closed all other Phase 8 work doesn't
have docker/container access; this plan documents the steps for
the operator to run at ship-prep time. Estimated time: 1–2 hours
per dialect (most of it is waiting on `docker pull` and writing
the seed migrations).

Once 9.1–9.3 land their schema.sql files under
`xtests/dump_compat/{mysql,pg}/{wordpress,rails,django}/`,
run `xtests/dump_compat/run.sh` to verify they all parse +
catalog-roundtrip. Any failures surface gaps that the audit
pass missed — fix at the same churn as the corpus addition,
then re-run the full four-gate check.
