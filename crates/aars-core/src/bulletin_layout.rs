//! Układ biuletynu z klocków (etap 13): kolejność, pauzy, kiedy klocek jest
//! nadawany i własne teksty. Panel i kompozytor czytają ten sam układ
//! z `module:bulletin:layout` (JSON).
//!
//! Format JSON (pola po polsku, wartości enumów w snake_case):
//!
//! ```json
//! {"bloki": [
//!   {"rodzaj": "wstep", "wlaczony": true, "pauza_przed_ms": 0, "pauza_po_ms": 700,
//!    "kiedy": {"typ": "zawsze"}, "tekst": "Tu {znak}.", "mow_gdy_brak": false},
//!   {"rodzaj": "propagacja", "kiedy": {"typ": "co_n_cykli", "n": 3}},
//!   {"rodzaj": "tekst", "kiedy": {"typ": "w_godzinach", "od": "22:00", "do": "06:00"},
//!    "tekst": "Dobranoc."}
//! ]}
//! ```
//!
//! Brakujące pola biorą wartości domyślne (`wlaczony`: tak, pauzy 0,
//! `kiedy`: zawsze, tekst pusty, `mow_gdy_brak`: nie); nieznane pola to błąd.

use chrono::{Datelike, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::settings::SettingsRepo;

/// Klucz ustawienia z układem.
pub const LAYOUT_KEY: &str = "module:bulletin:layout";
/// Klucze sprzed etapu 13, przenoszone do układu przy pierwszym odczycie.
pub const LEGACY_PARTS_KEY: &str = "module:bulletin:parts";
pub const LEGACY_INTRO_KEY: &str = "module:bulletin:intro";

pub const MAX_BLOCKS: usize = 24;
pub const MAX_PAUSE_MS: u32 = 10_000;
pub const MAX_TEXT_CHARS: usize = 500;
pub const MIN_EVERY_N: u32 = 2;
pub const MAX_EVERY_N: u32 = 24;
/// Zmienne szablonu wstępu, zakończenia i własnego tekstu.
pub const TEMPLATE_VARS: &[&str] = &["znak", "dzien", "data", "godzina"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Wstep,
    Ostrzezenia,
    PogodaTeraz,
    Prognoza,
    Slonce,
    Wody,
    Propagacja,
    Tekst,
    Zakonczenie,
}

impl BlockKind {
    pub const ALL: [BlockKind; 9] = [
        Self::Wstep,
        Self::Ostrzezenia,
        Self::PogodaTeraz,
        Self::Prognoza,
        Self::Slonce,
        Self::Wody,
        Self::Propagacja,
        Self::Tekst,
        Self::Zakonczenie,
    ];

    /// Polska nazwa klocka (panel, podgląd, dziennik).
    pub fn label(self) -> &'static str {
        match self {
            Self::Wstep => "Wstęp",
            Self::Ostrzezenia => "Obowiązujące ostrzeżenia",
            Self::PogodaTeraz => "Pogoda bieżąca",
            Self::Prognoza => "Prognoza pogody",
            Self::Slonce => "Wschód i zachód słońca",
            Self::Wody => "Stany wód w okolicy",
            Self::Propagacja => "Warunki propagacji KF",
            Self::Tekst => "Własny tekst",
            Self::Zakonczenie => "Zakończenie",
        }
    }

    /// Identyfikator części biuletynu (`BulletinPart::id`) dla klocka z danymi.
    pub fn part_id(self) -> Option<&'static str> {
        match self {
            Self::Ostrzezenia => Some("ostrzezenia"),
            Self::PogodaTeraz => Some("pogoda_teraz"),
            Self::Prognoza => Some("prognoza"),
            Self::Slonce => Some("slonce"),
            Self::Wody => Some("wody"),
            Self::Propagacja => Some("propagacja"),
            Self::Wstep | Self::Tekst | Self::Zakonczenie => None,
        }
    }

    /// Klocek z tekstem operatora (szablonem), nie z danych.
    pub fn is_text(self) -> bool {
        self.part_id().is_none()
    }

    /// Nie wypada przy limicie długości.
    pub fn is_protected(self) -> bool {
        matches!(self, Self::Wstep | Self::Ostrzezenia | Self::Zakonczenie)
    }

    /// Wartość `rodzaj` w JSON (jak serde).
    pub fn id(self) -> &'static str {
        match self {
            Self::Wstep => "wstep",
            Self::Ostrzezenia => "ostrzezenia",
            Self::PogodaTeraz => "pogoda_teraz",
            Self::Prognoza => "prognoza",
            Self::Slonce => "slonce",
            Self::Wody => "wody",
            Self::Propagacja => "propagacja",
            Self::Tekst => "tekst",
            Self::Zakonczenie => "zakonczenie",
        }
    }

    /// Jedno zdanie po polsku: co klocek mówi (edytor w panelu).
    pub fn description(self) -> &'static str {
        match self {
            Self::Wstep => "Przedstawienie stacji, dzień, data i godzina. Zawsze na początku komunikatu.",
            Self::Ostrzezenia => "Pełna treść wszystkich obowiązujących ostrzeżeń (IMGW, RSO, GIOŚ, EMSC, NOAA). Nigdy nie wypada przy limicie.",
            Self::PogodaTeraz => "Temperatura, wilgotność, ciśnienie z tendencją, wiatr i zachmurzenie z pozycji stacji.",
            Self::Prognoza => "Najbliższe godziny, potem reszta dnia albo noc i jutro.",
            Self::Slonce => "Godzina wschodu i zachodu słońca dla pozycji stacji.",
            Self::Wody => "Do trzech najbliższych wodowskazów ze świeżym pomiarem, z dopiskiem o stanie ostrzegawczym lub alarmowym.",
            Self::Propagacja => "Warunki propagacji na pasmach KF (indeksy słoneczne i geomagnetyczne).",
            Self::Tekst => "Twój stały tekst, np. zaproszenie na sieć albo informacja o przemienniku.",
            Self::Zakonczenie => "Pożegnanie ze znakiem stacji. Zawsze na końcu komunikatu.",
        }
    }

    /// Typowa długość klocka z danymi w znakach — do szacunku czasu
    /// w edytorze, zanim podgląd poda prawdziwą.
    pub fn typical_chars(self) -> usize {
        match self {
            Self::Ostrzezenia => 160,
            Self::PogodaTeraz => 230,
            Self::Prognoza => 260,
            Self::Slonce => 70,
            Self::Wody => 120,
            Self::Propagacja => 140,
            Self::Wstep | Self::Tekst | Self::Zakonczenie => 0,
        }
    }

    pub fn from_id(id: &str) -> Option<BlockKind> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }
}

