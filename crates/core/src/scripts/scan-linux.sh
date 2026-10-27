#!/bin/bash
# DupeMap scan script for Linux (TrueNAS SCALE, a Linux PC...), made by DupeMap @@VERSION@@.
#
# Hashes every file of at least @@MIN_SIZE_TEXT@@ on this computer's @@KINDS_TEXT@@ and writes a
# manifest DupeMap can import, in the current folder:
#   manifest-<hostname>-@@WHAT@@.tsv
#
# Run it as root, so it can read every folder:
#   sudo bash @@FILE_NAME@@
#   sudo bash @@FILE_NAME@@ /mnt/pool1/photos /mnt/pool2/backup    # only these folders
# A long run? Let it go on after you log out, and follow it:
#   sudo nohup bash @@FILE_NAME@@ > dupemap-scan.log 2>&1 &
#   tail -f dupemap-scan.log
# Then copy the manifest to the computer with DupeMap and import it there
# (File > Import manifests, or drag the file onto DupeMap's window).
#
# Settings (environment variables):
#   MIN=102400      smaller files are not hashed (bytes; default @@MIN_SIZE@@)
#   JOBS=2          files hashed at the same time (default 4; use 1 for a single spinning disk)
#   FRESH=1         start over instead of continuing an interrupted run
#   KINDS="internal usb network"   which file systems to scan when no folders are given
#
# An interrupted run continues where it stopped when you run it again from the same folder.
# Each file system is scanned on its own (it does not wander into other mounts); ZFS snapshots
# (.zfs), /proc, /sys, /usr, /var... of the system disk and TrueNAS app data are skipped.

set -u
KINDS=${KINDS:-"@@KINDS@@"}
MIN=${MIN:-@@MIN_SIZE@@}
JOBS=${JOBS:-4}
HOST=$(hostname)
export HOST

roots=()
if [ $# -gt 0 ]; then
  what=folders
  for p in "$@"; do roots+=("$(readlink -f -- "$p")"); done
else
  what=$(echo "$KINDS" | tr -s ' ' '+')
  while read -r src target fstype _; do
    target=$(printf '%b' "$target")   # /proc/self/mounts writes a space as \040
    case "$fstype" in
      nfs|nfs4|cifs|smb3|smbfs|fuse.sshfs|sshfs|9p|davfs|fuse.rclone|ceph|glusterfs) kind=network ;;
      ext2|ext3|ext4|xfs|btrfs|zfs|f2fs|ntfs|ntfs3|fuseblk|vfat|exfat|hfsplus|jfs|reiserfs|bcachefs|msdos)
        kind=internal
        case "$src" in
          /dev/*) if readlink -f "/sys/class/block/${src##*/}" 2>/dev/null | grep -q '/usb'; then kind=usb; fi ;;
        esac ;;
      *) continue ;;
    esac
    case " $KINDS " in *" $kind "*) ;; *) continue ;; esac
    case "$target" in
      /boot|/boot/*|/snap|/snap/*|/proc|/proc/*|/sys|/sys/*|/dev|/dev/*|/efi|/efi/*|/recovery|/recovery/*) continue ;;
      /var/lib|/var/lib/*|/run/user|/run/user/*) continue ;;
      */.*|*ix-applications*) continue ;;   # hidden datasets (TrueNAS app data)
    esac
    roots+=("$target")
  done < /proc/self/mounts
fi
if [ ${#roots[@]} -eq 0 ]; then
  echo "Nothing to scan: no file systems of the chosen kinds ($KINDS) were found." >&2
  exit 1
fi

OUT="$(pwd)/manifest-$HOST-$what.tsv"
DONE=$(mktemp) || exit 1
LIST=$(mktemp) || exit 1
TODO=$(mktemp) || exit 1
trap 'rm -f "$DONE" "$LIST" "$TODO"; [ -n "${TICK:-}" ] && kill "$TICK" 2>/dev/null' EXIT

if [ -s "$OUT" ] && [ "${FRESH:-0}" != 1 ]; then
  # a line cut off by an interruption is removed
  if [ -n "$(tail -c 1 "$OUT")" ]; then sed -i '$ d' "$OUT"; fi
  tail -n +2 "$OUT" | cut -f4- | LC_ALL=C sort -u > "$DONE"
  echo "Continuing: $(wc -l < "$DONE") files are already in $OUT"
else
  printf 'hash\tsize\thost\tpath\n' > "$OUT"
fi

size_test=()
if [ "$MIN" -gt 0 ] 2>/dev/null; then size_test=(-size "+$((MIN - 1))c"); fi

# progress every minute
( while sleep 60; do echo "  $(( $(wc -l < "$OUT") - 1 )) files in the manifest"; done ) &
TICK=$!

for r in "${roots[@]}"; do
  echo "Listing $r ..."
  if [ "$r" = / ]; then
    prune=( \( -path /proc -o -path /sys -o -path /dev -o -path /run -o -path /tmp -o -path /var -o -path /usr \
              -o -path /lib -o -path /lib32 -o -path /lib64 -o -path /libx32 -o -path /bin -o -path /sbin \
              -o -path /etc -o -path /boot -o -path /snap -o -path /efi \) -prune -o )
  else
    prune=()
  fi
  find "$r" -xdev ${prune[@]+"${prune[@]}"} -name .zfs -prune -o -type f ${size_test[@]+"${size_test[@]}"} \
      ! -name $'*\n*' ! -path "$OUT" -print 2>/dev/null |
    LC_ALL=C sort > "$LIST"
  LC_ALL=C comm -23 "$LIST" "$DONE" > "$TODO"
  echo "Hashing $(wc -l < "$TODO") files under $r ($JOBS at a time) ..."
  tr '\n' '\0' < "$TODO" | xargs -0 -n 1 -P "$JOBS" bash -c '
    f=$1
    s=$(stat -c %s -- "$f" 2>/dev/null) || exit 0
    h=$(sha256sum -- "$f" 2>/dev/null) || { echo "Could not read: $f" >&2; exit 0; }
    h=${h%% *}
    h=${h#\\}
    printf "%s\t%s\t%s\t%s\n" "$h" "$s" "$HOST" "$f"
  ' _ >> "$OUT"
done

echo "Done: $(( $(wc -l < "$OUT") - 1 )) files -> $OUT"
echo "Copy the manifest to the computer with DupeMap and import it there (File > Import manifests)."
