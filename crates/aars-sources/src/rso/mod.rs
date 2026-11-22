//! Źródło Regionalnego Systemu Ostrzegania (RSO): Alert RCB, komunikaty
//! wojewódzkie, sanepid, próby syren i alarmy z powietrza.

pub mod parse;
pub mod speech;

use std::collections::HashSet;
use std::time::Duration;

use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};
use aars_core::priority::Priority;
use chrono::TimeDelta;
use sha2::{Digest, Sha256};

use crate::http;
use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};
use parse::News;

pub const MAX_BODY: usize = 2 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_CATEGORIES: &str = "ogolne,atak-z-powietrza";
const DEFAULT_MAX_AGE_H: i64 = 24;
const FINGERPRINT_CHARS: usize = 2000;

/// (kod TERYT, slug RSO, nazwa mówiona).
pub const VOIVODESHIPS: [(&str, &str, &str); 16] = [
    ("02", "dolnoslaskie", "województwo dolnośląskie"),
    ("04", "kujawsko-pomorskie", "województwo kujawsko-pomorskie"),
    ("06", "lubelskie", "województwo lubelskie"),
    ("08", "lubuskie", "województwo lubuskie"),
    ("10", "lodzkie", "województwo łódzkie"),
    ("12", "malopolskie", "województwo małopolskie"),
    ("14", "mazowieckie", "województwo mazowieckie"),
    ("16", "opolskie", "województwo opolskie"),
    ("18", "podkarpackie", "województwo podkarpackie"),
    ("20", "podlaskie", "województwo podlaskie"),
    ("22", "pomorskie", "województwo pomorskie"),
    ("24", "slaskie", "województwo śląskie"),
    ("26", "swietokrzyskie", "województwo świętokrzyskie"),
    ("28", "warminsko-mazurskie", "województwo warmińsko-mazurskie"),
    ("30", "wielkopolskie", "województwo wielkopolskie"),
    ("32", "zachodniopomorskie", "województwo zachodniopomorskie"),
];

const CATEGORIES: &[(&str, &str)] = &[
    ("ogolne", "Ogólne (Alert RCB, sanepid, WCZK)"),
    ("atak-z-powietrza", "Atak z powietrza"),
    ("informacje-drogowe", "Informacje drogowe"),
    ("meteorologiczne", "Meteorologiczne (dubluje IMGW)"),
    ("hydrologiczne", "Hydrologiczne"),
];

const ATTACK: &str = "atak-z-powietrza";

pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: "rso",
    name: "Regionalny System Ostrzegania (RSO)",
    description: "Komunikaty RSO: Alert RCB, komunikaty wojewódzkie i sanepidu, próby syren, alarmy z powietrza.",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Area,
    settings: &[
        SettingSpec {
            key: "area",
            label: "Obszar",
            help: "Jedna linia (albo wpis po średniku) na obszar: dwucyfrowy kod województwa (np. 10) albo czterocyfrowy kod powiatu z nazwą, którą stacja ma wypowiadać (np. 1061 Łódź). RSO publikuje komunikaty na poziomie województwa: liczy się województwo z dwóch pierwszych cyfr kodu. Najwyżej 40 wpisów; puste pole to brak obszaru — źródło nic nie ogłasza.",
            kind: SettingKind::Area,
            default: "",
        },
        SettingSpec {
            key: "categories",
            label: "Kategorie",
            help: "Które kategorie komunikatów RSO pobierać. Atak z powietrza jest pobierany zawsze, nawet odznaczony. Alert RCB i atak z powietrza przerywają audycję.",
            kind: SettingKind::List(CATEGORIES),
            default: DEFAULT_CATEGORIES,
        },
        SettingSpec {
            key: "keywords",
            label: "Słowa kluczowe",
            help: "Słowa po przecinku; gdy podane, zwykły komunikat musi zawierać któreś (bez względu na wielkość liter). Alert RCB i atak z powietrza są ogłaszane zawsze.",
            kind: SettingKind::Text { max_len: 300 },
            default: "",
        },
        SettingSpec {
            key: "max_age_h",
            label: "Maksymalny wiek komunikatu (h)",
            help: "Starsze, jeszcze nieogłoszone komunikaty są pomijane (nie dotyczy Alertu RCB i ataku z powietrza).",
            kind: SettingKind::Integer { min: 1, max: 720 },
            default: "24",
        },
        SettingSpec {
            key: "interval_s",
            label: "Interwał odpytywania (s)",
            help: "Jak często odpytywać źródło.",
            kind: SettingKind::Integer { min: 120, max: 3600 },
            default: "180",
        },
    ],
    see_also: None,
};

/// Kategoria RSO (slug z adresu).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Category(pub &'static str);

impl Category {
    fn is_attack(self) -> bool {
        self.0 == ATTACK
    }
}