/// Kiedy klocek jest nadawany.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "typ", rename_all = "snake_case", deny_unknown_fields)]
pub enum When {
    #[default]
    Zawsze,
    /// Co N-ty cykl: numer cyklu (minuty od północy / interwał) podzielny przez N.
    CoNCykli { n: u32 },
    /// Między `od` a `do` (czas Warszawy, `do` wyłącznie); przez północ dozwolone.
    WGodzinach {
        #[serde(with = "hhmm")]
        od: NaiveTime,
        #[serde(rename = "do", with = "hhmm")]
        do_: NaiveTime,
    },
}

impl When {
    /// `cycle` — numer cyklu w trybie cyklicznym (`None` w trybie pór: „co
    /// N-ty cykl” wtedy zawsze pasuje); `local` — wypowiadana pora (Warszawa).
    pub fn matches(&self, cycle: Option<u32>, local: NaiveTime) -> bool {
        match *self {
            Self::Zawsze => true,
            Self::CoNCykli { n } => cycle.is_none_or(|c| n == 0 || c.is_multiple_of(n)),
            Self::WGodzinach { od, do_ } => {
                if od <= do_ {
                    od <= local && local < do_
                } else {
                    local >= od || local < do_
                }
            }
        }
    }

    /// Powód pominięcia po polsku, gdy klocek nie pasuje.
    pub fn describe(&self) -> String {
        match *self {
            Self::Zawsze => "zawsze".to_owned(),
            Self::CoNCykli { n } => format!("co {n}. cykl"),
            Self::WGodzinach { od, do_ } => {
                format!("w godzinach {}–{}", od.format("%H:%M"), do_.format("%H:%M"))
            }
        }
    }
}

mod hhmm {
    use chrono::NaiveTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &NaiveTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&t.format("%H:%M").to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<NaiveTime, D::Error> {
        let raw = String::deserialize(d)?;
        NaiveTime::parse_from_str(raw.trim(), "%H:%M")
            .map_err(|_| serde::de::Error::custom(format!("pora „{raw}” ma mieć postać HH:MM")))
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    #[serde(rename = "rodzaj")]
    pub kind: BlockKind,
    #[serde(rename = "wlaczony", default = "yes")]
    pub enabled: bool,
    #[serde(rename = "pauza_przed_ms", default)]
    pub pause_before_ms: u32,
    #[serde(rename = "pauza_po_ms", default)]
    pub pause_after_ms: u32,
    #[serde(rename = "kiedy", default)]
    pub when: When,
    /// Wstęp/zakończenie: szablon; własny tekst: treść; klocki danych: puste.
    #[serde(rename = "tekst", default)]
    pub text: String,
    /// Tylko ostrzeżenia: mówić „Brak obowiązujących ostrzeżeń…”, gdy nic nie obowiązuje.
    #[serde(rename = "mow_gdy_brak", default)]
    pub say_when_empty: bool,
}

impl Block {
    pub fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            enabled: true,
            pause_before_ms: 0,
            pause_after_ms: 0,
            when: When::Zawsze,
            text: String::new(),
            say_when_empty: false,
        }
    }

    fn with_text(mut self, text: &str) -> Self {
        self.text = text.to_owned();
        self
    }

    fn pause_after(mut self, ms: u32) -> Self {
        self.pause_after_ms = ms;
        self
    }
}

impl Block {
    /// Reguły jednego klocka (`before` — klocki przed nim). Błąd bez przedrostka „klocek N”.
    fn check(&self, before: &[Block]) -> Result<(), String> {
        if self.kind != BlockKind::Tekst && before.iter().any(|b| b.kind == self.kind) {
            return Err("ten klocek już jest w układzie".to_owned());
        }
        for ms in [self.pause_before_ms, self.pause_after_ms] {
            if ms > MAX_PAUSE_MS {
                return Err(format!("pauza najwyżej {MAX_PAUSE_MS} ms"));
            }
        }
        match self.when {
            When::CoNCykli { n } if !(MIN_EVERY_N..=MAX_EVERY_N).contains(&n) => {
                return Err(format!("co N-ty cykl, N od {MIN_EVERY_N} do {MAX_EVERY_N}"));
            }
            When::WGodzinach { od, do_ } if od == do_ => {
                return Err("początek i koniec godzin muszą się różnić".to_owned());
            }
            _ => {}
        }
        if self.kind.is_text() {
            let text = self.text.trim();
            if text.is_empty() {
                return Err("wpisz tekst".to_owned());
            }
            if text.chars().count() > MAX_TEXT_CHARS {
                return Err(format!("tekst najwyżej {MAX_TEXT_CHARS} znaków"));
            }
            if crate::pause::has_marker(text) {
                return Err("pauzy ustawia się polami pauzy przed i po klocku".to_owned());
            }
            template_vars(text)?;
        } else if !self.text.trim().is_empty() {
            return Err("ten klocek nie ma własnego tekstu".to_owned());
        }
        if self.say_when_empty && self.kind != BlockKind::Ostrzezenia {
            return Err("„mów, gdy brak” dotyczy tylko ostrzeżeń".to_owned());
        }
        Ok(())
    }
}

