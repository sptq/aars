//! Obszar, o którym stacja nadaje ostrzeżenia: województwa i powiaty TERYT
//! (etap 8, R8). Tabelę 380 powiatów celowo pomijamy — nazwę powiatu, którą
//! słychać na antenie, podaje operator.

/// Najwięcej wpisów w obszarze.
pub const MAX_ENTRIES: usize = 40;

const VOIVODESHIPS: [(&str, &str); 16] = [
    ("02", "dolnośląskie"),
    ("04", "kujawsko-pomorskie"),
    ("06", "lubelskie"),
    ("08", "lubuskie"),
    ("10", "łódzkie"),
    ("12", "małopolskie"),
    ("14", "mazowieckie"),
    ("16", "opolskie"),
    ("18", "podkarpackie"),
    ("20", "podlaskie"),
    ("22", "pomorskie"),
    ("24", "śląskie"),
    ("26", "świętokrzyskie"),
    ("28", "warmińsko-mazurskie"),
    ("30", "wielkopolskie"),
    ("32", "zachodniopomorskie"),
];

/// Nazwa województwa po dwucyfrowym kodzie TERYT.
pub fn voivodeship_name(code: &str) -> Option<&'static str> {
    VOIVODESHIPS.iter().find(|(c, _)| *c == code).map(|(_, name)| *name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaEntry {
    /// 2 cyfry (województwo) albo 4 cyfry (powiat).
    pub code: String,
    /// Tekst czytany na antenie.
    pub label: String,
}

impl AreaEntry {
    /// Czy czterocyfrowy kod powiatu leży w tym wpisie.
    pub fn matches(&self, teryt: &str) -> bool {
        match self.code.len() {
            2 => teryt.len() == 4 && teryt.starts_with(self.code.as_str()),
            4 => teryt == self.code,
            _ => false,
        }
    }
}

fn parse_entry(part: &str) -> Result<AreaEntry, String> {
    let (code, name) = match part.split_once(char::is_whitespace) {
        Some((code, name)) => (code, name.trim()),
        None => (part, ""),
    };
    if !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("„{code}”: kod ma mieć 2 cyfry (województwo) albo 4 (powiat)"));
    }
    match code.len() {
        2 => {
            let voivodeship = voivodeship_name(code)
                .ok_or_else(|| format!("nie ma województwa o kodzie {code}"))?;
            let label = if name.is_empty() {
                format!("województwo {voivodeship}")
            } else {
                name.to_owned()
            };
            Ok(AreaEntry { code: code.to_owned(), label })
        }
        4 => {
            let (voivodeship, county) = code.split_at(2);
            if voivodeship_name(voivodeship).is_none() {
                return Err(format!("powiat {code}: nie ma województwa o kodzie {voivodeship}"));
            }
            if county == "00" {
                return Err(format!("powiat {code}: numer powiatu to 01–99"));
            }
            if name.is_empty() {
                return Err(format!("powiat {code}: podaj nazwę do odczytu na antenie"));
            }
            Ok(AreaEntry { code: code.to_owned(), label: name.to_owned() })
        }
        _ => Err(format!("„{code}”: kod ma mieć 2 cyfry (województwo) albo 4 (powiat)")),
    }
}

/// Linie albo wpisy rozdzielone średnikiem: `KOD [nazwa]`. Pusty tekst to
/// pusty obszar.
pub fn parse_area(text: &str) -> Result<Vec<AreaEntry>, String> {
    let mut entries: Vec<AreaEntry> = Vec::new();
    for part in text.split(['\n', ';']).map(str::trim).filter(|p| !p.is_empty()) {
        let entry = parse_entry(part)?;
        if entries.iter().any(|e| e.code == entry.code) {
            return Err(format!("kod {} występuje dwa razy", entry.code));
        }
        entries.push(entry);
        if entries.len() > MAX_ENTRIES {
            return Err(format!("obszar ma najwyżej {MAX_ENTRIES} wpisów"));
        }
    }
    Ok(entries)
}

/// Postać kanoniczna: jedna linia na wpis. Ją zapisujemy i nią porównujemy
/// obszar (R9).
pub fn area_text(entries: &[AreaEntry]) -> String {
    entries
        .iter()
        .map(|e| format!("{} {}", e.code, e.label))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voivodeship_code_matches_every_county_inside_it() {
        let area = parse_area("10").unwrap();
        assert!(area[0].matches("1061"));
        assert!(area[0].matches("1020"));
        assert!(!area[0].matches("1461"));
    }

    #[test]
    fn a_county_code_matches_only_itself() {
        let area = parse_area("1061 Łódź").unwrap();
        assert!(area[0].matches("1061"));
        assert!(!area[0].matches("1062"));
        assert!(!area[0].matches("10"));
    }

    #[test]
    fn the_voivodeship_gets_its_name_by_default() {
        let area = parse_area("10").unwrap();
        assert_eq!(area[0].label, "województwo łódzkie");
        assert_eq!(voivodeship_name("10"), Some("łódzkie"));
        assert_eq!(voivodeship_name("11"), None);
    }

    #[test]
    fn a_county_needs_a_spoken_name() {
        assert_eq!(
            parse_area("1061").unwrap_err(),
            "powiat 1061: podaj nazwę do odczytu na antenie"
        );
        assert!(parse_area("1061 Łódź").is_ok());
    }

    #[test]
    fn bad_codes_are_refused() {
        for bad in ["11", "1000 x", "3401 x", "abc", "10\n10", "123 x"] {
            assert!(parse_area(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn entries_can_be_separated_by_lines_or_semicolons() {
        let area = parse_area("1061 Łódź; 1020 powiat zgierski").unwrap();
        assert_eq!(area.len(), 2);
        assert_eq!(area[1].label, "powiat zgierski");
        let text = area_text(&area);
        assert_eq!(text, "1061 Łódź\n1020 powiat zgierski");
        assert_eq!(parse_area(&text).unwrap(), area);
        assert_eq!(parse_area("  \n ").unwrap(), vec![]);
    }

    #[test]
    fn the_area_has_at_most_forty_entries() {
        let many: Vec<String> = (1..=41).map(|n| format!("10{n:02} x")).collect();
        assert!(parse_area(&many[..40].join(";")).is_ok());
        assert!(parse_area(&many.join(";")).is_err());
    }
}
