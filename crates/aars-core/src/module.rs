//! Rejestr modułów stacji (etap 9): opisy modułów jako dane, przełącznik
//! włączenia i schemat ustawień sprawdzany po stronie serwera.

use std::collections::BTreeMap;

use chrono::NaiveTime;
use sqlx::SqlitePool;

use crate::area::{area_text, parse_area};
use crate::error::DbError;
use crate::settings::SettingsRepo;
use crate::source_state::SourceStateRepo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Source,
    Scheduler,
    Ident,
    Bulletin,
}

impl ModuleKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Source => "źródło danych",
            Self::Scheduler => "harmonogram",
            Self::Ident => "identyfikacja stacji",
            Self::Bulletin => "biuletyn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Switchable,
    /// Powód, dla którego nie wolno wyłączyć.
    AlwaysOn(&'static str),
    /// Kiedy moduł będzie dostępny.
    Planned(&'static str),
}

/// Czego źródło potrzebuje od stacji, żeby w ogóle pracować.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    /// Obszar (TERYT) z ustawień źródła.
    Area,
    /// Pozycja stacji (lokator albo współrzędne).
    Position,
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingKind {
    Text { max_len: usize },
    Integer { min: i64, max: i64 },
    Area,
    Choice(&'static [(&'static str, &'static str)]),
    /// Liczba z jedną cyfrą po przecinku; wejście z kropką albo przecinkiem.
    Decimal { min: f64, max: f64 },
    /// Podzbiór wartości z listy (wartość, etykieta), zapisany po przecinku.
    List(&'static [(&'static str, &'static str)]),
    /// Pory doby `HH:MM` po przecinku; kanonicznie posortowane, bez duplikatów.
    TimeList { max: usize },
    /// Układ biuletynu z klocków (JSON, `bulletin_layout::Layout`); pusty = domyślny.
    BulletinLayout,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettingSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: SettingKind,
    pub default: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModuleDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub kind: ModuleKind,
    pub toggle: Toggle,
    pub default_enabled: bool,
    pub needs: Needs,
    /// Obszar liczony z pozycji stacji, gdy ręczny jest pusty (etap 13).
    pub auto_area: Option<crate::area_auto::AreaGranularity>,
    pub settings: &'static [SettingSpec],
    /// (tekst linku, ścieżka)
    pub see_also: Option<(&'static str, &'static str)>,
}

impl ModuleDescriptor {
    /// Źródła zachowują klucze z etapu 8 (`source:{id}:{key}`).
    pub fn setting_key(&self, key: &str) -> String {
        match self.kind {
            ModuleKind::Source => format!("source:{}:{key}", self.id),
            _ => format!("module:{}:{key}", self.id),
        }
    }

    /// Sprawdza i sprowadza do postaci kanonicznej wszystkie ustawienia
    /// z opisu; obce klucze wejścia są pomijane.
    pub fn validate(&self, input: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
        let mut out = BTreeMap::new();
        for spec in self.settings {
            let raw = input
                .get(spec.key)
                .ok_or_else(|| format!("{}: brak wartości", spec.label))?;
            let value = match spec.kind {
                SettingKind::Text { max_len } => {
                    let v = raw.trim();
                    if v.chars().count() > max_len {
                        return Err(format!("{}: najwyżej {max_len} znaków", spec.label));
                    }
                    v.to_owned()
                }
                SettingKind::Integer { min, max } => match raw.trim().parse::<i64>() {
                    Ok(n) if (min..=max).contains(&n) => n.to_string(),
                    _ => return Err(format!("{}: {min}–{max}", spec.label)),
                },
                SettingKind::Area => area_text(&parse_area(raw)?),
                SettingKind::Choice(options) => {
                    let v = raw.trim();
                    if !options.iter().any(|(value, _)| *value == v) {
                        return Err(format!("{}: nieznana wartość", spec.label));
                    }
                    v.to_owned()
                }
                SettingKind::Decimal { min, max } => {
                    let range = || format!("{}–{}", comma(min), comma(max));
                    match raw.trim().replace(',', ".").parse::<f64>() {
                        Ok(n) if n.is_finite() && n >= min && n <= max => format!("{n:.1}"),
                        _ => return Err(format!("{}: {}", spec.label, range())),
                    }
                }
                SettingKind::List(options) => {
                    let wanted: Vec<&str> =
                        raw.split(',').map(str::trim).filter(|v| !v.is_empty()).collect();
                    if wanted.is_empty() {
                        return Err(format!("{}: wybierz co najmniej jedną", spec.label));
                    }
                    if wanted.iter().any(|w| !options.iter().any(|(v, _)| v == w)) {
                        return Err(format!("{}: nieznana wartość", spec.label));
                    }
                    options
                        .iter()
                        .filter(|(v, _)| wanted.contains(v))
                        .map(|(v, _)| *v)
                        .collect::<Vec<_>>()
                        .join(",")
                }
                SettingKind::TimeList { max } => {
                    let mut times = Vec::new();
                    for part in raw.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                        let time = parse_clock(part).ok_or_else(|| {
                            format!("{}: pora „{part}” ma mieć postać HH:MM", spec.label)
                        })?;
                        times.push(time);
                    }
                    times.sort_unstable();
                    times.dedup();
                    if times.is_empty() {
                        return Err(format!("{}: podaj co najmniej jedną porę", spec.label));
                    }
                    if times.len() > max {
                        return Err(format!("{}: najwyżej {max} pór", spec.label));
                    }
                    times
                        .iter()
                        .map(|t| t.format("%H:%M").to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                }
                SettingKind::BulletinLayout => {
                    if raw.trim().is_empty() {
                        String::new()
                    } else {
                        crate::bulletin_layout::Layout::parse(raw)?.to_json()
                    }
                }
            };
            out.insert(spec.key.to_owned(), value);
        }
        Ok(out)
    }
}

/// `HH:MM` albo `H:MM`; nic więcej.
fn parse_clock(text: &str) -> Option<NaiveTime> {
    let (h, m) = text.split_once(':')?;
    let digits = |s: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    };
    if !digits(h, 1..=2) || !digits(m, 2..=2) {
        return None;
    }
    NaiveTime::from_hms_opt(h.parse().ok()?, m.parse().ok()?, 0)
}

fn comma(n: f64) -> String {
    format!("{n:.1}").replace('.', ",")
}

/// Tryby nadawania biuletynu (wartość, etykieta).
pub const BULLETIN_MODES: &[(&str, &str)] = &[
    ("cykl", "Co określony czas"),
    ("pory", "O stałych porach"),
];

pub const BULLETIN: ModuleDescriptor = ModuleDescriptor {
    id: "bulletin",
    name: "Biuletyn",
    description: "Pełny komunikat informacyjny sklejany z klocków (ostrzeżenia, pogoda, prognoza, wody, propagacja) — cyklicznie albo o stałych porach.",
    kind: ModuleKind::Bulletin,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Nothing,
    auto_area: None,
    settings: &[
        SettingSpec {
            key: "mode",
            label: "Kiedy nadawać",
            help: "Co określony czas: wyrównane do pełnych minut od północy (np. …:00, :10, :20). O stałych porach: według listy pór.",
            kind: SettingKind::Choice(BULLETIN_MODES),
            default: "cykl",
        },
        SettingSpec {
            key: "interval_min",
            label: "Co ile minut",
            help: "Tylko w trybie „co określony czas”.",
            kind: SettingKind::Integer { min: 5, max: 60 },
            default: "10",
        },
        SettingSpec {
            key: "times",
            label: "Pory nadania",
            help: "Tylko w trybie „o stałych porach”. Czas lokalny, HH:MM po przecinku. Pora w oknie ciszy nocnej jest pomijana, gdy cisza zatrzymuje komunikaty zwykłe.",
            kind: SettingKind::TimeList { max: 8 },
            default: "07:00,19:00",
        },
        SettingSpec {
            key: "layout",
            label: "Układ biuletynu",
            help: "Klocki w kolejności nadawania (JSON). Puste: układ domyślny. Zmienne w tekstach: {znak}, {dzien}, {data}, {godzina}.",
            kind: SettingKind::BulletinLayout,
            default: "",
        },
    ],
    see_also: Some(("Pozycja, znak i okno ciszy", "/settings")),
};

/// Moduły wbudowane w rdzeń (R13). Źródła dokłada `aars_sources::modules()`.
pub fn builtin() -> Vec<ModuleDescriptor> {
    vec![
        ModuleDescriptor {
            id: "scheduler",
            name: "Harmonogram",
            description: "Nadaje komunikaty o zaplanowanych porach.",
            kind: ModuleKind::Scheduler,
            toggle: Toggle::Switchable,
            default_enabled: true,
            needs: Needs::Nothing,
            auto_area: None,
            settings: &[],
            see_also: Some(("Tolerancja spóźnień", "/settings")),
        },
        ModuleDescriptor {
            id: "auto-id",
            name: "Automatyczna identyfikacja",
            description: "Co jakiś czas podaje znak stacji. Wymóg przepisów, więc zawsze włączona.",
            kind: ModuleKind::Ident,
            toggle: Toggle::AlwaysOn("identyfikacja stacji jest obowiązkowa"),
            default_enabled: true,
            needs: Needs::Nothing,
            auto_area: None,
            settings: &[],
            see_also: Some(("Znak i tryb identyfikacji", "/settings")),
        },
        BULLETIN,
    ]
}

/// Opis źródła o tym samym schemacie co `imgw-meteo`, do testów panelu.
#[cfg(any(test, feature = "testing"))]
pub fn sample_source(id: &'static str) -> ModuleDescriptor {
    ModuleDescriptor {
        id,
        name: "Źródło testowe",
        description: "Źródło do testów.",
        kind: ModuleKind::Source,
        toggle: Toggle::Switchable,
        default_enabled: false,
        needs: Needs::Area,
        auto_area: None,
        settings: &[
            SettingSpec {
                key: "area",
                label: "Obszar",
                help: "Kody TERYT, jeden wpis w linii.",
                kind: SettingKind::Area,
                default: "",
            },
            SettingSpec {
                key: "interval_s",
                label: "Interwał odpytywania (s)",
                help: "Jak często odpytywać źródło.",
                kind: SettingKind::Integer { min: 60, max: 3600 },
                default: "300",
            },
        ],
        see_also: None,
    }
}

pub struct ModuleRegistry {
    modules: Vec<ModuleDescriptor>,
}

impl ModuleRegistry {
    /// Powtórzone `id` to błąd programisty, stąd panika.
    pub fn new(modules: Vec<ModuleDescriptor>) -> Self {
        for (i, m) in modules.iter().enumerate() {
            assert!(
                !modules[..i].iter().any(|o| o.id == m.id),
                "moduł {} zarejestrowany dwa razy",
                m.id
            );
        }
        Self { modules }
    }

    pub fn get(&self, id: &str) -> Option<&ModuleDescriptor> {
        self.modules.iter().find(|m| m.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModuleDescriptor> {
        self.modules.iter()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("tego modułu nie da się wyłączyć: {0}")]
    AlwaysOn(&'static str),
    #[error("moduł jeszcze niedostępny ({0})")]
    Planned(&'static str),
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for ModuleError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.into())
    }
}

pub struct ModuleRepo {
    pool: SqlitePool,
}

impl ModuleRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    fn enabled_key(m: &ModuleDescriptor) -> String {
        format!("module:{}:enabled", m.id)
    }

    /// Dopisuje brakujące wiersze; istniejących nie rusza.
    pub async fn register(&self, registry: &ModuleRegistry) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        for m in registry.iter() {
            if m.kind == ModuleKind::Source {
                sqlx::query("INSERT OR IGNORE INTO source_state (source_id, enabled) VALUES (?, ?)")
                    .bind(m.id)
                    .bind(i64::from(m.default_enabled))
                    .execute(&mut *tx)
                    .await?;
            } else if m.toggle == Toggle::Switchable {
                sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, ?)")
                    .bind(Self::enabled_key(m))
                    .bind(if m.default_enabled { "true" } else { "false" })
                    .execute(&mut *tx)
                    .await?;
            }
            for spec in m.settings {
                sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, ?)")
                    .bind(m.setting_key(spec.key))
                    .bind(spec.default)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        if registry.iter().any(|m| m.kind == ModuleKind::Bulletin) {
            crate::bulletin_layout::migrate_legacy(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn is_enabled(&self, m: &ModuleDescriptor) -> Result<bool, DbError> {
        match m.toggle {
            Toggle::AlwaysOn(_) => return Ok(true),
            Toggle::Planned(_) => return Ok(false),
            Toggle::Switchable => {}
        }
        if m.kind == ModuleKind::Source {
            let v: Option<i64> = sqlx::query_scalar("SELECT enabled FROM source_state WHERE source_id = ?")
                .bind(m.id)
                .fetch_optional(&self.pool)
                .await?;
            return Ok(v.map_or(m.default_enabled, |v| v != 0));
        }
        let v = SettingsRepo::new(self.pool.clone()).kv_get(&Self::enabled_key(m)).await?;
        Ok(match v.as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => m.default_enabled,
        })
    }

    /// Włączenie modułu, który jest zawsze włączony, nic nie zmienia;
    /// wyłączenie go jest odmówione.
    pub async fn set_enabled(&self, m: &ModuleDescriptor, enabled: bool) -> Result<(), ModuleError> {
        match m.toggle {
            Toggle::AlwaysOn(reason) => {
                return if enabled { Ok(()) } else { Err(ModuleError::AlwaysOn(reason)) };
            }
            Toggle::Planned(when) => return Err(ModuleError::Planned(when)),
            Toggle::Switchable => {}
        }
        if m.kind == ModuleKind::Source {
            let n = SourceStateRepo::new(self.pool.clone()).set_enabled(m.id, enabled).await?;
            if n == 0 {
                return Err(DbError::Invalid(format!("nieznane źródło: {}", m.id)).into());
            }
        } else {
            SettingsRepo::new(self.pool.clone())
                .kv_put(&Self::enabled_key(m), if enabled { "true" } else { "false" })
                .await?;
        }
        Ok(())
    }

    /// Brakujące klucze dają wartości domyślne.
    pub async fn settings(&self, m: &ModuleDescriptor) -> Result<BTreeMap<String, String>, DbError> {
        let kv = SettingsRepo::new(self.pool.clone());
        let mut out = BTreeMap::new();
        for spec in m.settings {
            let v = kv.kv_get(&m.setting_key(spec.key)).await?;
            out.insert(spec.key.to_owned(), v.unwrap_or_else(|| spec.default.to_owned()));
        }
        Ok(out)
    }

    /// Zapisuje (w jednej transakcji) klucze z opisu, które są w `values`.
    /// Wartości mają być wcześniej sprawdzone przez `validate`.
    pub async fn save_settings(
        &self,
        m: &ModuleDescriptor,
        values: &BTreeMap<String, String>,
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        for spec in m.settings {
            let Some(value) = values.get(spec.key) else { continue };
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES (?, ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                 updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            )
            .bind(m.setting_key(spec.key))
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn by_id(id: &str) -> ModuleDescriptor {
        *ModuleRegistry::new(builtin()).get(id).unwrap()
    }

    #[test]
    fn builtin_modules_are_scheduler_auto_id_and_bulletin() {
        let b = builtin();
        let ids: Vec<_> = b.iter().map(|m| m.id).collect();
        assert_eq!(ids, ["scheduler", "auto-id", "bulletin"]);
        assert_eq!(b[0].toggle, Toggle::Switchable);
        assert!(matches!(b[1].toggle, Toggle::AlwaysOn(_)));
        assert_eq!(b[2].toggle, Toggle::Switchable);
        assert_eq!(b[0].see_also, Some(("Tolerancja spóźnień", "/settings")));
        assert_eq!(b[1].see_also, Some(("Znak i tryb identyfikacji", "/settings")));
        assert_eq!(ModuleKind::Ident.label(), "identyfikacja stacji");
    }

    #[test]
    fn source_settings_keep_their_stage_8_keys() {
        assert_eq!(sample_source("imgw-meteo").setting_key("area"), "source:imgw-meteo:area");
        assert_eq!(by_id("scheduler").setting_key("x"), "module:scheduler:x");
    }

    #[test]
    fn validation_canonicalises_the_area_and_checks_ranges() {
        let m = sample_source("imgw-meteo");
        let ok = m.validate(&map(&[("area", "1061 Łódź; 10"), ("interval_s", " 300 ")])).unwrap();
        assert_eq!(ok["area"].lines().count(), 2);
        assert_eq!(ok["interval_s"], "300");
        assert_eq!(
            m.validate(&map(&[("area", ""), ("interval_s", "30")])).unwrap_err(),
            "Interwał odpytywania (s): 60–3600"
        );
        assert!(m.validate(&map(&[("area", "1061"), ("interval_s", "300")])).is_err());
        assert_eq!(m.validate(&map(&[("area", "")])).unwrap_err(), "Interwał odpytywania (s): brak wartości");
    }

    #[test]
    fn text_and_choice_settings_are_checked() {
        const CHOICES: &[(&str, &str)] = &[("a", "A"), ("b", "B")];
        const SPECS: &[SettingSpec] = &[
            SettingSpec { key: "t", label: "Tekst", help: "", kind: SettingKind::Text { max_len: 3 }, default: "" },
            SettingSpec { key: "c", label: "Wybór", help: "", kind: SettingKind::Choice(CHOICES), default: "a" },
        ];
        let m = ModuleDescriptor { settings: SPECS, ..sample_source("x") };
        assert!(m.validate(&map(&[("t", "abc"), ("c", "b")])).is_ok());
        assert_eq!(m.validate(&map(&[("t", "abcd"), ("c", "b")])).unwrap_err(), "Tekst: najwyżej 3 znaków");
        assert_eq!(m.validate(&map(&[("t", "a"), ("c", "z")])).unwrap_err(), "Wybór: nieznana wartość");
    }

    #[test]
    fn decimal_settings_accept_a_comma_and_store_one_decimal() {
        const SPECS: &[SettingSpec] = &[SettingSpec {
            key: "m", label: "Próg", help: "", kind: SettingKind::Decimal { min: 1.0, max: 9.0 }, default: "3.0",
        }];
        let m = ModuleDescriptor { settings: SPECS, ..sample_source("x") };
        let v = |s: &str| m.validate(&map(&[("m", s)]));
        assert_eq!(v("3").unwrap()["m"], "3.0");
        assert_eq!(v("4,5").unwrap()["m"], "4.5");
        assert_eq!(v(" 2.0 ").unwrap()["m"], "2.0");
        for bad in ["0.5", "9.5", "abc", "NaN"] {
            let e = v(bad).unwrap_err();
            assert!(e.contains("Próg") && e.contains("1,0–9,0"), "{bad}: {e}");
        }
    }

    #[test]
    fn list_settings_store_known_slugs_in_declared_order() {
        const OPTS: &[(&str, &str)] = &[
            ("ogolne", "Ogólne"),
            ("atak-z-powietrza", "Atak z powietrza"),
            ("informacje-drogowe", "Drogowe"),
        ];
        const SPECS: &[SettingSpec] = &[SettingSpec {
            key: "l", label: "Kategorie", help: "", kind: SettingKind::List(OPTS), default: "ogolne",
        }];
        let m = ModuleDescriptor { settings: SPECS, ..sample_source("x") };
        let v = |s: &str| m.validate(&map(&[("l", s)]));
        assert_eq!(v("informacje-drogowe, ogolne").unwrap()["l"], "ogolne,informacje-drogowe");
        assert_eq!(v("ogolne,ogolne").unwrap()["l"], "ogolne");
        assert_eq!(v("").unwrap_err(), "Kategorie: wybierz co najmniej jedną");
        assert_eq!(v("stany-wod").unwrap_err(), "Kategorie: nieznana wartość");
    }

    #[test]
    fn builtin_modules_need_nothing() {
        assert!(builtin().iter().all(|m| m.needs == Needs::Nothing));
        assert_eq!(sample_source("x").needs, Needs::Area);
    }

    #[tokio::test]
    async fn register_inserts_missing_rows_without_touching_existing_ones() {
        let pool = test_pool().await;
        let repo = ModuleRepo::new(pool.clone());
        let registry = ModuleRegistry::new({
            let mut v = builtin();
            v.push(sample_source("imgw-meteo"));
            v.push(sample_source("test-src"));
            v
        });
        repo.register(&registry).await.unwrap();
        let imgw = registry.get("imgw-meteo").unwrap();
        let test = registry.get("test-src").unwrap();
        assert!(!repo.is_enabled(imgw).await.unwrap(), "z migracji 0003 wyłączone");
        assert!(!repo.is_enabled(test).await.unwrap());
        assert_eq!(repo.settings(test).await.unwrap()["interval_s"], "300");
        let stored: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'source:test-src:interval_s'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), Some("300"));

        repo.save_settings(test, &map(&[("area", "10 łódzkie"), ("interval_s", "600")])).await.unwrap();
        repo.set_enabled(test, true).await.unwrap();
        repo.register(&registry).await.unwrap();
        assert!(repo.is_enabled(test).await.unwrap());
        assert_eq!(repo.settings(test).await.unwrap()["interval_s"], "600");
    }

    #[tokio::test]
    async fn the_scheduler_toggle_round_trips() {
        let repo = ModuleRepo::new(test_pool().await);
        let s = by_id("scheduler");
        assert!(repo.is_enabled(&s).await.unwrap());
        repo.set_enabled(&s, false).await.unwrap();
        assert!(!repo.is_enabled(&s).await.unwrap());
        repo.set_enabled(&s, true).await.unwrap();
        assert!(repo.is_enabled(&s).await.unwrap());
    }

    #[tokio::test]
    async fn auto_id_cannot_be_disabled() {
        let repo = ModuleRepo::new(test_pool().await);
        let m = by_id("auto-id");
        assert!(matches!(repo.set_enabled(&m, false).await, Err(ModuleError::AlwaysOn(_))));
        assert!(repo.is_enabled(&m).await.unwrap());
    }

    #[tokio::test]
    async fn the_bulletin_can_be_enabled() {
        let repo = ModuleRepo::new(test_pool().await);
        let m = by_id("bulletin");
        assert!(!repo.is_enabled(&m).await.unwrap());
        repo.set_enabled(&m, true).await.unwrap();
        assert!(repo.is_enabled(&m).await.unwrap());
    }

    #[test]
    fn the_bulletin_is_switchable_with_four_settings() {
        let m = by_id("bulletin");
        assert_eq!(m.toggle, Toggle::Switchable);
        assert!(!m.default_enabled);
        let keys: Vec<_> = m.settings.iter().map(|s| s.key).collect();
        assert_eq!(keys, ["mode", "interval_min", "times", "layout"]);
        let defaults: BTreeMap<String, String> =
            m.settings.iter().map(|s| (s.key.to_owned(), s.default.to_owned())).collect();
        assert_eq!(defaults["mode"], "cykl");
        assert_eq!(defaults["interval_min"], "10");
        assert_eq!(m.validate(&defaults).unwrap(), defaults);
        let mut input = defaults.clone();
        input.insert("interval_min".into(), "4".into());
        assert_eq!(m.validate(&input).unwrap_err(), "Co ile minut: 5–60");
        input.insert("interval_min".into(), "60".into());
        input.insert("mode".into(), "pory".into());
        input.insert("layout".into(), r#"{"bloki":[{"rodzaj":"wody"}]}"#.into());
        let ok = m.validate(&input).unwrap();
        assert_eq!(ok["layout"], crate::bulletin_layout::Layout::parse(&input["layout"]).unwrap().to_json());
        input.insert("layout".into(), r#"{"bloki":[{"rodzaj":"wstep","tekst":"{stacja}"}]}"#.into());
        assert!(m.validate(&input).unwrap_err().contains("nieznana zmienna"));
    }

    #[tokio::test]
    async fn registering_the_bulletin_migrates_old_parts() {
        let pool = test_pool().await;
        let kv = SettingsRepo::new(pool.clone());
        kv.kv_put("module:bulletin:parts", "ostrzezenia").await.unwrap();
        ModuleRepo::new(pool.clone()).register(&ModuleRegistry::new(builtin())).await.unwrap();
        let stored = kv.kv_get("module:bulletin:layout").await.unwrap().unwrap();
        let layout = crate::bulletin_layout::Layout::parse(&stored).unwrap();
        assert_eq!(layout, crate::bulletin_layout::Layout::from_legacy(Some("ostrzezenia"), None));
        assert_eq!(kv.kv_get("module:bulletin:parts").await.unwrap(), None);
        assert_eq!(kv.kv_get("module:bulletin:mode").await.unwrap().as_deref(), Some("cykl"));
    }

    #[test]
    fn time_lists_are_sorted_deduplicated_and_bounded() {
        const SPECS: &[SettingSpec] = &[SettingSpec {
            key: "t", label: "Pory", help: "", kind: SettingKind::TimeList { max: 3 }, default: "07:00",
        }];
        let m = ModuleDescriptor { settings: SPECS, ..sample_source("x") };
        let v = |s: &str| m.validate(&map(&[("t", s)]));
        assert_eq!(v("19:00, 7:00,07:00").unwrap()["t"], "07:00,19:00");
        assert_eq!(v("").unwrap_err(), "Pory: podaj co najmniej jedną porę");
        for bad in ["25:00", "7.00"] {
            let e = v(bad).unwrap_err();
            assert!(e.contains(&format!("„{bad}”")) && e.contains("HH:MM"), "{bad}: {e}");
        }
        assert_eq!(v("01:00,02:00,03:00,04:00").unwrap_err(), "Pory: najwyżej 3 pór");
    }

    #[tokio::test]
    async fn a_source_toggle_writes_source_state() {
        let pool = test_pool().await;
        let repo = ModuleRepo::new(pool.clone());
        let imgw = sample_source("imgw-meteo");
        repo.set_enabled(&imgw, true).await.unwrap();
        let cfg = SourceStateRepo::new(pool).config("imgw-meteo").await.unwrap().unwrap();
        assert!(cfg.enabled);
        assert!(repo.is_enabled(&imgw).await.unwrap());
    }

    #[test]
    #[should_panic(expected = "dwa razy")]
    fn duplicate_ids_are_a_programming_error() {
        let _ = ModuleRegistry::new(vec![sample_source("a"), sample_source("a")]);
    }
}
