//! Znacznik pauzy w tekście komunikatu: `[[pauza:MS]]`. Synteza go nie widzi:
//! tekst jest dzielony na odcinki mowy i ciszy, zanim trafi do syntezatora.

use std::time::Duration;

const PREFIX: &str = "[[pauza";
const CLOSE: &str = "]]";
/// Górna granica jednej pauzy.
pub const MAX_PAUSE_MS: u64 = 10_000;
/// Górna granica sumy pauz w jednym tekście.
pub const MAX_TOTAL_PAUSE_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Pause(Duration),
}

enum Token<'a> {
    Text(&'a str),
    Pause(u64),
}

/// Czy tekst zawiera cokolwiek, co wygląda na znacznik pauzy.
pub fn has_marker(text: &str) -> bool {
    text.contains(PREFIX)
}

/// Jedyny parser: tekst i poprawne pauzy (już obcięte do limitu). Każdy
/// fragment `[[pauza…`, który nie jest poprawnym znacznikiem (zły argument,
/// brak `]]`, brak `:`), jest usuwany z ostrzeżeniem, więc nigdy nie trafi do
/// syntezatora.
fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut total = 0u64;
    while let Some(start) = rest.find(PREFIX) {
        if start > 0 {
            out.push(Token::Text(&rest[..start]));
        }
        let after = &rest[start + PREFIX.len()..];
        let closed = after.strip_prefix(':').and_then(|a| {
            a.find(CLOSE)
                .map(|end| (&a[..end], &a[end + CLOSE.len()..]))
        });
        match closed {
            Some((arg, tail)) => {
                match parse_ms(arg.trim()) {
                    Some(ms) => {
                        let allowed = ms.min(MAX_PAUSE_MS).min(MAX_TOTAL_PAUSE_MS - total);
                        if allowed < ms.min(MAX_PAUSE_MS) {
                            tracing::warn!("suma pauz w tekście ponad 30 s, nadmiar odrzucony");
                        }
                        total += allowed;
                        if allowed > 0 {
                            out.push(Token::Pause(allowed));
                        }
                    }
                    None => tracing::warn!(znacznik = arg, "błędny znacznik pauzy, usunięty"),
                }
                rest = tail;
            }
            None => {
                tracing::warn!("niedomknięty znacznik pauzy, usunięty");
                let skip = after
                    .find(|c: char| !(c == ':' || c.is_ascii_digit() || c == ' '))
                    .unwrap_or(after.len());
                rest = &after[skip..];
            }
        }
    }
    if !rest.is_empty() {
        out.push(Token::Text(rest));
    }
    out
}

/// Dzieli tekst na odcinki mowy i ciszy. Sąsiednie kawałki tekstu (rozdzielone
/// usuniętym znacznikiem) są sklejane, puste odcinki pomijane.
pub fn split(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Segment>| {
        if !buf.trim().is_empty() {
            out.push(Segment::Text(std::mem::take(buf)));
        } else {
            buf.clear();
        }
    };
    for token in tokens(text) {
        match token {
            Token::Text(t) => buf.push_str(t),
            Token::Pause(ms) => {
                flush(&mut buf, &mut out);
                out.push(Segment::Pause(Duration::from_millis(ms)));
            }
        }
    }
    flush(&mut buf, &mut out);
    out
}

/// Usuwa wszystkie znaczniki pauzy (poprawne i błędne). Dla tekstów z
/// zewnętrznych źródeł: pauzy może wstawiać tylko operator.
pub fn strip_markers(text: &str) -> String {
    // Do punktu stałego: usunięcie wewnętrznego znacznika nie może skleić
    // nowego (`[[pau[[pauza:1]]za:9999]]`). Każdy przebieg skraca tekst,
    // więc pętla się kończy; ostatecznie zostaje tylko tekst bez `[[pauza`.
    let mut out = text.to_owned();
    while has_marker(&out) {
        let next: String = tokens(&out)
            .into_iter()
            .filter_map(|t| match t {
                Token::Text(s) => Some(s),
                Token::Pause(_) => None,
            })
            .collect();
        out = if next.len() < out.len() {
            next
        } else {
            out.replace(PREFIX, "")
        };
    }
    out
}

