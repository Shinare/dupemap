#!/bin/bash
# DupeMap delete script for "@@HOST@@", made by DupeMap @@VERSION@@ on @@MADE@@.
#
# It removes @@COUNT_TEXT@@ (@@BYTES_TEXT@@) that you marked for deletion in DupeMap.
@@LAST_NOTE@@
# DupeMap chose them from the manifests it had imported; if files changed since, scan again
# first. Nothing is touched before you confirm.
#
# Run it as root on @@HOST@@, in a folder where it may write its log:
#   sudo bash @@FILE_NAME@@ --dry-run     # check, and show what would happen
#   sudo bash @@FILE_NAME@@
# It checks every file first (still there, a plain file, same size@@HASH_NOTE@@), shows what it
# will do and asks before it starts. Options:
#   --dry-run       only check, and show what would happen
#   --move          move the files into "_dupemap_deleted/<date>" at the top of their file system
#                   (on ZFS: of their dataset); no space is freed until you delete it@@DEFAULT_MOVE@@
#   --delete        delete them with rm@@DEFAULT_DELETE@@
#   --check-hash    also check each file's SHA-256 first (reads every file, so it is slow)@@HASH_DEFAULT@@
#   --hard-links    also remove files that have other hard links (removing those frees no space)
#   --yes           do not ask anything (for running it unattended)
#   --other-host    run it on a computer that is not called @@HOST@@
# ZFS: the space comes back only once no snapshot holds the files any more
#   (zfs list -t snapshot -o name,used -s used).
# Everything it does is written to @@LOG_NAME@@ in the current folder.

set -u
MODE=@@MODE@@
CHECK_HASH=@@CHECK_HASH@@
HARD_LINKS=0
DRY=0
YES=0
OTHER=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    --move) MODE=move ;;
    --delete) MODE=delete ;;
    --check-hash) CHECK_HASH=1 ;;
    --no-check-hash) CHECK_HASH=0 ;;
    --hard-links) HARD_LINKS=1 ;;
    --yes) YES=1 ;;
    --other-host) OTHER=1 ;;
    *) echo "Unknown option: $a (the options are at the top of this script)" >&2; exit 2 ;;
  esac
done
SOURCE=@@HOST_SH@@
LOG="$(pwd)/@@LOG_NAME@@"
log() { printf '%s\t%s\t%s\n' "$(date '+%F %T')" "$1" "$2" >> "$LOG"; }
gb() { awk -v b="$1" 'BEGIN { printf "%.1f GB", b / 1073741824 }'; }

if [ "$(hostname)" != "$SOURCE" ] && [ "$OTHER" = 0 ]; then
  echo "These files were scanned on $SOURCE, but this computer is $(hostname)." >&2
  echo "Run it on $SOURCE, or with --other-host if this is the same computer under another name." >&2
  exit 1
fi

# size, SHA-256, path (an empty size or hash: not known)
S=()
H=()
P=()
add() { S+=("$1"); H+=("$2"); P+=("$3"); }
@@LIST@@

