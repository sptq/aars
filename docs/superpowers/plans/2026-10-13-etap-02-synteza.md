# Etap 2: Synteza — normalizacja tekstu, Piper, cache

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Zamiana dowolnego polskiego tekstu w PCM gotowy do nadania: normalizacja liczb i jednostek na słowa, synteza przez Piper, cache adresowany treścią.

**Architecture:** Normalizacja tekstu to czysta funkcja bez I/O — dlatego dostaje najgęstsze testy w projekcie (spec §4.1: tu komunikat staje się zrozumiały przez QRM). `Synthesizer` jest traitem w `aars-core`, więc kolejka i sekwencer nie wiedzą, czy pod spodem jest Piper, czy fake. Cache jest adresowany hashem treści i parametrów głosu, więc podgląd w panelu i nadanie na pasmo korzystają z tego samego pliku.

**Tech Stack:** hound 3.5 (WAV), rubato 5 (resampling), sha2, Piper jako podproces.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md`

## Global Constraints

- Źródła i kolejka produkują tekst, nie audio; synteza jest w jednym miejscu (spec D8).
- Domyślny backend syntezy woła binarkę `piper` jako podproces; native `piper-rs` zostaje za feature flagiem (odstępstwo od D4, patrz `README.md` planu).
- Cache adresowany treścią, czyszczony po LRU przy przekroczeniu `storage.cache_max_bytes` (spec §4.1).
- Podgląd audio w panelu przechodzi tym samym pipeline'em co nadanie, włącznie z normalizacją (spec §8).
- Żaden test nie dotyka karty dźwiękowej, GPIO ani sieci (spec §11). Testy wymagające binarki `piper` są `#[ignore]` i uruchamiane jawnie.
- `thiserror` w bibliotekach, `tracing` wszędzie.
- TDD: test przed implementacją, commit po każdym zielonym cyklu.

---

### Task 1: Liczebniki polskie

**Files:**
- Create: `crates/aars-audio/Cargo.toml`
- Create: `crates/aars-audio/src/lib.rs`
- Create: `crates/aars-audio/src/text/mod.rs`
- Create: `crates/aars-audio/src/text/numbers.rs`
- Modify: `Cargo.toml` (dopisz `crates/aars-audio` do `members`)

**Interfaces:**
- Consumes: nic
- Produces: `aars_audio::text::numbers::{cardinal, hour_ordinal, PluralForm, plural_form}`; `cardinal(n: i64) -> String`; `hour_ordinal(h: u8) -> &'static str`; `plural_form(n: i64) -> PluralForm` gdzie `PluralForm` to `{ Singular, PluralNominative, PluralGenitive }`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-audio/src/text/numbers.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardinals_cover_the_shapes_that_appear_in_bulletins() {
        let cases = [
            (0, "zero"),
            (1, "jeden"),
            (2, "dwa"),
            (7, "siedem"),
            (10, "dziesięć"),
            (11, "jedenaście"),
            (12, "dwanaście"),
            (15, "piętnaście"),
            (20, "dwadzieścia"),
            (21, "dwadzieścia jeden"),
            (42, "czterdzieści dwa"),
            (100, "sto"),
            (101, "sto jeden"),
            (115, "sto piętnaście"),
            (200, "dwieście"),
            (300, "trzysta"),
            (500, "pięćset"),
            (999, "dziewięćset dziewięćdziesiąt dziewięć"),
            (1000, "tysiąc"),
            (1001, "tysiąc jeden"),
            (1013, "tysiąc trzynaście"),
            (2000, "dwa tysiące"),
            (5000, "pięć tysięcy"),
            (22000, "dwadzieścia dwa tysiące"),
            (123456, "sto dwadzieścia trzy tysiące czterysta pięćdziesiąt sześć"),
        ];
        for (n, expected) in cases {
            assert_eq!(cardinal(n), expected, "liczba {n}");
        }
    }

    #[test]
    fn negative_numbers_are_spoken_as_minus() {
        assert_eq!(cardinal(-7), "minus siedem");
        assert_eq!(cardinal(-12), "minus dwanaście");
    }

    #[test]
    fn plural_form_follows_polish_grammar() {
        // 1 → metr; 2-4 → metry; 5-21 → metrów; 22-24 → metry
        assert_eq!(plural_form(1), PluralForm::Singular);
        assert_eq!(plural_form(2), PluralForm::PluralNominative);
        assert_eq!(plural_form(4), PluralForm::PluralNominative);
        assert_eq!(plural_form(5), PluralForm::PluralGenitive);
        assert_eq!(plural_form(11), PluralForm::PluralGenitive);
        assert_eq!(plural_form(12), PluralForm::PluralGenitive, "dwanaście metrów, nie metry");
        assert_eq!(plural_form(14), PluralForm::PluralGenitive);
        assert_eq!(plural_form(21), PluralForm::PluralGenitive);
        assert_eq!(plural_form(22), PluralForm::PluralNominative);
        assert_eq!(plural_form(112), PluralForm::PluralGenitive);
        assert_eq!(plural_form(122), PluralForm::PluralNominative);
        assert_eq!(plural_form(0), PluralForm::PluralGenitive);
    }

    #[test]
    fn hours_use_feminine_ordinals() {
        assert_eq!(hour_ordinal(0), "dwudziesta czwarta");
        assert_eq!(hour_ordinal(1), "pierwsza");
        assert_eq!(hour_ordinal(12), "dwunasta");
        assert_eq!(hour_ordinal(18), "osiemnasta");
        assert_eq!(hour_ordinal(23), "dwudziesta trzecia");
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-audio numbers`
Expected: FAIL — `cannot find function cardinal`

- [ ] **Step 3: Zaimplementuj liczebniki**

`crates/aars-audio/Cargo.toml`:

```toml
[package]
name = "aars-audio"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
aars-core = { path = "../aars-core" }
thiserror.workspace = true
tracing.workspace = true
hound = "3.5"
rubato = "5"
sha2 = "0.10"
hex = "0.4"
```

`crates/aars-audio/src/lib.rs`:

```rust
pub mod text;
```

`crates/aars-audio/src/text/mod.rs`:

```rust
pub mod numbers;
```

`crates/aars-audio/src/text/numbers.rs` — nad blokiem testów:

```rust
const ONES: [&str; 20] = [
    "zero", "jeden", "dwa", "trzy", "cztery", "pięć", "sześć", "siedem", "osiem", "dziewięć",
    "dziesięć", "jedenaście", "dwanaście", "trzynaście", "czternaście", "piętnaście",
    "szesnaście", "siedemnaście", "osiemnaście", "dziewiętnaście",
];

const TENS: [&str; 10] = [
    "", "", "dwadzieścia", "trzydzieści", "czterdzieści", "pięćdziesiąt", "sześćdziesiąt",
    "siedemdziesiąt", "osiemdziesiąt", "dziewięćdziesiąt",
];

const HUNDREDS: [&str; 10] = [
    "", "sto", "dwieście", "trzysta", "czterysta", "pięćset", "sześćset", "siedemset",
    "osiemset", "dziewięćset",
];

const HOURS: [&str; 24] = [
    "dwudziesta czwarta", "pierwsza", "druga", "trzecia", "czwarta", "piąta", "szósta",
    "siódma", "ósma", "dziewiąta", "dziesiąta", "jedenasta", "dwunasta", "trzynasta",
    "czternasta", "piętnasta", "szesnasta", "siedemnasta", "osiemnasta", "dziewiętnasta",
    "dwudziesta", "dwudziesta pierwsza", "dwudziesta druga", "dwudziesta trzecia",
];

/// Forma gramatyczna rzeczownika po liczebniku.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralForm {
    /// 1 — „metr"
    Singular,
    /// 2–4 (poza 12–14) — „metry"
    PluralNominative,
    /// pozostałe — „metrów"
    PluralGenitive,
}

