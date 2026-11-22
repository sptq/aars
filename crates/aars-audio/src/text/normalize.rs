use super::numbers::{
    PluralForm, cardinal, cardinal_genitive, day_ordinal_genitive, hour_ordinal,
    hour_ordinal_genitive, plural_form,
};

/// Nazwy miesięcy w dopełniaczu — po dniu („23 lipca"). Używa ich też `imgw_meteo::speech`.
pub const MONTHS_GENITIVE: [&str; 12] = [
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

/// Przypadek godziny wymagany przez poprzedzający przyimek.
#[derive(Clone, Copy)]
enum HourCase {
    Nominative,
    Genitive,
    Locative,
}

fn hour_case(previous_core: Option<&str>) -> HourCase {
    match previous_core.map(str::to_lowercase).as_deref() {
        Some("od" | "do" | "około") => HourCase::Genitive,
        Some("o" | "po") => HourCase::Locative,
        _ => HourCase::Nominative,
    }
}

/// Rozwinięcie „godz.” zależne od słowa przed nim: słowo „godzina” we
/// właściwym przypadku, czy godzina idzie w dopełniaczu i czy to zakres
/// („w godz. 7:00 – 15:00” → „w godzinach siódma – piętnasta”).
fn godz_expansion(previous_core: Option<&str>) -> (&'static str, bool, bool) {
    match previous_core.map(str::to_lowercase).as_deref() {
        Some("od" | "do" | "około") => ("godziny", true, false),
        Some("o" | "po") => ("godzinie", true, false),
        Some("w") => ("godzinach", false, true),
        _ => ("godzina", false, false),
    }
}

/// Myślniki między godzinami zakresu.
const RANGE_DASHES: &[&str] = &["-", "–", "—"];

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Przyimki, po których liczebnik stoi w dopełniaczu („do 30 mm").
fn takes_genitive_number(previous_core: Option<&str>) -> bool {
    matches!(
        previous_core.map(str::to_lowercase).as_deref(),
        Some("od" | "do" | "około" | "ponad" | "powyżej" | "poniżej" | "z")
    )
}

/// Liczba w dopełniaczu, gdy przyimek tego wymaga i da się ją zapisać
/// (całkowita, 0–999); inaczej `None` i zostaje mianownik.
fn genitive_number(value: &NumberText, previous_core: Option<&str>) -> Option<String> {
    if value.fraction.is_some() || !takes_genitive_number(previous_core) {
        return None;
    }
    let n = value.integer.parse::<i64>().ok()?;
    let spoken = cardinal_genitive(n.checked_abs()?)?;
    Some(if n < 0 { format!("minus {spoken}") } else { spoken })
}

fn is_month(core: &str) -> bool {
    let lower = core.to_lowercase();
    MONTHS_GENITIVE.contains(&lower.as_str())
}

/// Jednostka w trzech formach: po 1, po 2–4, po pozostałych liczbach.
struct Unit {
    token: &'static str,
    singular: &'static str,
    plural_nom: &'static str,
    plural_gen: &'static str,
    /// Dopełniacz liczby pojedynczej — forma po ułamku („dwa przecinek pięć metra").
    genitive_sg: &'static str,
}

/// Kolejność ma znaczenie: dłuższe tokeny muszą być sprawdzane pierwsze,
/// żeby „m/s" nie dopasowało się jako „m".
const UNITS: &[Unit] = &[
    Unit {
        token: "km/h",
        singular: "kilometr na godzinę",
        plural_nom: "kilometry na godzinę",
        plural_gen: "kilometrów na godzinę",
        genitive_sg: "kilometra na godzinę",
    },
    Unit {
        token: "m/s",
        singular: "metr na sekundę",
        plural_nom: "metry na sekundę",
        plural_gen: "metrów na sekundę",
        genitive_sg: "metra na sekundę",
    },
    Unit {
        token: "µSv/h",
        singular: "mikrosiwert na godzinę",
        plural_nom: "mikrosiwerty na godzinę",
        plural_gen: "mikrosiwertów na godzinę",
        genitive_sg: "mikrosiwerta na godzinę",
    },
    Unit {
        token: "uSv/h",
        singular: "mikrosiwert na godzinę",
        plural_nom: "mikrosiwerty na godzinę",
        plural_gen: "mikrosiwertów na godzinę",
        genitive_sg: "mikrosiwerta na godzinę",
    },
    Unit {
        token: "hPa",
        singular: "hektopaskal",
        plural_nom: "hektopaskale",
        plural_gen: "hektopaskali",
        genitive_sg: "hektopaskala",
    },
    Unit {
        token: "°C",
        singular: "stopień Celsjusza",
        plural_nom: "stopnie Celsjusza",
        plural_gen: "stopni Celsjusza",
        genitive_sg: "stopnia Celsjusza",
    },
    Unit {
        token: "mm",
        singular: "milimetr",
        plural_nom: "milimetry",
        plural_gen: "milimetrów",
        genitive_sg: "milimetra",
    },
    Unit {
        token: "cm",
        singular: "centymetr",
        plural_nom: "centymetry",
        plural_gen: "centymetrów",
        genitive_sg: "centymetra",
    },
    Unit {
        token: "km",
        singular: "kilometr",
        plural_nom: "kilometry",
        plural_gen: "kilometrów",
        genitive_sg: "kilometra",
    },
    Unit {
        token: "%",
        singular: "procent",
        plural_nom: "procent",
        plural_gen: "procent",
        genitive_sg: "procent",
    },
    Unit {
        token: "m",
        singular: "metr",
        plural_nom: "metry",
        plural_gen: "metrów",
        genitive_sg: "metra",
    },
];

const ABBREVIATIONS: &[(&str, &str)] = &[
    ("NE", "północno-wschodni"),
    ("NW", "północno-zachodni"),
    ("SE", "południowo-wschodni"),
    ("SW", "południowo-zachodni"),
    ("N", "północny"),
    ("S", "południowy"),
    ("E", "wschodni"),
    ("W", "zachodni"),
    ("IMGW", "I M G W"),
    ("PAA", "P A A"),
    ("KSE", "K S E"),
    ("RCB", "R C B"),
    ("RSO", "R S O"),
    ("NOAA", "N O A A"),
    ("SWPC", "S W P C"),
    ("EMSC", "E M S C"),
    ("GIOŚ", "G I O Ś"),
    ("KF", "K F"),
    ("PM10", "P M dziesięć"),
    ("PM2,5", "P M dwa i pół"),
    ("PM2.5", "P M dwa i pół"),
    ("MUF", "M U F"),
    ("KC2G", "K C dwa G"),
];

/// Interpunkcja, która może poprzedzać słowo.
const LEADING_PUNCT: &[char] = &['(', '[', '"', '„', '“', '\''];

/// Interpunkcja, która może zamykać słowo. Zostaje w wyniku, bo przecinek i
/// kropka kształtują intonację syntezatora.
const TRAILING_PUNCT: &[char] = &[',', '.', ';', ':', '!', '?', ')', ']', '"', '”', '…'];

/// Rozkłada token na interpunkcję wiodącą, rdzeń i interpunkcję końcową.
///
/// Bez tego „22 m/s," nie dopasowałoby się do jednostki, a w biuletynie
/// każdy człon kończy się przecinkiem.
fn split_punct(token: &str) -> (&str, &str, &str) {
    let after_lead = token.trim_start_matches(|c| LEADING_PUNCT.contains(&c));
    let lead = &token[..token.len() - after_lead.len()];
    let core = after_lead.trim_end_matches(|c| TRAILING_PUNCT.contains(&c));
    let trail = &after_lead[core.len()..];
    (lead, core, trail)
}

fn push_word(out: &mut Vec<String>, lead: &str, word: &str, trail: &str) {
    out.push(format!("{lead}{word}{trail}"));
}

/// Zamienia tekst w formę nadającą się do syntezy: liczby i jednostki na
/// słowa, godziny na mowę, skróty rozwinięte, białe znaki zwinięte.
///
/// Wejście, którego nie rozumie, zostawia w spokoju — lepiej nadać surowy
/// token niż zgubić treść ostrzeżenia.
pub fn normalize(input: &str) -> String {
    let raw: Vec<&str> = input.split_whitespace().collect();
    let parts: Vec<(&str, &str, &str)> = raw.iter().map(|token| split_punct(token)).collect();

    let mut out: Vec<String> = Vec::with_capacity(parts.len());
    let mut idx = 0usize;

    while idx < parts.len() {
        let (lead, core, trail) = parts[idx];

        let previous_core = idx.checked_sub(1).map(|p| parts[p].1);

        // „godz. 8:30”: skrót rozwinięty w przypadku, którego żąda słowo
        // przed nim, a godzina w tym samym przypadku, bez drugiego „godzina”.
        if core.eq_ignore_ascii_case("godz")
            && trail == "."
            && let Some(&(next_lead, next_core, next_trail)) = parts.get(idx + 1)
            && next_lead.is_empty()
            && let Some((hour, minute)) = clock_time(next_core)
        {
            let (word, genitive, range) = godz_expansion(previous_core);
            let word = if core.starts_with('G') { capitalized(word) } else { word.to_owned() };
            push_word(&mut out, lead, &word, "");
            push_word(&mut out, "", &bare_time(hour, minute, genitive), next_trail);
            idx += 2;
            if range
                && next_trail.is_empty()
                && let Some(dash) = raw.get(idx)
                && RANGE_DASHES.contains(dash)
                && let Some(&(end_lead, end_core, end_trail)) = parts.get(idx + 1)
                && end_lead.is_empty()
                && let Some((hour, minute)) = clock_time(end_core)
            {
                out.push((*dash).to_owned());
                push_word(&mut out, "", &bare_time(hour, minute, false), end_trail);
                idx += 2;
            }
            continue;
        }

        if let Some(spoken) = spoken_time(core, hour_case(previous_core)) {
            push_word(&mut out, lead, &spoken, trail);
            idx += 1;
            continue;
        }

        // „do minus 5": przyimek stoi przed słowem „minus".
        let genitive_prev = match previous_core {
            Some(p) if p.eq_ignore_ascii_case("minus") => {
                idx.checked_sub(2).map(|p| parts[p].1)
            }
            other => other,
        };
        if let Some((value, rest)) = number_prefix(core) {
            // Liczba + jednostka jako dwa tokeny: „12 m/s,"
            if rest.is_empty()
                && let Some(&(next_lead, next_core, next_trail)) = parts.get(idx + 1)
                && next_lead.is_empty()
                && let Some(unit) = match_unit(next_core)
                && unit.len() == next_core.len()
            {
                let genitive = genitive_number(&value, genitive_prev);
                push_word(
                    &mut out,
                    lead,
                    &genitive.clone().unwrap_or_else(|| spoken_number(&value)),
                    "",
                );
                push_word(
                    &mut out,
                    "",
                    unit_form(next_core, &value, genitive.is_some()),
                    next_trail,
                );
                idx += 2;
                continue;
            }

            // Liczba zlepiona z jednostką jako jeden token: „12m/s,"
            if !rest.is_empty()
                && let Some(unit) = match_unit(rest)
                && unit.len() == rest.len()
            {
                let genitive = genitive_number(&value, genitive_prev);
                push_word(
                    &mut out,
                    lead,
                    &genitive.clone().unwrap_or_else(|| spoken_number(&value)),
                    "",
                );
                push_word(
                    &mut out,
                    "",
                    unit_form(rest, &value, genitive.is_some()),
                    trail,
                );
                idx += 1;
                continue;
            }

            // Dzień przed nazwą miesiąca: „23 lipca"
            if rest.is_empty()
                && value.fraction.is_none()
                && let Some(&(_, next_core, _)) = parts.get(idx + 1)
                && is_month(next_core)
                && let Ok(day) = value.integer.parse::<u8>()
                && let Some(ordinal) = day_ordinal_genitive(day)
            {
                push_word(&mut out, lead, ordinal, trail);
                idx += 1;
                continue;
            }

            // Sama liczba
            if rest.is_empty() {
                let spoken =
                    genitive_number(&value, genitive_prev).unwrap_or_else(|| spoken_number(&value));
                push_word(&mut out, lead, &spoken, trail);
                idx += 1;
                continue;
            }
        }

        if let Some((_, expanded)) = ABBREVIATIONS.iter().find(|(abbr, _)| *abbr == core) {
            push_word(&mut out, lead, expanded, trail);
            idx += 1;
            continue;
        }

        out.push(raw[idx].to_owned());
        idx += 1;
    }

    out.join(" ")
}

/// Wartość liczbowa jako tekst: część całkowita i opcjonalna dziesiętna.
struct NumberText {
    integer: String,
    fraction: Option<String>,
}

/// Odcina z początku tokenu liczbę (z opcjonalnym minusem i częścią
/// dziesiętną po przecinku lub kropce) i zwraca ją wraz z resztą tokenu.
fn number_prefix(token: &str) -> Option<(NumberText, &str)> {
    let bytes = token.as_bytes();
    let mut i = 0usize;

    if bytes.first() == Some(&b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    let integer = token[..i].to_owned();

    let mut fraction = None;
    if i < bytes.len() && (bytes[i] == b',' || bytes[i] == b'.') {
        let frac_start = i + 1;
        let mut j = frac_start;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > frac_start {
            fraction = Some(token[frac_start..j].to_owned());
            i = j;
        }
    }

    Some((NumberText { integer, fraction }, &token[i..]))
}

fn spoken_number(value: &NumberText) -> String {
    let integer = match value.integer.parse::<i64>() {
        Ok(n) => cardinal(n),
        // Liczba poza zakresem i64 — nadaj surowo, nie gub treści.
        Err(_) => value.integer.clone(),
    };
    match &value.fraction {
        None => integer,
        Some(frac) => {
            let digits: Vec<String> = frac
                .chars()
                .filter_map(|c| c.to_digit(10))
                .map(|d| cardinal(d as i64))
                .collect();
            format!("{integer} przecinek {}", digits.join(" "))
        }
    }
}

fn find_unit(text: &str) -> Option<&'static Unit> {
    UNITS.iter().find(|unit| text.starts_with(unit.token))
}

fn match_unit(text: &str) -> Option<&'static str> {
    find_unit(text).map(|unit| unit.token)
}

/// Dobiera formę jednostki. Ułamek i liczba, której nie da się sparsować,
/// biorą dopełniacz liczby pojedynczej — przy nieznanej wielkości to
/// najbezpieczniejsza forma.
///
/// Po liczebniku w dopełniaczu (`genitive`) jednostka jest w dopełniaczu:
/// „do jednego milimetra", „do dwóch milimetrów".
fn unit_form(text: &str, value: &NumberText, genitive: bool) -> &'static str {
    let unit = find_unit(text).expect("unit_form wołane tylko po match_unit");

    if value.fraction.is_some() {
        return unit.genitive_sg;
    }
    if genitive {
        return if value.integer.parse::<i64>().is_ok_and(|n| n.abs() == 1) {
            unit.genitive_sg
        } else {
            unit.plural_gen
        };
    }

    match value.integer.parse::<i64>() {
        Err(_) => unit.genitive_sg,
        Ok(n) => match plural_form(n) {
            PluralForm::Singular => unit.singular,
            PluralForm::PluralNominative => unit.plural_nom,
            PluralForm::PluralGenitive => unit.plural_gen,
        },
    }
}