dry=""
[ "$DRY" = 1 ] && dry=", dry run"
log start "${#P[@]} files, mode $MODE$dry"
echo "Checking ${#P[@]} files..."
todo=()
todo_bytes=0
missing=0
changed=0
notplain=0
linked=0
for i in "${!P[@]}"; do
  f=${P[$i]}
  if [ -L "$f" ]; then notplain=$((notplain + 1)); log "a link now (left out)" "$f"; continue; fi
  if [ ! -e "$f" ]; then missing=$((missing + 1)); log "not there (left out)" "$f"; continue; fi
  if [ ! -f "$f" ]; then notplain=$((notplain + 1)); log "not a plain file (left out)" "$f"; continue; fi
  st=$(stat -c '%s %h' -- "$f" 2>/dev/null) || { missing=$((missing + 1)); log "cannot read (left out)" "$f"; continue; }
  size=${st% *}
  links=${st#* }
  if [ -n "${S[$i]}" ] && [ "$size" != "${S[$i]}" ]; then
    changed=$((changed + 1)); log "another size: $size bytes, was ${S[$i]} (left out)" "$f"; continue
  fi
  if [ "$links" -gt 1 ] && [ "$HARD_LINKS" = 0 ]; then
    linked=$((linked + 1)); log "has $links hard links (left out)" "$f"; continue
  fi
  todo+=("$i")
  todo_bytes=$((todo_bytes + size))
done

if [ "$MODE" = move ]; then verb="Move into _dupemap_deleted"; else verb="Delete"; fi
echo
echo "$verb: ${#todo[@]} files, $(gb "$todo_bytes")"
[ "$missing" -gt 0 ] && echo "  $missing are not there any more (left out)"
[ "$changed" -gt 0 ] && echo "  $changed have another size than when they were scanned (left out: they changed)"
[ "$notplain" -gt 0 ] && echo "  $notplain are links or not plain files now (left out)"
[ "$linked" -gt 0 ] && echo "  $linked have other hard links, so removing them frees no space (left out; --hard-links to include them)"
[ "$CHECK_HASH" = 1 ] && echo "  each file's SHA-256 is checked first; a file that changed is left alone"

if [ "$DRY" = 1 ]; then
  n=0
  for i in ${todo[@]+"${todo[@]}"}; do
    [ "$n" -lt 20 ] && printf '  %s\n' "${P[$i]}"
    n=$((n + 1))
    log "would remove" "${P[$i]}"
  done
  [ "${#todo[@]}" -gt 20 ] && echo "  ... and $(( ${#todo[@]} - 20 )) more (all of them are in $LOG)"
  echo "Dry run: nothing was changed."
  exit 0
fi
if [ "${#todo[@]}" -eq 0 ]; then echo "Nothing to do."; exit 0; fi
if [ "$YES" = 0 ]; then
  answer=""
  read -r -p "Type YES to go ahead (${#todo[@]} files): " answer
  if [ "$answer" != YES ] && [ "$answer" != yes ]; then echo "Nothing was changed."; exit 0; fi
fi

STAMP=$(date '+%Y-%m-%d %H%M')
done_n=0
done_bytes=0
failed=0
hash_changed=0
zfs=0
n=0
for i in "${todo[@]}"; do
  f=${P[$i]}
  n=$((n + 1))
  [ $((n % 500)) -eq 0 ] && echo "  $n of ${#todo[@]}..."
  if [ "$CHECK_HASH" = 1 ] && [ -n "${H[$i]}" ]; then
    h=$(sha256sum -- "$f" 2>/dev/null) || { failed=$((failed + 1)); log "cannot read (left alone)" "$f"; continue; }
    h=${h#\\}
    h=${h:0:64}
    if [ "$h" != "${H[$i]}" ]; then hash_changed=$((hash_changed + 1)); log "another SHA-256 (left alone)" "$f"; continue; fi
  fi
  size=$(stat -c %s -- "$f" 2>/dev/null || echo 0)
  [ "$(stat -f -c %T -- "$f" 2>/dev/null)" = zfs ] && zfs=1
  if [ "$MODE" = move ]; then
    # the top of the file's file system (another top only in DupeMap's tests)
    root=${DUPEMAP_MOVE_ROOT:-$(stat -c %m -- "$f" 2>/dev/null)}
    if [ -z "$root" ]; then failed=$((failed + 1)); log "cannot tell its file system (left alone)" "$f"; continue; fi
    rel=${f#"$root"}
    rel=${rel#/}
    dest="${root%/}/_dupemap_deleted/$STAMP/$rel"
    if mkdir -p -- "${dest%/*}" && mv -n -- "$f" "$dest" && [ ! -e "$f" ]; then
      done_n=$((done_n + 1)); done_bytes=$((done_bytes + size)); log moved "$f"
    else
      failed=$((failed + 1)); log "could not move" "$f"
    fi
  else
    if rm -f -- "$f" && [ ! -e "$f" ]; then
      done_n=$((done_n + 1)); done_bytes=$((done_bytes + size)); log deleted "$f"
    else
      failed=$((failed + 1)); log "could not delete" "$f"
    fi
  fi
done

echo
echo "Done: $done_n files, $(gb "$done_bytes")."
if [ "$MODE" = move ]; then
  echo "They are in _dupemap_deleted/$STAMP at the top of each file system: look through them, then delete those folders to free the space."
fi
[ "$hash_changed" -gt 0 ] && echo "$hash_changed had another SHA-256 than when they were scanned (they changed) and were left alone."
[ "$failed" -gt 0 ] && echo "$failed could not be removed (see the log)."
[ "$zfs" = 1 ] && echo "ZFS: the space comes back only once no snapshot holds these files (zfs list -t snapshot -o name,used -s used)."
echo "Log: $LOG"
log done "$done_n files, $done_bytes bytes; $failed failed, $hash_changed changed"
exit 0
