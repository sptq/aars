//! Manifest wydania (R4) dla `release.yml`: sumy SHA-256, rozmiary i rodzaje
//! plików wydania. Podpisuje go właściciel poza CI (`scripts/sign-release.sh`).
//!
//! ```text
//! cargo run -p aars --example make_manifest -- --version 0.10.0 \
//!     [--notes-file notatki.txt] dist/* > aars-manifest.json
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};

use aars_core::update::{
    Asset, AssetKind, MANIFEST_NAME, Manifest, SIGNATURE_NAME, TARGET, binary_archive_name,
};
use sha2::{Digest, Sha256};

/// Nazwa listy dla Raspberry Pi Imager w wydaniu.
const IMAGER_LIST: &str = "aars-os-list.json";

/// Rodzaj pliku wydania według nazwy. Nieznana nazwa to błąd: każdy plik
/// wydania musi być w manifeście, a manifest zna tylko te rodzaje.
fn kind_of(name: &str, version: &str) -> Result<AssetKind, String> {
    if name == binary_archive_name(version) {
        Ok(AssetKind::Binary)
    } else if name == format!("aars-{version}-rpi.img.xz") {
        Ok(AssetKind::Image)
    } else if name == IMAGER_LIST {
        Ok(AssetKind::ImagerList)
    } else if name.starts_with("piper_tts-") && name.ends_with(".tar.gz") {
        Ok(AssetKind::Source)
    } else {
        Err(format!("nieznany plik wydania: {name}"))
    }
}

fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let hex = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((hex, size))
}

/// Buduje manifest i sprawdza go tym samym `Manifest::parse`, którego
/// używa stacja.
fn build(version: &str, notes: &str, published_at: &str, files: &[PathBuf]) -> Result<String, String> {
    let mut assets = Vec::new();
    for path in files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("zła nazwa pliku: {}", path.display()))?;
        if name == MANIFEST_NAME || name == SIGNATURE_NAME {
            return Err(format!("{name} nie może być w manifeście"));
        }
        let kind = kind_of(name, version)?;
        let (sha256, size) = sha256_file(path)?;
        assets.push(Asset { name: name.to_owned(), sha256, size, kind });
    }
    assets.sort_by(|a, b| a.name.cmp(&b.name));
    let manifest = Manifest {
        format: 1,
        version: version.to_owned(),
        target: TARGET.to_owned(),
        published_at: published_at.to_owned(),
        notes: notes.trim().to_owned(),
        assets,
    };
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    let parsed = Manifest::parse(text.as_bytes())?;
    if parsed != manifest {
        return Err("manifest po odczycie różni się od zapisanego".to_owned());
    }
    if !manifest.assets.iter().any(|a| a.kind == AssetKind::Image) {
        return Err("wydanie bez obrazu".to_owned());
    }
    Ok(text + "\n")
}

fn run() -> Result<String, String> {
    let mut args = std::env::args().skip(1);
    let mut version = None;
    let mut notes = String::new();
    let mut files = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => version = args.next(),
            "--notes-file" => {
                let path = args.next().ok_or("--notes-file bez ścieżki")?;
                notes = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
            }
            other if other.starts_with("--") => return Err(format!("nieznana opcja {other}")),
            _ => files.push(PathBuf::from(arg)),
        }
    }
    let version = version.ok_or("brak --version")?;
    let published_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    build(&version, &notes, &published_at, &files)
}

fn main() {
    match run() {
        Ok(text) => print!("{text}"),
        Err(err) => {
            eprintln!("make_manifest: {err}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_lists_every_file_with_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for (name, body) in [
            ("aars-0.10.0-aarch64-linux-gnu.tar.gz", "binarka"),
            ("aars-0.10.0-rpi.img.xz", "obraz"),
            ("aars-os-list.json", "{}"),
            ("piper_tts-1.8.0.tar.gz", "źródło"),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            files.push(path);
        }
        let text = build("0.10.0", "Notatki.\n", "2026-11-21T12:00:00Z", &files).unwrap();
        let manifest = Manifest::parse(text.as_bytes()).unwrap();
        assert_eq!(manifest.version, "0.10.0");
        assert_eq!(manifest.target, TARGET);
        assert_eq!(manifest.notes, "Notatki.");
        assert_eq!(manifest.assets.len(), 4);
        let binary = manifest.binary();
        assert_eq!(binary.name, "aars-0.10.0-aarch64-linux-gnu.tar.gz");
        assert_eq!(binary.size, 7);
        // printf binarka | sha256sum
        assert_eq!(binary.sha256, "a81e472a7a92ace94e2068d9014d252c5e79b634436f543b47933b334e77e4ce");
        let kinds: Vec<_> = manifest.assets.iter().map(|a| (a.name.as_str(), a.kind)).collect();
        assert!(kinds.contains(&("aars-os-list.json", AssetKind::ImagerList)));
        assert!(kinds.contains(&("piper_tts-1.8.0.tar.gz", AssetKind::Source)));
        assert!(kinds.contains(&("aars-0.10.0-rpi.img.xz", AssetKind::Image)));
    }

    #[test]
    fn unknown_files_and_a_wrong_version_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("aars-0.10.0-aarch64-linux-gnu.tar.gz");
        let image = dir.path().join("aars-0.10.0-rpi.img.xz");
        let extra = dir.path().join("notatki.txt");
        for p in [&binary, &image, &extra] {
            std::fs::write(p, "x").unwrap();
        }
        let at = "2026-11-21T12:00:00Z";
        assert!(build("0.10.0", "", at, &[binary.clone(), image.clone(), extra]).is_err());
        assert!(build("0.11.0", "", at, &[binary.clone(), image.clone()]).is_err());
        assert!(build("0.10.0", "", at, std::slice::from_ref(&binary)).is_err(), "bez obrazu");
        let manifest = dir.path().join(MANIFEST_NAME);
        std::fs::write(&manifest, "{}").unwrap();
        assert!(build("0.10.0", "", at, &[binary, image, manifest]).is_err());
    }
}
