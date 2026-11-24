//! Znacznik pauzy w tekście komunikatu: `[[pauza:MS]]`. Synteza go nie widzi:
//! tekst jest dzielony na odcinki mowy i ciszy, zanim trafi do syntezatora.

use std::time::Duration;

const OPEN: &str = "[[pauza:";
const CLOSE: &str = "]]";
/// Górna granica jednej pauzy.
pub const MAX_PAUSE_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Pause(Duration),
}

/// Czy tekst zawiera początek znacznika pauzy.
pub fn has_marker(text: &str) -> bool {
    text.contains(OPEN)
}

/// Dzieli tekst na odcinki. Pauza ponad limit jest obcinana do 10 s, błędny
/// znacznik (nie-liczba) jest usuwany z ostrzeżeniem w logu. Puste odcinki
/// tekstu są pomijane.
pub fn split(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Segment>| {
        if !buf.trim().is_empty() {
            out.push(Segment::Text(std::mem::take(buf)));
        } else {
            buf.clear();
        }
    };
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else { break };
        buf.push_str(&rest[..start]);
        let arg = after[..end].trim();
        match parse_ms(arg) {
            Some(ms) => {
                flush(&mut buf, &mut out);
                out.push(Segment::Pause(Duration::from_millis(ms.min(MAX_PAUSE_MS))));
            }
            None => tracing::warn!(znacznik = arg, "błędny znacznik pauzy, usunięty z tekstu"),
        }
        rest = &after[end + CLOSE.len()..];
    }
    buf.push_str(rest);
    flush(&mut buf, &mut out);
    out
}

fn parse_ms(arg: &str) -> Option<u64> {
    if arg.is_empty() || !arg.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Za duża liczba (przepełnienie) to też „ponad limit”.
    Some(arg.parse::<u64>().unwrap_or(MAX_PAUSE_MS))
}

/// Tekst do wyświetlenia (panel, dziennik): znacznik jako „⏸ 1,5 s”,
/// błędny znacznik usunięty.
pub fn display(text: &str) -> String {
    if !has_marker(text) {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else { break };
        out.push_str(&rest[..start]);
        if let Some(ms) = parse_ms(after[..end].trim()) {
            let ms = ms.min(MAX_PAUSE_MS);
            let label = if ms % 1000 == 0 {
                format!("{}", ms / 1000)
            } else {
                format!("{:.1}", ms as f64 / 1000.0).replace('.', ",")
            };
            out.push_str(&format!("⏸ {label} s"));
        }
        rest = &after[end + CLOSE.len()..];
    }
    out.push_str(rest);
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
    fn unclosed_marker_stays_text() {
        assert_eq!(
            split("a [[pauza:5"),
            vec![Segment::Text("a [[pauza:5".into())]
        );
    }

    #[test]
    fn display_renders_marker_compactly() {
        assert_eq!(display("a [[pauza:1500]] b"), "a ⏸ 1,5 s b");
        assert_eq!(display("[[pauza:2000]]"), "⏸ 2 s");
        assert_eq!(display("a[[pauza:x]]b"), "ab");
        assert_eq!(display("zwykły"), "zwykły");
    }
}