/// Błąd układu: którego klocka dotyczy (`None` — całego układu albo zapisu
/// JSON), pełny komunikat (`message`, z „klocek N (nazwa): …”) i sam opis
/// (`detail`, do pokazania przy klocku).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutError {
    pub block: Option<usize>,
    pub message: String,
    pub detail: String,
}

impl LayoutError {
    fn whole(message: String) -> Self {
        Self {
            block: None,
            detail: message.clone(),
            message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    #[serde(rename = "bloki")]
    pub blocks: Vec<Block>,
}

pub const DEFAULT_INTRO: &str =
    "Tu automatyczna stacja informacyjna {znak}. {dzien}, {data}, godzina {godzina}.";
pub const DEFAULT_OUTRO: &str = "Tu {znak}. Koniec komunikatu.";

impl Layout {
    pub fn default_layout() -> Layout {
        let mut warnings = Block::new(BlockKind::Ostrzezenia).pause_after(700);
        warnings.say_when_empty = true;
        let mut propagation = Block::new(BlockKind::Propagacja);
        propagation.when = When::CoNCykli { n: 3 };
        Layout {
            blocks: vec![
                Block::new(BlockKind::Wstep)
                    .with_text(DEFAULT_INTRO)
                    .pause_after(700),
                warnings,
                Block::new(BlockKind::PogodaTeraz),
                Block::new(BlockKind::Prognoza).pause_after(500),
                Block::new(BlockKind::Slonce),
                Block::new(BlockKind::Wody),
                propagation,
                Block::new(BlockKind::Zakonczenie).with_text(DEFAULT_OUTRO),
            ],
        }
    }

    pub fn parse(json: &str) -> Result<Layout, String> {
        Self::parse_located(json).map_err(|e| e.message)
    }

    /// Jak `parse`, ale błąd mówi też, którego klocka dotyczy (panel
    /// zaznacza ten klocek); błąd zapisu JSON nie ma klocka.
    pub fn parse_located(json: &str) -> Result<Layout, LayoutError> {
        let layout = Self::parse_unchecked(json).map_err(LayoutError::whole)?;
        layout.validate_located()?;
        Ok(layout)
    }

    /// Sam odczyt JSON, bez reguł układu (panel pokazuje wpisany układ
    /// razem z błędem przy klocku).
    pub fn parse_unchecked(json: &str) -> Result<Layout, String> {
        serde_json::from_str(json).map_err(|e| format!("układ biuletynu: niepoprawny zapis ({e})"))
    }

    /// Postać kanoniczna (z wcięciami — operator może ją edytować ręcznie).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Wartość do zapisania w ustawieniu: pusta, gdy układ jest domyślnym
    /// (zmiany domyślnego w nowych wersjach dochodzą wtedy do stacji), inaczej JSON.
    pub fn stored(&self) -> String {
        if *self == Self::default_layout() {
            String::new()
        } else {
            self.to_json()
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        self.validate_located().map_err(|e| e.message)
    }

    pub fn validate_located(&self) -> Result<(), LayoutError> {
        if self.blocks.is_empty() {
            return Err(LayoutError::whole("układ biuletynu: dodaj co najmniej jeden klocek".to_owned()));
        }
        if self.blocks.len() > MAX_BLOCKS {
            return Err(LayoutError::whole(format!("układ biuletynu: najwyżej {MAX_BLOCKS} klocków")));
        }
        for (i, block) in self.blocks.iter().enumerate() {
            block.check(&self.blocks[..i]).map_err(|detail| LayoutError {
                block: Some(i),
                message: format!("klocek {} ({}): {detail}", i + 1, block.kind.label()),
                detail,
            })?;
        }
        Ok(())
    }

    /// Układ z ustawień sprzed etapu 13. Lista części nieustawiona albo równa
    /// staremu domyślnemu wyborowi (operator jej nie zmieniał) → pełny nowy
    /// układ (także stany wód); lista zmieniona przez operatora → części
    /// nieobecne na niej to klocki wyłączone. Wstęp operatora → tekst wstępu
    /// (z godziną, jak dawniej).
    pub fn from_legacy(parts: Option<&str>, intro: Option<&str>) -> Layout {
        let mut layout = Self::default_layout();
        if let Some(parts) = parts.filter(|p| !is_legacy_default(p)) {
            let chosen: Vec<&str> = parts.split(',').map(str::trim).collect();
            for block in &mut layout.blocks {
                let legacy = match block.kind {
                    BlockKind::PogodaTeraz | BlockKind::Prognoza | BlockKind::Slonce => "pogoda",
                    BlockKind::Wody => "wody",
                    BlockKind::Propagacja => "propagacja",
                    BlockKind::Ostrzezenia => "ostrzezenia",
                    BlockKind::Wstep | BlockKind::Tekst | BlockKind::Zakonczenie => continue,
                };
                block.enabled = chosen.contains(&legacy);
            }
        }
        let intro = intro
            .map(|i| crate::pause::strip_markers(i).replace(['{', '}'], ""))
            .unwrap_or_default();
        let intro = intro.trim().trim_end_matches(['.', '!', '?']).trim();
        if !intro.is_empty() {
            let text: String = format!("{intro}. Godzina {{godzina}}.")
                .chars()
                .take(MAX_TEXT_CHARS)
                .collect();
            if let Some(block) = layout
                .blocks
                .iter_mut()
                .find(|b| b.kind == BlockKind::Wstep)
            {
                block.text = text;
            }
        }
        layout
    }
}

/// Domyślny wybór części sprzed etapu 13 (w dowolnej kolejności).
const LEGACY_DEFAULT_PARTS: [&str; 3] = ["ostrzezenia", "pogoda", "propagacja"];

fn is_legacy_default(parts: &str) -> bool {
    let mut chosen: Vec<&str> = parts
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    chosen.sort_unstable();
    chosen.dedup();
    chosen == LEGACY_DEFAULT_PARTS
}

/// Nazwy zmiennych `{…}` w szablonie; nieznana zmienna albo niedomknięty
/// nawias to błąd.
pub fn template_vars(text: &str) -> Result<Vec<&str>, String> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let open = rest.find('{');
        let close = rest.find('}');
        match (open, close) {
            (None, None) => return Ok(out),
            (Some(o), Some(c)) if o < c => {
                let name = &rest[o + 1..c];
                if name.contains('{') {
                    return Err("niedomknięty nawias „{” w tekście".to_owned());
                }
                if !TEMPLATE_VARS.contains(&name) {
                    return Err(format!(
                        "nieznana zmienna „{{{name}}}” (dozwolone: {{znak}}, {{dzien}}, {{data}}, {{godzina}})"
                    ));
                }
                out.push(name);
                rest = &rest[c + 1..];
            }
            (Some(_), None) => return Err("niedomknięty nawias „{” w tekście".to_owned()),
            _ => return Err("nawias „}” bez „{” w tekście".to_owned()),
        }
    }
}

const DAYS: [&str; 7] = [
    "poniedziałek",
    "wtorek",
    "środa",
    "czwartek",
    "piątek",
    "sobota",
    "niedziela",
];

const MONTHS_GENITIVE: [&str; 12] = [
    "stycznia",
    "lutego",
    "marca",
    "kwietnia",
    "maja",
    "czerwca",
    "lipca",
    "sierpnia",
    "września",
    "października",
    "listopada",
    "grudnia",
];

/// Zdania tekstu: każde kończy się na `.`, `!` albo `?` przed odstępem
/// (albo na końcu tekstu).
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = i + c.len_utf8();
        let boundary = matches!(c, '.' | '!' | '?')
            && chars.peek().is_none_or(|(_, next)| next.is_whitespace());
        if boundary {
            let s = text[start..end].trim();
            if !s.is_empty() {
                out.push(s);
            }
            start = end;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Wypełnia szablon na chwilę `local` (czas Warszawy). Bez znaku stacji
/// zdania z `{znak}` wypadają w całości („Tu {znak}.” nie zostaje jako „Tu .”).
/// Zdanie zaczynające się od zmiennej (np. `{dzien}`) dostaje wielką literę.
pub fn render_template(template: &str, callsign: &str, local: NaiveDateTime) -> String {
    let callsign = callsign.trim();
    let day = DAYS[local.weekday().num_days_from_monday() as usize];
    let date = format!(
        "{} {}",
        local.day(),
        MONTHS_GENITIVE[local.month0() as usize]
    );
    let time = format!("{}:{:02}", local.hour(), local.minute());
    sentences(template)
        .into_iter()
        .filter(|s| !callsign.is_empty() || !s.contains("{znak}"))
        .map(|s| {
            let filled = s
                .replace("{znak}", callsign)
                .replace("{dzien}", day)
                .replace("{data}", &date)
                .replace("{godzina}", &time);
            capitalize_first(&filled)
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

async fn get(tx: &mut sqlx::SqliteConnection, key: &str) -> Result<Option<String>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(tx)
            .await?,
    )
}

/// Przenosi `parts`/`intro` sprzed etapu 13 do układu (tylko gdy układu
/// jeszcze nie ma) i usuwa stare klucze — potem są ignorowane. Bez starych
/// kluczy nic nie robi.
pub async fn migrate_legacy(pool: &SqlitePool) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let parts = get(&mut tx, LEGACY_PARTS_KEY).await?;
    let intro = get(&mut tx, LEGACY_INTRO_KEY).await?;
    if parts.is_none() && intro.is_none() {
        return Ok(());
    }
    let current = get(&mut tx, LAYOUT_KEY).await?;
    if current.as_deref().is_none_or(|v| v.trim().is_empty()) {
        let layout = Layout::from_legacy(parts.as_deref(), intro.as_deref());
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        )
        .bind(LAYOUT_KEY)
        .bind(layout.stored())
        .execute(&mut *tx)
        .await?;
        tracing::info!("układ biuletynu przeniesiony ze starych ustawień (części, wstęp)");
    }
    sqlx::query("DELETE FROM settings WHERE key IN (?, ?)")
        .bind(LEGACY_PARTS_KEY)
        .bind(LEGACY_INTRO_KEY)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Układ stacji: najpierw migracja starych kluczy, potem zapisany układ;
/// pusty → domyślny; nieczytelny (np. ręcznie zepsuty) → domyślny z ostrzeżeniem.
pub async fn load(pool: &SqlitePool) -> Result<Layout, DbError> {
    migrate_legacy(pool).await?;
    let raw = SettingsRepo::new(pool.clone()).kv_get(LAYOUT_KEY).await?;
    Ok(resolve(raw.as_deref().unwrap_or("")))
}

/// Układ z wartości ustawienia: pusta → domyślny, błędna → domyślny.
pub fn resolve(raw: &str) -> Layout {
    if raw.trim().is_empty() {
        return Layout::default_layout();
    }
    Layout::parse(raw).unwrap_or_else(|e| {
        tracing::warn!(błąd = %e, "zapisany układ biuletynu nieczytelny, używam domyślnego");
        Layout::default_layout()
    })
}

/// Znaki mowy (po normalizacji) na sekundę przy tempie 1,0. Pomiar
/// 2026-11-25 na głosie Gosia: biuletyn 603 znaki (≈ 905 po normalizacji),
/// tempo 0,87 → 71,6 s mowy, czyli ≈ 14,5 znaku/s przy 1,0; przyjęte 13,5
/// z zapasem ok. 7%. Dyspozytor i tak nie kluczy komunikatu dłuższego niż limit.
pub const SPOKEN_CHARS_PER_S: f64 = 13.5;
/// O ile normalizator wydłuża tekst przed syntezą („1024 hPa” → „tysiąc
/// dwadzieścia cztery hektopaskale”); pomiar na pełnym biuletynie dał 1,51.
pub const NORMALIZER_EXPANSION: f64 = 1.5;
/// Sekundy limitu nadawania zostawione na identyfikację i ogony.
pub const TX_RESERVE_S: u64 = 10;
/// Górna granica długości tekstu biuletynu w znakach.
pub const MAX_BULLETIN_CHARS: usize = 1500;

/// Budżet długości biuletynu: ile znaków tekstu (przed normalizacją)
/// zmieści się w limicie nadawania i ile znaków „kosztuje” sekunda pauzy.
/// Liczone zachowawczo: tekst po normalizacji jest ~1,5× dłuższy, a mowa
/// przy tempie `speech_rate` < 1 wolniejsza. Kompozytor i edytor w panelu
/// liczą tym samym budżetem.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// Najwięcej znaków (mowa + pauzy przeliczone na znaki).
    pub chars: usize,
    /// Znaki tekstu na sekundę nadawania.
    pub chars_per_s: f64,
}

impl Budget {
    /// Budżet dla limitu nadawania i tempa mowy stacji (tempo spoza
    /// rozsądnego zakresu → domyślne).
    pub fn new(tx_max_duration_s: u64, speech_rate: f32) -> Self {
        let rate = if speech_rate.is_finite() && speech_rate > 0.0 {
            speech_rate
        } else {
            crate::settings::DEFAULT_SPEECH_RATE
        };
        let chars_per_s = SPOKEN_CHARS_PER_S * f64::from(rate) / NORMALIZER_EXPANSION;
        let seconds = tx_max_duration_s.saturating_sub(TX_RESERVE_S) as f64;
        // Ułamek zaokrąglony w dół; drobny błąd zmiennoprzecinkowy (np. 985,999…)
        // nie może zabrać znaku.
        let chars = (seconds * chars_per_s + 1e-6).floor();
        Self { chars: (chars as usize).min(MAX_BULLETIN_CHARS), chars_per_s }
    }

