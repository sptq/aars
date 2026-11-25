//! Tekst części „Jakość powietrza”: ocena słowna według European AQI,
//! pogorszenie w ciągu doby i pyłki. Atrybucja (Open-Meteo, CAMS) jest
//! w panelu i README, nie na antenie.

use chrono::{DateTime, Datelike, TimeDelta, Timelike, Utc};
use chrono_tz::Europe::Warsaw;

use super::parse::{AirHour, AirQuality, Pollen};
use crate::speech::round_int;

/// Klasy European AQI (EEA, wersja 2024) z polskimi nazwami jak w indeksie GIOŚ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 0–20 („good”)
    BardzoDobra,
    /// 20–40 („fair”)
    Dobra,
    /// 40–60 („moderate”)
    Umiarkowana,
    /// 60–80 („poor”)
    Dostateczna,
    /// 80–100 („very poor”)
    Zla,
    /// ponad 100 („extremely poor”)
    BardzoZla,
}

impl Level {
    /// Granica należy do lepszej klasy (20 → bardzo dobra).
    pub fn from_aqi(aqi: f64) -> Level {
        match aqi {
            x if x <= 20.0 => Self::BardzoDobra,
            x if x <= 40.0 => Self::Dobra,
            x if x <= 60.0 => Self::Umiarkowana,
            x if x <= 80.0 => Self::Dostateczna,
            x if x <= 100.0 => Self::Zla,
            _ => Self::BardzoZla,
        }
    }

    pub fn words(self) -> &'static str {
        match self {
            Self::BardzoDobra => "bardzo dobra",
            Self::Dobra => "dobra",
            Self::Umiarkowana => "umiarkowana",
            Self::Dostateczna => "dostateczna",
            Self::Zla => "zła",
            Self::BardzoZla => "bardzo zła",
        }
    }
}

/// Od tej klasy mówimy stężenie pyłu (niżej sama ocena słowna). Właściciel
/// chce liczb już przy umiarkowanej: długość komunikatu nie jest problemem.
const PM_FROM: Level = Level::Umiarkowana;

/// Klasa pyłu według progów godzinowych EAQI (µg/m³: górne granice klas
/// od „bardzo dobra” do „zła”).
fn pm_level(value: f64, bounds: [f64; 5]) -> Level {
    const LEVELS: [Level; 5] = [
        Level::BardzoDobra,
        Level::Dobra,
        Level::Umiarkowana,
        Level::Dostateczna,
        Level::Zla,
    ];
    bounds
        .iter()
        .zip(LEVELS)
        .find(|(b, _)| value <= **b)
        .map_or(Level::BardzoZla, |(_, l)| l)
}

const PM2_5_BOUNDS: [f64; 5] = [5.0, 15.0, 50.0, 90.0, 140.0];
const PM10_BOUNDS: [f64; 5] = [15.0, 45.0, 120.0, 195.0, 270.0];

/// Pył, który bardziej psuje jakość, i jego najwyższe stężenie w `hours`;
/// `None`, gdy żaden nie sięga `PM_FROM` (jakość psuje np. ozon).
fn worst_pm<'a>(hours: impl Iterator<Item = &'a AirHour> + Clone) -> Option<(&'static str, f64)> {
    let max = |f: fn(&AirHour) -> Option<f64>| hours.clone().filter_map(f).reduce(f64::max);
    let candidates = [
        max(|h| h.pm2_5).map(|v| ("PM2,5", v, pm_level(v, PM2_5_BOUNDS))),
        max(|h| h.pm10).map(|v| ("PM10", v, pm_level(v, PM10_BOUNDS))),
    ];
    candidates
        .into_iter()
        .flatten()
        .filter(|(_, _, level)| *level >= PM_FROM)
        // Równe klasy: PM2,5 (pierwszy), bo groźniejszy.
        .reduce(|a, b| if b.2 > a.2 { b } else { a })
        .map(|(name, v, _)| (name, v))
}

/// „mikrogram”, „mikrogramy”, „mikrogramów” po liczbie w mianowniku.
fn micrograms(n: i64) -> &'static str {
    let (last, tens) = (n % 10, n % 100);
    if n == 1 {
        "mikrogram"
    } else if (2..=4).contains(&last) && !(12..=14).contains(&tens) {
        "mikrogramy"
    } else {
        "mikrogramów"
    }
}

