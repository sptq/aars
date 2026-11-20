//! Pozycja stacji i lokator QTH (Maidenhead), etap 9.

use sqlx::SqlitePool;

use crate::error::DbError;
use crate::settings::SettingsRepo;

const MSG_LAT: &str = "szerokość: liczba od −90 do 90";
const MSG_LON: &str = "długość: liczba od −180 do 180";
const MSG_BOTH: &str = "podaj obie współrzędne albo lokator";
const MSG_LOCATOR: &str = "lokator: 4, 6 albo 8 znaków, np. JO91 albo JO91RS";

#[derive(Debug, Clone, PartialEq)]
pub struct StationPosition {
    pub lat: f64,
    pub lon: f64,
    pub locator: String,
}

/// Lokator o `len` ∈ {4, 6, 8} znakach. Szerokość 90 i długość 180 leżą na
/// krawędzi świata, więc wpadają do ostatniej komórki, nie poza alfabet.
pub fn locator_from(lat: f64, lon: f64, len: usize) -> Result<String, String> {
    if !matches!(len, 4 | 6 | 8) {
        return Err(MSG_LOCATOR.to_owned());
    }
    if !(-90.0..=90.0).contains(&lat) {
        return Err(MSG_LAT.to_owned());
    }
    if !(-180.0..=180.0).contains(&lon) {
        return Err(MSG_LON.to_owned());
    }
    // Rachunek w liczbach całkowitych: pozycja w jednostkach 1e-5° (zaokrąglona),
    // komórka najdrobniejszego poziomu to 1/120° w długości i 1/240° w
    // szerokości, czyli indeks = n * 120 / 100_000 = n * 3 / 2500 (długość)
    // i n * 3 / 1250 (szerokość). Dzięki temu okrągłe wartości dziesiętne,
    // leżące dokładnie na krawędzi komórki, nie spadają o jedną komórkę w dół.
    let lon_units = ((lon + 180.0) * 1e5).round() as u64;
    let lat_units = ((lat + 90.0) * 1e5).round() as u64;
    let lon_cells = (lon_units * 3 / 2500).min(360 * 120 - 1);
    let lat_cells = (lat_units * 3 / 1250).min(180 * 240 - 1);
    let letter = |base: u8, n: u64| char::from(base + n as u8);
    let digit = |n: u64| char::from(b'0' + n as u8);

    let mut out = String::with_capacity(8);
    out.push(letter(b'A', lon_cells / 2400));
    out.push(letter(b'A', lat_cells / 2400));
    out.push(digit(lon_cells / 240 % 10));
    out.push(digit(lat_cells / 240 % 10));
    if len >= 6 {
        out.push(letter(b'A', lon_cells / 10 % 24));
        out.push(letter(b'A', lat_cells / 10 % 24));
    }
    if len == 8 {
        out.push(digit(lon_cells % 10));
        out.push(digit(lat_cells % 10));
    }
    Ok(out)
}

/// Środek kwadratu lokatora jako `(lat, lon)` z 5 miejscami.
pub fn locator_center(locator: &str) -> Result<(f64, f64), String> {
    let bad = || MSG_LOCATOR.to_owned();
    let chars: Vec<char> = locator.trim().to_ascii_uppercase().chars().collect();
    if !matches!(chars.len(), 4 | 6 | 8) {
        return Err(bad());
    }
    let letter = |c: char, max: u8| -> Result<f64, String> {
        if c.is_ascii_uppercase() && (c as u8 - b'A') < max {
            Ok(f64::from(c as u8 - b'A'))
        } else {
            Err(bad())
        }
    };
    let digit = |c: char| -> Result<f64, String> {
        c.to_digit(10).map(f64::from).ok_or_else(bad)
    };
    let mut lon = -180.0 + letter(chars[0], 18)? * 20.0 + digit(chars[2])? * 2.0;
    let mut lat = -90.0 + letter(chars[1], 18)? * 10.0 + digit(chars[3])?;
    let (mut dlon, mut dlat) = (2.0, 1.0);
    if chars.len() >= 6 {
        lon += letter(chars[4], 24)? * (2.0 / 24.0);
        lat += letter(chars[5], 24)? * (1.0 / 24.0);
        (dlon, dlat) = (2.0 / 24.0, 1.0 / 24.0);
    }
    if chars.len() == 8 {
        lon += digit(chars[6])? * (dlon / 10.0);
        lat += digit(chars[7])? * (dlat / 10.0);
        (dlon, dlat) = (dlon / 10.0, dlat / 10.0);
    }
    let round5 = |v: f64| (v * 1e5).round() / 1e5;
    Ok((round5(lat + dlat / 2.0), round5(lon + dlon / 2.0)))
}

fn parse_coord(text: &str, lo: f64, hi: f64, msg: &str) -> Result<f64, String> {
    let v: f64 = text.trim().replace(',', ".").parse().map_err(|_| msg.to_owned())?;
    if v.is_finite() && (lo..=hi).contains(&v) {
        // `+ 0.0` zamienia -0.0 na 0.0
        Ok((v * 1e5).round() / 1e5 + 0.0)
    } else {
        Err(msg.to_owned())
    }
}

