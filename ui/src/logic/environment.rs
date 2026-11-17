//! The environment tag's colours: the configured fill, with black or white
//! text, whichever contrasts more (WCAG relative luminance), so the tag reads
//! the same on the light and dark themes.

use buckle_api::Environment;

/// Inline custom properties for the tag, or `None` for the neutral tag.
pub fn tag_style(env: &Environment) -> Option<String> {
    let (r, g, b) = env.rgb()?;
    let ink = if luminance(r, g, b) > 0.179 {
        "#000"
    } else {
        "#fff"
    };
    let fill = format!("#{r:02x}{g:02x}{b:02x}");
    Some(format!(
        "--yi-tag-bg: {fill}; --yi-tag-border: {fill}; --yi-tag-fg: {ink};"
    ))
}

/// The tab title: the environment first, so it shows in a narrow tab.
pub fn page_title(env: Option<&Environment>) -> String {
    match env {
        Some(env) => format!("{} · Buckle", env.name),
        None => "Buckle".into(),
    }
}

fn luminance(r: u8, g: u8, b: u8) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(colour: Option<&str>) -> Environment {
        Environment {
            name: "Production".into(),
            colour: colour.map(Into::into),
        }
    }

    #[test]
    fn text_is_whichever_of_black_and_white_reads_on_the_fill() {
        for (colour, ink) in [
            ("#c62828", "#fff"),
            ("#1565c0", "#fff"),
            ("#000", "#fff"),
            ("#ffeb3b", "#000"),
            ("#fff", "#000"),
            ("#66bb6a", "#000"),
        ] {
            let style = tag_style(&env(Some(colour))).unwrap();
            assert!(
                style.contains(&format!("--yi-tag-fg: {ink};")),
                "{colour}: {style}"
            );
        }
    }

    #[test]
    fn without_a_usable_colour_the_tag_is_neutral() {
        assert_eq!(tag_style(&env(None)), None);
        assert_eq!(tag_style(&env(Some("red"))), None);
    }

    #[test]
    fn the_title_leads_with_the_environment() {
        assert_eq!(page_title(None), "Buckle");
        assert!(page_title(Some(&env(None))).starts_with("Production"));
    }
}