/// Stan bieżący: `current`, gdy świeży (do 90 min), inaczej godzina z
/// prognozy obejmująca `now`.
fn now_hour(aq: &AirQuality, now: DateTime<Utc>) -> Option<&AirHour> {
    let fresh = aq
        .current
        .as_ref()
        .filter(|c| c.aqi.is_some() && (now - c.at).abs() <= TimeDelta::minutes(90));
    fresh.or_else(|| {
        aq.hours
            .iter()
            .rfind(|h| h.aqi.is_some() && h.at <= now && now - h.at < TimeDelta::hours(1))
    })
}

/// Pora godziny `at` względem `now` (czas Warszawy): „Dziś wieczorem”,
/// „W nocy”, „Jutro rano”…
fn when(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let (at, now) = (at.with_timezone(&Warsaw), now.with_timezone(&Warsaw));
    let days = (at.date_naive() - now.date_naive()).num_days();
    let hour = at.hour();
    let part = match hour {
        5..=9 => "rano",
        10..=12 => "przed południem",
        13..=17 => "po południu",
        18..=21 => "wieczorem",
        _ => "w nocy",
    };
    // Noc liczymy od 22:00 do 5:00, więc 2:00 po północy to jeszcze „ta noc”.
    let night_of = if hour < 5 { days - 1 } else { days };
    match (part, night_of) {
        ("w nocy", d) if d <= 0 => "W nocy".to_owned(),
        ("w nocy", 1) => "Jutro w nocy".to_owned(),
        (part, 0) => format!("Dziś {part}"),
        (part, 1) => format!("Jutro {part}"),
        (part, _) => format!("{} {part}", weekday(at.weekday())),
    }
}

fn weekday(d: chrono::Weekday) -> &'static str {
    use chrono::Weekday::*;
    match d {
        Mon => "W poniedziałek",
        Tue => "We wtorek",
        Wed => "W środę",
        Thu => "W czwartek",
        Fri => "W piątek",
        Sat => "W sobotę",
        Sun => "W niedzielę",
    }
}

/// Okno prognozy pogorszenia.
const FORECAST_WINDOW: TimeDelta = TimeDelta::hours(24);
/// Okno pyłków (bieżąca godzina i najbliższe).
const POLLEN_WINDOW: TimeDelta = TimeDelta::hours(12);

fn current_sentence(hour: &AirHour, level: Level) -> String {
    let mut s = format!("Jakość powietrza {}", level.words());
    if level >= PM_FROM
        && let Some((name, v)) = worst_pm(std::iter::once(hour))
    {
        let n = round_int(v);
        s.push_str(&format!(
            ", pył zawieszony {name}: {n} {} na metr sześcienny",
            micrograms(n)
        ));
    }
    s.push('.');
    s
}

/// Najgorsza klasa w ciągu doby, gdy gorsza niż teraz: „Jutro rano
/// dostateczna, pył zawieszony PM2,5 do 68 mikrogramów na metr sześcienny.”
fn forecast_sentence(aq: &AirQuality, now: DateTime<Utc>, level_now: Level) -> Option<String> {
    let ahead: Vec<(&AirHour, Level)> = aq
        .hours
        .iter()
        .filter(|h| h.at > now && h.at - now <= FORECAST_WINDOW)
        .filter_map(|h| Some((h, Level::from_aqi(h.aqi?))))
        .collect();
    let worst = ahead.iter().map(|(_, l)| *l).max()?;
    if worst <= level_now {
        return None;
    }
    let first = ahead.iter().find(|(_, l)| *l == worst)?.0;
    let mut s = format!("{} {}", when(first.at, now), worst.words());
    if worst >= PM_FROM {
        let at_worst = ahead.iter().filter(|(_, l)| *l == worst).map(|(h, _)| *h);
        if let Some((name, v)) = worst_pm(at_worst) {
            let n = round_int(v);
            let unit = if n == 1 { "mikrograma" } else { "mikrogramów" };
            s.push_str(&format!(
                ", pył zawieszony {name} do {n} {unit} na metr sześcienny"
            ));
        }
    }
    s.push('.');
    Some(s)
}

/// „brzozy”, „brzozy i olchy”, „brzozy, olchy i traw”.
fn join_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} i {last}", rest.join(", ")),
    }
}

/// „Wysokie stężenie pyłków brzozy. Średnie stężenie pyłków traw.” —
/// najwyższe stężenie w bieżącej godzinie i 12 następnych; niższe niż
/// średnie nie jest mówione.
fn pollen_sentences(aq: &AirQuality, now: DateTime<Utc>) -> Vec<String> {
    let window = |h: &&AirHour| h.at + TimeDelta::hours(1) > now && h.at - now <= POLLEN_WINDOW;
    let (mut high, mut medium) = (Vec::new(), Vec::new());
    for p in Pollen::ALL {
        let Some(max) = aq
            .hours
            .iter()
            .filter(window)
            .filter_map(|h| h.pollen(p))
            .reduce(f64::max)
        else {
            continue;
        };
        let (mid, top) = p.thresholds();
        if max >= top {
            high.push(p.genitive());
        } else if max >= mid {
            medium.push(p.genitive());
        }
    }
    [("Wysokie", high), ("Średnie", medium)]
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(word, names)| format!("{word} stężenie pyłków {}.", join_names(&names)))
        .collect()
}