/// R17: współrzędne wygrywają z lokatorem (lokator wyliczamy, 6 znaków, albo
/// zostawiamy podany, jeśli pasuje); sam lokator daje środek kwadratu.
pub fn parse_position(lat: &str, lon: &str, locator: &str) -> Result<Option<StationPosition>, String> {
    let (lat, lon, locator) = (lat.trim(), lon.trim(), locator.trim());
    match (lat.is_empty(), lon.is_empty()) {
        (true, true) => {
            if locator.is_empty() {
                return Ok(None);
            }
            let (lat, lon) = locator_center(locator)?;
            Ok(Some(StationPosition { lat, lon, locator: locator.to_ascii_uppercase() }))
        }
        (false, false) => {
            let lat = parse_coord(lat, -90.0, 90.0, MSG_LAT)?;
            let lon = parse_coord(lon, -180.0, 180.0, MSG_LON)?;
            let kept = locator.to_ascii_uppercase();
            let locator = match locator_center(&kept) {
                Ok(_) if locator_from(lat, lon, kept.len()).as_deref() == Ok(kept.as_str()) => kept,
                _ => locator_from(lat, lon, 6)?,
            };
            Ok(Some(StationPosition { lat, lon, locator }))
        }
        _ => Err(MSG_BOTH.to_owned()),
    }
}

pub struct PositionRepo {
    pool: SqlitePool,
}