/// Kategorie z ustawień w kolejności z listy; nieznane slugi pomijane,
/// brak albo nieczytelna wartość to domyślne.
fn categories(ctx: &SourceCtx) -> Vec<Category> {
    let parse = |raw: &str| -> Vec<Category> {
        CATEGORIES
            .iter()
            .filter(|(slug, _)| raw.split(',').map(str::trim).any(|w| w == *slug))
            .map(|(slug, _)| Category(slug))
            .collect()
    };
    let chosen = ctx.settings.get("categories").map(|v| parse(v)).unwrap_or_default();
    if chosen.is_empty() { parse(DEFAULT_CATEGORIES) } else { chosen }
}

fn categories_text(cats: &[Category]) -> String {
    cats.iter().map(|c| c.0).collect::<Vec<_>>().join(",")
}

fn keywords(ctx: &SourceCtx) -> Vec<String> {
    ctx.settings
        .get("keywords")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

fn max_age(ctx: &SourceCtx) -> TimeDelta {
    let hours = ctx
        .settings
        .get("max_age_h")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| (1..=720).contains(n))
        .unwrap_or(DEFAULT_MAX_AGE_H);
    TimeDelta::hours(hours)
}

pub struct Rso {
    client: reqwest::Client,
    base: String,
}

impl Rso {
    pub fn new(base: &str) -> Result<Self, SourceError> {
        Self::with_timeout(base, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(base: &str, timeout: Duration) -> Result<Self, SourceError> {
        Ok(Self {
            client: http::client(timeout)?,
            base: base.trim_end_matches('/').to_owned(),
        })
    }
}

/// Województwo z dwóch pierwszych cyfr kodu obszaru (2 albo 4 cyfry).
fn voivodeship_of(code: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    let prefix = code.get(..2)?;
    VOIVODESHIPS.iter().find(|(c, _, _)| *c == prefix)
}

/// Unikalne slugi województw obszaru w kolejności obszaru.
fn area_slugs(ctx: &SourceCtx) -> Vec<&'static str> {
    let mut slugs: Vec<&'static str> = Vec::new();
    for entry in &ctx.area {
        let slug = voivodeship_of(&entry.code).map(|(_, slug, _)| *slug);
        if let Some(slug) = slug
            && !slugs.contains(&slug)
        {
            slugs.push(slug);
        }
    }
    slugs
}

impl DataSource for Rso {
    fn id(&self) -> &'static str {
        "rso"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(180)
    }

    fn scope(&self, ctx: &SourceCtx) -> String {
        format!(
            "{}|cat={}|kw={}",
            ctx.area_text,
            categories_text(&categories(ctx)),
            keywords_text(ctx)
        )
    }

    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            // Atak z powietrza zawsze: odznaczenie go nie może zgubić alarmu.
            let mut cats = categories(ctx);
            if !cats.iter().any(|c| c.is_attack()) {
                cats.push(Category(ATTACK));
            }
            let mut batches = Vec::new();
            let mut skipped = 0;
            let mut failed = 0;
            let mut first_error: Option<SourceError> = None;
            let mut requests = 0;
            // Po kolei, żeby nie obciążać serwera.
            for slug in area_slugs(ctx) {
                for cat in &cats {
                    requests += 1;
                    let url = format!("{}/{slug}/{}/0?_format=json", self.base, cat.0);
                    let result = match http::get_limited(&self.client, &url, MAX_BODY).await {
                        Ok(body) => parse::parse_news(&body),
                        Err(e) => Err(e),
                    };
                    match result {
                        Ok((mut news, s)) => {
                            skipped += s;
                            for n in &mut news {
                                n.feed_slug = Some(slug.to_owned());
                            }
                            batches.push((*cat, news));
                        }
                        Err(e) => {
                            tracing::warn!("RSO {slug}/{}: {e}", cat.0);
                            failed += 1;
                            first_error.get_or_insert(e);
                        }
                    }
                }
            }
            if requests > 0
                && failed == requests
                && let Some(e) = first_error
            {
                return Err(e);
            }
            Ok(to_output(batches, skipped, failed, ctx))
        })
    }
}

fn keywords_text(ctx: &SourceCtx) -> String {
    keywords(ctx).join(",")
}

const NEGATIONS: [&str; 5] = ["nie jest to", "to nie jest", "nie są to", "to nie są", "to nie"];

/// Próba/ćwiczenie: w TYTULE słowo zaczynające się od „test” albo „ćwicz”,
/// o ile nie poprzedza go zaprzeczenie („To nie są ćwiczenia”).
fn mentions_test(n: &News) -> bool {
    let title = n.title.to_lowercase();
    for stem in ["test", "ćwicz"] {
        for (at, _) in title.match_indices(stem) {
            let before = title.get(..at).unwrap_or_default();
            if before.chars().next_back().is_some_and(char::is_alphanumeric) {
                continue;
            }
            let before = before.trim_end();
            if NEGATIONS.iter().any(|neg| before.ends_with(neg)) {
                continue;
            }
            return true;
        }
    }
    false
}