/// Dobiera formę rzeczownika po liczebniku zgodnie z polską gramatyką.
pub fn plural_form(n: i64) -> PluralForm {
    let n = n.unsigned_abs();
    if n == 1 {
        return PluralForm::Singular;
    }
    let last_two = n % 100;
    let last = n % 10;
    if (2..=4).contains(&last) && !(12..=14).contains(&last_two) {
        PluralForm::PluralNominative
    } else {
        PluralForm::PluralGenitive
    }
}

/// Liczebnik główny słowami. Zakres do 999 999 — biuletyny nie mówią większymi liczbami.
pub fn cardinal(n: i64) -> String {
    if n < 0 {
        return format!("minus {}", cardinal(-n));
    }
    let n = n as u64;
    if n == 0 {
        return ONES[0].to_owned();
    }

    let mut parts: Vec<String> = Vec::new();

    let thousands = n / 1000;
    if thousands > 0 {
        let word = match plural_form(thousands as i64) {
            PluralForm::Singular => "tysiąc",
            PluralForm::PluralNominative => "tysiące",
            PluralForm::PluralGenitive => "tysięcy",
        };
        if thousands == 1 {
            parts.push(word.to_owned());
        } else {
            parts.push(format!("{} {}", under_thousand(thousands), word));
        }
    }

    let rest = n % 1000;
    if rest > 0 {
        parts.push(under_thousand(rest));
    }

    parts.join(" ")
}

fn under_thousand(n: u64) -> String {
    debug_assert!(n > 0 && n < 1000);
    let mut parts: Vec<&str> = Vec::new();

    let h = (n / 100) as usize;
    if h > 0 {
        parts.push(HUNDREDS[h]);
    }

    let rest = n % 100;
    if rest > 0 {
        if rest < 20 {
            parts.push(ONES[rest as usize]);
        } else {
            parts.push(TENS[(rest / 10) as usize]);
            let ones = (rest % 10) as usize;
            if ones > 0 {
                parts.push(ONES[ones]);
            }
        }
    }

    parts.join(" ")
}

/// Godzina jako liczebnik porządkowy w rodzaju żeńskim („osiemnasta").
pub fn hour_ordinal(h: u8) -> &'static str {
    HOURS[(h % 24) as usize]
}
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-audio numbers`
Expected: PASS — cztery testy

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/aars-audio
git commit -m "feat(audio): polskie liczebniki i formy liczby mnogiej"
```

---

### Task 2: Normalizacja tekstu komunikatu

**Files:**
- Create: `crates/aars-audio/src/text/normalize.rs`
- Modify: `crates/aars-audio/src/text/mod.rs`

