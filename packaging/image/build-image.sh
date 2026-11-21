#!/usr/bin/env bash
# Obraz Raspberry Pi z AARS (etap 10, R2): oficjalny Raspberry Pi OS Lite
# 64-bit rozszerzony w chroocie. Działa jako root na Linuksie arm64 (runner
# ubuntu-24.04-arm), natywnie, bez qemu.
#
# Użycie:
#   sudo packaging/image/build-image.sh --version 0.10.0 \
#     --binary dist/aars-0.10.0-aarch64-linux-gnu.tar.gz \
#     --out dist/aars-0.10.0-rpi.img.xz \
#     [--cache .cache/image] [--os-list dist/aars-os-list.json \
#      --download-url https://github.com/sptq/aars/releases/download/v0.10.0/aars-0.10.0-rpi.img.xz]
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG="$REPO/packaging"

VERSION=""
BINARY=""
OUT=""
CACHE="$REPO/.cache/image"
OS_LIST=""
DOWNLOAD_URL=""

log() { printf '\n==> %s\n' "$*" >&2; }
die() { printf 'BŁĄD: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --binary) BINARY="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --cache) CACHE="$2"; shift 2 ;;
    --os-list) OS_LIST="$2"; shift 2 ;;
    --download-url) DOWNLOAD_URL="$2"; shift 2 ;;
    *) die "nieznany argument: $1" ;;
  esac
done

[ -n "$VERSION" ] || die "brak --version"
[ -f "$BINARY" ] || die "brak archiwum binarki (--binary): $BINARY"
[ -n "$OUT" ] || die "brak --out"
if [ -n "$OS_LIST" ] && [ -z "$DOWNLOAD_URL" ]; then
  die "--os-list wymaga --download-url"
fi
[ "$(id -u)" -eq 0 ] || die "skrypt wymaga roota (losetup, mount, chroot)"
[ "$(uname -m)" = "aarch64" ] || die "skrypt działa natywnie na arm64 (chroot bez qemu), a to $(uname -m)"
for tool in curl sha256sum xz losetup parted e2fsck resize2fs zerofree chroot tar truncate; do
  command -v "$tool" >/dev/null || die "brak narzędzia: $tool"
done

# Wartość klucza z pliku KLUCZ=wartość (bez wykonywania pliku).
lock_value() {
  local value
  value="$(sed -n "s/^$2=//p" "$1" | head -n 1)"
  [ -n "$value" ] || die "brak $2 w $1"
  printf '%s' "$value"
}

# Sprawdza sumę SHA-256 pliku; przy niezgodności kończy skrypt.
check_sha256() {
  local file="$1" want="$2" got
  [[ "$want" =~ ^[0-9a-f]{64}$ ]] || die "suma w pliku blokady nie jest SHA-256: $want"
  got="$(sha256sum "$file" | cut -d' ' -f1)"
  [ "$got" = "$want" ] || die "zła suma $file: $got, oczekiwana $want"
}

BASE_LOCK="$PKG/image/base.lock"
BASE_URL="$(lock_value "$BASE_LOCK" URL)"
ARCHIVE_SHA256="$(lock_value "$BASE_LOCK" ARCHIVE_SHA256)"
IMAGE_SHA256="$(lock_value "$BASE_LOCK" IMAGE_SHA256)"
case "$BASE_URL" in
  https://downloads.raspberrypi.com/*) ;;
  *) die "baza spoza downloads.raspberrypi.com: $BASE_URL" ;;
esac

mkdir -p "$CACHE" "$(dirname "$OUT")"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/aars-image.XXXXXX")"
ROOT="$WORK/root"
IMG="$WORK/aars-$VERSION-rpi.img"
LOOP=""