fn parse_ms(arg: &str) -> Option<u64> {
    if arg.is_empty() || !arg.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Za duża liczba (przepełnienie) to też „ponad limit”.
    Some(arg.parse::<u64>().unwrap_or(MAX_PAUSE_MS))
}

/// Tekst do wyświetlenia (panel, dziennik): pauza jako „⏸ 1,5 s” oddzielona
/// spacjami od otoczenia, błędne znaczniki usunięte.
pub fn display(text: &str) -> String {
    if !has_marker(text) {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut space_after = false;
    for token in tokens(text) {
        match token {
            Token::Text(t) => {
                if space_after && !t.starts_with(char::is_whitespace) {
                    out.push(' ');
                }
                space_after = false;
                out.push_str(t);
            }
            Token::Pause(ms) => {
                if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                    out.push(' ');
                }
                let label = if ms % 1000 == 0 {
                    (ms / 1000).to_string()
                } else {
                    format!("{:.1}", ms as f64 / 1000.0).replace('.', ",")
                };
                out.push_str(&format!("⏸ {label} s"));
                space_after = true;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Segment {
        Segment::Pause(Duration::from_millis(n))
    }

    #[test]
    fn splits_text_and_pauses() {
        assert_eq!(
            split("Ala [[pauza:500]] kot"),
            vec![
                Segment::Text("Ala ".into()),
                ms(500),
                Segment::Text(" kot".into())
            ]
        );
    }

    #[test]
    fn clamps_and_drops_bad_markers() {
        assert_eq!(split("[[pauza:99999]]"), vec![ms(10_000)]);
        assert_eq!(split("[[pauza:99999999999999999999999]]"), vec![ms(10_000)]);
        assert_eq!(split("a[[pauza:x]]b"), vec![Segment::Text("ab".into())]);
        assert_eq!(split("[[pauza:x]]"), vec![]);
        assert_eq!(split("a [[pauza: ]] b"), vec![Segment::Text("a  b".into())]);
    }

    #[test]
    fn dangling_fragments_never_survive() {
        for text in [
            "a [[pauza:5",
            "a [[pauza",
            "a [[pauza: 5 b",
            "a [[pauzax]] b",
            "a [[pauza",
        ] {
            for segment in split(text) {
                if let Segment::Text(t) = segment {
                    assert!(!t.contains("[[pauza"), "{text:?} -> {t:?}");
                }
            }
        }
        assert_eq!(split("a [[pauza:5"), vec![Segment::Text("a ".into())]);
        assert_eq!(split("a [[pauza:5 b"), vec![Segment::Text("a b".into())]);
    }

    #[test]
    fn total_pause_is_capped_at_30_seconds() {
        let text = "[[pauza:10000]][[pauza:10000]][[pauza:10000]][[pauza:5000]]a";
        let total: Duration = split(text)
            .iter()
            .filter_map(|s| {
                if let Segment::Pause(d) = s {
                    Some(*d)
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(total, Duration::from_secs(30));
    }

    #[test]
    fn strip_removes_valid_and_malformed_markers() {
        assert_eq!(strip_markers("a[[pauza:500]]b[[pauza:x]]c[[pauza:5"), "abc");
        assert_eq!(strip_markers("zwykły"), "zwykły");
        for nested in [
            "a[[pau[[pauza:1]]za:9999]]b",
            "[[pa[[pauza:1]]u[[pauza:2]]za:5000]]",
            "[[[[pauza:1]]pauza:3000]]",
        ] {
            let out = strip_markers(nested);
            assert!(!has_marker(&out), "{nested} -> {out}");
            assert!(split(&out).iter().all(|s| !matches!(s, Segment::Pause(_))), "{out}");
        }
    }

    #[test]
    fn display_renders_marker_compactly_with_spaces() {
        assert_eq!(display("a [[pauza:1500]] b"), "a ⏸ 1,5 s b");
        assert_eq!(display("Uwaga[[pauza:1500]]koniec"), "Uwaga ⏸ 1,5 s koniec");
        assert_eq!(display("[[pauza:2000]]"), "⏸ 2 s");
        assert_eq!(display("a[[pauza:x]]b"), "ab");
        assert_eq!(display("a [[pauza:5"), "a ");
        assert_eq!(display("zwykły"), "zwykły");
    }
}