**Interfaces:**
- Consumes: `aars_audio::text::numbers::{cardinal, hour_ordinal, plural_form, PluralForm}`
- Produces: `aars_audio::text::normalize::normalize(input: &str) -> String`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-audio/src/text/normalize.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        normalize(s)
    }

    #[test]
    fn units_get_the_right_grammatical_form() {
        assert_eq!(n("wiatr 12 m/s"), "wiatr dwanaście metrów na sekundę");
        assert_eq!(n("wiatr 1 m/s"), "wiatr jeden metr na sekundę");
        assert_eq!(n("wiatr 3 m/s"), "wiatr trzy metry na sekundę");
        assert_eq!(n("temperatura 22 °C"), "temperatura dwadzieścia dwa stopnie Celsjusza");
        assert_eq!(n("temperatura -5 °C"), "temperatura minus pięć stopni Celsjusza");
        assert_eq!(n("ciśnienie 1013 hPa"), "ciśnienie tysiąc trzynaście hektopaskali");
        assert_eq!(n("stan wody 245 cm"), "stan wody dwieście czterdzieści pięć centymetrów");
        assert_eq!(n("opad 2 mm"), "opad dwa milimetry");
        assert_eq!(n("wilgotność 85 %"), "wilgotność osiemdziesiąt pięć procent");
    }

    #[test]
    fn units_work_without_a_space_before_them() {
        assert_eq!(n("12m/s"), "dwanaście metrów na sekundę");
        assert_eq!(n("22°C"), "dwadzieścia dwa stopnie Celsjusza");
    }

    #[test]
    fn clock_times_become_spoken_hours() {
        assert_eq!(n("od 18:00"), "od godzina osiemnasta");
        assert_eq!(n("do 06:30"), "do godzina szósta trzydzieści");
        assert_eq!(n("o 23:05"), "o godzina dwudziesta trzecia zero pięć");
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
    fn whitespace_is_collapsed_and_text_is_left_otherwise_intact() {
        assert_eq!(n("  ostrzeżenie   pierwszego  stopnia \n"), "ostrzeżenie pierwszego stopnia");
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        assert_eq!(n(""), "");
        assert_eq!(n("   "), "");
        assert_eq!(n("m/s"), "m/s", "jednostka bez liczby zostaje jak jest");
        assert_eq!(n("99:99"), "99:99", "niepoprawna godzina zostaje jak jest");
        assert_eq!(n("12345678901234567890 m"), "12345678901234567890 metra",
            "liczba poza zakresem nie wywala normalizacji");
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-audio normalize`
Expected: FAIL — `cannot find function normalize`

- [ ] **Step 3: Zaimplementuj normalizację**

`crates/aars-audio/src/text/normalize.rs` — nad blokiem testów:

```rust
use super::numbers::{PluralForm, cardinal, hour_ordinal, plural_form};

/// Jednostka w trzech formach: po 1, po 2–4, po pozostałych liczbach.
struct Unit {
    token: &'static str,
    singular: &'static str,
    plural_nom: &'static str,
    plural_gen: &'static str,
}

/// Kolejność ma znaczenie: dłuższe tokeny muszą być sprawdzane pierwsze,
/// żeby „m/s" nie dopasowało się jako „m".
const UNITS: &[Unit] = &[
    Unit { token: "km/h", singular: "kilometr na godzinę", plural_nom: "kilometry na godzinę", plural_gen: "kilometrów na godzinę" },
    Unit { token: "m/s",  singular: "metr na sekundę",     plural_nom: "metry na sekundę",     plural_gen: "metrów na sekundę" },
    Unit { token: "µSv/h", singular: "mikrosiwert na godzinę", plural_nom: "mikrosiwerty na godzinę", plural_gen: "mikrosiwertów na godzinę" },
    Unit { token: "uSv/h", singular: "mikrosiwert na godzinę", plural_nom: "mikrosiwerty na godzinę", plural_gen: "mikrosiwertów na godzinę" },
    Unit { token: "hPa",  singular: "hektopaskal",         plural_nom: "hektopaskale",         plural_gen: "hektopaskali" },
    Unit { token: "°C",   singular: "stopień Celsjusza",   plural_nom: "stopnie Celsjusza",    plural_gen: "stopni Celsjusza" },
    Unit { token: "mm",   singular: "milimetr",            plural_nom: "milimetry",            plural_gen: "milimetrów" },
    Unit { token: "cm",   singular: "centymetr",           plural_nom: "centymetry",           plural_gen: "centymetrów" },
    Unit { token: "km",   singular: "kilometr",            plural_nom: "kilometry",            plural_gen: "kilometrów" },
    Unit { token: "%",    singular: "procent",             plural_nom: "procent",              plural_gen: "procent" },
    Unit { token: "m",    singular: "metr",                plural_nom: "metry",                plural_gen: "metrów" },
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
];

/// Zamienia tekst w formę nadającą się do syntezy: liczby i jednostki na
/// słowa, godziny na mowę, skróty rozwinięte, białe znaki zwinięte.
///
/// Wejście, którego nie rozumie, zostawia w spokoju — lepiej nadać surowy
/// token niż zgubić treść ostrzeżenia.
pub fn normalize(input: &str) -> String {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut idx = 0usize;

    while idx < tokens.len() {
        let token = tokens[idx];

        if let Some(spoken) = spoken_time(token) {
            out.push(spoken);
            idx += 1;
            continue;
        }

        // Liczba + jednostka jako dwa tokeny: „12 m/s"
        if let Some((value, text)) = number_prefix(token)
            && text.is_empty()
            && let Some(next) = tokens.get(idx + 1)
            && let Some(unit) = match_unit(next)
            && unit.len() == next.len()
        {
            out.push(spoken_number(&value));
            out.push(unit_form(next, &value).to_owned());
            idx += 2;
            continue;
        }

        // Liczba zlepiona z jednostką jako jeden token: „12m/s"
        if let Some((value, rest)) = number_prefix(token)
            && !rest.is_empty()
            && let Some(unit) = match_unit(rest)
            && unit.len() == rest.len()
        {
            out.push(spoken_number(&value));
            out.push(unit_form(rest, &value).to_owned());
            idx += 1;
            continue;
        }

        // Sama liczba
        if let Some((value, rest)) = number_prefix(token)
            && rest.is_empty()
        {
            out.push(spoken_number(&value));
            idx += 1;
            continue;
        }

        if let Some(expanded) = ABBREVIATIONS.iter().find(|(abbr, _)| *abbr == token) {
            out.push(expanded.1.to_owned());
            idx += 1;
            continue;
        }

        out.push(token.to_owned());
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

fn match_unit(text: &str) -> Option<&'static str> {
    UNITS
        .iter()
        .find(|unit| text.starts_with(unit.token))
        .map(|unit| unit.token)
}

/// Dobiera formę jednostki. Liczba z częścią dziesiętną bierze dopełniacz
/// liczby pojedynczej („dwa przecinek pięć metra").
fn unit_form(text: &str, value: &NumberText) -> &'static str {
    let unit = UNITS
        .iter()
        .find(|unit| text.starts_with(unit.token))
        .expect("unit_form wołane tylko po match_unit");

    if value.fraction.is_some() {
        return genitive_singular(unit);
    }

    let n: i64 = value.integer.parse().unwrap_or(0);
    match plural_form(n) {
        PluralForm::Singular => unit.singular,
        PluralForm::PluralNominative => unit.plural_nom,
        PluralForm::PluralGenitive => unit.plural_gen,
    }
}

/// Dopełniacz liczby pojedynczej dla jednostek używanych z ułamkami.
fn genitive_singular(unit: &Unit) -> &'static str {
    match unit.token {
        "m" => "metra",
        "cm" => "centymetra",
        "mm" => "milimetra",
        "km" => "kilometra",
        "m/s" => "metra na sekundę",
        "km/h" => "kilometra na godzinę",
        "hPa" => "hektopaskala",
        "°C" => "stopnia Celsjusza",
        "%" => "procent",
        _ => unit.plural_gen,
    }
}

/// „18:00" → „godzina osiemnasta"; „06:30" → „godzina szósta trzydzieści".
fn spoken_time(token: &str) -> Option<String> {
    let (h, m) = token.split_once(':')?;
    if h.is_empty() || m.len() != 2 {
        return None;
    }
    let hour: u8 = h.parse().ok()?;
    let minute: u8 = m.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }

    let mut spoken = format!("godzina {}", hour_ordinal(hour));
    if minute > 0 {
        if minute < 10 {
            spoken.push_str(&format!(" zero {}", cardinal(minute as i64)));
        } else {
            spoken.push(' ');
            spoken.push_str(&cardinal(minute as i64));
        }
    }
    Some(spoken)
}
```

`crates/aars-audio/src/text/mod.rs`:

```rust
pub mod normalize;
pub mod numbers;