    /// Ile znaków budżetu zajmuje pauza (w górę).
    pub fn pause_chars(&self, pause_ms: u64) -> usize {
        (pause_ms as f64 * self.chars_per_s / 1000.0 - 1e-6).ceil().max(0.0) as usize
    }

    /// Szacunkowe sekundy mowy dla `chars` znaków tekstu.
    pub fn seconds(&self, chars: usize) -> f64 {
        chars as f64 / self.chars_per_s
    }

    /// Ile sekund nadawania mieści budżet (w dół).
    pub fn limit_s(&self) -> u64 {
        (self.seconds(self.chars) + 1e-6).floor() as u64
    }
}

/// Limit długości tekstu biuletynu w znakach (zob. [`Budget`]).
pub fn length_limit(tx_max_duration_s: u64, speech_rate: f32) -> usize {
    Budget::new(tx_max_duration_s, speech_rate).chars
}

/// Tryb biuletynu „co określony czas” (wartość ustawienia `mode`).
pub const MODE_CYCLE: &str = "cykl";
/// Tryb biuletynu „o stałych porach”.
pub const MODE_TIMES: &str = "pory";
pub const DEFAULT_INTERVAL_MIN: u32 = 10;
pub const MIN_INTERVAL_MIN: u32 = 5;
pub const MAX_INTERVAL_MIN: u32 = 60;
const DAY_MIN: u32 = 24 * 60;

/// Tryb z ustawień modułu (`mode`, `interval_min`, bez prefiksu):
/// `Some(interwał w minutach)` w trybie cyklicznym (także gdy tryb nie jest
/// ustawiony), `None` w trybie stałych pór. Interwał spoza 5–60 → 10.
/// Jedno źródło prawdy dla runnera i panelu.
pub fn cycle_interval(settings: &std::collections::BTreeMap<String, String>) -> Option<u32> {
    if settings.get("mode").map(|m| m.trim()) == Some(MODE_TIMES) {
        return None;
    }
    Some(
        settings
            .get("interval_min")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|n| (MIN_INTERVAL_MIN..=MAX_INTERVAL_MIN).contains(n))
            .unwrap_or(DEFAULT_INTERVAL_MIN),
    )
}