/// Cały tekst części; `None` bez bieżącej oceny jakości.
pub fn text(aq: &AirQuality, now: DateTime<Utc>, pollen: bool) -> Option<String> {
    let hour = now_hour(aq, now)?;
    let level = Level::from_aqi(hour.aqi?);
    let mut parts = vec![current_sentence(hour, level)];
    parts.extend(forecast_sentence(aq, now, level));
    if pollen {
        parts.extend(pollen_sentences(aq, now));
    }
    Some(parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::powietrze::parse::parse;
    use crate::speech::assert_speakable;

    const FIX: &[u8] =
        include_bytes!("../../tests/fixtures/powietrze/open-meteo-aq-rzeszow-2026-11-25.json");

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn hour(t: &str, aqi: Option<f64>, pm2_5: f64, pm10: f64) -> AirHour {
        AirHour {
            at: at(t),
            aqi,
            pm2_5: Some(pm2_5),
            pm10: Some(pm10),
            pollen: [None; 6],
        }
    }

    /// Godziny od `start` co godzinę z podanymi AQI (PM2,5 = AQI, PM10 = 10).
    fn series(start: &str, aqi: &[f64]) -> AirQuality {
        let start = at(start);
        AirQuality {
            current: None,
            hours: aqi
                .iter()
                .enumerate()
                .map(|(i, a)| AirHour {
                    at: start + TimeDelta::hours(i as i64),
                    aqi: Some(*a),
                    pm2_5: Some(*a),
                    pm10: Some(10.0),
                    pollen: [None; 6],
                })
                .collect(),
        }
    }

    #[test]
    fn the_scale_follows_the_european_aqi_bands() {
        let cases = [
            (0.0, "bardzo dobra"),
            (20.0, "bardzo dobra"),
            (20.5, "dobra"),
            (40.0, "dobra"),
            (59.0, "umiarkowana"),
            (61.0, "dostateczna"),
            (80.0, "dostateczna"),
            (99.0, "zła"),
            (150.0, "bardzo zła"),
        ];
        for (aqi, words) in cases {
            assert_eq!(Level::from_aqi(aqi).words(), words, "{aqi}");
        }
    }

    #[test]
    fn spoken_texts_for_typical_cases() {
        let night = "2026-11-25T21:00:00Z"; // 22:00 w Warszawie
        let cases: Vec<(&str, AirQuality, &str, bool, Option<&str>)> = vec![
            (
                "dobra bez prognozy",
                series("2026-11-25T08:00:00Z", &[30.0, 25.0, 35.0, 22.0]),
                "2026-11-25T08:10:00Z",
                true,
                Some("Jakość powietrza dobra."),
            ),
            (
                "pogorszenie jutro rano",
                {
                    let mut aq = series(night, &[30.0; 12]);
                    // 7:00 i 8:00 w Warszawie: umiarkowana.
                    aq.hours[9].aqi = Some(45.0);
                    aq.hours[10].aqi = Some(55.0);
                    aq
                },
                night,
                true,
                Some("Jakość powietrza dobra. Jutro rano umiarkowana, pył zawieszony PM2,5 do 30 mikrogramów na metr sześcienny."),
            ),
            (
                "pogorszenie do dostatecznej z pyłem",
                {
                    let mut aq = series(night, &[30.0; 12]);
                    aq.hours[9].aqi = Some(65.0);
                    aq.hours[9].pm2_5 = Some(58.4);
                    aq.hours[10].aqi = Some(72.0);
                    aq.hours[10].pm2_5 = Some(66.6);
                    aq
                },
                night,
                true,
                Some(
                    "Jakość powietrza dobra. Jutro rano dostateczna, pył zawieszony PM2,5 do 67 mikrogramów na metr sześcienny.",
                ),
            ),
            (
                "zła teraz z PM10",
                AirQuality {
                    current: Some(hour("2026-11-25T08:00:00Z", Some(85.0), 40.0, 202.0)),
                    hours: Vec::new(),
                },
                "2026-11-25T08:20:00Z",
                true,
                Some(
                    "Jakość powietrza zła, pył zawieszony PM10: 202 mikrogramy na metr sześcienny.",
                ),
            ),
            (
                "dostateczna przez ozon: bez liczb pyłu",
                AirQuality {
                    current: Some(hour("2026-11-25T08:00:00Z", Some(70.0), 8.0, 12.0)),
                    hours: Vec::new(),
                },
                "2026-11-25T08:20:00Z",
                true,
                Some("Jakość powietrza dostateczna."),
            ),
            (
                "brak danych (null w JSON)",
                AirQuality {
                    current: Some(hour("2026-11-25T08:00:00Z", None, 8.0, 12.0)),
                    hours: vec![hour("2026-11-25T08:00:00Z", None, 8.0, 12.0)],
                },
                "2026-11-25T08:20:00Z",
                true,
                None,
            ),
            (
                "stare dane",
                series("2026-11-24T08:00:00Z", &[30.0, 30.0]),
                "2026-11-25T08:20:00Z",
                true,
                None,
            ),
        ];
        for (name, aq, now, pollen, want) in cases {
            let got = text(&aq, at(now), pollen);
            assert_eq!(got.as_deref(), want, "{name}");
            if let Some(t) = &got {
                assert_speakable(&[t]);
            }
        }
    }

    #[test]
    fn pollen_is_spoken_only_when_significant_and_wanted() {
        let mut aq = series("2026-04-10T08:00:00Z", &[15.0; 6]);
        let set = |aq: &mut AirQuality, p: Pollen, i: usize, v: f64| {
            let k = Pollen::ALL.iter().position(|x| *x == p).unwrap();
            aq.hours[i].pollen[k] = Some(v);
        };
        set(&mut aq, Pollen::Birch, 2, 140.0);
        set(&mut aq, Pollen::Alder, 1, 120.0);
        set(&mut aq, Pollen::Grass, 0, 25.0);
        set(&mut aq, Pollen::Mugwort, 0, 3.0);
        set(&mut aq, Pollen::Olive, 0, 0.0);
        let now = at("2026-04-10T08:15:00Z");
        assert_eq!(
            text(&aq, now, true).as_deref(),
            Some(
                "Jakość powietrza bardzo dobra. Wysokie stężenie pyłków olchy i brzozy. Średnie stężenie pyłków traw."
            )
        );
        assert_eq!(
            text(&aq, now, false).as_deref(),
            Some("Jakość powietrza bardzo dobra.")
        );
        assert_speakable(&[&text(&aq, now, true).unwrap()]);
    }

    #[test]
    fn the_real_fixture_warns_of_the_evening() {
        let aq = parse(FIX).unwrap();
        let t = text(&aq, at("2026-11-25T08:24:00Z"), true).unwrap();
        assert_eq!(
            t,
            "Jakość powietrza umiarkowana, pył zawieszony PM2,5: 19 mikrogramów na metr sześcienny. Dziś wieczorem dostateczna, pył zawieszony PM2,5 do 68 mikrogramów na metr sześcienny."
        );
        assert_speakable(&[&t]);
        let spoken = aars_audio::text::normalize(&t);
        assert!(spoken.contains("P M dwa i pół"), "{spoken}");
        assert!(!spoken.contains('µ') && !spoken.contains('³'), "{spoken}");
    }

    #[test]
    fn times_of_day_are_named_in_warsaw_time() {
        let now = at("2026-11-25T08:00:00Z"); // 9:00
        let cases = [
            ("2026-11-25T10:00:00Z", "Dziś przed południem"),
            ("2026-11-25T14:00:00Z", "Dziś po południu"),
            ("2026-11-25T18:00:00Z", "Dziś wieczorem"),
            ("2026-11-25T22:00:00Z", "W nocy"),
            ("2026-11-26T01:00:00Z", "W nocy"),
            ("2026-11-26T06:00:00Z", "Jutro rano"),
            ("2026-11-26T21:30:00Z", "Jutro w nocy"),
            ("2026-11-27T01:00:00Z", "Jutro w nocy"),
            ("2026-11-27T08:00:00Z", "W piątek rano"),
        ];
        for (t, want) in cases {
            assert_eq!(when(at(t), now), want, "{t}");
        }
    }

    #[test]
    fn microgram_forms_agree_with_the_number() {
        for (n, want) in [
            (1, "mikrogram"),
            (2, "mikrogramy"),
            (5, "mikrogramów"),
            (12, "mikrogramów"),
            (22, "mikrogramy"),
            (64, "mikrogramy"),
            (111, "mikrogramów"),
        ] {
            assert_eq!(micrograms(n), want, "{n}");
        }
    }
}