pub use normalize::normalize;
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-audio`
Expected: PASS — dwanaście testów

- [ ] **Step 5: Commit**

```bash
git add crates/aars-audio
git commit -m "feat(audio): normalizacja tekstu komunikatu na mowę"
```

---

### Task 3: Trait `Synthesizer` i typ `Pcm`

**Files:**
- Create: `crates/aars-core/src/synth.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Consumes: nic
- Produces: `aars_core::synth::{Pcm, VoiceParams, Synthesizer, SynthError, FakeSynth}`; `Pcm { samples: Vec<i16>, sample_rate: u32 }` z `Pcm::duration()`, `Pcm::silence(sample_rate, duration)`, `Pcm::append(&mut self, other: &Pcm)`; `Synthesizer::synthesize(&self, text: &str, voice: &VoiceParams) -> Result<Pcm, SynthError>`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/synth.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn duration_follows_sample_count_and_rate() {
        let pcm = Pcm { samples: vec![0i16; 48_000], sample_rate: 48_000 };
        assert_eq!(pcm.duration(), Duration::from_secs(1));
    }

    #[test]
    fn silence_has_the_requested_length() {
        let pcm = Pcm::silence(48_000, Duration::from_millis(250));
        assert_eq!(pcm.samples.len(), 12_000);
        assert!(pcm.samples.iter().all(|s| *s == 0));
    }

    #[test]
    fn append_concatenates_and_rejects_mismatched_rates() {
        let mut a = Pcm { samples: vec![1, 2], sample_rate: 48_000 };
        a.append(&Pcm { samples: vec![3], sample_rate: 48_000 }).unwrap();
        assert_eq!(a.samples, vec![1, 2, 3]);

        let err = a.append(&Pcm { samples: vec![4], sample_rate: 22_050 }).unwrap_err();
        assert!(matches!(err, SynthError::SampleRateMismatch { .. }));
    }

    #[test]
    fn fake_synth_is_deterministic_and_records_what_it_was_asked_for() {
        let synth = FakeSynth::new(48_000);
        let voice = VoiceParams::default();

        let a = synth.synthesize("ostrzeżenie", &voice).unwrap();
        let b = synth.synthesize("ostrzeżenie", &voice).unwrap();
        assert_eq!(a.samples, b.samples, "ten sam tekst musi dać to samo audio");

        let c = synth.synthesize("ostrzeżenie dłuższe", &voice).unwrap();
        assert!(c.samples.len() > a.samples.len(), "dłuższy tekst to dłuższe audio");

        assert_eq!(synth.requests(), vec!["ostrzeżenie", "ostrzeżenie", "ostrzeżenie dłuższe"]);
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core synth`
Expected: FAIL — `cannot find type Pcm`

- [ ] **Step 3: Zaimplementuj trait i typy**

`crates/aars-core/src/synth.rs` — nad blokiem testów:

```rust
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum SynthError {
    #[error("silnik syntezy zawiódł: {0}")]
    Engine(String),
    #[error("nie można złożyć audio o różnych częstotliwościach: {left} vs {right}")]
    SampleRateMismatch { left: u32, right: u32 },
    #[error("błąd wejścia/wyjścia: {0}")]
    Io(#[from] std::io::Error),
}

/// Mono PCM 16-bit. Jedyna reprezentacja audio w systemie — sekwencer,
/// cache i wyjście audio mówią tym samym typem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pcm {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

impl Pcm {
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / self.sample_rate as f64)
    }

    pub fn silence(sample_rate: u32, duration: Duration) -> Self {
        let count = (duration.as_secs_f64() * sample_rate as f64).round() as usize;
        Self { samples: vec![0i16; count], sample_rate }
    }

    pub fn append(&mut self, other: &Pcm) -> Result<(), SynthError> {
        if self.sample_rate != other.sample_rate {
            return Err(SynthError::SampleRateMismatch {
                left: self.sample_rate,
                right: other.sample_rate,
            });
        }
        self.samples.extend_from_slice(&other.samples);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceParams {
    /// Nazwa głosu; `None` znaczy głos domyślny z konfiguracji.
    pub voice: Option<String>,
    /// Mnożnik tempa mowy; 1.0 to tempo modelu.
    pub rate: f32,
}

impl Default for VoiceParams {
    fn default() -> Self {
        Self { voice: None, rate: 1.0 }
    }
}

pub trait Synthesizer: Send + Sync {
    fn synthesize(&self, text: &str, voice: &VoiceParams) -> Result<Pcm, SynthError>;
}

/// Syntezator testowy: deterministyczny ton o długości proporcjonalnej do
/// tekstu. Pozwala testować sekwencer i kolejkę bez karty dźwiękowej.
pub struct FakeSynth {
    sample_rate: u32,
    requests: Mutex<Vec<String>>,
}

impl FakeSynth {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate, requests: Mutex::new(Vec::new()) }
    }

    /// Teksty, o których syntezę ktoś poprosił, w kolejności.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("mutex fake'a").clone()
    }
}

