#!/usr/bin/env bash
# Podpis wydania AARS (R4). Uruchamia właściciel na swoim komputerze, poza CI:
# klucz prywatny nigdy nie trafia na GitHuba.
#
#   scripts/sign-release.sh v0.10.0 [--compare ścieżka/do/aars]
#
# Co robi, zanim cokolwiek podpisze:
#   1. pobiera WSZYSTKIE pliki szkicu wydania razem z manifestem,
#   2. liczy lokalnie SHA-256 i rozmiar każdego pliku i porównuje
#      z manifestem; odmawia, gdy plik z manifestu nie istnieje w szkicu,
#      gdy szkic ma plik spoza manifestu albo gdy suma lub rozmiar się różni,
#   3. sprawdza manifest tymi samymi regułami co `Manifest::parse` w AARS
#      (format 1, wersja = znacznik bez „v”, target aarch64-unknown-linux-gnu,
#      dokładnie jedna binarka o właściwej nazwie, dozwolone nazwy i rodzaje,
#      sumy 64 małe znaki szesnastkowe, limity rozmiarów stacji),
#   4. z --compare: porównuje SHA-256 binarki `aars` z archiwum wydania
#      z binarką zbudowaną lokalnie (różnica = odmowa),
#   5. pokazuje podsumowanie z commitem źródłowym znacznika i czeka, aż
#      właściciel wpisze numer wersji.
# Dopiero potem: podpis z komentarzem zaufanym "aars {wersja}", kontrola
# kluczem z keys/aars-release.pub, dołączenie podpisu i publikacja szkicu.
#
# Zostaje zaufanie do CI: binarkę i obraz zbudował GitHub Actions. Skrypt
# dowodzi, że podpisujesz dokładnie te pliki, które widziałeś, i tylko je;
# niezależną kontrolę binarki daje --compare.
#
# Wymaga: gh, minisign, jq, sha256sum albo shasum, tar.
# Klucz: zmienna MINISIGN_KEY (domyślnie ~/.minisign/aars.key).
# Repozytorium: zmienna AARS_REPO (domyślnie sptq/aars).
set -euo pipefail

die() { printf 'BŁĄD: %s\n' "$*" >&2; exit 1; }

TARGET="aarch64-unknown-linux-gnu"
# Limity stacji (crates/aars/src/update/{fetch,verify}.rs): manifest 64 KiB,
# archiwum binarki 64 MiB.
MAX_MANIFEST=$((64 * 1024))
MAX_ARCHIVE=$((64 * 1024 * 1024))

TAG=""
COMPARE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --compare)
      [ $# -ge 2 ] || die "--compare wymaga ścieżki do binarki"
      COMPARE="$2"
      shift 2
      ;;
    -*) die "nieznana opcja: $1" ;;
    *)
      [ -z "$TAG" ] || die "jeden znacznik naraz"
      TAG="$1"
      shift
      ;;
  esac
done

