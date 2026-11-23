//! Straże wyglądu panelu: kontrast palety, kompletność ikon, etykiety pól
//! i brak skryptów inline. Czytają `assets/` i `templates/` wprost z dysku.

use std::collections::{BTreeMap, BTreeSet};

fn css() -> String {
    let path = format!("{}/../../assets/app.css", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

fn sprite() -> String {
    let path = format!("{}/../../assets/icons.svg", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

/// Nazwa i treść każdego szablonu z `templates/`.
fn templates() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        out.push((name, std::fs::read_to_string(&path).unwrap()));
    }
    out.sort();
    out
}

/// Tokeny `--nazwa: #rrggbb;` z bloku zaczynającego się od `selector`
/// (do pierwszego `}`). Wartości, które nie są kolorem, są pomijane.
fn tokens(css: &str, selector: &str) -> BTreeMap<String, [u8; 3]> {
    let start = css
        .find(selector)
        .unwrap_or_else(|| panic!("brak bloku {selector}"))
        + selector.len();
    let body = &css[start..];
    let body = &body[..body.find('}').unwrap()];
    let mut map = BTreeMap::new();
    for decl in body.split(';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if let Some(hex) = value.strip_prefix('#')
            && hex.len() == 6
            && let Ok(n) = u32::from_str_radix(hex, 16)
        {
            let rgb = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            map.insert(name.trim_start_matches("--").to_owned(), rgb);
        }
    }
    map
}

fn luminance(c: [u8; 3]) -> f64 {
    let lin = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// Stosunek kontrastu WCAG 2.x.
fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

const LIGHT: &str = ":root {";
const DARK: &str = ":root[data-theme=\"dark\"] {";
const DARK_SYSTEM: &str = ":root:not([data-theme=\"light\"]) {";

#[test]
fn the_palette_meets_wcag_aa_in_both_themes() {
    let css = css();
    let text_pairs = [
        ("text", "bg"),
        ("text", "surface"),
        ("text", "surface-2"),
        ("muted", "bg"),
        ("muted", "surface"),
        ("accent", "bg"),
        ("accent", "surface"),
        ("on-accent", "accent"),
        ("on-danger", "danger"),
        ("ok", "ok-bg"),
        ("warn", "warn-bg"),
        ("error", "error-bg"),
        ("disabled", "disabled-bg"),
        ("error", "surface"),
    ];
    let outline_pairs = [("focus", "bg"), ("focus", "surface")];
    for (theme, selector) in [("jasny", LIGHT), ("ciemny", DARK)] {
        let t = tokens(&css, selector);
        for (pairs, min) in [(&text_pairs[..], 4.5), (&outline_pairs[..], 3.0)] {
            for (fg, bg) in pairs {
                let (Some(f), Some(b)) = (t.get(*fg), t.get(*bg)) else {
                    panic!("motyw {theme}: brak tokenu {fg} albo {bg}");
                };
                let ratio = contrast(*f, *b);
                assert!(
                    ratio >= min,
                    "motyw {theme}: {fg}/{bg} = {ratio:.2} (min {min})"
                );
            }
        }
    }
}

#[test]
fn the_dark_palette_is_the_same_for_the_system_and_the_manual_switch() {
    let css = css();
    let system = tokens(&css, DARK_SYSTEM);
    assert!(!system.is_empty());
    assert_eq!(system, tokens(&css, DARK));
}

#[test]
fn every_colour_token_is_defined_in_both_themes() {
    let css = css();
    let light: BTreeSet<_> = tokens(&css, LIGHT).into_keys().collect();
    let dark: BTreeSet<_> = tokens(&css, DARK).into_keys().collect();
    assert_eq!(light, dark);
    for need in [
        "bg",
        "surface",
        "surface-2",
        "line",
        "text",
        "muted",
        "accent",
        "on-accent",
        "danger",
        "on-danger",
        "ok",
        "ok-bg",
        "warn",
        "warn-bg",
        "error",
        "error-bg",
        "disabled",
        "disabled-bg",
        "focus",
    ] {
        assert!(light.contains(need), "brak tokenu --{need}");
    }
    // kolory wyłącznie jako #rrggbb: bez rgb()/rgba()/hsl() i skrótów #rgb
    for bad in ["rgb(", "rgba(", "hsl(", "hsla("] {
        assert!(!css.contains(bad), "app.css używa {bad}");
    }
    for line in css.lines().filter(|l| l.contains(": #")) {
        let hex = line.split(": #").nth(1).unwrap_or_default();
        let digits = hex.chars().take_while(char::is_ascii_hexdigit).count();
        assert_eq!(digits, 6, "kolor nie jest #rrggbb: {line}");
    }
}

#[test]
fn the_icon_sprite_has_every_icon_the_templates_use() {
    let sprite = sprite();
    for (name, text) in templates() {
        for part in text.split("icons.svg#i-").skip(1) {
            let id: String = part
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            assert!(
                sprite.contains(&format!("id=\"i-{id}\"")),
                "{name}: brak ikony i-{id} w icons.svg"
            );
        }
        for part in text.split("<svg class=\"icon\"").skip(1) {
            let tag = &part[..part.find('>').unwrap_or(part.len())];
            assert!(
                tag.contains("aria-hidden=\"true\""),
                "{name}: ikona bez aria-hidden"
            );
        }
    }
    let symbols = sprite.matches("<symbol").count();
    assert!(symbols >= 15, "za mało ikon: {symbols}");
    for part in sprite.split("<symbol").skip(1) {
        let tag = &part[..part.find('>').unwrap()];
        assert!(
            tag.contains("viewBox=\"0 0 24 24\""),
            "symbol bez viewBox 24x24: {tag}"
        );
    }
}

#[test]
fn every_form_field_has_a_label() {
    for (name, text) in templates() {
        for tag_name in ["<input", "<select", "<textarea"] {
            for (at, _) in text.match_indices(tag_name) {
                let tag = &text[at..at + text[at..].find('>').unwrap()];
                if ["hidden", "submit", "button"]
                    .iter()
                    .any(|t| tag.contains(&format!("type=\"{t}\"")))
                {
                    continue;
                }
                let labelled_by_for = tag.split("id=\"").nth(1).is_some_and(|rest| {
                    let id = rest.split('"').next().unwrap_or_default();
                    text.contains(&format!("for=\"{id}\""))
                });
                let aria = tag.contains("aria-label=");
                let before = &text[..at];
                let inside_label = before
                    .rfind("<label")
                    .is_some_and(|open| before.rfind("</label>").is_none_or(|close| close < open));
                assert!(
                    labelled_by_for || aria || inside_label,
                    "{name}: pole bez etykiety: {tag}>"
                );
            }
        }
    }
}

#[test]
fn no_template_adds_an_inline_script() {
    let allowed = ["compose.html", "message_form.html", "modules.html"];
    for (name, text) in templates() {
        if allowed.contains(&name.as_str()) {
            continue;
        }
        let inline = text.match_indices("<script").any(|(at, _)| {
            !text[at..text[at..].find('>').map_or(text.len(), |e| at + e)].contains("src=")
        });
        assert!(!inline, "{name}: skrypt inline");
    }
}

/// Szablony bez atrybutu `style=`; lista rośnie z kolejnymi zadaniami.
const RESTYLED: &[&str] = &[
    "base.html",
    "dashboard.html",
    "compose.html",
    "messages.html",
    "message_form.html",
    "queue.html",
    "queue_fragment.html",
    "status_bar.html",
    "macros.html",
    "sources.html",
    "modules.html",
    "map.html",
];

#[test]
fn restyled_templates_have_no_inline_styles() {
    let all = templates();
    for name in RESTYLED {
        let Some((_, text)) = all.iter().find(|(n, _)| n == name) else {
            panic!("brak szablonu {name}");
        };
        assert!(!text.contains("style="), "{name}: atrybut style=");
    }
}