impl Synthesizer for FakeSynth {
    fn synthesize(&self, text: &str, _voice: &VoiceParams) -> Result<Pcm, SynthError> {
        self.requests.lock().expect("mutex fake'a").push(text.to_owned());

        // 50 ms na znak — krótko, żeby testy były szybkie, ale mierzalnie.
        let samples_per_char = self.sample_rate as usize / 20;
        let count = text.chars().count() * samples_per_char;
        let samples = (0..count)
            .map(|i| {
                let t = i as f32 / self.sample_rate as f32;
                (f32::sin(t * 2.0 * std::f32::consts::PI * 440.0) * 8000.0) as i16
            })
            .collect();

        Ok(Pcm { samples, sample_rate: self.sample_rate })
    }
}
```

`crates/aars-core/src/lib.rs`:

```rust
pub mod config;
pub mod db;
pub mod error;
pub mod synth;
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core`
Expected: PASS — jedenaście testów

- [ ] **Step 5: Commit**

```bash
git add crates/aars-core
git commit -m "feat(core): trait Synthesizer, typ Pcm i syntezator testowy"
```

---

### Task 4: Backend Piper

**Files:**
- Create: `crates/aars-audio/src/piper.rs`
- Create: `crates/aars-audio/src/wav.rs`
- Create: `crates/aars-audio/src/resample.rs`
- Modify: `crates/aars-audio/src/lib.rs`

**Interfaces:**
- Consumes: `aars_core::synth::{Pcm, Synthesizer, SynthError, VoiceParams}`, `aars_audio::text::normalize`
- Produces: `aars_audio::piper::PiperSynth::new(piper_bin, model, target_sample_rate) -> PiperSynth`; `aars_audio::wav::{read_wav_mono, write_wav_mono}`; `aars_audio::resample::{resample, apply_gain_db}`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-audio/src/wav.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::Pcm;

    #[test]
    fn a_wav_round_trips_through_write_and_read() {
        let dir = std::env::temp_dir().join(format!("aars-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.wav");

        let pcm = Pcm { samples: vec![0, 1000, -1000, 32767, -32768], sample_rate: 22_050 };
        write_wav_mono(&path, &pcm).unwrap();
        let read = read_wav_mono(&path).unwrap();

        assert_eq!(read, pcm);
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

`crates/aars-audio/src/resample.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::Pcm;

    fn tone(sample_rate: u32, secs: f32) -> Pcm {
        let count = (sample_rate as f32 * secs) as usize;
        let samples = (0..count)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (f32::sin(t * 2.0 * std::f32::consts::PI * 440.0) * 10_000.0) as i16
            })
            .collect();
        Pcm { samples, sample_rate }
    }

    #[test]
    fn resampling_preserves_duration_within_a_millisecond() {
        let input = tone(22_050, 1.0);
        let output = resample(&input, 48_000).unwrap();

        assert_eq!(output.sample_rate, 48_000);
        let delta = output.duration().as_secs_f64() - input.duration().as_secs_f64();
        assert!(delta.abs() < 0.001, "różnica długości {delta}s");
    }

    #[test]
    fn resampling_to_the_same_rate_is_a_no_op() {
        let input = tone(48_000, 0.1);
        let output = resample(&input, 48_000).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn gain_scales_towards_the_target_peak_and_never_clips() {
        let quiet = Pcm { samples: vec![100, -100, 50], sample_rate: 48_000 };
        let loud = apply_gain_db(&quiet, -3.0);

        let peak = loud.samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        let expected = (32_767.0 * 10f32.powf(-3.0 / 20.0)) as u32;
        assert!(
            (peak as i64 - expected as i64).abs() < 64,
            "szczyt {peak}, oczekiwany ~{expected}"
        );
        assert!(loud.samples.iter().all(|s| *s != i16::MIN), "brak przepełnienia");
    }

    #[test]
    fn gain_leaves_silence_alone() {
        let silence = Pcm { samples: vec![0; 100], sample_rate: 48_000 };
        let out = apply_gain_db(&silence, -3.0);
        assert!(out.samples.iter().all(|s| *s == 0), "cisza nie ma czego wzmacniać");
    }
}
```

`crates/aars-audio/src/piper.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::{Synthesizer, VoiceParams};

    /// Wymaga binarki `piper` i modelu wskazanych przez AARS_TEST_PIPER_BIN
    /// i AARS_TEST_PIPER_MODEL. Uruchamiane jawnie:
    /// `cargo test -p aars-audio -- --ignored`
    #[test]
    #[ignore = "wymaga lokalnej instalacji Pipera"]
    fn piper_synthesizes_audible_audio_at_the_target_rate() {
        let bin = std::env::var("AARS_TEST_PIPER_BIN").expect("AARS_TEST_PIPER_BIN");
        let model = std::env::var("AARS_TEST_PIPER_MODEL").expect("AARS_TEST_PIPER_MODEL");

        let synth = PiperSynth::new(bin, model, 48_000);
        let pcm = synth.synthesize("Uwaga, ostrzeżenie testowe.", &VoiceParams::default()).unwrap();

        assert_eq!(pcm.sample_rate, 48_000);
        assert!(pcm.duration().as_millis() > 500, "zbyt krótkie: {:?}", pcm.duration());
        let peak = pcm.samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 1000, "audio praktycznie ciche, szczyt {peak}");
    }

    #[test]
    fn a_missing_piper_binary_is_a_clear_engine_error() {
        let synth = PiperSynth::new("/nie/ma/takiego/pipera", "/nie/ma/modelu.onnx", 48_000);
        let err = synth.synthesize("test", &VoiceParams::default()).unwrap_err();

        assert!(matches!(err, SynthError::Engine(_)), "{err:?}");
        assert!(err.to_string().contains("piper"), "komunikat musi wskazać winowajcę: {err}");
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-audio`
Expected: FAIL — `cannot find function write_wav_mono`, `cannot find type PiperSynth`

- [ ] **Step 3: Zaimplementuj WAV, resampling i Piper**

`crates/aars-audio/src/wav.rs` — nad blokiem testów:

```rust
use std::path::Path;

use aars_core::synth::{Pcm, SynthError};

pub fn write_wav_mono(path: &Path, pcm: &Pcm) -> Result<(), SynthError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: pcm.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| SynthError::Engine(format!("zapis WAV {}: {e}", path.display())))?;
    for sample in &pcm.samples {
        writer
            .write_sample(*sample)
            .map_err(|e| SynthError::Engine(format!("zapis próbki: {e}")))?;
    }
    writer.finalize().map_err(|e| SynthError::Engine(format!("domknięcie WAV: {e}")))?;
    Ok(())
}

pub fn read_wav_mono(path: &Path) -> Result<Pcm, SynthError> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| SynthError::Engine(format!("odczyt WAV {}: {e}", path.display())))?;
    let spec = reader.spec();

    let samples: Result<Vec<i16>, _> = reader.samples::<i16>().collect();
    let samples =
        samples.map_err(|e| SynthError::Engine(format!("odczyt próbek {}: {e}", path.display())))?;

    // Gdy plik jest wielokanałowy, bierzemy pierwszy kanał: nadajemy mono.
    let samples = if spec.channels > 1 {
        samples.iter().step_by(spec.channels as usize).copied().collect()
    } else {
        samples
    };

    Ok(Pcm { samples, sample_rate: spec.sample_rate })
}
```

`crates/aars-audio/src/resample.rs` — nad blokiem testów:

```rust
use aars_core::synth::{Pcm, SynthError};
use rubato::{FftFixedIn, Resampler};