impl PositionRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Brak albo zepsute klucze dają `None`; zepsute są zgłaszane w logu.
    pub async fn load(&self) -> Result<Option<StationPosition>, DbError> {
        let kv = SettingsRepo::new(self.pool.clone());
        let get = |k: &'static str| {
            let kv = &kv;
            async move { kv.kv_get(k).await }
        };
        let (lat, lon, loc) = (get("station:lat").await?, get("station:lon").await?, get("station:locator").await?);
        if lat.is_none() && lon.is_none() && loc.is_none() {
            return Ok(None);
        }
        let parsed = match (&lat, &lon, &loc) {
            (Some(la), Some(lo), Some(lc)) => parse_coord(la, -90.0, 90.0, MSG_LAT)
                .and_then(|la| parse_coord(lo, -180.0, 180.0, MSG_LON).map(|lo| (la, lo)))
                .and_then(|(la, lo)| locator_center(lc).map(|_| StationPosition { lat: la, lon: lo, locator: lc.clone() })),
            _ => Err("brak części kluczy".to_owned()),
        };
        match parsed {
            Ok(p) => Ok(Some(p)),
            Err(e) => {
                tracing::error!("zepsuta pozycja stacji w ustawieniach ({e}); pomijam");
                Ok(None)
            }
        }
    }

    pub async fn save(&self, p: Option<&StationPosition>) -> Result<(), DbError> {
        let kv = SettingsRepo::new(self.pool.clone());
        match p {
            Some(p) => {
                kv.kv_put("station:lat", &format!("{:.5}", p.lat)).await?;
                kv.kv_put("station:lon", &format!("{:.5}", p.lon)).await?;
                kv.kv_put("station:locator", &p.locator).await?;
            }
            None => {
                for key in ["station:lat", "station:lon", "station:locator"] {
                    kv.kv_delete_prefix(key).await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Wartości z briefu zadania (policzone niezależnie 2026-11-20): Monachium
    // JN58TD, Warszawa KO02MF, Montevideo GF15VC, Łódź JO91RS42; schemat
    // Maidenhead: pole 20°x10°, kwadrat 2°x1°, podkwadrat 1/12°x1/24°.
    #[test]
    fn known_points_give_known_locators() {
        assert_eq!(locator_from(51.759, 19.456, 4).unwrap(), "JO91");
        assert_eq!(locator_from(51.759, 19.456, 6).unwrap(), "JO91RS");
        assert_eq!(locator_from(51.759, 19.456, 8).unwrap(), "JO91RS42");
        assert_eq!(locator_from(48.14666, 11.60833, 6).unwrap(), "JN58TD");
        assert_eq!(locator_from(52.22977, 21.01178, 6).unwrap(), "KO02MF");
        assert_eq!(locator_from(-34.91, -56.21, 6).unwrap(), "GF15VC");
        assert!(locator_from(51.0, 19.0, 5).is_err());
    }

    #[test]
    fn a_locator_means_the_centre_of_its_square() {
        assert_eq!(locator_center("JO91").unwrap(), (51.5, 19.0));
        assert_eq!(locator_center("jo91rs").unwrap(), (51.77083, 19.45833));
        assert_eq!(locator_center("JO91RS42").unwrap(), (51.76042, 19.45417));
        for bad in ["JO9", "JO91R", "ZZ91", "JO91ZZ", "JOAB", ""] {
            assert!(locator_center(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn edges_of_the_world_stay_inside_the_alphabet() {
        assert_eq!(locator_from(90.0, 180.0, 6).unwrap(), "RR99XX");
        assert_eq!(locator_from(-90.0, -180.0, 6).unwrap(), "AA00AA");
        assert_eq!(locator_from(0.0, 0.0, 6).unwrap(), "JJ00AA");
    }

    #[test]
    fn a_decimal_comma_is_accepted() {
        let p = parse_position("51,759", "19,456", "").unwrap().unwrap();
        assert_eq!((p.lat, p.lon), (51.759, 19.456));
        assert_eq!(p.locator, "JO91RS");
    }

    #[test]
    fn coordinates_win_and_keep_a_matching_locator() {
        let p = parse_position("51.759", "19.456", "JO91").unwrap().unwrap();
        assert_eq!(p.locator, "JO91");
        let p = parse_position("51.759", "19.456", "KO02").unwrap().unwrap();
        assert_eq!(p.locator, "JO91RS");
        assert_eq!((p.lat, p.lon), (51.759, 19.456));
    }

    #[test]
    fn a_locator_alone_gives_its_centre() {
        let p = parse_position("", "", "jo91rs").unwrap().unwrap();
        assert_eq!(p, StationPosition { lat: 51.77083, lon: 19.45833, locator: "JO91RS".into() });
        assert!(parse_position("", "", "JO9").is_err());
    }

    #[test]
    fn half_a_position_is_refused() {
        assert_eq!(parse_position("51.7", "", "").unwrap_err(), "podaj obie współrzędne albo lokator");
        assert_eq!(parse_position("", "19.4", "JO91").unwrap_err(), "podaj obie współrzędne albo lokator");
    }

    #[test]
    fn out_of_range_is_refused() {
        assert_eq!(parse_position("91", "19", "").unwrap_err(), "szerokość: liczba od −90 do 90");
        assert_eq!(parse_position("51", "181", "").unwrap_err(), "długość: liczba od −180 do 180");
        assert!(parse_position("abc", "19", "").is_err());
        assert!(parse_position("NaN", "19", "").is_err());
    }

    #[test]
    fn all_empty_clears() {
        assert_eq!(parse_position(" ", "", " ").unwrap(), None);
    }

    #[tokio::test]
    async fn position_round_trips_and_none_clears() {
        let repo = PositionRepo::new(crate::db::test_pool().await);
        assert_eq!(repo.load().await.unwrap(), None);
        let p = parse_position("51,759", "19,456", "").unwrap().unwrap();
        repo.save(Some(&p)).await.unwrap();
        assert_eq!(repo.load().await.unwrap(), Some(p));
        repo.save(None).await.unwrap();
        assert_eq!(repo.load().await.unwrap(), None);
    }

    #[tokio::test]
    async fn broken_keys_give_none() {
        let pool = crate::db::test_pool().await;
        let kv = SettingsRepo::new(pool.clone());
        kv.kv_put("station:lat", "xx").await.unwrap();
        assert_eq!(PositionRepo::new(pool).load().await.unwrap(), None);
    }

    #[test]
    fn round_decimal_degrees_land_in_the_cell_that_contains_them() {
        // Krawędzie komórek 8-znakowych (1/120°, 1/240°) leżą na okrągłych
        // wartościach dziesiętnych; sprawdzamy, że wejście mieści się w
        // komórce wyliczonego lokatora: |środek - wejście| < połowa komórki
        // (z lewym końcem włączonym).
        let (hl, ha) = (1.0 / 240.0, 1.0 / 480.0);
        for i in -1799..=1799 {
            let lon = f64::from(i) / 10.0;
            for j in [-899, -450, -1, 0, 1, 450, 899] {
                let lat = f64::from(j) / 10.0;
                let loc = locator_from(lat, lon, 8).unwrap();
                let (clat, clon) = locator_center(&loc).unwrap();
                let eps = 2e-5;
                assert!(lon >= clon - hl - eps && lon < clon + hl + eps, "{lat} {lon} {loc}");
                assert!(lat >= clat - ha - eps && lat < clat + ha + eps, "{lat} {lon} {loc}");
            }
        }
        for j in -899..=899 {
            let lat = f64::from(j) / 10.0;
            let loc = locator_from(lat, -179.9, 8).unwrap();
            let (clat, _) = locator_center(&loc).unwrap();
            assert!(lat >= clat - ha - 2e-5 && lat < clat + ha + 2e-5, "{lat} {loc}");
        }
    }

    #[test]
    fn a_typed_8_char_locator_survives_parse_position() {
        for loc in ["JO91RS42", "AA00AA00", "RR99XX99", "JN58TD00"] {
            let (lat, lon) = locator_center(loc).unwrap();
            let p = parse_position(&lat.to_string(), &lon.to_string(), loc).unwrap().unwrap();
            assert_eq!(p.locator, loc);
        }
        let p = parse_position("-0", "0", "").unwrap().unwrap();
        assert_eq!(format!("{:.5}", p.lat), "0.00000");
    }
}
