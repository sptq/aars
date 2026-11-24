//! Obszar ostrzeżeń liczony z pozycji stacji (etap 13): powiat z wbudowanych
//! granic (GeoJSON, lon/lat) i województwo z pierwszych dwóch cyfr TERYT.

use std::sync::OnceLock;

use crate::area::{AreaEntry, voivodeship_name};
use crate::geo::StationPosition;

static POWIATY: &[u8] = include_bytes!("../../../assets/powiaty.geojson");

/// Powiat, w którym leży punkt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct County {
    /// 4 cyfry.
    pub teryt: String,
    /// Nazwa z granic, np. „powiat opoczyński”.
    pub name: String,
}

/// Jak gruby ma być obszar liczony z pozycji.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaGranularity {
    County,
    Voivodeship,
}

type Ring = Vec<(f64, f64)>;

struct CountyShape {
    county: County,
    /// (min_lon, min_lat, max_lon, max_lat)
    bbox: (f64, f64, f64, f64),
    /// Wielokąty: pierwszy pierścień zewnętrzny, reszta to dziury.
    polygons: Vec<Vec<Ring>>,
}

fn parse_ring(value: &serde_json::Value) -> Option<Ring> {
    value
        .as_array()?
        .iter()
        .map(|p| {
            let p = p.as_array()?;
            Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
        })
        .collect()
}

fn parse_shapes(bytes: &[u8]) -> Vec<CountyShape> {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Vec::new();
    };
    let Some(features) = root["features"].as_array() else {
        return Vec::new();
    };
    features
        .iter()
        .filter_map(|f| {
            let teryt = f["properties"]["teryt"].as_str()?.to_owned();
            let name = f["properties"]["nazwa"].as_str()?.to_owned();
            let polygons: Vec<Vec<Ring>> = f["geometry"]["coordinates"]
                .as_array()?
                .iter()
                .map(|poly| {
                    poly.as_array()?
                        .iter()
                        .map(parse_ring)
                        .collect::<Option<Vec<_>>>()
                })
                .collect::<Option<_>>()?;
            let mut bbox = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for &(lon, lat) in polygons.iter().flatten().flatten() {
                bbox = (
                    bbox.0.min(lon),
                    bbox.1.min(lat),
                    bbox.2.max(lon),
                    bbox.3.max(lat),
                );
            }
            Some(CountyShape {
                county: County { teryt, name },
                bbox,
                polygons,
            })
        })
        .collect()
}

fn shapes() -> &'static [CountyShape] {
    static SHAPES: OnceLock<Vec<CountyShape>> = OnceLock::new();
    SHAPES.get_or_init(|| parse_shapes(POWIATY))
}

