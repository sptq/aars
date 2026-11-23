//! Mapa ostrzeżeń meteo w panelu (etap 11c): strona `/map` i dane `/map/data`.
//!
//! Ostrzeżenia pochodzą z pamięci źródła `imgw-meteo` (ostatnie udane
//! pobranie, wywołanie zwrotne `WarningsMap`). Stan inny niż `ok` nie wysyła
//! żadnych ostrzeżeń: mapa bez kolorów z banerem nie może wyglądać jak
//! „brak ostrzeżeń” (C12).

use aars_core::geo::{PositionRepo, StationPosition};
use aars_core::source_state::{SourceConfig, SourceStateRepo};
use askama::Template;
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::auth::{AuthRejection, CurrentUser};
use crate::chrome::Chrome;
use crate::render::{db_error, page};
use crate::state::{AppState, WarningsSnapshot};

const SOURCE: &str = "imgw-meteo";

#[derive(Debug, Serialize)]
pub(crate) struct MapData {
    source: &'static str,
    state: &'static str,
    fetched_at: Option<String>,
    warnings: Vec<MapWarningView>,
    station: Option<LatLon>,
    area: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct MapWarningView {
    teryt: String,
    level: u8,
    event: String,
    until: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct LatLon {
    lat: f64,
    lon: f64,
}

pub(crate) fn map_data(
    cfg: Option<&SourceConfig>,
    snapshot: Option<WarningsSnapshot>,
    station: Option<&StationPosition>,
    now: DateTime<Utc>,
) -> MapData {
    let area = cfg
        .map(|c| c.area.iter().map(|e| e.code.clone()).collect())
        .unwrap_or_default();
    let station = station.map(|p| LatLon {
        lat: p.lat,
        lon: p.lon,
    });
    let mut data = MapData {
        source: SOURCE,
        state: "disabled",
        fetched_at: None,
        warnings: Vec::new(),
        station,
        area,
    };

    // Pamięci wyłączonego źródła nie pokazujemy wcale, nawet czasu pobrania.
    let Some(cfg) = cfg.filter(|c| c.enabled) else {
        return data;
    };
    // Pusta pamięć (np. po restarcie) to „never”, choćby baza pamiętała
    // świeży sukces sprzed restartu (rozstrzygnięcie 5).
    let Some(snapshot) = snapshot else {
        data.state = "never";
        return data;
    };
    data.fetched_at = Some(iso(snapshot.fetched_at));
    let limit_s = i64::try_from(cfg.interval_s.saturating_mul(3).max(1800)).unwrap_or(i64::MAX);
    if (now - snapshot.fetched_at).num_seconds() > limit_s {
        data.state = "stale";
        return data;
    }

    data.state = "ok";
    let mut warnings: Vec<MapWarningView> = Vec::new();
    for w in snapshot.warnings.into_iter().filter(|w| w.until > now) {
        let view = MapWarningView {
            teryt: w.teryt,
            // Ostrzeżenie bez stopnia lepiej pokazać jako 1. niż ukryć (C12).
            level: w.level.clamp(1, 3),
            event: w.event,
            until: iso(w.until),
        };
        if !warnings.contains(&view) {
            warnings.push(view);
        }
    }
    warnings.sort_by(|a, b| a.teryt.cmp(&b.teryt).then(b.level.cmp(&a.level)));
    data.warnings = warnings;
    data
}

fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[derive(Template)]
#[template(path = "map.html")]
struct MapPage {
    chrome: Chrome,
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    Ok(page(&MapPage {
        chrome: Chrome::load(&state, &current).await,
    }))
}

pub async fn data(
    State(state): State<AppState>,
    _current: CurrentUser,
) -> Result<Response, AuthRejection> {
    let cfg = match SourceStateRepo::new(state.pool.clone())
        .config(SOURCE)
        .await
    {
        Ok(cfg) => cfg,
        Err(err) => {
            return Ok(db_error(
                "odczyt konfiguracji źródła dla mapy zawiódł",
                &err,
            ));
        }
    };
    let station = match PositionRepo::new(state.pool.clone()).load().await {
        Ok(station) => station,
        Err(err) => return Ok(db_error("odczyt pozycji stacji dla mapy zawiódł", &err)),
    };
    let snapshot = state.warnings_map.as_ref().and_then(|memory| memory());
    let body = map_data(cfg.as_ref(), snapshot, station.as_ref(), Utc::now());
    Ok(Json(body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router;
    use crate::state::MapWarning;
    use crate::tests::{login, state};
    use aars_core::area::{area_text, parse_area};
    use aars_core::user::{Role, UserRepo};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use chrono::Duration;
    use std::sync::Arc;
    use tower::ServiceExt;

    const NOW: &str = "2026-07-23T12:00:00Z";

    fn now() -> DateTime<Utc> {
        NOW.parse().unwrap()
    }

    fn cfg(enabled: bool, area: &str, interval_s: u64) -> SourceConfig {
        let area = parse_area(area).unwrap();
        let area_text = area_text(&area);
        SourceConfig {
            enabled,
            area,
            area_text,
            interval_s,
        }
    }

    fn snap(fetched_at: DateTime<Utc>, warnings: Vec<MapWarning>) -> WarningsSnapshot {
        WarningsSnapshot {
            fetched_at,
            warnings,
        }
    }

    fn mw(teryt: &str, level: u8, event: &str, until: DateTime<Utc>) -> MapWarning {
        MapWarning {
            teryt: teryt.to_owned(),
            level,
            event: event.to_owned(),
            until,
        }
    }

    fn one_live(fetched_at: DateTime<Utc>) -> WarningsSnapshot {
        snap(
            fetched_at,
            vec![mw("1863", 2, "Silny wiatr", now() + Duration::hours(1))],
        )
    }

    #[test]
    fn a_fresh_snapshot_is_ok_with_live_warnings_only() {
        let now = now();
        let d = map_data(
            Some(&cfg(true, "1863 Rzeszów", 300)),
            Some(snap(
                now - Duration::minutes(2),
                vec![
                    mw("1863", 2, "Silny wiatr", now + Duration::hours(1)),
                    mw("1806", 1, "Burze", now - Duration::minutes(1)),
                ],
            )),
            None,
            now,
        );
        assert_eq!(d.state, "ok");
        assert_eq!(d.warnings.len(), 1);
        assert_eq!(
            (d.warnings[0].teryt.as_str(), d.warnings[0].level),
            ("1863", 2)
        );
        assert_eq!(d.warnings[0].until, "2026-07-23T13:00:00Z");
        assert_eq!(d.fetched_at.as_deref(), Some("2026-07-23T11:58:00Z"));
        assert_eq!(d.area, ["1863"]);
        assert!(d.station.is_none());
    }

    #[test]
    fn warnings_are_one_per_county_and_event_sorted_by_county_then_level() {
        let now = now();
        let until = now + Duration::hours(3);
        let d = map_data(
            Some(&cfg(true, "18", 300)),
            Some(snap(
                now,
                vec![
                    mw("1863", 1, "Burze", until),
                    mw("1801", 1, "Upał", until),
                    mw("1863", 3, "Silny wiatr", until),
                    mw("1863", 1, "Burze", until),
                    mw("1801", 0, "Bez stopnia", until),
                ],
            )),
            None,
            now,
        );
        let got: Vec<(&str, u8, &str)> = d
            .warnings
            .iter()
            .map(|w| (w.teryt.as_str(), w.level, w.event.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("1801", 1, "Upał"),
                ("1801", 1, "Bez stopnia"),
                ("1863", 3, "Silny wiatr"),
                ("1863", 1, "Burze")
            ]
        );
    }

    #[test]
    fn an_empty_memory_is_never_even_if_the_database_says_fresh() {
        let d = map_data(Some(&cfg(true, "18", 300)), None, None, now());
        assert_eq!(
            (d.state, d.fetched_at, d.warnings.len()),
            ("never", None, 0)
        );
        assert_eq!(d.area, ["18"]);
    }

    #[test]
    fn an_old_snapshot_is_stale_and_uncoloured() {
        let now = now();
        let at = |interval: u64, minutes: i64| {
            map_data(
                Some(&cfg(true, "1863 Rzeszów", interval)),
                Some(one_live(now - Duration::minutes(minutes))),
                None,
                now,
            )
        };
        let stale = at(300, 31);
        assert_eq!(stale.state, "stale");
        assert!(stale.warnings.is_empty());
        assert_eq!(stale.fetched_at.as_deref(), Some("2026-07-23T11:29:00Z"));
        assert_eq!(at(300, 29).state, "ok");
        assert_eq!(at(300, 29).warnings.len(), 1);
        assert_eq!(at(900, 44).state, "ok");
        assert_eq!(at(900, 46).state, "stale");
        assert!(at(900, 46).warnings.is_empty());
    }

    #[test]
    fn a_disabled_or_unconfigured_source_is_disabled() {
        let now = now();
        let off = map_data(
            Some(&cfg(false, "1863 Rzeszów", 300)),
            Some(one_live(now)),
            None,
            now,
        );
        assert_eq!(off.state, "disabled");
        assert!(off.warnings.is_empty());
        assert_eq!(off.area, ["1863"]);

        let unknown = map_data(None, Some(one_live(now)), None, now);
        assert_eq!(unknown.state, "disabled");
        assert!(unknown.warnings.is_empty());
        assert!(unknown.area.is_empty());
    }

    async fn viewer_get(state: AppState, uri: &str) -> (StatusCode, String, String) {
        UserRepo::new(state.pool.clone())
            .create("v", "haslo", Role::Viewer)
            .await
            .unwrap();
        let app = router(state);
        let (cookie, _) = login(&app, "v", "haslo").await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let mime = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, mime, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn map_data_is_json_for_a_viewer_with_the_station() {
        let fetched = Utc::now() - Duration::minutes(1);
        let state = state().await.with_warnings_map(Arc::new(move || {
            let until = fetched + Duration::hours(1);
            Some(snap(fetched, vec![mw("1863", 2, "Silny wiatr", until)]))
        }));
        let repo = SourceStateRepo::new(state.pool.clone());
        repo.save_config(SOURCE, "1863 Rzeszów", 300).await.unwrap();
        repo.set_enabled(SOURCE, true).await.unwrap();
        PositionRepo::new(state.pool.clone())
            .save(Some(&StationPosition {
                lat: 50.04,
                lon: 21.99,
                locator: "KN09BX".to_owned(),
            }))
            .await
            .unwrap();

        let (status, mime, body) = viewer_get(state, "/map/data").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(mime.contains("application/json"), "{mime}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["station"]["lat"], 50.04);
        assert_eq!(v["state"], "ok");
        assert_eq!(v["source"], "imgw-meteo");
        assert_eq!(v["area"], serde_json::json!(["1863"]));
        assert_eq!(v["warnings"][0]["teryt"], "1863");
    }

    #[tokio::test]
    async fn the_map_page_carries_the_map_and_the_attribution() {
        let (status, _, html) = viewer_get(state().await, "/map").await;
        assert_eq!(status, StatusCode::OK);
        for needle in [
            "id=\"warnings-map\"",
            "/assets/warnings-map.js",
            "/assets/powiaty.geojson",
            "Źródłem pochodzenia danych jest IMGW-PIB; dane zostały przetworzone",
            "Granice: PRG, GUGiK",
            "href=\"/map\"",
            // Bez JS albo Leafletu komunikat zostaje widoczny; JS go chowa.
            "<p id=\"map-banner\" class=\"error\">Ładowanie mapy…</p>",
        ] {
            assert!(html.contains(needle), "brak {needle}");
        }
    }

    /// Skrypt mapy: zawieszone pobranie przerwane po 20 s, starsza odpowiedź
    /// nie nadpisuje nowszej, dane starsze niż 30 min nie barwią mapy.
    #[test]
    fn the_map_script_guards_against_hung_and_old_data() {
        const JS: &str = include_str!("../../../assets/warnings-map.js");
        for needle in [
            "new AbortController()",
            "FETCH_TIMEOUT_MS = 20 * 1000",
            "seq !== latest",
            "MAX_AGE_MS = 30 * 60 * 1000",
            "'visibilitychange'",
            "BANNERS.old",
        ] {
            assert!(JS.contains(needle), "brak {needle}");
        }
    }
}