[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "użycie: $0 v{wersja} [--compare binarka], np. v0.10.0"
VERSION="${TAG#v}"
REPO="${AARS_REPO:-sptq/aars}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUB="$REPO_DIR/keys/aars-release.pub"
KEY="${MINISIGN_KEY:-$HOME/.minisign/aars.key}"
BINARY="aars-${VERSION}-aarch64-linux-gnu.tar.gz"

for tool in gh minisign jq tar; do
  command -v "$tool" >/dev/null || die "brak narzędzia: $tool"
done
if command -v sha256sum >/dev/null; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "brak sha256sum ani shasum"
fi
size_of() { wc -c < "$1" | tr -d '[:space:]'; }

# Ścieżki klucza nie wypisujemy nigdzie.
[ -f "$KEY" ] || die "brak klucza prywatnego (ustaw MINISIGN_KEY)"
if grep -q UNSET "$PUB"; then
  die "keys/aars-release.pub zawiera UNSET — najpierw wpisz klucz publiczny wydań i zbuduj z nim wydanie"
fi
if [ -n "$COMPARE" ]; then
  [ -f "$COMPARE" ] || die "brak pliku do porównania: $COMPARE"
  COMPARE="$(cd "$(dirname "$COMPARE")" && pwd)/$(basename "$COMPARE")"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir "$WORK/assets"
cd "$WORK"

echo "==> Szkic $TAG z $REPO"
[ "$(gh release view "$TAG" --repo "$REPO" --json isDraft --jq .isDraft)" = "true" ] \
  || die "$TAG nie jest szkicem — podpisuje się tylko szkic"
gh release download "$TAG" --repo "$REPO" --dir assets
[ -f assets/aars-manifest.json ] || die "szkic bez aars-manifest.json"
[ ! -e assets/aars-manifest.json.minisig ] || die "szkic ma już podpis — nie podpisuję drugi raz"
mv assets/aars-manifest.json manifest.json

echo "==> Kontrola manifestu"
[ "$(size_of manifest.json)" -le "$MAX_MANIFEST" ] || die "manifest większy niż 64 KiB"
jq -e . manifest.json >/dev/null || die "manifest nie jest poprawnym JSON-em"
check() { jq -e "$1" manifest.json >/dev/null || die "$2"; }
check '.format == 1' "format manifestu inny niż 1"
jq -e --arg v "$VERSION" '.version == $v' manifest.json >/dev/null || die "wersja w manifeście ≠ $VERSION"
jq -e --arg t "$TARGET" '.target == $t' manifest.json >/dev/null || die "target w manifeście ≠ $TARGET"
check '(.published_at | type) == "string" and (.notes | type) == "string"' "brak published_at albo notes"
check '(.assets | type) == "array" and (.assets | length) > 0' "manifest bez plików"
check 'all(.assets[]; (.name | type) == "string"
          and (.name | test("^[A-Za-z0-9_+][A-Za-z0-9._+-]{0,254}$"))
          and (.sha256 | type) == "string" and (.sha256 | test("^[0-9a-f]{64}$"))
          and (.size | type) == "number" and .size >= 0 and (.size | floor) == .size
          and (.kind | IN("binary", "image", "imager-list", "source")))' \
  "niedozwolona nazwa, suma, rozmiar albo rodzaj pliku w manifeście"
check '([.assets[].name] | length) == ([.assets[].name] | unique | length)' "plik w manifeście dwa razy"
check '[.assets[] | select(.kind == "binary")] | length == 1' "manifest musi mieć dokładnie jedną binarkę"
jq -e --arg b "$BINARY" '[.assets[] | select(.kind == "binary")][0].name == $b' manifest.json >/dev/null \
  || die "binarka w manifeście nie nazywa się $BINARY"
jq -e --argjson max "$MAX_ARCHIVE" '[.assets[] | select(.kind == "binary")][0].size <= $max' manifest.json >/dev/null \
  || die "archiwum binarki większe niż 64 MiB (limit stacji)"
check 'all(.assets[]; .name != "aars-manifest.json" and .name != "aars-manifest.json.minisig")' \
  "manifest wymienia sam siebie albo podpis"

echo "==> Sumy wszystkich plików szkicu"
listed="$(jq -r '.assets[].name' manifest.json | LC_ALL=C sort)"
present="$(cd assets && find . -mindepth 1 -maxdepth 1 -print | sed 's|^\./||' | LC_ALL=C sort)"
if [ "$listed" != "$present" ]; then
  printf 'W manifeście:\n%s\nW szkicu:\n%s\n' "$listed" "$present" >&2
  die "pliki szkicu nie zgadzają się z manifestem"
fi
while IFS=$'\t' read -r name want_sha want_size; do
  file="assets/$name"
  if [ -L "$file" ] || [ ! -f "$file" ]; then
    die "$name nie jest zwykłym plikiem"
  fi
  got_size="$(size_of "$file")"
  [ "$got_size" = "$want_size" ] || die "$name: rozmiar $got_size B, w manifeście $want_size B"
  got_sha="$(sha256 "$file")"
  [ "$got_sha" = "$want_sha" ] || die "$name: SHA-256 $got_sha, w manifeście $want_sha"
