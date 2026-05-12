#!/usr/bin/env bash
# Ephemeral local Postgres for swell development and tests.
#
# Usage:
#   scripts/dev-pg.sh start   — initdb (idempotent) + pg_ctl start, prints DATABASE_URL
#   scripts/dev-pg.sh stop    — pg_ctl stop
#   scripts/dev-pg.sh reset   — stop + remove data dir + start fresh (drops all DBs)
#   scripts/dev-pg.sh status  — pg_ctl status
#   scripts/dev-pg.sh psql    — open a psql shell against the test DB
#   scripts/dev-pg.sh url     — print the DATABASE_URL (after start)
#
# Lives entirely under $PROJECT_ROOT/.postgres-data and listens on a unix
# socket at $PROJECT_ROOT/.postgres-sock — never binds a TCP port.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PGDATA="$ROOT/.postgres-data"
PGSOCK="$ROOT/.postgres-sock"
PGLOG="$PGDATA/postgres.log"
DBNAME="${SWELL_DBNAME:-swell_test}"
URL="postgres://postgres@localhost/${DBNAME}?host=${PGSOCK}"

# Postgres refuses to run as root. If we are root, pick an unprivileged user
# to drop privileges to for the server process. SWELL_PG_USER overrides.
PGUSER_NAME="${SWELL_PG_USER:-wojtek}"

# Wraps a command so that it runs as PGUSER_NAME when we're root, otherwise
# runs it directly. Keeps PATH/env intact (we need rustc, postgres etc. from
# the nix dev shell).
as_pguser() {
  if [ "$(id -u)" -eq 0 ]; then
    # Use runuser which preserves env and doesn't need a login shell.
    # `runuser` resets PATH; -w preserves it. Same for sudo --preserve-env=PATH.
    sudo -u "$PGUSER_NAME" --preserve-env=PATH -- "$@"
  else
    "$@"
  fi
}

ensure_socket_dir() {
  mkdir -p "$PGSOCK"
  if [ "$(id -u)" -eq 0 ]; then
    chown "$PGUSER_NAME": "$PGSOCK"
  fi
}

ensure_data_owner() {
  if [ "$(id -u)" -eq 0 ] && [ -d "$PGDATA" ]; then
    chown -R "$PGUSER_NAME": "$PGDATA"
  fi
}

cmd_start() {
  ensure_socket_dir
  if [ ! -d "$PGDATA/base" ]; then
    mkdir -p "$PGDATA"
    if [ "$(id -u)" -eq 0 ]; then chown "$PGUSER_NAME": "$PGDATA"; fi
    echo "→ initdb in $PGDATA"
    as_pguser initdb --pgdata="$PGDATA" --auth=trust --username=postgres --no-locale --encoding=UTF8 > /dev/null
  fi
  ensure_data_owner
  if as_pguser pg_ctl --pgdata="$PGDATA" status > /dev/null 2>&1; then
    echo "✓ already running"
  else
    echo "→ pg_ctl start (socket: $PGSOCK)"
    as_pguser pg_ctl --pgdata="$PGDATA" \
      -l "$PGLOG" \
      -o "-k '$PGSOCK' -h '' -c plan_cache_mode=force_generic_plan" \
      start
  fi
  if ! as_pguser psql -h "$PGSOCK" -U postgres -lqt | cut -d\| -f1 | grep -qw "$DBNAME"; then
    echo "→ createdb $DBNAME"
    as_pguser createdb -h "$PGSOCK" -U postgres "$DBNAME"
  fi
  echo "DATABASE_URL=$URL"
}

cmd_stop() {
  if as_pguser pg_ctl --pgdata="$PGDATA" status > /dev/null 2>&1; then
    as_pguser pg_ctl --pgdata="$PGDATA" stop -m fast
  else
    echo "(not running)"
  fi
}

cmd_reset() {
  cmd_stop || true
  rm -rf "$PGDATA" "$PGSOCK"
  cmd_start
}

cmd_status() {
  as_pguser pg_ctl --pgdata="$PGDATA" status 2>&1 || true
}

cmd_psql() {
  if [ "$(id -u)" -eq 0 ]; then
    exec sudo -u "$PGUSER_NAME" --preserve-env=PATH -- psql -h "$PGSOCK" -U postgres -d "$DBNAME" "$@"
  else
    exec psql -h "$PGSOCK" -U postgres -d "$DBNAME" "$@"
  fi
}

cmd_url() {
  echo "$URL"
}

case "${1:-}" in
  start)  cmd_start ;;
  stop)   cmd_stop ;;
  reset)  cmd_reset ;;
  status) cmd_status ;;
  psql)   shift; cmd_psql "$@" ;;
  url)    cmd_url ;;
  *)
    echo "usage: $0 {start|stop|reset|status|psql|url}" >&2
    exit 2
    ;;
esac
