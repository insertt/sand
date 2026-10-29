#!/bin/sh
# Stand-in for the `sbx` CLI used by the end-to-end tests. It records every
# invocation in $FAKE_SBX_DIR/log and keeps a list of "sandboxes" as files.
set -u
root="$FAKE_SBX_DIR"
mkdir -p "$root/sandboxes"
printf '%s\n' "$*" >> "$root/log"

cmd="${1:-}"
[ $# -gt 0 ] && shift

case "$cmd" in
  version)
    echo "sbx version: v0.0.0-fake"
    ;;
  ls)
    # A test signs sbx out by creating $root/logged-out; `sbx login` signs it back in.
    [ ! -e "$root/logged-out" ] || { echo "your Docker session has expired; run \`sbx login\` and try again (401 Unauthorized)" >&2; exit 1; }
    if [ "${1:-}" = "--json" ]; then
      [ -f "$root/ls.json" ] && cat "$root/ls.json" && exit 0
      exit 1
    fi
    ls "$root/sandboxes"
    ;;
  login)
    rm -f "$root/logged-out"
    ;;
  create)
    name=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --name) name="$2"; shift ;;
      esac
      shift
    done
    [ -n "$name" ] || { echo "fake sbx: create without --name" >&2; exit 2; }
    [ ! -e "$root/sandboxes/$name" ] || { echo "fake sbx: sandbox $name already exists" >&2; exit 1; }
    : > "$root/sandboxes/$name"
    echo "created $name"
    ;;
  rm)
    for arg in "$@"; do
      case "$arg" in -*) ;; *) rm -f "$root/sandboxes/$arg" ;; esac
    done
    ;;
  exec)
    while [ $# -gt 0 ]; do
      case "$1" in
        -i|-t|-it) shift ;;
        --workdir|-w) shift 2 ;;
        *) break ;;
      esac
    done
    sandbox="$1"; shift
    [ -e "$root/sandboxes/$sandbox" ] || { echo "fake sbx: no sandbox $sandbox" >&2; exit 1; }
    case "$1" in
      true) exit 0 ;;
      cat)
        # Files the "sandbox" contains are provided by the test under $root/files.
        file="$root/files$2"
        [ -f "$file" ] && cat "$file" && exit 0
        exit 1
        ;;
      sh)
        case "$2" in
          -c)
            case "$3" in
              "read _"*) read -r _ || true ;;
            esac
            ;;
        esac
        exit 0
        ;;
      bash)
        case "$3" in
          curl*) printf '{"ok":true}\n%s' "${FAKE_SBX_API_CODE:-200}" ;;
          exec*) echo "ACP-READY" ;;
        esac
        exit 0
        ;;
    esac
    ;;
  mount|umount)
    [ -z "${FAKE_SBX_MOUNT_FAIL:-}" ] || { echo "fake sbx: mount failed" >&2; exit 1; }
    if [ -n "${FAKE_SBX_MOUNT_WAIT:-}" ]; then
      : > "$FAKE_SBX_MOUNT_WAIT.started"
      while [ ! -e "$FAKE_SBX_MOUNT_WAIT" ]; do sleep 0.05; done
    fi
    ;;
  mcp)
    sub="$1"; shift
    case "$sub" in
      ls)
        case "${1:-}" in
          -q) cat "$root/mcp" 2>/dev/null || true ;;
          --json) [ -f "$root/mcp.json" ] && cat "$root/mcp.json" && exit 0; exit 1 ;;
        esac
        ;;
      add) echo "$1" >> "$root/mcp" ;;
      auth)
        # Signing in marks the server as ready; a server named in mcp-auth-fail refuses.
        grep -qx "$1" "$root/mcp-auth-fail" 2>/dev/null && { echo "fake sbx: authorization failed" >&2; exit 1; }
        echo "Open https://auth.example/$1 to sign in"
        sed "s/{\"name\": \"$1\"[^}]*}/{\"name\": \"$1\", \"status\": \"ready\"}/" "$root/mcp.json" > "$root/mcp.json.tmp" &&
          mv "$root/mcp.json.tmp" "$root/mcp.json"
        ;;
      load) ;;
    esac
    ;;
  run)
    echo "AGENT-STARTED"
    if [ -n "${FAKE_SBX_RUN_WAIT:-}" ]; then
      : > "$FAKE_SBX_RUN_WAIT.started"
      while [ ! -e "$FAKE_SBX_RUN_WAIT" ]; do sleep 0.05; done
    fi
    exit "${FAKE_SBX_RUN_EXIT:-0}"
    ;;
  policy)
    case "${1:-}" in
      log) cat "$root/policy.json" 2>/dev/null || echo '{}' ;;
      allow)
        [ -z "${FAKE_SBX_ALLOW_FAIL:-}" ] || { echo "fake sbx: allow refused" >&2; exit 1; }
        # Deny rules are files $root/deny/<scope>/<host> holding the rule id; an
        # allow beside a deny of the same scope is refused, as sbx does.
        if [ "${3:-}" = "--sandbox" ] && [ -f "$root/deny/$4/$5" ]; then
          echo "error: allow network rule: allow: \"$5\" conflicts with existing deny rule \"$(cat "$root/deny/$4/$5")\"" >&2
          exit 1
        fi
        ;;
      check)
        # check network --sandbox NAME HOST
        for scope in "$4" global; do
          if [ -f "$root/deny/$scope/$5" ]; then
            printf 'Denied: %s\nContext: sandbox:%s\nReason: denied by local rule rule "local:%s"\n' "$5" "$4" "$(cat "$root/deny/$scope/$5")"
            exit 0
          fi
        done
        printf 'Denied: %s\nReason: no matching allow rule - blocked by default deny policy\n' "$5"
        ;;
      rm)
        # rm network [--sandbox NAME] --id ID --force
        scope=global; id=""
        shift 2
        while [ $# -gt 0 ]; do
          case "$1" in
            --sandbox) scope="$2"; shift ;;
            --id) id="$2"; shift ;;
          esac
          shift
        done
        for rule in "$root/deny/$scope"/*; do
          [ -f "$rule" ] && [ "$(cat "$rule")" = "$id" ] && rm -f "$rule" && exit 0
        done
        echo "error: no rule $id in scope $scope" >&2
        exit 1
        ;;
      approval)
        case "${2:-}" in
          ls) [ -f "$root/approvals.json" ] && cat "$root/approvals.json" && exit 0; exit 1 ;;
          respond) ;;
        esac
        ;;
    esac
    ;;
  skills)
    [ -z "${FAKE_SBX_SKILLS_FAIL:-}" ] || { echo "fake sbx: unknown command skills" >&2; exit 1; }
    ;;
  secret)
    [ -z "${FAKE_SBX_SECRET_FAIL:-}" ] || { echo "fake sbx: secret source could not be resolved" >&2; exit 1; }
    ;;
  template)
    ;;
  *)
    echo "fake sbx: unsupported command '$cmd'" >&2
    exit 2
    ;;
esac