/// Stacja sprzed trybu cyklicznego: ma zapisane pory (`times`), ale nie
/// ma zapisanego trybu (`mode`) — po aktualizacji nadaje więc co
/// [`DEFAULT_INTERVAL_MIN`] minut zamiast o swoich porach. Start stacji
/// ostrzega o tym w logu.
pub async fn times_without_mode(pool: &SqlitePool) -> Result<bool, DbError> {
    let repo = SettingsRepo::new(pool.clone());
    let times = repo.kv_get("module:bulletin:times").await?;
    let mode = repo.kv_get("module:bulletin:mode").await?;
    Ok(mode.is_none() && times.is_some_and(|t| !t.trim().is_empty()))
}

/// Numer cyklu: minuty od północy (czas Warszawy) / interwał.
pub fn cycle_index(local: NaiveTime, interval_min: u32) -> u32 {
    (local.hour() * 60 + local.minute()) / interval_min.max(1)
}

/// Chwile cyklu w dobie jako minuty od północy: 0, N, 2N, … poniżej 24 h
/// (gdy N nie dzieli doby, cykl zaczyna się od nowa o północy).
pub fn cycle_minutes(interval_min: u32) -> impl Iterator<Item = u32> {
    let step = interval_min.clamp(1, DAY_MIN);
    (0..DAY_MIN).step_by(step as usize)
}