/// `{id}|{tytuł}|{skrót16}` dla każdej pozycji: RSO nie łączy starych i
/// nowych id, więc scalanie R4 po odcisku nic nie daje, a id chroni przed
/// scaleniem nowego komunikatu o identycznym szablonie ze starym. Odcisk nie
/// zależy od kategorii (alarm czy nie), więc częściowa awaria go nie zmienia.
fn fingerprint(n: &News) -> String {
    let clean = crate::speech::clean_text(&n.content);
    let head: String = clean.chars().take(FINGERPRINT_CHARS).collect();
    let digest = Sha256::digest(head.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{}|{}|{hex}", n.id, n.title)
}

/// Czysta funkcja: paczki (kategoria, pozycje) → ogłoszenia dla obszaru.
pub fn to_output(
    batches: Vec<(Category, Vec<News>)>,
    skipped: usize,
    failed_requests: usize,
    ctx: &SourceCtx,
) -> SourceOutput {
    let complete = skipped == 0 && failed_requests == 0;
    let words = keywords(ctx);
    let max_age = max_age(ctx);
    let attack_ids: HashSet<i64> = batches
        .iter()
        .filter(|(cat, _)| cat.is_attack())
        .flat_map(|(_, list)| list.iter().map(|n| n.id))
        .collect();
    let mut items: Vec<SourceItem> = Vec::new();
    for (_, list) in &batches {
        for n in list {
            if items.iter().any(|i| i.key == n.id.to_string()) {
                continue;
            }
            let key = n.id.to_string();
            let alarm = n.title.to_lowercase().contains("alert rcb") || attack_ids.contains(&n.id);
            // Pierwsze województwo obszaru, którego dotyczy pozycja. Alarm bez
            // województw należy do kanału, z którego przyszedł (nie gubimy go).
            let in_area = |slugs: &[&str]| {
                ctx.area.iter().find_map(|entry| {
                    let (_, slug, name) = voivodeship_of(&entry.code)?;
                    slugs.contains(slug).then_some(*name)
                })
            };
            let provinces: Vec<&str> = n.province_slugs.iter().map(String::as_str).collect();
            let voivodeship = if provinces.is_empty() && alarm {
                n.feed_slug.as_deref().and_then(|feed| in_area(&[feed]))
            } else {
                in_area(&provinces)
            };
            let Some(voivodeship) = voivodeship else {
                continue;
            };
            if !alarm {
                if !words.is_empty() {
                    let hay = [&n.title, &n.shortcut, &n.content]
                        .map(|t| crate::speech::clean_text(t))
                        .join(" ")
                        .to_lowercase();
                    if !words.iter().any(|w| hay.contains(w.as_str())) {
                        continue;
                    }
                }
                // Brak `updated_at`: wieku nie znamy, więc bezpieczniej pominąć.
                let stale = n.updated_at.is_none_or(|t| ctx.now - t > max_age);
                if stale && !ctx.tracked_keys.contains(&key) {
                    continue;
                }
            }
            let emergency = alarm && !mentions_test(n);
            let text = speech::announcement(n, voivodeship);
            items.push(SourceItem {
                topic: format!("rso:{}", n.id),
                fingerprint: fingerprint(n),
                change_text: speech::change(&text),
                text,
                cancel_text: speech::cancel(&n.title),
                summary: speech::summary(&n.title),
                priority: if emergency { Priority::Emergency } else { Priority::Normal },
                interrupting: emergency,
                valid_from: None,
                valid_until: n.valid_until,
                key,
            });
        }
    }
    SourceOutput { items, complete }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::area::parse_area;
    use chrono::{DateTime, TimeZone, Utc};
    use serde_json::{Value, json};

    const OGOLNE: &[u8] = include_bytes!("../../tests/fixtures/rso/wszystkie-ogolne-2026-11-22.json");
    const DROGOWE: &[u8] =
        include_bytes!("../../tests/fixtures/rso/wszystkie-informacje-drogowe-2026-11-22.json");
    const ATAK_PUSTO: &[u8] =
        include_bytes!("../../tests/fixtures/rso/wszystkie-atak-z-powietrza-2026-11-22-pusto.json");
    const PUSTO: &[u8] = include_bytes!("../../tests/fixtures/rso/lodzkie-ogolne-2026-11-22-pusto.json");

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 22, 15, 0, 0).unwrap()
    }

    fn ctx(area: &str, settings: &[(&str, &str)]) -> SourceCtx {
        let entries = parse_area(area).unwrap();
        SourceCtx {
            now: now(),
            area_text: aars_core::area::area_text(&entries),
            area: entries,
            position: None,
            settings: settings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            tracked_keys: Default::default(),
        }
    }

    /// Kontekst, w którym pozycja z id Alertu (23521833) jest już śledzona:
    /// zwykłe komunikaty z tym id przechodzą bramkę wieku.
    fn tracked(area: &str) -> SourceCtx {
        let mut c = ctx(area, &[]);
        c.tracked_keys.insert("23521833".into());
        c
    }

    fn news(body: &[u8]) -> Vec<News> {
        parse::parse_news(body).unwrap().0
    }

    fn ogolne() -> Category {
        Category("ogolne")
    }

    fn run(items: Vec<News>, cat: Category, c: &SourceCtx) -> SourceOutput {
        to_output(vec![(cat, items)], 0, 0, c)
    }

    fn alert() -> News {
        news(OGOLNE).into_iter().find(|n| n.id == 23_521_833).unwrap()
    }

    fn keys(out: &SourceOutput) -> Vec<&str> {
        out.items.iter().map(|i| i.key.as_str()).collect()
    }

    #[test]
    fn the_descriptor_has_the_documented_settings() {
        let d = &DESCRIPTOR;
        assert_eq!((d.id, d.name), ("rso", "Regionalny System Ostrzegania (RSO)"));
        assert_eq!(d.needs, Needs::Area);
        assert!(!d.default_enabled);
        let keys: Vec<_> = d.settings.iter().map(|s| (s.key, s.default)).collect();
        assert_eq!(
            keys,
            [
                ("area", ""),
                ("categories", "ogolne,atak-z-powietrza"),
                ("keywords", ""),
                ("max_age_h", "24"),
                ("interval_s", "180")
            ]
        );
        assert_eq!(d.settings[1].kind, SettingKind::List(CATEGORIES));
        assert_eq!(d.settings[2].kind, SettingKind::Text { max_len: 300 });
        assert_eq!(d.settings[3].kind, SettingKind::Integer { min: 1, max: 720 });
        assert_eq!(d.settings[4].kind, SettingKind::Integer { min: 120, max: 3600 });
        assert!(!CATEGORIES.iter().any(|(s, _)| *s == "stany-wod"));
    }

    #[test]
    fn the_quirky_json_parses() {
        let (all, skipped) = parse::parse_news(OGOLNE).unwrap();
        assert_eq!((all.len(), skipped), (42, 0));
        let a = all.iter().find(|n| n.id == 23_521_833).unwrap();
        assert_eq!(a.province_slugs, ["dolnoslaskie"]);
        assert_eq!(a.valid_until, Some(Utc.with_ymd_and_hms(2026, 11, 22, 22, 59, 0).unwrap()));
        assert_eq!(a.title, "Alert RCB");
        assert_eq!(news(DROGOWE).len(), 5);
        assert!(news(PUSTO).is_empty());
        assert!(matches!(parse::parse_news(b"{}"), Err(SourceError::Parse(_))));
        assert!(matches!(parse::parse_news(b"nie json"), Err(SourceError::Parse(_))));
    }

    #[test]
    fn an_item_without_id_is_skipped_and_counted() {
        let body = json!({"newses": [{"title": "bez id"}, {"id": 1, "title": "t", "provinces": []}]});
        let (items, skipped) = parse::parse_news(body.to_string().as_bytes()).unwrap();
        assert_eq!((items.len(), skipped), (1, 1));
        assert!(items[0].province_slugs.is_empty());
    }

    #[test]
    fn an_alert_rcb_is_an_interrupting_emergency_even_when_old() {
        let out = run(vec![alert()], ogolne(), &ctx("02", &[]));
        assert!(out.complete);
        assert_eq!(out.items.len(), 1);
        let i = &out.items[0];
        assert_eq!((i.key.as_str(), i.topic.as_str()), ("23521833", "rso:23521833"));
        assert_eq!(i.priority, Priority::Emergency);
        assert!(i.interrupting);
        assert!(
            i.text.starts_with("Alert RCB, województwo dolnośląskie. Na wniosek wojewody dolnośląskiego"),
            "{}",
            i.text
        );
        assert!(!i.text.contains('\n'));
        assert!(i.change_text.starts_with("Zmiana komunikatu RSO. Alert RCB, województwo"));
        assert_eq!(i.cancel_text, "Komunikat RSO „Alert RCB” nie obowiązuje.");
        assert_eq!(i.summary, "Alert RCB");
        assert_eq!(i.valid_until, Some(Utc.with_ymd_and_hms(2026, 11, 22, 22, 59, 0).unwrap()));
    }

    #[test]
    fn the_attack_category_is_an_interrupting_emergency() {
        let mut n = alert();
        n.title = "Alarm przeciwlotniczy".into();
        n.content = "Zachowaj spokój.".into();
        let out = run(vec![n], Category("atak-z-powietrza"), &ctx("02", &[]));
        let i = &out.items[0];
        assert_eq!(i.priority, Priority::Emergency);
        assert!(i.interrupting);
        assert_eq!(i.text, "Komunikat RSO, województwo dolnośląskie. Alarm przeciwlotniczy. Zachowaj spokój.");
    }

    #[test]
    fn a_plain_item_is_normal_and_does_not_interrupt() {
        let mut c = ctx("02", &[]);
        c.tracked_keys.insert("23524661".into());
        let n = news(OGOLNE).into_iter().find(|n| n.id == 23_524_661).unwrap();
        let out = run(vec![n], ogolne(), &c);
        let i = &out.items[0];
        assert_eq!((i.priority, i.interrupting), (Priority::Normal, false));
        assert!(
            i.text.starts_with("Komunikat RSO, województwo dolnośląskie. Woda częściowo niezdatna do spożycia. Uwaga!"),
            "{}",
            i.text
        );
        assert!(i.text.ends_with("gospodarczych."), "{}", i.text);
        assert!(!i.text.contains("Pełna treść"));
    }

    #[test]
    fn old_untracked_items_are_skipped_but_tracked_ones_stay() {
        let n = news(OGOLNE).into_iter().find(|n| n.id == 23_524_661).unwrap();
        let out = run(vec![n.clone()], ogolne(), &ctx("02", &[]));
        assert!(out.items.is_empty());
        assert!(out.complete);
        let mut c = ctx("02", &[]);
        c.tracked_keys.insert("23524661".into());
        assert_eq!(keys(&run(vec![n.clone()], ogolne(), &c)), ["23524661"]);
        // Dłuższy próg też je przepuszcza.
        assert_eq!(keys(&run(vec![n.clone()], ogolne(), &ctx("02", &[("max_age_h", "48")]))), ["23524661"]);
        // Brak updated_at: wieku nie da się sprawdzić, więc nieśledzona pozycja
        // jest pomijana (bezpieczniej niż nadać stary komunikat), śledzona zostaje.
        let mut undated = n;
        undated.updated_at = None;
        assert!(run(vec![undated.clone()], ogolne(), &ctx("02", &[])).items.is_empty());
        assert_eq!(keys(&run(vec![undated.clone()], ogolne(), &c)), ["23524661"]);
        // Alarm bez updated_at przechodzi zawsze.
        undated.title = "Alert RCB".into();
        assert_eq!(keys(&run(vec![undated], ogolne(), &ctx("02", &[]))), ["23524661"]);
    }

    #[test]
    fn a_test_alert_does_not_interrupt() {
        let normal = |title: &str, content: &str| {
            let mut n = alert();
            n.title = title.into();
            n.content = content.into();
            let i = run(vec![n], ogolne(), &ctx("02", &[])).items[0].clone();
            (i.priority, i.interrupting)
        };
        let test = (Priority::Normal, false);
        let real = (Priority::Emergency, true);
        assert_eq!(normal("Alert RCB – test", "Treść."), test);
        assert_eq!(normal("Alert RCB – Test", "Treść."), test);
        assert_eq!(normal("Alert RCB: ćwiczenia", "Treść."), test);
        assert_eq!(normal("Alert RCB (TEST)", "Treść."), test);
        assert_eq!(normal("Test syren alarmowych Alert RCB", "Treść."), test);
        assert_eq!(normal("Alert RCB: protest rolników, utrudnienia", "Treść."), real);
        assert_eq!(normal("Alert RCB. To nie są ćwiczenia", "Treść."), real);
        assert_eq!(normal("Alert RCB. Nie jest to test", "Treść."), real);
        assert_eq!(normal("Alert RCB. To nie jest test", "Treść."), real);
        assert_eq!(normal("Alert RCB", "To jest ćwiczenie? Ćwiczenia syren. Test."), real);
        assert_eq!(normal("Alert RCB", "Próba syren: TEST systemu."), real);
        assert_eq!(normal("Alert RCB atest kontestować", "x"), real);
    }

    #[test]
    fn an_attack_item_first_seen_in_another_category_stays_an_alarm() {
        let mut n = alert();
        n.title = "Komunikat dla ludności".into();
        n.updated_at = None;
        let out = to_output(
            vec![(ogolne(), vec![n.clone()]), (Category("atak-z-powietrza"), vec![n])],
            0,
            0,
            &ctx("02", &[]),
        );
        assert_eq!(out.items.len(), 1);
        assert_eq!((out.items[0].priority, out.items[0].interrupting), (Priority::Emergency, true));
    }

    #[test]
    fn every_fingerprint_carries_the_id() {
        let fp = |id: i64, title: &str, cat: Category| {
            let mut n = alert();
            n.id = id;
            n.title = title.into();
            n.updated_at = None;
            let mut c = ctx("02", &[]);
            c.tracked_keys.insert(id.to_string());
            run(vec![n], cat, &c).items[0].fingerprint.clone()
        };
        assert_ne!(fp(1, "Alert RCB", ogolne()), fp(2, "Alert RCB", ogolne()));
        assert!(fp(1, "Alert RCB", ogolne()).starts_with("1|Alert RCB|"));
        let atak = Category("atak-z-powietrza");
        assert_ne!(fp(1, "Alarm", atak), fp(2, "Alarm", atak));
        // RSO nie łączy starych i nowych id, więc odcisk bez id nic nie daje,
        // a przy częściowej awarii zmieniałby się z kategorią (fałszywa zmiana).
        assert_ne!(fp(1, "Woda", ogolne()), fp(2, "Woda", ogolne()));
        assert!(fp(1, "Woda", ogolne()).starts_with("1|Woda|"));
        let mut n = alert();
        n.title = "Komunikat".into();
        n.updated_at = None;
        let with_attack = to_output(
            vec![(ogolne(), vec![n.clone()]), (Category("atak-z-powietrza"), vec![n.clone()])],
            0,
            0,
            &tracked("02"),
        );
        let attack_failed = to_output(vec![(ogolne(), vec![n])], 0, 1, &tracked("02"));
        assert_eq!(with_attack.items[0].fingerprint, attack_failed.items[0].fingerprint);
    }

    #[test]
    fn a_long_title_is_cut_in_the_cancellation_too() {
        let mut n = alert();
        n.title = "Bardzo długi tytuł komunikatu ".repeat(20);
        let i = &run(vec![n], ogolne(), &tracked("02")).items[0];
        assert!(i.cancel_text.chars().count() <= 200, "{}", i.cancel_text);
        assert!(i.cancel_text.starts_with("Komunikat RSO „Bardzo długi"), "{}", i.cancel_text);
        assert!(i.cancel_text.ends_with("” nie obowiązuje."), "{}", i.cancel_text);
    }

    #[test]
    fn an_alarm_without_provinces_belongs_to_the_voivodeship_of_its_feed() {
        let mut n = alert();
        n.province_slugs.clear();
        n.feed_slug = Some("lodzkie".into());
        let out = run(vec![n.clone()], ogolne(), &ctx("02\n10", &[]));
        assert_eq!(keys(&out), ["23521833"]);
        assert!(out.items[0].text.starts_with("Alert RCB, województwo łódzkie."), "{}", out.items[0].text);
        // Atak z powietrza też.
        let mut attack = n.clone();
        attack.title = "Alarm".into();
        assert_eq!(run(vec![attack], Category("atak-z-powietrza"), &ctx("10", &[])).items.len(), 1);
        // Zwykły komunikat bez województw dalej odpada, nawet śledzony.
        let mut plain = n;
        plain.title = "Woda".into();
        let mut c = ctx("10", &[]);
        c.tracked_keys.insert(plain.id.to_string());
        assert!(run(vec![plain], ogolne(), &c).items.is_empty());
    }

    #[test]
    fn rso_texts_survive_normalization() {
        let mut c = ctx("02", &[]);
        let all = news(OGOLNE);
        for n in &all {
            c.tracked_keys.insert(n.id.to_string());
        }
        let out = run(all, ogolne(), &c);
        assert!(out.items.len() >= 2);
        for i in &out.items {
            crate::speech::assert_speakable(&[&i.text, &i.change_text, &i.cancel_text]);
        }
    }

    #[test]
    fn a_long_title_is_cut_in_the_spoken_text() {
        let mut n = alert();
        n.title = "Bardzo długi tytuł komunikatu ".repeat(20);
        n.content = "Zdanie numer jeden jest tutaj. ".repeat(15);
        n.updated_at = None;
        let i = &run(vec![n], ogolne(), &tracked("02")).items[0];
        assert!(i.text.chars().count() <= 750, "{}", i.text.chars().count());
        assert!(i.text.starts_with("Komunikat RSO, województwo dolnośląskie. Bardzo długi"));
        assert!(!i.text.contains("Pełna treść"), "{}", i.text);
    }

    #[test]
    fn non_matching_keywords_do_not_filter_an_attack_item() {
        let mut n = alert();
        n.title = "Alarm przeciwlotniczy".into();
        let c = ctx("02", &[("keywords", "xyz")]);
        assert_eq!(run(vec![n], Category("atak-z-powietrza"), &c).items.len(), 1);
    }

    #[test]
    fn keywords_match_cleaned_text() {
        let mut n = alert();
        n.title = "Woda".into();
        n.updated_at = None;
        n.content = "Awaria &amp; <b>wodociągu</b> w Pieńsku".into();
        let mut c = ctx("02", &[("keywords", "awaria & wodociągu")]);
        c.tracked_keys.insert(n.id.to_string());
        assert_eq!(run(vec![n], ogolne(), &c).items.len(), 1);
    }

    #[test]
    fn keywords_filter_everything_but_alarms() {
        let all = news(OGOLNE);
        let mut c = ctx("02", &[("keywords", " pieńsk , ,")]);
        for n in &all {
            c.tracked_keys.insert(n.id.to_string());
        }
        // Bez słów: Alert RCB i woda w Lądku.
        let mut none = ctx("02", &[]);
        none.tracked_keys = c.tracked_keys.clone();
        let base = run(all.clone(), ogolne(), &none);
        assert!(keys(&base).contains(&"23524661") && keys(&base).contains(&"23521833"));
        // „PIEŃSK” w treści Alertu, nie w wodzie z Lądka.
        let hit = run(all.clone(), ogolne(), &c);
        assert_eq!(keys(&hit), ["23521833"]);
        let mut d = ctx("02", &[("keywords", "Lądek-Zdrój")]);
        d.tracked_keys = c.tracked_keys.clone();
        let both = run(all.clone(), ogolne(), &d);
        assert!(keys(&both).contains(&"23524661") && keys(&both).contains(&"23521833"));
        let mut x = ctx("02", &[("keywords", "xyz")]);
        x.tracked_keys = c.tracked_keys.clone();
        assert_eq!(keys(&run(all, ogolne(), &x)), ["23521833"]);
    }

    #[test]
    fn only_items_of_the_area_voivodeships_are_kept() {
        let mut v: Value = serde_json::from_slice(OGOLNE).unwrap();
        let list = v["newses"].as_array_mut().unwrap();
        list[0]["provinces"] = json!({"9": {"id": "9", "name": "łódzkie", "city": "", "slug_name": "lodzkie"}});
        list[0]["updated_at"] = "2026-11-22 15:00:00".into();
        let all = news(&serde_json::to_vec(&v).unwrap());
        let out = run(all.clone(), ogolne(), &ctx("10\n1061 Łódź", &[]));
        assert_eq!(keys(&out), ["23501558"]);
        assert!(out.items[0].text.starts_with("Komunikat RSO, województwo łódzkie. "));
        assert!(run(all, ogolne(), &ctx("14", &[])).items.is_empty());
    }

    #[test]
    fn the_first_matching_area_voivodeship_is_spoken() {
        let mut n = alert();
        n.province_slugs = vec!["lodzkie".into(), "dolnoslaskie".into()];
        let out = run(vec![n], ogolne(), &ctx("02\n10", &[]));
        assert!(out.items[0].text.starts_with("Alert RCB, województwo dolnośląskie."));
        let mut n = alert();
        n.province_slugs = vec!["lodzkie".into(), "dolnoslaskie".into()];
        let out = run(vec![n], ogolne(), &ctx("10\n02", &[]));
        assert!(out.items[0].text.starts_with("Alert RCB, województwo łódzkie."));
    }

    #[test]
    fn long_content_is_cut_at_a_sentence() {
        let mut n = alert();
        n.title = "Komunikat".into();
        n.updated_at = None;
        n.content = "Zdanie numer jeden jest tutaj. ".repeat(70);
        assert!(n.content.chars().count() >= 2000);
        let i = &run(vec![n], ogolne(), &tracked("02")).items[0];
        assert!(i.text.chars().count() <= 700, "{}", i.text.chars().count());
        assert!(i.text.ends_with("tutaj. Pełna treść na stronie RSO."), "{}", i.text);
    }

    #[test]
    fn the_fingerprint_ignores_validity() {
        let a = alert();
        let fp = |n: News| run(vec![n], ogolne(), &ctx("02", &[])).items[0].fingerprint.clone();
        let base = fp(a.clone());
        assert!(base.starts_with("23521833|Alert RCB|"), "{base}");
        let hex = base.rsplit('|').next().unwrap();
        assert_eq!(hex.len(), 16);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        let mut b = a.clone();
        b.valid_until = None;
        assert_eq!(fp(b), base);
        let mut c = a;
        c.content.push_str(" Nowe zdanie.");
        assert_ne!(fp(c), base);
    }

    #[test]
    fn a_missing_valid_to_is_open_ended() {
        let mut v: Value = serde_json::from_slice(OGOLNE).unwrap();
        for n in v["newses"].as_array_mut().unwrap() {
            n["valid_to"] = Value::Null;
        }
        let all = news(&serde_json::to_vec(&v).unwrap());
        let out = run(all, ogolne(), &ctx("02", &[]));
        assert!(out.complete);
        assert_eq!(out.items[0].valid_until, None);
    }

    #[test]
    fn duplicate_ids_across_categories_keep_the_first() {
        let mut first = alert();
        first.title = "Alert RCB".into();
        let mut second = alert();
        second.content = "Inna treść.".into();
        let out = to_output(
            vec![(ogolne(), vec![first]), (Category("atak-z-powietrza"), vec![second])],
            0,
            0,
            &ctx("02", &[]),
        );
        assert_eq!(out.items.len(), 1);
        assert!(out.items[0].text.contains("Na wniosek wojewody"));
    }

    #[test]
    fn the_summary_is_cut_to_80_chars() {
        let mut n = alert();
        n.title = "Ł".repeat(120);
        n.updated_at = None;
        let out = run(vec![n], ogolne(), &tracked("02"));
        assert_eq!(out.items[0].summary.chars().count(), 80);
    }

    #[test]
    fn failures_and_skips_make_the_output_incomplete() {
        let c = ctx("02", &[]);
        assert!(!to_output(vec![], 1, 0, &c).complete);
        assert!(!to_output(vec![], 0, 1, &c).complete);
        assert!(to_output(vec![], 0, 0, &c).complete);
    }

    #[test]
    fn the_scope_carries_the_filters() {
        let src = Rso::new("http://127.0.0.1:1").unwrap();
        let c = ctx("10\n1061 Łódź", &[("categories", "atak-z-powietrza,ogolne"), ("keywords", " woda ")]);
        assert_eq!(src.scope(&c), format!("{}|cat=ogolne,atak-z-powietrza|kw=woda", c.area_text));
        let a = ctx("02", &[("keywords", "a,b")]);
        let b = ctx("02", &[("keywords", " a , b ,")]);
        assert_eq!(src.scope(&a), src.scope(&b));
        // Brak, śmieci i nieznane slugi: domyślne kategorie.
        for raw in [None, Some("zzz"), Some("stany-wod"), Some("")] {
            let settings: Vec<(&str, &str)> = raw.map(|r| ("categories", r)).into_iter().collect();
            let c = ctx("02", &settings);
            assert_eq!(src.scope(&c), format!("{}|cat=ogolne,atak-z-powietrza|kw=", c.area_text));
        }
        let c = ctx("02", &[("categories", "stany-wod,hydrologiczne")]);
        assert!(src.scope(&c).contains("|cat=hydrologiczne|"));
    }

    mod http_tests {
        use super::*;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn json_body(body: &[u8]) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_raw(body.to_vec(), "application/json")
        }

        async fn mount(server: &MockServer, p: &str, resp: ResponseTemplate, expect: u64) {
            Mock::given(method("GET"))
                .and(path(p))
                .and(query_param("_format", "json"))
                .respond_with(resp)
                .expect(expect)
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn one_request_per_voivodeship_and_category() {
            let server = MockServer::start().await;
            mount(&server, "/lodzkie/ogolne/0", json_body(PUSTO), 1).await;
            mount(&server, "/lodzkie/atak-z-powietrza/0", json_body(ATAK_PUSTO), 1).await;
            let src = Rso::new(&server.uri()).unwrap();
            let out = src.fetch(&ctx("10\n1061 Łódź", &[])).await.unwrap();
            assert!(out.items.is_empty());
            assert!(out.complete);
            assert_eq!(src.id(), "rso");
            assert_eq!(src.default_interval(), Duration::from_secs(180));
            assert_eq!(src.needs(), Needs::Area);
        }

        #[tokio::test]
        async fn the_attack_feed_is_fetched_even_when_unchecked() {
            let server = MockServer::start().await;
            mount(&server, "/lodzkie/ogolne/0", json_body(PUSTO), 1).await;
            mount(&server, "/lodzkie/atak-z-powietrza/0", json_body(ATAK_PUSTO), 1).await;
            let src = Rso::new(&server.uri()).unwrap();
            let out = src.fetch(&ctx("10", &[("categories", "ogolne")])).await.unwrap();
            assert!(out.complete);
            let help = DESCRIPTOR.settings[1].help;
            assert!(help.contains("Atak z powietrza jest pobierany zawsze"), "{help}");
        }

        #[tokio::test]
        async fn an_alarm_without_provinces_from_a_feed_is_kept() {
            let server = MockServer::start().await;
            let mut v: Value = serde_json::from_slice(OGOLNE).unwrap();
            for n in v["newses"].as_array_mut().unwrap() {
                n["provinces"] = json!([]);
            }
            mount(&server, "/lodzkie/ogolne/0", json_body(&serde_json::to_vec(&v).unwrap()), 1).await;
            mount(&server, "/lodzkie/atak-z-powietrza/0", json_body(ATAK_PUSTO), 1).await;
            let src = Rso::new(&server.uri()).unwrap();
            let out = src.fetch(&ctx("10", &[])).await.unwrap();
            assert_eq!(keys(&out), ["23521833"]);
            assert!(out.items[0].text.starts_with("Alert RCB, województwo łódzkie."));
        }

        #[tokio::test]
        async fn two_voivodeships_are_fetched_separately() {
            let server = MockServer::start().await;
            mount(&server, "/lodzkie/ogolne/0", json_body(PUSTO), 1).await;
            mount(&server, "/dolnoslaskie/ogolne/0", json_body(OGOLNE), 1).await;
            mount(&server, "/lodzkie/atak-z-powietrza/0", json_body(ATAK_PUSTO), 1).await;
            mount(&server, "/dolnoslaskie/atak-z-powietrza/0", json_body(ATAK_PUSTO), 1).await;
            let src = Rso::new(&server.uri()).unwrap();
            let c = ctx("10\n02", &[("categories", "ogolne")]);
            let out = src.fetch(&c).await.unwrap();
            assert_eq!(keys(&out), ["23521833"]);
        }

        #[tokio::test]
        async fn one_failing_category_leaves_the_other() {
            let server = MockServer::start().await;
            mount(&server, "/dolnoslaskie/ogolne/0", json_body(OGOLNE), 1).await;
            mount(&server, "/dolnoslaskie/atak-z-powietrza/0", ResponseTemplate::new(503), 1).await;
            let src = Rso::new(&server.uri()).unwrap();
            let out = src.fetch(&ctx("02", &[])).await.unwrap();
            assert!(!out.complete);
            assert_eq!(keys(&out), ["23521833"]);
        }

        #[tokio::test]
        async fn all_requests_failing_is_the_first_error() {
            let server = MockServer::start().await;
            mount(&server, "/dolnoslaskie/ogolne/0", ResponseTemplate::new(503), 1).await;
            mount(&server, "/dolnoslaskie/atak-z-powietrza/0", ResponseTemplate::new(503), 1).await;
            let src = Rso::new(&server.uri()).unwrap();
            let err = src.fetch(&ctx("02", &[])).await.err().unwrap();
            assert!(matches!(err, SourceError::Status(503)), "{err:?}");
        }

        #[tokio::test]
        async fn an_unreadable_response_counts_as_a_failure() {
            let server = MockServer::start().await;
            mount(&server, "/dolnoslaskie/ogolne/0", json_body(OGOLNE), 1).await;
            mount(&server, "/dolnoslaskie/atak-z-powietrza/0", json_body(b"<html>"), 1).await;
            let src = Rso::new(&server.uri()).unwrap();
            let out = src.fetch(&ctx("02", &[])).await.unwrap();
            assert!(!out.complete);
        }
    }
}