/// „18:00" → „godzina osiemnasta"; „06:30" → „godzina szósta trzydzieści";
/// po „od/do/po/około" → „godziny …", po „o" → „godzinie …". „24:00" jest
/// poprawne (koniec doby), „24:30" już nie.
fn spoken_time(token: &str, case: HourCase) -> Option<String> {
    let (hour, minute) = clock_time(token)?;
    let (word, genitive) = match case {
        HourCase::Nominative => ("godzina", false),
        HourCase::Genitive => ("godziny", true),
        HourCase::Locative => ("godzinie", true),
    };
    Some(format!("{word} {}", bare_time(hour, minute, genitive)))
}

/// „H:MM”/„HH:MM” → (godzina, minuta); „24:00” jest poprawne, „24:30” nie.
fn clock_time(token: &str) -> Option<(u8, u8)> {
    let (h, m) = token.split_once(':')?;
    if h.is_empty() || m.len() != 2 {
        return None;
    }
    let hour: u8 = h.parse().ok()?;
    let minute: u8 = m.parse().ok()?;
    if hour > 24 || minute > 59 || (hour == 24 && minute != 0) {
        return None;
    }
    Some((hour, minute))
}

/// Godzina bez słowa „godzina”: „ósma trzydzieści” albo w dopełniaczu
/// (także dla miejscownika) „ósmej trzydzieści”.
fn bare_time(hour: u8, minute: u8, genitive: bool) -> String {
    let mut spoken = if genitive {
        hour_ordinal_genitive(hour).to_string()
    } else {
        hour_ordinal(hour).to_string()
    };
    if minute > 0 {
        if minute < 10 {
            spoken.push_str(&format!(" zero {}", cardinal(minute as i64)));
        } else {
            spoken.push(' ');
            spoken.push_str(&cardinal(minute as i64));
        }
    }
    spoken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        normalize(s)
    }

    #[test]
    fn godz_takes_the_case_of_the_word_before_it() {
        assert_eq!(n("od godz. 8:30"), "od godziny ósmej trzydzieści");
        assert_eq!(n("do godz. 18:00"), "do godziny osiemnastej");
        assert_eq!(n("około godz. 6:05"), "około godziny szóstej zero pięć");
        assert_eq!(n("o godz. 9:00"), "o godzinie dziewiątej");
        assert_eq!(n("po godz. 21:15"), "po godzinie dwudziestej pierwszej piętnaście");
        assert_eq!(n("godz. 9:22"), "godzina dziewiąta dwadzieścia dwa");
        assert_eq!(n("Godz. 09:22"), "Godzina dziewiąta dwadzieścia dwa");
        assert_eq!(n("w godz. 7:00 – 15:00"), "w godzinach siódma – piętnasta");
        assert_eq!(n("w godz. 8:00 - 16:00."), "w godzinach ósma - szesnasta.");
        assert_eq!(n("w godz. 7:00"), "w godzinach siódma");
        assert_eq!(n("18:00"), "godzina osiemnasta");
        // „godz.” bez godziny za nim zostaje jak jest.
        assert_eq!(
            n("czas oczekiwania 22 godz. Kolejka"),
            "czas oczekiwania dwadzieścia dwa godz. Kolejka"
        );
    }

    #[test]
    fn godz_in_rso_texts_reads_naturally() {
        // Zdania z fiksów RSO (`aars-sources/tests/fixtures/rso`).
        assert_eq!(
            n("od dnia 27 kwietnia (poniedziałek) od godz. 8:30 nastąpi całkowite zamknięcie"),
            "od dnia dwudziestego siódmego kwietnia (poniedziałek) od godziny ósmej trzydzieści nastąpi całkowite zamknięcie"
        );
        assert_eq!(
            n("od poniedziałku do piątku w godz. 7:00 – 15:00 z Kierownikiem"),
            "od poniedziałku do piątku w godzinach siódma – piętnasta z Kierownikiem"
        );
        assert_eq!(
            n("Data i godzina wydania: 25.06.2026 - godz. 09:22"),
            "Data i godzina wydania: 25.06.2026 - godzina dziewiąta dwadzieścia dwa"
        );
        assert_eq!(
            n("Ważność: od godz. 09:23 dnia 25.06.2026"),
            "Ważność: od godziny dziewiątej dwadzieścia trzy dnia 25.06.2026"
        );
    }

    #[test]
    fn muf_and_kc2g_are_spelled() {
        assert_eq!(n("MUF 10,6 megaherca."), "M U F dziesięć przecinek sześć megaherca.");
        assert_eq!(n("GIRO przez KC2G."), "GIRO przez K C dwa G.");
    }

    #[test]
    fn units_get_the_right_grammatical_form() {
        assert_eq!(n("wiatr 12 m/s"), "wiatr dwanaście metrów na sekundę");
        assert_eq!(n("wiatr 1 m/s"), "wiatr jeden metr na sekundę");
        assert_eq!(n("wiatr 3 m/s"), "wiatr trzy metry na sekundę");
        assert_eq!(
            n("temperatura 22 °C"),
            "temperatura dwadzieścia dwa stopnie Celsjusza"
        );
        assert_eq!(
            n("temperatura -5 °C"),
            "temperatura minus pięć stopni Celsjusza"
        );
        assert_eq!(
            n("ciśnienie 1013 hPa"),
            "ciśnienie tysiąc trzynaście hektopaskali"
        );
        assert_eq!(
            n("stan wody 245 cm"),
            "stan wody dwieście czterdzieści pięć centymetrów"
        );
        assert_eq!(n("opad 2 mm"), "opad dwa milimetry");
        assert_eq!(
            n("wilgotność 85 %"),
            "wilgotność osiemdziesiąt pięć procent"
        );
    }

    #[test]
    fn units_work_without_a_space_before_them() {
        assert_eq!(n("12m/s"), "dwanaście metrów na sekundę");
        assert_eq!(n("22°C"), "dwadzieścia dwa stopnie Celsjusza");
    }

    #[test]
    fn clock_times_become_spoken_hours() {
        assert_eq!(n("od 18:00"), "od godziny osiemnastej");
        assert_eq!(n("do 06:30"), "do godziny szóstej trzydzieści");
        assert_eq!(n("o 23:05"), "o godzinie dwudziestej trzeciej zero pięć");
    }

    #[test]
    fn clock_times_take_the_case_their_preposition_needs() {
        assert_eq!(n("od 18:00"), "od godziny osiemnastej");
        assert_eq!(n("do 06:30"), "do godziny szóstej trzydzieści");
        assert_eq!(n("o 23:05"), "o godzinie dwudziestej trzeciej zero pięć");
        assert_eq!(n("Od 10:00"), "Od godziny dziesiątej");
        assert_eq!(n("18:00"), "godzina osiemnasta");
        assert_eq!(n("do 24:00"), "do godziny dwudziestej czwartej");
        assert_eq!(n("24:30"), "24:30", "po 24:00 nie ma minut");
    }

    #[test]
    fn po_takes_the_locative() {
        assert_eq!(n("po 18:00"), "po godzinie osiemnastej");
        assert_eq!(n("po 7:30"), "po godzinie siódmej trzydzieści");
    }

    #[test]
    fn days_before_a_month_become_genitive_ordinals() {
        assert_eq!(n("23 lipca"), "dwudziestego trzeciego lipca");
        assert_eq!(n("1 stycznia,"), "pierwszego stycznia,");
        assert_eq!(
            n("30 grudnia od 02:00 do 10:00."),
            "trzydziestego grudnia od godziny drugiej do godziny dziesiątej."
        );
        assert_eq!(
            n("32 lipca"),
            "trzydzieści dwa lipca",
            "poza 1–31 zostaje liczebnik główny"
        );
    }

    #[test]
    fn numbers_after_od_do_take_the_genitive_with_their_unit() {
        assert_eq!(
            n("od 15 mm do 25 mm"),
            "od piętnastu milimetrów do dwudziestu pięciu milimetrów"
        );
        assert_eq!(n("do 30 mm"), "do trzydziestu milimetrów");
        assert_eq!(
            n("do 75 km/h"),
            "do siedemdziesięciu pięciu kilometrów na godzinę"
        );
        assert_eq!(
            n("do 35 km/h,"),
            "do trzydziestu pięciu kilometrów na godzinę,"
        );
        assert_eq!(n("1 mm"), "jeden milimetr");
        assert_eq!(n("do 1 mm"), "do jednego milimetra");
        assert_eq!(n("do 2 mm"), "do dwóch milimetrów");
        assert_eq!(n("około 120 m/s"), "około stu dwudziestu metrów na sekundę");
        assert_eq!(n("ponad 5 °C"), "ponad pięciu stopni Celsjusza");
        assert_eq!(n("powyżej 80 %"), "powyżej osiemdziesięciu procent");
        assert_eq!(n("poniżej 0 °C"), "poniżej zera stopni Celsjusza");
        assert_eq!(n("z 15 do 25"), "z piętnastu do dwudziestu pięciu");
        assert_eq!(n("do 30mm"), "do trzydziestu milimetrów");
        assert_eq!(
            n("do 2,5 mm"),
            "do dwa przecinek pięć milimetra",
            "ułamek bez zmian"
        );
        assert_eq!(
            n("do 1500 mm"),
            "do tysiąc pięćset milimetrów",
            "powyżej 999 mianownik"
        );
        assert_eq!(n("do 21 mm"), "do dwudziestu jeden milimetrów");
        assert_eq!(n("do 41 m"), "do czterdziestu jeden metrów");
        assert_eq!(n("do 101 mm"), "do stu jeden milimetrów");
        assert_eq!(n("do 991 mm"), "do dziewięciuset dziewięćdziesięciu jeden milimetrów");
        assert_eq!(n("do 11 mm"), "do jedenastu milimetrów");
        assert_eq!(n("do -5°C"), "do minus pięciu stopni Celsjusza");
        assert_eq!(n("do -5 °C"), "do minus pięciu stopni Celsjusza");
        assert_eq!(n("do -1 °C"), "do minus jednego stopnia Celsjusza");
        assert_eq!(n("do minus 5 °C"), "do minus pięciu stopni Celsjusza");
        assert_eq!(n("temperatura -5 °C"), "temperatura minus pięć stopni Celsjusza");
        assert_eq!(n("do 30 lipca"), "do trzydziestego lipca", "daty jak dotąd");
    }

    #[test]
    fn genitive_cardinals_cover_0_to_999() {
        use super::super::numbers::cardinal_genitive as g;
        let g = |n| g(n).unwrap();
        assert_eq!(g(0), "zera");
        assert_eq!(g(1), "jednego");
        assert_eq!(g(3), "trzech");
        assert_eq!(g(4), "czterech");
        assert_eq!(g(12), "dwunastu");
        assert_eq!(g(20), "dwudziestu");
        assert_eq!(g(22), "dwudziestu dwóch");
        assert_eq!(g(100), "stu");
        assert_eq!(g(200), "dwustu");
        assert_eq!(g(300), "trzystu");
        assert_eq!(g(400), "czterystu");
        assert_eq!(g(500), "pięciuset");
        assert_eq!(g(21), "dwudziestu jeden");
        assert_eq!(g(41), "czterdziestu jeden");
        assert_eq!(g(101), "stu jeden");
        assert_eq!(g(111), "stu jedenastu");
        assert_eq!(g(991), "dziewięciuset dziewięćdziesięciu jeden");
        assert_eq!(g(999), "dziewięciuset dziewięćdziesięciu dziewięciu");
        assert_eq!(super::super::numbers::cardinal_genitive(1000), None);
        for n in 0..=999 {
            assert!(!g(n).is_empty());
        }
    }

    #[test]
    fn bare_numbers_are_spelled_out() {
        assert_eq!(n("stopień 2"), "stopień dwa");
        assert_eq!(n("w 2026 roku"), "w dwa tysiące dwadzieścia sześć roku");
    }

    #[test]
    fn decimals_use_the_word_comma() {
        assert_eq!(n("2,5 m"), "dwa przecinek pięć metra");
        assert_eq!(n("0,3 mm"), "zero przecinek trzy milimetra");
    }

    #[test]
    fn abbreviations_are_expanded() {
        assert_eq!(n("kierunek N"), "kierunek północny");
        assert_eq!(n("kierunek SW"), "kierunek południowo-zachodni");
        assert_eq!(n("IMGW ostrzega"), "I M G W ostrzega");
    }

    #[test]
    fn source_acronyms_are_spelled_letter_by_letter() {
        assert_eq!(n("Alert RCB."), "Alert R C B.");
        assert_eq!(n("Komunikat RSO:"), "Komunikat R S O:");
        assert_eq!(n("Dane: NOAA SWPC."), "Dane: N O A A S W P C.");
        assert_eq!(n("Dane: EMSC."), "Dane: E M S C.");
        assert_eq!(n("Źródło: GIOŚ."), "Źródło: G I O Ś.");
        assert_eq!(n("pasma KF"), "pasma K F");
    }

    #[test]
    fn particulate_names_are_spoken() {
        assert_eq!(n("pył zawieszony PM10."), "pył zawieszony P M dziesięć.");
        assert_eq!(n("PM2,5"), "P M dwa i pół");
        assert_eq!(n("(PM2.5)"), "(P M dwa i pół)");
    }

    #[test]
    fn punctuation_glued_to_a_token_does_not_block_normalization() {
        // W biuletynie każdy człon kończy się przecinkiem — to norma, nie
        // przypadek skrajny.
        assert_eq!(
            n("wiatr 22 m/s, temperatura -5 °C, ciśnienie 1013 hPa."),
            "wiatr dwadzieścia dwa metry na sekundę, temperatura minus pięć stopni Celsjusza, \
             ciśnienie tysiąc trzynaście hektopaskali."
        );
        assert_eq!(
            n("od 18:00 do 06:30,"),
            "od godziny osiemnastej do godziny szóstej trzydzieści,"
        );
        assert_eq!(n("kierunek SW,"), "kierunek południowo-zachodni,");
        assert_eq!(n("stopień 2."), "stopień dwa.");
        assert_eq!(n("(12 m/s)"), "(dwanaście metrów na sekundę)");
    }

    #[test]
    fn whitespace_is_collapsed_and_text_is_left_otherwise_intact() {
        assert_eq!(
            n("  ostrzeżenie   pierwszego  stopnia \n"),
            "ostrzeżenie pierwszego stopnia"
        );
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        assert_eq!(n(""), "");
        assert_eq!(n("   "), "");
        assert_eq!(n("m/s"), "m/s", "jednostka bez liczby zostaje jak jest");
        assert_eq!(n("99:99"), "99:99", "niepoprawna godzina zostaje jak jest");
        assert_eq!(
            n("12345678901234567890 m"),
            "12345678901234567890 metra",
            "liczba poza zakresem nie wywala normalizacji"
        );
    }
}
