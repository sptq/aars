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
