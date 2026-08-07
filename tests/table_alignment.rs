//! Every table here colours its cells, and comfy-table measures a cell's width
//! over the raw string unless `custom_styling` is on. With the feature off a
//! coloured cell counts its escape sequences as printed characters, so it is
//! padded short and the row stops before the border.
//!
//! This is a test about a Cargo feature, which is why it renders a table rather
//! than calling into the CLI: dropping the feature breaks every table in the
//! binary and nothing else, so nothing else would fail.

use comfy_table::presets::UTF8_FULL;
use comfy_table::Table;

/// Printed width, with ANSI escape sequences discarded.
fn visible_width(line: &str) -> usize {
    let mut width = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Step over the '[' introducer before scanning, because it is
            // itself inside the final-byte range and would end the sequence
            // immediately.
            let mut rest = chars.by_ref().skip_while(|c| *c == '[');
            // Parameter and intermediate bytes run to a final byte, @ to ~.
            for c in rest.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

#[test]
fn a_coloured_cell_is_padded_to_the_same_width_as_a_plain_one() {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(vec!["Metric", "Value"]);

    // The colours the metrics table actually uses.
    table.add_row(vec![
        "\u{1b}[2mStatus\u{1b}[0m".to_string(),
        "\u{1b}[32mHEALTHY\u{1b}[0m".to_string(),
    ]);
    table.add_row(vec![
        "\u{1b}[2mURL\u{1b}[0m".to_string(),
        "\u{1b}[36mhttp://127.0.0.1:8090\u{1b}[0m".to_string(),
    ]);
    // Uncoloured, so it is padded correctly either way and gives the others
    // something to disagree with.
    table.add_row(vec!["Binding".to_string(), "bound".to_string()]);

    let rendered = table.to_string();
    let widths: Vec<usize> = rendered.lines().map(visible_width).collect();

    let first = widths[0];
    assert!(
        widths.iter().all(|w| *w == first),
        "table rows are not all {first} wide: {widths:?}\n{rendered}"
    );
}

#[test]
fn visible_width_ignores_escape_sequences() {
    assert_eq!(visible_width("plain"), 5);
    assert_eq!(visible_width("\u{1b}[32mplain\u{1b}[0m"), 5);
    assert_eq!(visible_width("\u{1b}[1;36mab\u{1b}[0mcd"), 4);
}