cleanup() {
  set +e
  if [ -n "${ROOT:-}" ] && [ -d "$ROOT" ]; then
    for m in dev/pts dev proc sys boot/firmware; do
      mountpoint -q "$ROOT/$m" && umount -l "$ROOT/$m"
    done
    mountpoint -q "$ROOT" && umount -l "$ROOT"
  fi
  [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

log "Baza: $BASE_URL"
ARCHIVE="$CACHE/$(basename "$BASE_URL")"
if [ -f "$ARCHIVE" ] && [ "$(sha256sum "$ARCHIVE" | cut -d' ' -f1)" = "$ARCHIVE_SHA256" ]; then
  log "Baza z cache: $ARCHIVE"
else
  rm -f "$ARCHIVE"
  curl -fL --retry 3 --proto '=https' -o "$ARCHIVE.part" "$BASE_URL"
  mv "$ARCHIVE.part" "$ARCHIVE"
fi
check_sha256 "$ARCHIVE" "$ARCHIVE_SHA256"

log "Rozpakowanie i suma obrazu"
xz -dc "$ARCHIVE" > "$IMG"
check_sha256 "$IMG" "$IMAGE_SHA256"

log "Powiększenie partycji głównej o 1 GiB"
truncate -s +1G "$IMG"
parted -s "$IMG" resizepart 2 100%
LOOP="$(losetup -P --show -f "$IMG")"
for _ in 1 2 3 4 5 6 7 8 9 10; do
  [ -b "${LOOP}p2" ] && break
  sleep 1
done
[ -b "${LOOP}p2" ] || die "brak urządzenia ${LOOP}p2"
e2fsck -fy "${LOOP}p2" || [ $? -le 1 ]
resize2fs "${LOOP}p2"

log "Montowanie"
mkdir -p "$ROOT"
mount "${LOOP}p2" "$ROOT"
mount "${LOOP}p1" "$ROOT/boot/firmware"
mount --bind /dev "$ROOT/dev"
mount --bind /dev/pts "$ROOT/dev/pts"
mount -t proc proc "$ROOT/proc"
mount -t sysfs sysfs "$ROOT/sys"

# Sieć dla apt w chroocie; oryginał wraca przed odmontowaniem.
RESOLV_BACKUP=""
if [ -e "$ROOT/etc/resolv.conf" ] || [ -L "$ROOT/etc/resolv.conf" ]; then
  RESOLV_BACKUP="$WORK/resolv.conf.orig"
  mv "$ROOT/etc/resolv.conf" "$RESOLV_BACKUP"
fi
cp -L /etc/resolv.conf "$ROOT/etc/resolv.conf"
# Usługi nie startują w chroocie.
printf '#!/bin/sh\nexit 101\n' > "$ROOT/usr/sbin/policy-rc.d"
chmod 0755 "$ROOT/usr/sbin/policy-rc.d"

in_root() { chroot "$ROOT" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 HOME=/root "$@"; }

log "Pakiety: python3-venv, libasound2t64, avahi-daemon"
in_root apt-get update
in_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
  python3-venv libasound2t64 avahi-daemon
in_root apt-get clean
rm -rf "$ROOT/var/lib/apt/lists/"*

log "Piper w venv /opt/aars/piper (przypięte hashe)"
mkdir -p "$ROOT/opt/aars"
cp "$PKG/piper/requirements.txt" "$ROOT/tmp/aars-piper-requirements.txt"
in_root python3 -m venv /opt/aars/piper
# Tylko PyPI: /etc/pip.conf Raspberry Pi OS dokłada piwheels, a hashe są
# przypięte do kół z PyPI.
in_root env PIP_CONFIG_FILE=/dev/null /opt/aars/piper/bin/pip install --no-cache-dir \
  --disable-pip-version-check --index-url https://pypi.org/simple \
  --require-hashes -r /tmp/aars-piper-requirements.txt
rm -f "$ROOT/tmp/aars-piper-requirements.txt"
[ -x "$ROOT/opt/aars/piper/bin/piper" ] || die "brak /opt/aars/piper/bin/piper po instalacji"

log "Głosy z voices.lock"
mkdir -p "$ROOT/var/lib/aars/voices"
grep -v '^[[:space:]]*\(#\|$\)' "$PKG/voices.lock" | while read -r name url sha size; do
  case "$name" in
    */*|.*|"") die "zła nazwa głosu w voices.lock: $name" ;;
  esac
  case "$url" in
    https://huggingface.co/rhasspy/piper-voices/resolve/*) ;;
    *) die "głos spoza przypiętego repozytorium: $url" ;;
  esac
  cached="$CACHE/voice-$sha-$name"
  if [ ! -f "$cached" ] || [ "$(sha256sum "$cached" | cut -d' ' -f1)" != "$sha" ]; then
    rm -f "$cached"
    curl -fL --retry 3 --proto '=https' -o "$cached.part" "$url"
    mv "$cached.part" "$cached"
  fi
  check_sha256 "$cached" "$sha"
  [ "$(stat -c %s "$cached")" = "$size" ] || die "zły rozmiar $name"
  install -m 0644 "$cached" "$ROOT/var/lib/aars/voices/$name"
done

log "AARS $VERSION w /opt/aars/releases/$VERSION"
REL="$ROOT/opt/aars/releases/$VERSION"
mkdir -p "$REL" "$WORK/release"
tar -xzf "$BINARY" -C "$WORK/release" --no-same-owner --no-same-permissions
for f in "$WORK/release"/* "$WORK/release"/.[!.]*; do
  [ -e "$f" ] || continue
  case "$(basename "$f")" in
    aars|VERSION|LICENSE) ;;
    *) die "nieoczekiwany plik w archiwum: $(basename "$f")" ;;
  esac
  if [ -L "$f" ] || [ ! -f "$f" ]; then
    die "w archiwum tylko zwykłe pliki: $(basename "$f")"
  fi
done
[ "$(tr -d '[:space:]' < "$WORK/release/VERSION")" = "$VERSION" ] || die "VERSION w archiwum ≠ $VERSION"
install -o root -g root -m 0755 "$WORK/release/aars" "$REL/aars"
install -o root -g root -m 0644 "$WORK/release/VERSION" "$REL/VERSION"
if [ -f "$WORK/release/LICENSE" ]; then
  install -o root -g root -m 0644 "$WORK/release/LICENSE" "$REL/LICENSE"
fi
chmod 0755 "$ROOT/opt/aars" "$ROOT/opt/aars/releases" "$REL"
# Dowiązanie względne (jak w instalacji z panelu); bez `previous`.
ln -sfn "releases/$VERSION" "$ROOT/opt/aars/current"
install -D -o root -g root -m 0755 "$PKG/aars-update-guard" "$ROOT/opt/aars/bin/aars-update-guard"

log "Usługi, użytkownik, katalogi i konfiguracja"
install -D -m 0644 "$PKG/systemd/aars.service" "$ROOT/etc/systemd/system/aars.service"
install -D -m 0644 "$PKG/systemd/aars-reset-admin.service" "$ROOT/etc/systemd/system/aars-reset-admin.service"
install -D -m 0644 "$PKG/sysusers.d/aars.conf" "$ROOT/usr/lib/sysusers.d/aars.conf"
install -D -m 0644 "$PKG/tmpfiles.d/aars.conf" "$ROOT/usr/lib/tmpfiles.d/aars.conf"
install -D -m 0644 "$PKG/etc/aars.toml" "$ROOT/etc/aars/aars.toml"
install -D -m 0644 "$PKG/THIRD-PARTY.md" "$ROOT/usr/share/doc/aars/THIRD-PARTY.md"
in_root systemd-sysusers /usr/lib/sysusers.d/aars.conf
in_root systemd-tmpfiles --create /usr/lib/tmpfiles.d/aars.conf
in_root chown -R aars:aars /var/lib/aars/voices
in_root systemctl enable aars.service aars-reset-admin.service

log "Nazwa hosta: aars (Imager może ją nadpisać)"
echo aars > "$ROOT/etc/hostname"
sed -i 's/^127\.0\.1\.1[[:space:]].*/127.0.1.1\taars/' "$ROOT/etc/hosts"

log "Test dymny w chroocie"
got="$(in_root /opt/aars/current/aars --version)"
[ "$got" = "aars $VERSION" ] || die "aars --version w obrazie: \"$got\", oczekiwane \"aars $VERSION\""
echo "Test głosu." | in_root /opt/aars/piper/bin/piper \
  --model /var/lib/aars/voices/pl_PL-darkman-medium.onnx --output_file /tmp/aars-smoke.wav
[ -s "$ROOT/tmp/aars-smoke.wav" ] || die "Piper nie zapisał pliku WAV"
rm -f "$ROOT/tmp/aars-smoke.wav"

log "Sprzątanie i odmontowanie"
rm -f "$ROOT/usr/sbin/policy-rc.d" "$ROOT/etc/resolv.conf"
if [ -n "$RESOLV_BACKUP" ]; then
  mv "$RESOLV_BACKUP" "$ROOT/etc/resolv.conf"
fi
for m in dev/pts dev proc sys boot/firmware; do
  umount "$ROOT/$m"
done
umount "$ROOT"
e2fsck -fy "${LOOP}p2" || [ $? -le 1 ]
zerofree "${LOOP}p2"
losetup -d "$LOOP"
LOOP=""

log "Kompresja"
EXTRACT_SIZE="$(stat -c %s "$IMG")"
EXTRACT_SHA256="$(sha256sum "$IMG" | cut -d' ' -f1)"
xz -T0 -9 -c "$IMG" > "$OUT.part"
mv "$OUT.part" "$OUT"
DOWNLOAD_SIZE="$(stat -c %s "$OUT")"
DOWNLOAD_SHA256="$(sha256sum "$OUT" | cut -d' ' -f1)"
log "Obraz: $OUT ($DOWNLOAD_SIZE B, sha256 $DOWNLOAD_SHA256; rozpakowany $EXTRACT_SIZE B, sha256 $EXTRACT_SHA256)"

if [ -n "$OS_LIST" ]; then
  log "Lista dla Raspberry Pi Imager: $OS_LIST"
  cat > "$OS_LIST" <<JSON
{
  "os_list": [
    {
      "name": "AARS $VERSION (Raspberry Pi OS Lite 64-bit)",
      "description": "Automatyczna stacja ostrzegawcza: Raspberry Pi OS Lite 64-bit z AARS, Piperem i polskim głosem. Panel pod http://aars.local:8080.",
      "url": "$DOWNLOAD_URL",
      "extract_size": $EXTRACT_SIZE,
      "extract_sha256": "$EXTRACT_SHA256",
      "image_download_size": $DOWNLOAD_SIZE,
      "image_download_sha256": "$DOWNLOAD_SHA256",
      "release_date": "$(date -u +%F)",
      "init_format": "cloudinit-rpi",
      "devices": ["pi5-64bit", "pi4-64bit", "pi3-64bit"]
    }
  ]
}
JSON
fi
log "Gotowe"
