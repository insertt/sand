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
      allow) [ -z "${FAKE_SBX_ALLOW_FAIL:-}" ] || { echo "fake sbx: allow refused" >&2; exit 1; } ;;
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
