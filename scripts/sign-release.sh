#!/usr/bin/env bash
# Podpis wydania AARS (R4). Uruchamia właściciel na swoim komputerze, poza CI:
# klucz prywatny nigdy nie trafia na GitHuba.
#
#   scripts/sign-release.sh v0.10.0
#
# Pobiera manifest ze szkicu wydania, podpisuje go z komentarzem zaufanym
# "aars {wersja}", sprawdza podpis kluczem z keys/aars-release.pub, dołącza
# podpis do wydania i publikuje szkic jako najnowsze wydanie.
# Klucz: MINISIGN_KEY (domyślnie ~/.minisign/aars.key).
set -euo pipefail

die() { printf 'BŁĄD: %s\n' "$*" >&2; exit 1; }

TAG="${1:-}"
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]] || die "użycie: $0 v{wersja}, np. v0.10.0"
VERSION="${TAG#v}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUB="$REPO_DIR/keys/aars-release.pub"
KEY="${MINISIGN_KEY:-$HOME/.minisign/aars.key}"

command -v gh >/dev/null || die "brak gh (GitHub CLI)"
command -v minisign >/dev/null || die "brak minisign"
[ -f "$KEY" ] || die "brak klucza prywatnego: $KEY"
grep -q UNSET "$PUB" && die "$PUB zawiera UNSET — najpierw wpisz klucz publiczny wydań i zbuduj z nim wydanie"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

echo "==> Manifest ze szkicu $TAG"
gh release download "$TAG" --repo sptq/aars -p aars-manifest.json
grep -q "\"version\": *\"$VERSION\"" aars-manifest.json || die "manifest nie jest dla wersji $VERSION"

echo "==> Podpis (komentarz zaufany: aars $VERSION)"
minisign -S -s "$KEY" -t "aars $VERSION" -m aars-manifest.json

echo "==> Kontrola podpisu kluczem z repo"
minisign -V -p "$PUB" -m aars-manifest.json
grep -qx "trusted comment: aars $VERSION" aars-manifest.json.minisig || die "zły komentarz zaufany"

echo "==> Podpis do wydania i publikacja"
gh release upload "$TAG" --repo sptq/aars aars-manifest.json.minisig
if [[ "$VERSION" == *-* ]]; then
  # Wydanie przedpremierowe: widoczne, ale nie jako najnowsze (R5).
  gh release edit "$TAG" --repo sptq/aars --draft=false --prerelease
else
  gh release edit "$TAG" --repo sptq/aars --draft=false --latest
fi
echo "Gotowe: $TAG opublikowane."
