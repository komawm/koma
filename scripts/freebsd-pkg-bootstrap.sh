#!/bin/sh
# freebsd-pkg-bootstrap.sh — bounded, fail-closed FreeBSD pkg bootstrap.
#
# Usage:
#   scripts/freebsd-pkg-bootstrap.sh --packages "git pkgconf ca_root_nss"
#                                    [--packages ...] [options]
#
# POSIX sh on purpose: the FreeBSD guest's default shell is /bin/sh and bash
# is not part of the base system, so VM prerequisite setup cannot depend on
# packages that pkg(8) has not installed yet.
#
# Why this exists: FreeBSD VM images commonly enable multiple repositories
# carrying `pkg` (`FreeBSD-ports`, `FreeBSD-ports-kmods`, `FreeBSD`) which can
# trigger a pkg self-upgrade re-exec loop or mid-sync `pkg update` failures.
# This helper configures a single `FreeBSD` repository on `/latest`, bounds
# every pkg invocation with a wall-clock timeout, retries transient errors, and
# verifies every requested package via `pkg info -e`.
#
# Exit codes: 0 = every requested package is installed,
#             1 = bootstrap or install failed (diagnostics on stderr),
#             2 = usage error.
set -eu
LC_ALL=C
export LC_ALL

PACKAGES=""
REPO_NAME="FreeBSD"
REPO_URL="https://pkg.FreeBSD.org/\${ABI}/latest"
REPOS_DIR="/usr/local/etc/pkg/repos"
BASE_CONFS_DIR="/etc/pkg"
CONF_NAME="99-koma-bootstrap.conf"
ATTEMPTS=2
BACKOFF=10
BOOTSTRAP_TIMEOUT=300
UPDATE_TIMEOUT=300
INSTALL_TIMEOUT=900
DIAGNOSE_TIMEOUT=60
TAIL_LINES=200
PKG_BIN="pkg"
PKG_STATIC_BIN="pkg-static"
SUDO_MODE="auto"
USE_SUDO=0
SKIP_BOOTSTRAP=0
DRY_RUN=0
KEEP_LOGS=0
LOG_DIR=""
LAST_LOG=""
LAST_RC=0

usage() {
  sed -n '2,/^# Exit codes/p' "$0" | sed -e 's/^# \{0,1\}//'
  cat <<'EOF'

Options:
  --packages LIST             Whitespace-separated packages to install (required; repeatable).
  --repo-url URL              Enabled repository URL (default: https://pkg.FreeBSD.org/${ABI}/latest).
  --repo-name NAME            Enabled repository name (default: FreeBSD).
  --repos-dir DIR             Local repo config dir (default: /usr/local/etc/pkg/repos).
  --base-confs-dir DIR        Base repo config dir (default: /etc/pkg).
  --conf-name NAME            Config file name written into --repos-dir (default: 99-koma-bootstrap.conf).
  --attempts N                Attempts per step (default: 2).
  --backoff SECONDS           Initial retry delay; doubles per attempt (default: 10).
  --bootstrap-timeout SECONDS Bound for `pkg-static bootstrap -f` (default: 300).
  --update-timeout SECONDS    Bound for `pkg update -f` (default: 300).
  --install-timeout SECONDS   Bound for `pkg install -y` (default: 900).
  --diagnose-timeout SECONDS  Bound for diagnostic `pkg` probes (default: 60).
  --tail-lines N              Lines dumped from failed step logs (default: 200).
  --pkg BINARY                pkg binary (default: pkg).
  --pkg-static BINARY         pkg-static binary (default: pkg-static).
  --sudo                      Prefix privileged commands with sudo.
  --no-sudo                   Never use sudo.
  --skip-bootstrap            Skip `pkg-static bootstrap -f`.
  --log-dir DIR               Retain logs in DIR instead of a temporary directory.
  --dry-run                   Print planned repo config and commands without executing.
  -h, --help                  Show this help.
EOF
}

usage_error() {
  printf 'freebsd-pkg-bootstrap[usage]: %s\n' "$*" >&2
  printf 'run with --help\n' >&2
  exit 2
}

die() {
  printf 'freebsd-pkg-bootstrap[error]: %s\n' "$*" >&2
  exit 1
}

log() {
  printf 'freebsd-pkg-bootstrap: %s\n' "$*" >&2
}

require_value() {
  [ "$2" -ge 2 ] || usage_error "$1 needs a value"
}

positive_int() {
  case "$2" in
  '' | *[!0-9]*) usage_error "$1 must be a positive integer (got '$2')" ;;
  *) [ "$2" -ge 1 ] || usage_error "$1 must be a positive integer (got '$2')" ;;
  esac
}