/// Najbliższa chwila cyklu po `local` (czas Warszawy) tego samego dnia;
/// `None` — następna jest jutro o północy.
pub fn next_cycle_after(interval_min: u32, local: NaiveTime) -> Option<NaiveTime> {
    let now = local.hour() * 60 + local.minute();
    cycle_minutes(interval_min)
        .find(|m| *m > now)
        .and_then(|m| NaiveTime::from_hms_opt(m / 60, m % 60, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use chrono::NaiveDate;

    fn hm(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    #[tokio::test]
    async fn stored_times_without_a_mode_are_reported_after_the_upgrade() {
        let pool = test_pool().await;
        let repo = SettingsRepo::new(pool.clone());
        assert!(!times_without_mode(&pool).await.unwrap(), "świeża instalacja");
        repo.kv_put("module:bulletin:times", "07:00,19:00").await.unwrap();
        assert!(times_without_mode(&pool).await.unwrap());
        repo.kv_put("module:bulletin:mode", MODE_TIMES).await.unwrap();
        assert!(!times_without_mode(&pool).await.unwrap(), "operator wybrał tryb");
    }

    #[test]
    fn the_budget_counts_normalizer_expansion_and_speech_rate() {
        // (180 − 10) × 13,5 × 0,87 ÷ 1,5 = 1331 znaków.
        let b = Budget::new(180, 0.87);
        assert_eq!(b.chars, 1331);
        assert_eq!(b.limit_s(), 169);
        assert_eq!(length_limit(180, 1.0), MAX_BULLETIN_CHARS);
        assert_eq!(length_limit(180, 0.70), 1070);
        assert_eq!(length_limit(60, 1.20), 540);
        // Górna granica jak dotąd.
        assert_eq!(length_limit(600, 1.20), MAX_BULLETIN_CHARS);
        assert_eq!(length_limit(10, 1.0), 0);
        assert_eq!(length_limit(5, 1.0), 0);
        // Tempo nieczytelne → domyślne.
        assert_eq!(Budget::new(180, f32::NAN), b);
        assert_eq!(Budget::new(180, 0.0), b);
        // Pauza: sekunda kosztuje tyle znaków, ile w niej da się powiedzieć
        // (przy tempie 1,5 — 13,5 znaku/s).
        let one = Budget::new(160, 1.5);
        assert_eq!(one.pause_chars(1000), 14);
        assert_eq!(one.pause_chars(150), 3);
        assert_eq!(one.pause_chars(0), 0);
        assert!((one.seconds(27) - 2.0).abs() < 1e-9);
        // Najdłuższy biuletyn mieści się w limicie: znaki × 1,5 przy tempie
        // mowy 13,5 × rate znaków/s plus rezerwa nie przekracza limitu.
        for rate in [0.70_f32, 0.87, 1.0, 1.20] {
            for tx in [30_u64, 60, 120, 180, 300, 600] {
                let b = Budget::new(tx, rate);
                let spoken_s = b.chars as f64 * NORMALIZER_EXPANSION
                    / (SPOKEN_CHARS_PER_S * f64::from(rate));
                assert!(spoken_s + TX_RESERVE_S as f64 <= tx as f64 + 1e-9, "{tx} {rate}");
            }
        }
    }

    fn kinds(l: &Layout) -> Vec<BlockKind> {
        l.blocks.iter().map(|b| b.kind).collect()
    }

    #[test]
    fn the_default_layout_is_the_full_bulletin() {
        use BlockKind::*;
        let l = Layout::default_layout();
        assert_eq!(
            kinds(&l),
            [
                Wstep,
                Ostrzezenia,
                PogodaTeraz,
                Prognoza,
                Slonce,
                Wody,
                Propagacja,
                Zakonczenie
            ]
        );
        assert!(l.validate().is_ok());
        assert!(l.blocks.iter().all(|b| b.enabled));
        assert_eq!(l.blocks[0].text, DEFAULT_INTRO);
        assert_eq!(l.blocks[0].pause_after_ms, 700);
        assert!(l.blocks[1].say_when_empty);
        assert_eq!(l.blocks[1].pause_after_ms, 700);
        assert_eq!(l.blocks[3].pause_after_ms, 500);
        assert_eq!(l.blocks[6].when, When::CoNCykli { n: 3 });
        assert_eq!(l.blocks[7].text, DEFAULT_OUTRO);
    }

    #[test]
    fn json_round_trips_with_polish_names() {
        let mut l = Layout::default_layout();
        l.blocks.push(Block {
            when: When::WGodzinach {
                od: hm("22:00"),
                do_: hm("06:00"),
            },
            ..Block::new(BlockKind::Tekst).with_text("Dobranoc.")
        });
        let json = l.to_json();
        for needle in [
            "\"bloki\"",
            "\"rodzaj\": \"pogoda_teraz\"",
            "\"typ\": \"co_n_cykli\"",
            "\"n\": 3",
            "\"typ\": \"w_godzinach\"",
            "\"od\": \"22:00\"",
            "\"do\": \"06:00\"",
            "\"pauza_po_ms\": 700",
            "\"mow_gdy_brak\": true",
            "\"wlaczony\": true",
        ] {
            assert!(json.contains(needle), "{needle} w {json}");
        }
        assert_eq!(Layout::parse(&json).unwrap(), l);
    }

    #[test]
    fn an_error_names_the_block_it_is_about() {
        let e = Layout::parse_located(
            r#"{"bloki":[{"rodzaj":"wody"},{"rodzaj":"tekst","tekst":"Tu {stacja}."}]}"#,
        )
        .unwrap_err();
        assert_eq!(e.block, Some(1));
        assert!(e.detail.starts_with("nieznana zmienna"), "{e:?}");
        assert!(e.message.starts_with("klocek 2 (Własny tekst): nieznana zmienna"), "{e:?}");
        let e = Layout::parse_located("nie json").unwrap_err();
        assert_eq!(e.block, None);
        let e = Layout::parse_located(r#"{"bloki":[]}"#).unwrap_err();
        assert_eq!(e.block, None);
        // Sam odczyt przepuszcza układ łamiący reguły (panel pokazuje go z błędem).
        assert!(Layout::parse_unchecked(r#"{"bloki":[]}"#).is_ok());
    }

    #[test]
    fn block_ids_match_the_json_names() {
        for kind in BlockKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.id()));
            assert_eq!(BlockKind::from_id(kind.id()), Some(kind));
            assert!(!kind.description().is_empty());
        }
    }

    #[test]
    fn missing_fields_take_defaults() {
        let l = Layout::parse(r#"{"bloki":[{"rodzaj":"wody"}]}"#).unwrap();
        assert_eq!(l.blocks, [Block::new(BlockKind::Wody)]);
    }

    #[test]
    fn bad_layouts_are_refused() {
        let bad = |json: &str| Layout::parse(json).unwrap_err();
        assert!(bad("nie json").contains("niepoprawny zapis"));
        assert!(bad(r#"{"bloki":[{"rodzaj":"wody","coś":1}]}"#).contains("niepoprawny zapis"));
        assert!(bad(r#"{"bloki":[{"rodzaj":"nieznany"}]}"#).contains("niepoprawny zapis"));
        assert!(bad(r#"{"bloki":[]}"#).contains("co najmniej jeden"));
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wody","pauza_przed_ms":10001}]}"#)
                .contains("pauza najwyżej")
        );
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wody","pauza_po_ms":20000}]}"#).contains("pauza najwyżej")
        );
        assert!(bad(r#"{"bloki":[{"rodzaj":"wody","pauza_po_ms":-1}]}"#).contains("niepoprawny"));
        assert!(bad(r#"{"bloki":[{"rodzaj":"wstep","tekst":"  "}]}"#).contains("wpisz tekst"));
        assert!(bad(r#"{"bloki":[{"rodzaj":"tekst"}]}"#).contains("wpisz tekst"));
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wody","tekst":"x"}]}"#).contains("nie ma własnego tekstu")
        );
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wstep","tekst":"Tu {stacja}."}]}"#)
                .contains("nieznana zmienna „{stacja}”")
        );
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wstep","tekst":"Tu {znak."}]}"#).contains("niedomknięty")
        );
        assert!(bad(r#"{"bloki":[{"rodzaj":"wstep","tekst":"Tu znak}."}]}"#).contains("bez „{”"));
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"tekst","tekst":"a [[pauza:500]] b"}]}"#).contains("pauzy")
        );
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wody","kiedy":{"typ":"co_n_cykli","n":1}}]}"#)
                .contains("N od 2 do 24")
        );
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wody","kiedy":{"typ":"co_n_cykli","n":25}}]}"#)
                .contains("N od 2 do 24")
        );
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wody","kiedy":{"typ":"w_godzinach","od":"7:00","do":"7:00"}}]}"#)
                .contains("muszą się różnić")
        );
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wody","kiedy":{"typ":"w_godzinach","od":"25:00","do":"7:00"}}]}"#)
                .contains("HH:MM")
        );
        assert!(bad(r#"{"bloki":[{"rodzaj":"wody"},{"rodzaj":"wody"}]}"#).contains("już jest"));
        assert!(
            bad(r#"{"bloki":[{"rodzaj":"wody","mow_gdy_brak":true}]}"#).contains("tylko ostrzeżeń")
        );
        let long = format!(
            r#"{{"bloki":[{{"rodzaj":"tekst","tekst":"{}"}}]}}"#,
            "a".repeat(501)
        );
        assert!(bad(&long).contains("najwyżej 500"));

        let mut l = Layout { blocks: Vec::new() };
        for _ in 0..25 {
            l.blocks.push(Block::new(BlockKind::Tekst).with_text("A."));
        }
        assert!(l.validate().unwrap_err().contains("najwyżej 24"));
        l.blocks.pop();
        assert!(
            l.validate().is_ok(),
            "24 klocki i powtórzone własne teksty są w porządku"
        );
    }

    #[test]
    fn every_nth_cycle_and_hours_across_midnight() {
        let every3 = When::CoNCykli { n: 3 };
        let noon = hm("12:00");
        assert!(every3.matches(Some(0), noon));
        assert!(!every3.matches(Some(1), noon));
        assert!(!every3.matches(Some(2), noon));
        assert!(every3.matches(Some(3), noon));
        assert!(every3.matches(None, noon), "tryb pór: zawsze");

        let night = When::WGodzinach {
            od: hm("22:00"),
            do_: hm("06:00"),
        };
        assert!(night.matches(None, hm("22:00")));
        assert!(night.matches(None, hm("23:59")));
        assert!(night.matches(None, hm("00:00")));
        assert!(night.matches(None, hm("05:59")));
        assert!(!night.matches(None, hm("06:00")));
        assert!(!night.matches(None, hm("12:00")));
        let day = When::WGodzinach {
            od: hm("08:00"),
            do_: hm("20:00"),
        };
        assert!(day.matches(None, hm("08:00")));
        assert!(!day.matches(None, hm("20:00")));
        assert!(!day.matches(None, hm("07:59")));
        assert_eq!(night.describe(), "w godzinach 22:00–06:00");
        assert_eq!(every3.describe(), "co 3. cykl");
    }

    #[test]
    fn the_cycle_schedule_is_shared() {
        let s = |pairs: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert_eq!(cycle_interval(&s(&[])), Some(10));
        assert_eq!(
            cycle_interval(&s(&[("mode", "cykl"), ("interval_min", "15")])),
            Some(15)
        );
        assert_eq!(
            cycle_interval(&s(&[("mode", "cykl"), ("interval_min", "3")])),
            Some(10)
        );
        assert_eq!(
            cycle_interval(&s(&[("mode", "cykl"), ("interval_min", "61")])),
            Some(10)
        );
        assert_eq!(
            cycle_interval(&s(&[("mode", "pory"), ("interval_min", "15")])),
            None
        );
        assert_eq!(cycle_index(hm("00:00"), 10), 0);
        assert_eq!(cycle_index(hm("00:09"), 10), 0);
        assert_eq!(cycle_index(hm("00:30"), 10), 3);
        assert_eq!(cycle_index(hm("23:50"), 10), 143);
        assert_eq!(cycle_index(hm("01:00"), 15), 4);
        assert_eq!(cycle_minutes(10).count(), 144);
        assert_eq!(cycle_minutes(7).last(), Some(23 * 60 + 55));
        assert_eq!(next_cycle_after(10, hm("07:00")), Some(hm("07:10")));
        assert_eq!(next_cycle_after(7, hm("23:50")), Some(hm("23:55")));
        assert_eq!(next_cycle_after(7, hm("23:55")), None);
        assert_eq!(next_cycle_after(10, hm("23:50")), None);
    }

    #[test]
    fn templates_fill_polish_dates_and_times() {
        // 24 listopada 2026 to wtorek.
        let at = local(2026, 11, 24, 7, 5);
        assert_eq!(
            render_template(DEFAULT_INTRO, "SP7TQ", at),
            "Tu automatyczna stacja informacyjna SP7TQ. Wtorek, 24 listopada, godzina 7:05."
        );
        assert_eq!(
            render_template(DEFAULT_OUTRO, "SP7TQ", at),
            "Tu SP7TQ. Koniec komunikatu."
        );
        assert_eq!(
            render_template(
                "Dziś {dzien}, {data}, {godzina}.",
                "X",
                local(2026, 3, 1, 23, 50)
            ),
            "Dziś niedziela, 1 marca, 23:50."
        );
    }

    #[test]
    fn without_a_callsign_sentences_with_it_drop_out() {
        let at = local(2026, 11, 24, 7, 0);
        assert_eq!(
            render_template(DEFAULT_INTRO, "  ", at),
            "Wtorek, 24 listopada, godzina 7:00."
        );
        assert_eq!(render_template(DEFAULT_OUTRO, "", at), "Koniec komunikatu.");
        assert_eq!(render_template("Tu {znak}.", "", at), "");
    }

    #[test]
    fn legacy_parts_and_intro_become_the_layout() {
        // Stary domyślny wybór (w dowolnej kolejności) albo brak listy → pełny nowy układ.
        for parts in [
            None,
            Some("pogoda,propagacja,ostrzezenia"),
            Some(" ostrzezenia, pogoda,propagacja "),
        ] {
            let l = Layout::from_legacy(parts, None);
            assert_eq!(l, Layout::default_layout(), "{parts:?}");
        }
        let l = Layout::from_legacy(Some("pogoda,propagacja,ostrzezenia"), Some("Tu SP9"));
        assert!(l.blocks.iter().all(|b| b.enabled));
        assert_eq!(l.blocks[0].text, "Tu SP9. Godzina {godzina}.");
        // Lista zmieniona przez operatora: niewybrane części wyłączone.
        let l = Layout::from_legacy(Some("pogoda,propagacja,ostrzezenia,wody"), None);
        assert!(l.blocks.iter().all(|b| b.enabled));

        let l = Layout::from_legacy(Some("pogoda,ostrzezenia"), Some("Tu stacja klubowa"));
        let on = |k| l.blocks.iter().find(|b| b.kind == k).unwrap().enabled;
        assert!(on(BlockKind::PogodaTeraz) && on(BlockKind::Prognoza) && on(BlockKind::Slonce));
        assert!(on(BlockKind::Ostrzezenia));
        assert!(!on(BlockKind::Wody) && !on(BlockKind::Propagacja));
        assert!(on(BlockKind::Wstep) && on(BlockKind::Zakonczenie));
        assert_eq!(l.blocks[0].text, "Tu stacja klubowa. Godzina {godzina}.");
        assert!(l.validate().is_ok());

        // Bez wstępu operatora zostaje domyślny; klamry i pauzy ze starego tekstu znikają.
        assert_eq!(
            Layout::from_legacy(None, Some("")).blocks[0].text,
            DEFAULT_INTRO
        );
        let l = Layout::from_legacy(None, Some("Tu {x} [[pauza:500]]SP9."));
        assert_eq!(l.blocks[0].text, "Tu x SP9. Godzina {godzina}.");
        assert!(l.validate().is_ok());
    }

    #[tokio::test]
    async fn migration_moves_old_keys_once() {
        let pool = test_pool().await;
        let kv = SettingsRepo::new(pool.clone());
        assert_eq!(load(&pool).await.unwrap(), Layout::default_layout());

        kv.kv_put(LEGACY_PARTS_KEY, "pogoda,propagacja")
            .await
            .unwrap();
        kv.kv_put(LEGACY_INTRO_KEY, "Tu SP9XYZ").await.unwrap();
        let l = load(&pool).await.unwrap();
        assert_eq!(
            l,
            Layout::from_legacy(Some("pogoda,propagacja"), Some("Tu SP9XYZ"))
        );
        assert_eq!(kv.kv_get(LEGACY_PARTS_KEY).await.unwrap(), None);
        assert_eq!(kv.kv_get(LEGACY_INTRO_KEY).await.unwrap(), None);
        assert_eq!(kv.kv_get(LAYOUT_KEY).await.unwrap(), Some(l.to_json()));
        assert_ne!(l, Layout::default_layout());

        // Stary klucz, który wróci (np. stara wersja), nie nadpisuje układu.
        kv.kv_put(LEGACY_PARTS_KEY, "wody").await.unwrap();
        assert_eq!(load(&pool).await.unwrap(), l);
        assert_eq!(kv.kv_get(LEGACY_PARTS_KEY).await.unwrap(), None);

        // Stacja, która nie ruszała części: pełny układ, stany wód włączone.
        kv.kv_put(LAYOUT_KEY, "").await.unwrap();
        kv.kv_put(LEGACY_PARTS_KEY, "pogoda,propagacja,ostrzezenia")
            .await
            .unwrap();
        let l = load(&pool).await.unwrap();
        assert!(
            l.blocks
                .iter()
                .find(|b| b.kind == BlockKind::Wody)
                .unwrap()
                .enabled
        );
        assert_eq!(l, Layout::default_layout());
        assert_eq!(kv.kv_get(LAYOUT_KEY).await.unwrap().as_deref(), Some(""));

        // Zepsuty zapis → układ domyślny, bez błędu.
        kv.kv_put(LAYOUT_KEY, "{zepsute").await.unwrap();
        assert_eq!(load(&pool).await.unwrap(), Layout::default_layout());
    }
}