/// Ray casting: czy punkt leży w pierścieniu.
fn in_ring(ring: &Ring, lon: f64, lat: f64) -> bool {
    let mut inside = false;
    let mut j = ring.len().saturating_sub(1);
    for i in 0..ring.len() {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn in_polygon(rings: &[Ring], lon: f64, lat: f64) -> bool {
    match rings.split_first() {
        Some((outer, holes)) => {
            in_ring(outer, lon, lat) && !holes.iter().any(|h| in_ring(h, lon, lat))
        }
        None => false,
    }
}

/// Powiat zawierający punkt; `None` poza Polską (albo na morzu).
pub fn county_at(lat: f64, lon: f64) -> Option<County> {
    shapes()
        .iter()
        .find(|s| {
            lon >= s.bbox.0
                && lat >= s.bbox.1
                && lon <= s.bbox.2
                && lat <= s.bbox.3
                && s.polygons.iter().any(|p| in_polygon(p, lon, lat))
        })
        .map(|s| s.county.clone())
}

/// Obszar z pozycji stacji; `None` bez pozycji albo poza Polską.
pub fn auto_area(
    position: Option<&StationPosition>,
    granularity: AreaGranularity,
) -> Option<AreaEntry> {
    let county = county_at(position?.lat, position?.lon)?;
    match granularity {
        AreaGranularity::County => Some(AreaEntry {
            code: county.teryt,
            label: county.name,
        }),
        AreaGranularity::Voivodeship => {
            let code = county.teryt.get(..2)?;
            let name = voivodeship_name(code)?;
            Some(AreaEntry {
                code: code.to_owned(),
                label: format!("województwo {name}"),
            })
        }
    }
}

/// Zdanie dla panelu o obszarze liczonym z pozycji.
pub fn auto_area_note(position: Option<&StationPosition>, granularity: AreaGranularity) -> String {
    if position.is_none() {
        return "Brak pozycji stacji — ustaw ją w Ustawieniach".to_owned();
    }
    match auto_area(position, granularity) {
        Some(entry) => format!("Automatycznie: {} ({})", entry.label, entry.code),
        None => "Pozycja stacji leży poza obszarem Polski — wpisz obszar ręcznie".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(lat: f64, lon: f64) -> StationPosition {
        StationPosition {
            lat,
            lon,
            locator: String::new(),
        }
    }

    #[test]
    fn counties_for_known_points() {
        let c = county_at(50.04, 21.99).unwrap();
        assert_eq!(
            (c.teryt.as_str(), c.name.as_str()),
            ("1863", "powiat Rzeszów")
        );
        let c = county_at(51.38, 20.28).unwrap();
        assert_eq!(
            (c.teryt.as_str(), c.name.as_str()),
            ("1007", "powiat opoczyński")
        );
        assert_eq!(county_at(52.23, 21.01).unwrap().teryt, "1465");
    }

    #[test]
    fn points_outside_poland_have_no_county() {
        assert_eq!(county_at(55.0, 18.0), None);
        assert_eq!(county_at(52.52, 13.40), None);
    }

    #[test]
    fn auto_area_by_granularity() {
        let p = pos(51.38, 20.28);
        let county = auto_area(Some(&p), AreaGranularity::County).unwrap();
        assert_eq!(
            (county.code.as_str(), county.label.as_str()),
            ("1007", "powiat opoczyński")
        );
        let voiv = auto_area(Some(&p), AreaGranularity::Voivodeship).unwrap();
        assert_eq!(
            (voiv.code.as_str(), voiv.label.as_str()),
            ("10", "województwo łódzkie")
        );
        assert_eq!(auto_area(None, AreaGranularity::County), None);
        assert_eq!(
            auto_area(Some(&pos(52.52, 13.40)), AreaGranularity::County),
            None
        );
    }

    #[test]
    fn the_automatic_entry_is_canonical_for_the_manual_parser() {
        let entry = auto_area(Some(&pos(51.38, 20.28)), AreaGranularity::County).unwrap();
        let parsed =
            crate::area::parse_area(&crate::area::area_text(std::slice::from_ref(&entry))).unwrap();
        assert_eq!(parsed, vec![entry]);
    }

    #[test]
    fn a_hole_is_not_inside() {
        let ring = |a: f64, b: f64| vec![(a, a), (b, a), (b, b), (a, b), (a, a)];
        let rings = vec![ring(0.0, 10.0), ring(4.0, 6.0)];
        assert!(in_polygon(&rings, 2.0, 2.0));
        assert!(!in_polygon(&rings, 5.0, 5.0));
        assert!(!in_polygon(&rings, 11.0, 5.0));
    }

    #[test]
    fn notes_for_the_panel() {
        assert_eq!(
            auto_area_note(Some(&pos(51.38, 20.28)), AreaGranularity::County),
            "Automatycznie: powiat opoczyński (1007)"
        );
        assert!(auto_area_note(None, AreaGranularity::County).starts_with("Brak pozycji stacji"));
        assert!(auto_area_note(Some(&pos(52.52, 13.40)), AreaGranularity::County).contains("poza"));
    }
}