parse_packages() {
  # shellcheck disable=SC2086 # the list is intentionally word-split
  for pp_name in $1; do
    case "$pp_name" in
    [A-Za-z0-9]*)
      case "$pp_name" in
      *[!A-Za-z0-9+._-]*) usage_error "invalid package name '$pp_name'" ;;
      esac
      ;;
    *) usage_error "invalid package name '$pp_name'" ;;
    esac
    PACKAGES="$PACKAGES $pp_name"
  done
}

while [ "$#" -gt 0 ]; do
  case "$1" in
  --packages)
    require_value "$1" "$#"
    parse_packages "$2"
    shift 2
    ;;
  --repo-url) require_value "$1" "$#"; REPO_URL="$2"; shift 2 ;;
  --repo-name) require_value "$1" "$#"; REPO_NAME="$2"; shift 2 ;;
  --repos-dir) require_value "$1" "$#"; REPOS_DIR="$2"; shift 2 ;;
  --base-confs-dir) require_value "$1" "$#"; BASE_CONFS_DIR="$2"; shift 2 ;;
  --conf-name) require_value "$1" "$#"; CONF_NAME="$2"; shift 2 ;;
  --attempts) require_value "$1" "$#"; ATTEMPTS="$2"; shift 2 ;;
  --backoff) require_value "$1" "$#"; BACKOFF="$2"; shift 2 ;;
  --bootstrap-timeout) require_value "$1" "$#"; BOOTSTRAP_TIMEOUT="$2"; shift 2 ;;
  --update-timeout) require_value "$1" "$#"; UPDATE_TIMEOUT="$2"; shift 2 ;;
  --install-timeout) require_value "$1" "$#"; INSTALL_TIMEOUT="$2"; shift 2 ;;
  --diagnose-timeout) require_value "$1" "$#"; DIAGNOSE_TIMEOUT="$2"; shift 2 ;;
  --tail-lines) require_value "$1" "$#"; TAIL_LINES="$2"; shift 2 ;;
  --pkg) require_value "$1" "$#"; PKG_BIN="$2"; shift 2 ;;
  --pkg-static) require_value "$1" "$#"; PKG_STATIC_BIN="$2"; shift 2 ;;
  --sudo) SUDO_MODE="yes"; shift ;;
  --no-sudo) SUDO_MODE="no"; shift ;;
  --skip-bootstrap) SKIP_BOOTSTRAP=1; shift ;;
  --log-dir) require_value "$1" "$#"; KEEP_LOGS=1; LOG_DIR="$2"; shift 2 ;;
  --dry-run) DRY_RUN=1; shift ;;
  -h | --help)
    usage
    exit 0
    ;;
  *) usage_error "unknown argument '$1'" ;;
  esac
done

[ -n "$PACKAGES" ] || usage_error "--packages is required"
positive_int "--attempts" "$ATTEMPTS"
positive_int "--bootstrap-timeout" "$BOOTSTRAP_TIMEOUT"
positive_int "--update-timeout" "$UPDATE_TIMEOUT"
positive_int "--install-timeout" "$INSTALL_TIMEOUT"
positive_int "--diagnose-timeout" "$DIAGNOSE_TIMEOUT"
case "$BACKOFF" in
'' | *[!0-9]*) usage_error "--backoff must be a non-negative integer" ;;
esac