/// Przelicza PCM na docelową częstotliwość. Piper daje 22,05 kHz, karta
/// zwykle chce 48 kHz.
pub fn resample(input: &Pcm, target_rate: u32) -> Result<Pcm, SynthError> {
    if input.sample_rate == target_rate || input.samples.is_empty() {
        return Ok(Pcm { samples: input.samples.clone(), sample_rate: target_rate });
    }

    let chunk = 1024usize;
    let mut resampler =
        FftFixedIn::<f32>::new(input.sample_rate as usize, target_rate as usize, chunk, 2, 1)
            .map_err(|e| SynthError::Engine(format!("inicjalizacja resamplera: {e}")))?;

    let floats: Vec<f32> = input.samples.iter().map(|s| *s as f32 / 32_768.0).collect();
    let mut out: Vec<f32> = Vec::with_capacity(
        (floats.len() as f64 * target_rate as f64 / input.sample_rate as f64) as usize + chunk,
    );

    for block in floats.chunks(chunk) {
        // Ostatni blok dopełniamy ciszą — FftFixedIn wymaga pełnych bloków.
        let mut padded = block.to_vec();
        padded.resize(chunk, 0.0);
        let processed = resampler
            .process(&[padded], None)
            .map_err(|e| SynthError::Engine(format!("resampling: {e}")))?;
        out.extend_from_slice(&processed[0]);
    }

    // Obcinamy nadmiar wynikający z dopełnienia ostatniego bloku.
    let expected =
        (input.samples.len() as f64 * target_rate as f64 / input.sample_rate as f64).round() as usize;
    out.truncate(expected);

    let samples = out
        .iter()
        .map(|v| (v.clamp(-1.0, 1.0) * 32_767.0) as i16)
        .collect();

    Ok(Pcm { samples, sample_rate: target_rate })
}

/// Normalizuje szczyt do zadanego poziomu w dBFS. Nie wzmacnia ciszy i
/// nigdy nie przepełnia — nadajnik dostaje przewidywalny poziom.
pub fn apply_gain_db(input: &Pcm, target_dbfs: f32) -> Pcm {
    let peak = input.samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    if peak == 0 {
        return input.clone();
    }

    let target_peak = 32_767.0 * 10f32.powf(target_dbfs / 20.0);
    let factor = target_peak / peak as f32;

    let samples = input
        .samples
        .iter()
        .map(|s| ((*s as f32 * factor).clamp(-32_767.0, 32_767.0)) as i16)
        .collect();

    Pcm { samples, sample_rate: input.sample_rate }
}
```

`crates/aars-audio/src/piper.rs` — nad blokiem testów:

```rust
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use aars_core::synth::{Pcm, SynthError, Synthesizer, VoiceParams};

use crate::resample::resample;
use crate::text::normalize;
use crate::wav::read_wav_mono;

/// Synteza przez binarkę `piper`. Podproces, a nie wiązanie natywne:
/// `piper-rs` 0.2 ciągnie natywne espeak-ng i ort, a jego dokumentacja nie
/// buduje się na docs.rs — nie jest fundamentem dla stacji ostrzegawczej.
/// Trait `Synthesizer` pozwala podmienić to później bez ruszania wywołujących.
pub struct PiperSynth {
    piper_bin: String,
    model: String,
    target_sample_rate: u32,
}

impl PiperSynth {
    pub fn new(
        piper_bin: impl Into<String>,
        model: impl Into<String>,
        target_sample_rate: u32,
    ) -> Self {
        Self {
            piper_bin: piper_bin.into(),
            model: model.into(),
            target_sample_rate,
        }
    }

    fn output_path(&self) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aars-piper-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }
}