done < <(jq -r '.assets[] | [.name, .sha256, (.size | tostring)] | @tsv' manifest.json)

echo "==> Zawartość archiwum binarki"
entries="$(tar -tzf "assets/$BINARY" | sed 's|^\./||' | LC_ALL=C sort | tr '\n' ' ')"
case "$entries" in
  "LICENSE VERSION aars "|"VERSION aars ") ;;
  *) die "nieoczekiwana zawartość $BINARY: $entries" ;;
esac
mkdir unpacked
tar -xzf "assets/$BINARY" -C unpacked
[ "$(tr -d '[:space:]' < unpacked/VERSION)" = "$VERSION" ] || die "VERSION w archiwum ≠ $VERSION"
RELEASE_AARS_SHA="$(sha256 unpacked/aars)"
if [ -n "$COMPARE" ]; then
  LOCAL_SHA="$(sha256 "$COMPARE")"
  [ "$LOCAL_SHA" = "$RELEASE_AARS_SHA" ] \
    || die "binarka z wydania ($RELEASE_AARS_SHA) ≠ binarka lokalna ($LOCAL_SHA)"
  COMPARE_NOTE="zgodna z lokalną kompilacją"
else
  COMPARE_NOTE="bez porównania z lokalną kompilacją (--compare)"
fi

echo "==> Commit źródłowy znacznika"
ref_type="$(gh api "repos/$REPO/git/ref/tags/$TAG" --jq .object.type)"
ref_sha="$(gh api "repos/$REPO/git/ref/tags/$TAG" --jq .object.sha)"
if [ "$ref_type" = "tag" ]; then
  ref_sha="$(gh api "repos/$REPO/git/tags/$ref_sha" --jq .object.sha)"
fi
[[ "$ref_sha" =~ ^[0-9a-f]{40}$ ]] || die "nie da się ustalić commitu znacznika $TAG"
local_sha="$(git -C "$REPO_DIR" rev-parse -q --verify "$TAG^{commit}" 2>/dev/null || true)"
if [ -n "$local_sha" ] && [ "$local_sha" != "$ref_sha" ]; then
  die "znacznik $TAG na GitHubie ($ref_sha) ≠ lokalny ($local_sha)"
fi

cat <<EOF

================ PODSUMOWANIE ================
Znacznik:   $TAG
Wersja:     $VERSION
Target:     $TARGET
Commit:     $ref_sha${local_sha:+ (zgodny z lokalnym repo)}
Binarka:    aars sha256 $RELEASE_AARS_SHA — $COMPARE_NOTE
Pliki:
EOF
jq -r '.assets[] | "  \(.name)  \(.size) B  \(.sha256)  [\(.kind)]"' manifest.json
echo "Notatki:"
jq -r '.notes' manifest.json | sed 's/^/  /'
echo "=============================================="
echo
[ -r /dev/tty ] || die "potwierdzenie wymaga terminala"
printf 'Wpisz wersję (%s), żeby podpisać: ' "$VERSION" > /dev/tty
read -r answer < /dev/tty
[ "$answer" = "$VERSION" ] || die "bez potwierdzenia — nic nie podpisano"

echo "==> Podpis (komentarz zaufany: aars $VERSION)"
cp manifest.json aars-manifest.json
minisign -S -s "$KEY" -t "aars $VERSION" -m aars-manifest.json

echo "==> Kontrola podpisu kluczem z repo"
minisign -V -p "$PUB" -m aars-manifest.json
grep -qx "trusted comment: aars $VERSION" aars-manifest.json.minisig || die "zły komentarz zaufany"

echo "==> Podpis do wydania i publikacja"
gh release upload "$TAG" --repo "$REPO" aars-manifest.json.minisig
if [[ "$VERSION" == *-* ]]; then
  # Wydanie przedpremierowe: widoczne, ale nie jako najnowsze (R5).
  gh release edit "$TAG" --repo "$REPO" --draft=false --prerelease
else
  gh release edit "$TAG" --repo "$REPO" --draft=false --latest
fi
echo "Gotowe: $TAG opublikowane."