if [ "$SUDO_MODE" != "no" ] && [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
  USE_SUDO=1
  ROOT_DESC="sudo env ASSUME_ALWAYS_YES=yes BATCH=yes"
else
  USE_SUDO=0
  ROOT_DESC="env ASSUME_ALWAYS_YES=yes BATCH=yes"
  if [ "$SUDO_MODE" = "yes" ]; then
    usage_error "--sudo was requested but sudo is unavailable or unneeded"
  fi
fi

TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"

run_bounded() {
  rb_limit="$1"
  rb_log="$2"
  shift 2
  if [ -n "$TIMEOUT_BIN" ]; then
    rb_rc=0
    "$TIMEOUT_BIN" -k 15 "$rb_limit" "$@" >"$rb_log" 2>&1 || rb_rc=$?
    return "$rb_rc"
  fi
  "$@" >"$rb_log" 2>&1 &
  rb_pid=$!
  rb_waited=0
  while kill -0 "$rb_pid" 2>/dev/null; do
    if [ "$rb_waited" -ge "$rb_limit" ]; then
      kill -TERM "$rb_pid" 2>/dev/null || true
      sleep 5
      kill -KILL "$rb_pid" 2>/dev/null || true
      wait "$rb_pid" 2>/dev/null || true
      printf 'bounded wait of %ss exceeded; process killed\n' "$rb_limit" >>"$rb_log"
      return 124
    fi
    sleep 1
    rb_waited=$((rb_waited + 1))
  done
  rb_rc=0
  wait "$rb_pid" || rb_rc=$?
  return "$rb_rc"
}

bounded_cmd() {
  bc_bound="$1"
  bc_log="$2"
  shift 2
  if [ "$USE_SUDO" -eq 1 ]; then
    run_bounded "$bc_bound" "$bc_log" \
      sudo env ASSUME_ALWAYS_YES=yes BATCH=yes "$@"
  else
    run_bounded "$bc_bound" "$bc_log" \
      env ASSUME_ALWAYS_YES=yes BATCH=yes "$@"
  fi
}

root_mkdir() {
  if [ "$USE_SUDO" -eq 1 ]; then
    sudo mkdir -p "$1"
  else
    mkdir -p "$1"
  fi
}

root_tee() {
  if [ "$USE_SUDO" -eq 1 ]; then
    sudo tee "$1"
  else
    tee "$1"
  fi
}

repo_present() {
  for rp_dir in "$REPOS_DIR" "$BASE_CONFS_DIR"; do
    [ -d "$rp_dir" ] || continue
    if grep -rlE "^[[:space:]]*${1}[[:space:]]*:" "$rp_dir" 2>/dev/null |
      grep -vxF "$REPOS_DIR/$CONF_NAME" | grep -q .; then
      return 0
    fi
  done
  return 1
}

repo_conf_text() {
  for rct_repo in FreeBSD-ports FreeBSD-ports-kmods; do
    if repo_present "$rct_repo"; then
      printf '%s: { enabled: no }\n' "$rct_repo"
    fi
  done
  printf '%s: { enabled: yes, url: "%s" }\n' "$REPO_NAME" "$REPO_URL"
}

write_repo_conf() {
  root_mkdir "$REPOS_DIR" >/dev/null 2>&1 ||
    die "cannot create $REPOS_DIR (needs root: pass --sudo or run as root)"
  repo_conf_text | root_tee "$REPOS_DIR/$CONF_NAME" >/dev/null
  log "wrote $REPOS_DIR/$CONF_NAME"
}

pkg_vv_repos() {
  awk '
    {
      line = $0
      if (match(line, /^[[:space:]]*[A-Za-z0-9_.+-]+:[[:space:]]*\{[[:space:]]*$/)) {
        name = line
        sub(/^[[:space:]]*/, "", name)
        sub(/[[:space:]]*:.*/, "", name)
        if (prev != "") printf "%s\t%s\t%s\n", prev, en, url
        prev = name; en = "no"; url = ""
        next
      }
      if (match(line, /^[[:space:]]*enabled[[:space:]]*:/)) {
        en = line
        sub(/^[^:]*:[[:space:]]*/, "", en)
        sub(/[^A-Za-z].*$/, "", en)
        next
      }
      if (match(line, /^[[:space:]]*url[[:space:]]*:/)) {
        url = line
        sub(/^[^:]*:[[:space:]]*/, "", url)
        gsub(/["[:space:]]/, "", url)
        next
      }
    }
    END { if (prev != "") printf "%s\t%s\t%s\n", prev, en, url }
  ' "$1"
}

assert_single_repo() {
  if ! bounded_cmd "$DIAGNOSE_TIMEOUT" "$LOG_DIR/pkg-vv-check.log" "$PKG_BIN" -vv; then
    log "'pkg -vv' failed while checking the repository set"
    return 1
  fi
  asr_enabled="$(pkg_vv_repos "$LOG_DIR/pkg-vv-check.log" | awk -F'\t' '$2 == "yes" { print $1 }')"
  asr_count="$(printf '%s\n' "$asr_enabled" | grep -c . || true)"
  if [ "$asr_count" -ne 1 ]; then
    log "expected exactly one enabled repository, found $asr_count: ${asr_enabled:-<none>}"
    return 1
  fi
  if [ "$asr_enabled" != "$REPO_NAME" ]; then
    log "enabled repository is '$asr_enabled', expected '$REPO_NAME'"
    return 1
  fi
  if ! pkg_vv_repos "$LOG_DIR/pkg-vv-check.log" | awk -F'\t' -v n="$REPO_NAME" \
    '$1 == n && $2 == "yes" && $3 ~ /\/latest/ { found = 1 } END { exit !found }'; then
    log "repository '$REPO_NAME' is not on a /latest URL"
    pkg_vv_repos "$LOG_DIR/pkg-vv-check.log" |
      awk -F'\t' -v n="$REPO_NAME" '$1 == n { print "  url: " $3 }' >&2
    return 1
  fi
  log "repository set is clean: $REPO_NAME on the latest train"
}

dump_tail() {
  if [ ! -s "$1" ]; then
    printf '    (no output captured)\n' >&2
    return 0
  fi
  tail -n "$2" "$1" | sed -e 's/^/    /' >&2 || true
}

diagnose() {
  {
    printf '\nfreebsd-pkg-bootstrap: FAILED: %s\n' "$1"
    printf '  attempts=%s backoff=%ss bounds: bootstrap=%ss update=%ss install=%ss\n' \
      "$ATTEMPTS" "$BACKOFF" "$BOOTSTRAP_TIMEOUT" "$UPDATE_TIMEOUT" "$INSTALL_TIMEOUT"
  } >&2

  printf '\n--- repositories (pkg -vv, bound %ss) ---\n' "$DIAGNOSE_TIMEOUT" >&2
  if ! bounded_cmd "$DIAGNOSE_TIMEOUT" "$LOG_DIR/pkg-vv.log" "$PKG_BIN" -vv; then
    printf '  (pkg -vv exited non-zero; raw output follows)\n' >&2
  fi
  dump_tail "$LOG_DIR/pkg-vv.log" "$TAIL_LINES"

  printf '\n--- repository configuration ---\n' >&2
  for dg_conf in "$REPOS_DIR"/*.conf "$BASE_CONFS_DIR"/*.conf; do
    [ -f "$dg_conf" ] || continue
    printf '  %s:\n' "$dg_conf" >&2
    sed -e 's/^/    /' "$dg_conf" >&2 || true
  done

  if [ -n "$LAST_LOG" ]; then
    printf '\n--- last %s lines of %s (exit %s) ---\n' \
      "$TAIL_LINES" "$LAST_LOG" "$LAST_RC" >&2
    dump_tail "$LAST_LOG" "$TAIL_LINES"
  fi
  printf '\n' >&2
}

fail() {
  diagnose "$1"
  exit 1
}

refresh_metadata() {
  log "refreshing repository metadata before the next attempt"
  if ! bounded_cmd "$UPDATE_TIMEOUT" "$LOG_DIR/pkg-update-retry.log" "$PKG_BIN" update -f; then
    log "metadata refresh failed; retrying the install anyway"
  fi
  return 0
}

retry_step() {
  rs_name="$1"
  rs_bound="$2"
  rs_refresh="$3"
  shift 3
  rs_log="$LOG_DIR/$rs_name.log"
  rs_attempt=1
  rs_delay="$BACKOFF"
  while [ "$rs_attempt" -le "$ATTEMPTS" ]; do
    log "step '$rs_name': attempt $rs_attempt/$ATTEMPTS, ${rs_bound}s bound"
    rs_rc=0
    bounded_cmd "$rs_bound" "$rs_log" "$@" || rs_rc=$?
    LAST_LOG="$rs_log"
    LAST_RC="$rs_rc"
    if [ "$rs_rc" -eq 0 ]; then
      return 0
    fi
    if [ "$rs_rc" -eq 124 ]; then
      log "step '$rs_name' hit its ${rs_bound}s bound without returning"
    else
      log "step '$rs_name' exited $rs_rc"
    fi
    dump_tail "$rs_log" 40
    if [ "$rs_attempt" -ge "$ATTEMPTS" ]; then
      break
    fi
    sleep "$rs_delay"
    rs_delay=$((rs_delay * 2))
    if [ "$rs_refresh" -eq 1 ]; then
      refresh_metadata
    fi
    rs_attempt=$((rs_attempt + 1))
  done
  return "$rs_rc"
}

verify_packages() {
  vp_missing=""
  vp_first_log=""
  # shellcheck disable=SC2086 # PACKAGES is a validated, space-separated list
  for vp_pkg in $PACKAGES; do
    vp_log="$LOG_DIR/verify-$vp_pkg.log"
    if ! bounded_cmd "$DIAGNOSE_TIMEOUT" "$vp_log" "$PKG_BIN" info -e "$vp_pkg"; then
      vp_missing="$vp_missing $vp_pkg"
      if [ -z "$vp_first_log" ]; then vp_first_log="$vp_log"; fi
    fi
  done
  if [ -n "$vp_missing" ]; then
    LAST_LOG="$vp_first_log"
    LAST_RC=1
    fail "packages not installed after a successful install:$vp_missing"
  fi
  log "verified installed packages:$PACKAGES"
}

if [ "$KEEP_LOGS" -eq 1 ]; then
  mkdir -p "$LOG_DIR"
else
  LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/koma-pkg-bootstrap.XXXXXX")"
  # shellcheck disable=SC2064 # expand now so trap removes this specific directory
  trap "rm -rf '$LOG_DIR'" EXIT
fi

WORST_CASE=$((ATTEMPTS * (UPDATE_TIMEOUT + INSTALL_TIMEOUT)))
if [ "$SKIP_BOOTSTRAP" -eq 0 ]; then
  WORST_CASE=$((WORST_CASE + ATTEMPTS * BOOTSTRAP_TIMEOUT))
fi
log "packages:$PACKAGES"
log "worst-case pkg time: ${WORST_CASE}s ($ATTEMPTS attempt(s) per step)"
if [ -z "$TIMEOUT_BIN" ]; then
  log "no timeout(1) on PATH; bounding each step with a background watchdog"
fi
if [ "$USE_SUDO" -eq 1 ]; then
  log "privileged commands run through sudo"
fi

if [ "$DRY_RUN" -eq 1 ]; then
  log "dry run: nothing is written or executed"
  printf '\n--- %s ---\n' "$REPOS_DIR/$CONF_NAME" >&2
  repo_conf_text | sed -e 's/^/    /' >&2
  printf '\n--- commands ---\n' >&2
  if [ "$SKIP_BOOTSTRAP" -eq 0 ]; then
    printf '    [%ss, repair, non-fatal] %s %s bootstrap -f\n' \
      "$BOOTSTRAP_TIMEOUT" "$ROOT_DESC" "$PKG_STATIC_BIN" >&2
  fi
  printf '    [%ss] %s %s update -f\n' "$UPDATE_TIMEOUT" "$ROOT_DESC" "$PKG_BIN" >&2
  printf '    [%ss] %s %s install -y%s\n' \
    "$INSTALL_TIMEOUT" "$ROOT_DESC" "$PKG_BIN" "$PACKAGES" >&2
  # shellcheck disable=SC2086 # validated, space-separated package list
  for dr_pkg in $PACKAGES; do
    printf '    [verify, %ss] %s %s info -e %s\n' \
      "$DIAGNOSE_TIMEOUT" "$ROOT_DESC" "$PKG_BIN" "$dr_pkg" >&2
  done
  printf '\n' >&2
  exit 0
fi

command -v "$PKG_BIN" >/dev/null 2>&1 ||
  fail "pkg client '$PKG_BIN' is not on PATH; the image has no usable pkg"

write_repo_conf

if [ "$SKIP_BOOTSTRAP" -eq 0 ]; then
  if command -v "$PKG_STATIC_BIN" >/dev/null 2>&1; then
    if retry_step pkg-bootstrap "$BOOTSTRAP_TIMEOUT" 0 "$PKG_STATIC_BIN" bootstrap -f; then
      log "pkg-static bootstrap succeeded"
    else
      log "pkg-static bootstrap did not complete (exit $LAST_RC, bound ${BOOTSTRAP_TIMEOUT}s per attempt)"
      log "continuing with the installed pkg; install and verification decide the outcome"
    fi
  else
    log "pkg-static is not on PATH; skipping the bootstrap repair step"
  fi
fi

if ! retry_step pkg-update "$UPDATE_TIMEOUT" 0 "$PKG_BIN" update -f; then
  fail "'pkg update -f' failed with exit $LAST_RC after $ATTEMPTS attempt(s)"
fi

assert_single_repo || fail "repository selection is not the single latest-train repo"

# shellcheck disable=SC2086 # PACKAGES is a validated, space-separated name list
if ! retry_step pkg-install "$INSTALL_TIMEOUT" 1 "$PKG_BIN" install -y $PACKAGES; then
  fail "'pkg install -y' failed with exit $LAST_RC after $ATTEMPTS attempt(s)"
fi

verify_packages

log "pkg bootstrap complete"