impl Synthesizer for PiperSynth {
    fn synthesize(&self, text: &str, voice: &VoiceParams) -> Result<Pcm, SynthError> {
        let spoken = normalize(text);
        if spoken.is_empty() {
            return Ok(Pcm { samples: Vec::new(), sample_rate: self.target_sample_rate });
        }

        let out_path = self.output_path();
        let mut cmd = Command::new(&self.piper_bin);
        cmd.arg("--model")
            .arg(&self.model)
            .arg("--output_file")
            .arg(&out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        if (voice.rate - 1.0).abs() > f32::EPSILON {
            // Piper wyraża tempo odwrotnie: większa długość to wolniejsza mowa.
            cmd.arg("--length_scale").arg(format!("{:.3}", 1.0 / voice.rate));
        }

        let mut child = cmd.spawn().map_err(|e| {
            SynthError::Engine(format!("nie udało się uruchomić piper ({}): {e}", self.piper_bin))
        })?;

        child
            .stdin
            .as_mut()
            .ok_or_else(|| SynthError::Engine("piper nie przyjął wejścia".to_owned()))?
            .write_all(spoken.as_bytes())?;
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .map_err(|e| SynthError::Engine(format!("piper nie zakończył się: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&out_path);
            return Err(SynthError::Engine(format!(
                "piper zakończył się błędem {:?}: {}",
                output.status.code(),
                stderr.trim()
            )));
        }

        let raw = read_wav_mono(&out_path)?;
        let _ = std::fs::remove_file(&out_path);

        resample(&raw, self.target_sample_rate)
    }
}
```

`crates/aars-audio/src/lib.rs`:

```rust
pub mod piper;
pub mod resample;
pub mod text;
pub mod wav;
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-audio`
Expected: PASS — siedemnaście testów, jeden pominięty jako `ignored`

- [ ] **Step 5: Commit**

```bash
git add crates/aars-audio
git commit -m "feat(audio): backend Piper, WAV i resampling"
```

---

### Task 5: Cache audio adresowany treścią

**Files:**
- Create: `crates/aars-audio/src/cache.rs`
- Modify: `crates/aars-audio/src/lib.rs`
- Modify: `crates/aars-audio/Cargo.toml` (dodaj `sqlx`, `chrono`, dev-dep `tokio`)

**Interfaces:**
- Consumes: `aars_core::synth::{Pcm, Synthesizer, SynthError, VoiceParams}`, `aars_audio::wav::{read_wav_mono, write_wav_mono}`, pula `sqlx::SqlitePool` z tabelą `audio_cache`
- Produces: `aars_audio::cache::AudioCache::new(pool, dir, max_bytes)`; `AudioCache::render(&self, synth: &dyn Synthesizer, text: &str, voice: &VoiceParams) -> Result<(PathBuf, Pcm), SynthError>`; `AudioCache::prune(&self) -> Result<u64, SynthError>`; `AudioCache::cache_key(text, voice) -> String`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-audio/src/cache.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::FakeSynth;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn tmp_dir(tag: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("aars-cache-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }

    #[tokio::test]
    async fn the_same_text_is_synthesized_only_once() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("hit");
        let cache = AudioCache::new(pool, dir.0.clone(), 10 * 1024 * 1024);
        let synth = FakeSynth::new(48_000);
        let voice = VoiceParams::default();

        let (path_a, pcm_a) = cache.render(&synth, "ostrzeżenie", &voice).await.unwrap();
        let (path_b, pcm_b) = cache.render(&synth, "ostrzeżenie", &voice).await.unwrap();

        assert_eq!(path_a, path_b);
        assert_eq!(pcm_a, pcm_b);
        assert_eq!(synth.requests().len(), 1, "drugie wywołanie musi trafić w cache");
    }

    #[tokio::test]
    async fn different_voice_params_are_different_cache_entries() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("voice");
        let cache = AudioCache::new(pool, dir.0.clone(), 10 * 1024 * 1024);
        let synth = FakeSynth::new(48_000);

        cache.render(&synth, "test", &VoiceParams::default()).await.unwrap();
        cache
            .render(&synth, "test", &VoiceParams { voice: None, rate: 1.2 })
            .await
            .unwrap();

        assert_eq!(synth.requests().len(), 2, "inne tempo to inne audio");
    }

    #[tokio::test]
    async fn a_cache_entry_whose_file_vanished_is_resynthesized() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("gone");
        let cache = AudioCache::new(pool, dir.0.clone(), 10 * 1024 * 1024);
        let synth = FakeSynth::new(48_000);
        let voice = VoiceParams::default();

        let (path, _) = cache.render(&synth, "test", &voice).await.unwrap();
        std::fs::remove_file(&path).unwrap();

        let (again, _) = cache.render(&synth, "test", &voice).await.unwrap();
        assert!(again.exists());
        assert_eq!(synth.requests().len(), 2);
    }

    #[tokio::test]
    async fn prune_evicts_least_recently_used_until_under_the_limit() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("prune");
        let synth = FakeSynth::new(48_000);
        let voice = VoiceParams::default();

        // Limit na tyle mały, że zmieści się tylko najnowszy wpis.
        let cache = AudioCache::new(pool.clone(), dir.0.clone(), 20_000);
        let (old_path, _) = cache.render(&synth, "stary komunikat", &voice).await.unwrap();
        let (new_path, _) = cache.render(&synth, "nowy komunikat", &voice).await.unwrap();

        // Cofnij czas użycia starszego wpisu, by LRU miało co wybrać.
        sqlx::query("UPDATE audio_cache SET last_used_at = '2000-01-01T00:00:00Z' WHERE path = ?")
            .bind(old_path.to_string_lossy().as_ref())
            .execute(&pool)
            .await
            .unwrap();

        let freed = cache.prune().await.unwrap();

        assert!(freed > 0, "prune musiał coś zwolnić");
        assert!(!old_path.exists(), "najdawniej używany wpis powinien zniknąć");
        assert!(new_path.exists(), "najnowszy wpis musi zostać");
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-audio cache`
Expected: FAIL — `cannot find type AudioCache`

- [ ] **Step 3: Zaimplementuj cache**

`crates/aars-audio/Cargo.toml` — dopisz:

```toml
sqlx.workspace = true
chrono.workspace = true

[dev-dependencies]
aars-core = { path = "../aars-core", features = ["testing"] }
tokio = { workspace = true, features = ["rt", "macros"] }
```

`crates/aars-audio/src/cache.rs` — nad blokiem testów:

```rust
use std::path::PathBuf;

use aars_core::synth::{Pcm, SynthError, Synthesizer, VoiceParams};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::wav::{read_wav_mono, write_wav_mono};

/// Cache audio adresowany treścią: ten sam tekst i te same parametry głosu
/// dają ten sam plik, więc podgląd w panelu i nadanie na pasmo korzystają z
/// jednego WAV-a (spec §4.1).
pub struct AudioCache {
    pool: SqlitePool,
    dir: PathBuf,
    max_bytes: u64,
}

impl AudioCache {
    pub fn new(pool: SqlitePool, dir: PathBuf, max_bytes: u64) -> Self {
        Self { pool, dir, max_bytes }
    }

    /// Klucz zawiera tekst i parametry głosu — zmiana któregokolwiek z nich
    /// daje inne audio, więc musi dać inny wpis.
    pub fn cache_key(text: &str, voice: &VoiceParams) -> String {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hasher.update([0u8]);
        hasher.update(voice.voice.as_deref().unwrap_or("").as_bytes());
        hasher.update([0u8]);
        hasher.update(format!("{:.3}", voice.rate).as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Zwraca ścieżkę i PCM dla tekstu, syntezując tylko wtedy, gdy trzeba.
    pub async fn render(
        &self,
        synth: &dyn Synthesizer,
        text: &str,
        voice: &VoiceParams,
    ) -> Result<(PathBuf, Pcm), SynthError> {
        let key = Self::cache_key(text, voice);

        let existing: Option<String> =
            sqlx::query_scalar("SELECT path FROM audio_cache WHERE hash = ?")
                .bind(&key)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("odczyt cache: {e}")))?;

        if let Some(path) = existing {
            let path = PathBuf::from(path);
            if path.exists() {
                let pcm = read_wav_mono(&path)?;
                sqlx::query(
                    "UPDATE audio_cache SET last_used_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE hash = ?",
                )
                .bind(&key)
                .execute(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("aktualizacja cache: {e}")))?;
                return Ok((path, pcm));
            }
            // Wpis bez pliku: ktoś wyczyścił katalog pod nami. Syntezujemy ponownie.
            tracing::warn!(%key, path = %path.display(), "wpis cache bez pliku, ponowna synteza");
            sqlx::query("DELETE FROM audio_cache WHERE hash = ?")
                .bind(&key)
                .execute(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("czyszczenie wpisu cache: {e}")))?;
        }

        let pcm = synth.synthesize(text, voice)?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{key}.wav"));
        write_wav_mono(&path, &pcm)?;

        let bytes = std::fs::metadata(&path)?.len() as i64;
        sqlx::query(
            "INSERT INTO audio_cache (hash, path, bytes, duration_ms) VALUES (?, ?, ?, ?)",
        )
        .bind(&key)
        .bind(path.to_string_lossy().as_ref())
        .bind(bytes)
        .bind(pcm.duration().as_millis() as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| SynthError::Engine(format!("zapis wpisu cache: {e}")))?;

        Ok((path, pcm))
    }

    /// Usuwa najdawniej używane wpisy, aż suma rozmiarów zmieści się w limicie.
    /// Zwraca liczbę zwolnionych bajtów.
    pub async fn prune(&self) -> Result<u64, SynthError> {
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT hash, path, bytes FROM audio_cache ORDER BY last_used_at DESC")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("odczyt cache do czyszczenia: {e}")))?;

        let mut kept: u64 = 0;
        let mut freed: u64 = 0;

        for (hash, path, bytes) in rows {
            let bytes = bytes.max(0) as u64;
            if kept + bytes <= self.max_bytes {
                kept += bytes;
                continue;
            }

            std::fs::remove_file(&path).ok();
            sqlx::query("DELETE FROM audio_cache WHERE hash = ?")
                .bind(&hash)
                .execute(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("usuwanie wpisu cache: {e}")))?;
            freed += bytes;
        }

        if freed > 0 {
            tracing::info!(freed_bytes = freed, "cache audio wyczyszczony");
        }
        Ok(freed)
    }
}
```

`crates/aars-audio/src/lib.rs`:

```rust
pub mod cache;
pub mod piper;
pub mod resample;
pub mod text;
pub mod wav;
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-audio`
Expected: PASS — dwadzieścia dwa testy, jeden `ignored`

- [ ] **Step 5: Commit**

```bash
git add crates/aars-audio
git commit -m "feat(audio): cache audio adresowany treścią z czyszczeniem LRU"
```

---

### Task 6: Podkomenda `aars say`

**Files:**
- Modify: `crates/aars/src/cli.rs`
- Modify: `crates/aars/src/main.rs`
- Modify: `crates/aars/Cargo.toml`
- Modify: `crates/aars/tests/cli.rs`

**Interfaces:**
- Consumes: `aars_audio::{cache::AudioCache, piper::PiperSynth, text::normalize}`, `aars_core::synth::VoiceParams`
- Produces: `aars say <tekst> [--out plik.wav] [--dry-run]`

- [ ] **Step 1: Napisz test, który ma nie przejść**

Dopisz do `crates/aars/tests/cli.rs`:

```rust
#[test]
fn say_dry_run_prints_the_normalized_text_without_synthesizing() {
    let out = aars()
        .args(["say", "--dry-run", "wiatr 12 m/s o 18:00"])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("wiatr dwanaście metrów na sekundę o godzina osiemnasta"),
        "stdout: {stdout}"
    );
}
```

- [ ] **Step 2: Uruchom test i potwierdź, że nie przechodzi**

Run: `cargo test -p aars say_dry_run`
Expected: FAIL — `unrecognized subcommand 'say'`

- [ ] **Step 3: Zaimplementuj podkomendę**

`crates/aars/Cargo.toml` — dopisz do `[dependencies]`:

```toml
aars-audio = { path = "../aars-audio" }
```

`crates/aars/src/cli.rs` — dopisz do `enum Command`:

```rust
    /// Syntezuje tekst i zapisuje do pliku WAV.
    Say {
        /// Treść komunikatu.
        text: String,
        /// Plik wyjściowy. Pominięty — audio ląduje w cache.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Tylko wypisz znormalizowany tekst, bez syntezy.
        #[arg(long)]
        dry_run: bool,
    },
```

`crates/aars/src/main.rs` — dopisz gałąź w `match cli.command`:

```rust
        cli::Command::Say { text, out, dry_run } => {
            let spoken = aars_audio::text::normalize(&text);
            println!("{spoken}");

            if dry_run {
                return Ok(());
            }

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;

            let synth = aars_audio::piper::PiperSynth::new(
                &config.tts.piper_bin,
                &config.tts.model,
                config.audio.sample_rate,
            );
            let cache = aars_audio::cache::AudioCache::new(
                pool,
                config.storage.cache_dir.clone(),
                config.storage.cache_max_bytes,
            );

            let voice = aars_core::synth::VoiceParams::default();
            let (path, pcm) =
                cache.render(&synth, &text, &voice).await.context("synteza komunikatu")?;

            if let Some(out) = out {
                std::fs::copy(&path, &out).context("zapis pliku wyjściowego")?;
                println!("zapisano {} ({:?})", out.display(), pcm.duration());
            } else {
                println!("cache {} ({:?})", path.display(), pcm.duration());
            }

            cache.prune().await.context("czyszczenie cache")?;
        }
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Sprawdź ręcznie na prawdziwym Piperze, jeśli jest dostępny**

Run: `cargo run -p aars -- say --dry-run "ostrzeżenie: wiatr 22 m/s, temperatura -5 °C"`
Expected: `ostrzeżenie: wiatr dwadzieścia dwa metry na sekundę, temperatura minus pięć stopni Celsjusza`

- [ ] **Step 6: Commit**

```bash
git add crates/aars
git commit -m "feat(bin): podkomenda say z podglądem normalizacji"
```

---

## Kryterium wyjścia z etapu

- `cargo test --workspace` zielone; `cargo clippy --workspace --all-targets -- -D warnings` bez zastrzeżeń.
- `aars say --dry-run` pokazuje znormalizowany tekst dla liczb, jednostek, godzin i skrótów.
- `aars say "tekst" --out plik.wav` zapisuje WAV o docelowej częstotliwości (wymaga lokalnego Pipera).
- Powtórna synteza tego samego tekstu nie woła silnika — trafia w cache.
- Żaden test nie dotyka karty dźwiękowej ani sieci; test Pipera jest `#[ignore]`.
