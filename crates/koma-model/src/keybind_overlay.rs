//! Pure layout for the compositor-rendered keybind overview.
//!
//! The overlay lists the effective keymap returned by
//! [`koma_lua::effective_keymap::merge_binds`](https://github.com/komawm/koma) in multiple
//! columns sized to the focused output. When even the widest useful column count cannot fit every
//! row, the last column ends with a `+N more` row instead of silently dropping bindings.
//!
//! This module has no renderer dependency: it produces panel-relative text runs that
//! `koma-render` rasterises with its built-in 5x7 glyphs.

use crate::geometry::{LogicalRect, LogicalViewport};

/// Horizontal margin between the panel and the output edges (logical pixels).
pub const OVERLAY_MARGIN: u32 = 32;
/// Inner panel padding.
pub const OVERLAY_PADDING: u32 = 26;
/// Space between two columns.
pub const OVERLAY_GUTTER: u32 = 28;
/// Vertical pitch of one row, including group headers.
pub const OVERLAY_LINE_HEIGHT: u32 = 21;
/// Maximum characters reserved for a chord column (`Mod + Ctrl + Shift + R` compacted).
pub const OVERLAY_CHORD_CHARS: usize = 18;
/// Maximum characters reserved for a description column.
pub const OVERLAY_DESCRIPTION_CHARS: usize = 40;
/// Space between the chord and description areas of one column.
pub const OVERLAY_COLUMN_GAP: u32 = 16;
/// Header block above the first row.
pub const OVERLAY_TITLE_BLOCK: u32 = 34;
/// Footer block below the last row (close hint).
pub const OVERLAY_FOOTER_BLOCK: u32 = 22;
/// Glyph advance of the renderer's 5x7 font at its built-in 2x scale.
pub const OVERLAY_CHAR_ADVANCE: u32 = 12;
/// Smallest column that still leaves a useful description width.
const OVERLAY_MIN_COLUMN_WIDTH: u32 = 480;
/// Hard cap on columns so the overlay stays scannable on ultra-wide outputs.
const OVERLAY_MAX_COLUMNS: usize = 4;

/// One effective binding shown by the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindOverlayRow {
    pub group: &'static str,
    pub chord: String,
    pub description: String,
}

impl KeybindOverlayRow {
    #[must_use]
    pub fn new(
        group: &'static str,
        chord: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            group,
            chord: chord.into(),
            description: description.into(),
        }
    }
}

/// Role of a text run, used by the renderer to choose a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindOverlayTextRole {
    Title,
    Hint,
    Group,
    Chord,
    Description,
    More,
}

/// A panel-relative text run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindOverlayText {
    /// X offset inside the panel in logical pixels.
    pub x: u32,
    /// Y offset inside the panel in logical pixels (top of the glyph box).
    pub y: u32,
    pub text: String,
    pub role: KeybindOverlayTextRole,
}

/// Result of laying out the effective keymap for one output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindOverlayLayout {
    /// Panel rectangle in output-local logical coordinates.
    pub panel: LogicalRect,
    pub texts: Vec<KeybindOverlayText>,
    /// Number of columns used.
    pub columns: usize,
    pub rows_per_column: usize,
    /// Rows that did not fit and were replaced by the final `+N more` row.
    pub hidden: usize,
}

impl KeybindOverlayLayout {
    /// Visible binding rows (group headers and the `+N more` row excluded).
    #[must_use]
    pub fn visible_bind_count(&self) -> usize {
        self.texts
            .iter()
            .filter(|text| text.role == KeybindOverlayTextRole::Chord)
            .count()
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 2 {
        return text.chars().take(max_chars).collect();
    }
    let mut out: String = text.chars().take(max_chars - 2).collect();
    out.push_str("..");
    out
}

/// Compact chord for the overlay (`Mod + Shift + Slash` -> `Mod+Shift+Slash`).
#[must_use]
pub fn compact_chord(chord: &str) -> String {
    chord.replace(" + ", "+")
}

/// Lays the effective bindings out on `viewport`.
///
/// `rows` must already be ordered by presentation group; a group header is emitted whenever the
/// group name changes.
#[must_use]
pub fn layout_keybind_overlay(
    rows: &[KeybindOverlayRow],
    viewport: LogicalViewport,
) -> KeybindOverlayLayout {
    let viewport_w = viewport.width.max(1);
    let viewport_h = viewport.height.max(1);
    let margin_w = viewport_w.saturating_sub(2 * OVERLAY_MARGIN);
    let available_width = margin_w.saturating_sub(2 * OVERLAY_PADDING);
    let available_height = viewport_h
        .saturating_sub(2 * OVERLAY_MARGIN + 2 * OVERLAY_PADDING)
        .saturating_sub(OVERLAY_TITLE_BLOCK + OVERLAY_FOOTER_BLOCK);
    let rows_per_column = (available_height / OVERLAY_LINE_HEIGHT).max(1) as usize;

    // Chord/description widths shrink to the widest content, so a plain default keymap does not
    // reserve an empty half-panel.
    let longest_chord = rows
        .iter()
        .map(|row| compact_chord(&row.chord).chars().count())
        .max()
        .unwrap_or(0)
        .clamp(1, OVERLAY_CHORD_CHARS);
    let longest_description = rows
        .iter()
        .map(|row| row.description.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(1, OVERLAY_DESCRIPTION_CHARS);
    let chord_area = longest_chord as u32 * OVERLAY_CHAR_ADVANCE;
    let natural_width =
        chord_area + OVERLAY_COLUMN_GAP + longest_description as u32 * OVERLAY_CHAR_ADVANCE;

    // Count header slots as well; they occupy the same vertical pitch as a bind row.
    let mut slots: Vec<(bool, Option<usize>)> = Vec::with_capacity(rows.len() + 8);
    let mut active_group: Option<&str> = None;
    for (index, row) in rows.iter().enumerate() {
        if active_group != Some(row.group) {
            active_group = Some(row.group);
            slots.push((true, None));
        }
        slots.push((false, Some(index)));
    }
    let total_slots = slots.len();

    let needed_columns = total_slots.div_ceil(rows_per_column).max(1);
    let max_columns_by_width = ((available_width + OVERLAY_GUTTER)
        / (OVERLAY_MIN_COLUMN_WIDTH + OVERLAY_GUTTER))
        .max(1) as usize;
    let columns = needed_columns
        .min(max_columns_by_width)
        .clamp(1, OVERLAY_MAX_COLUMNS);

    let visible_slots = if total_slots <= columns * rows_per_column {
        total_slots
    } else {
        // Reserve the last slot for the `+N more` row.
        (columns * rows_per_column).saturating_sub(1)
    };
    // `hidden` counts bindings only: a dropped group header must not inflate `+N more`.
    let visible_bindings = slots[..visible_slots]
        .iter()
        .filter(|(is_header, _)| !is_header)
        .count();
    let hidden = rows.len() - visible_bindings;

    let column_width = (((available_width
        .saturating_sub((columns.saturating_sub(1) as u32) * OVERLAY_GUTTER))
        / columns as u32)
        .min(natural_width.max(OVERLAY_MIN_COLUMN_WIDTH)))
    .max(OVERLAY_MIN_COLUMN_WIDTH.min(available_width.max(1)));
    let description_chars = (((column_width.saturating_sub(chord_area + OVERLAY_COLUMN_GAP))
        / OVERLAY_CHAR_ADVANCE) as usize)
        .clamp(1, OVERLAY_DESCRIPTION_CHARS);

    // Columns fill top-to-bottom, so the tallest column is the first one until the last partial
    // column; with truncation every column is full.
    let used_rows = if hidden > 0 {
        rows_per_column
    } else {
        visible_slots.min(rows_per_column).max(1)
    };
    let panel_width = columns as u32 * column_width
        + (columns.saturating_sub(1) as u32) * OVERLAY_GUTTER
        + 2 * OVERLAY_PADDING;
    let panel_height = 2 * OVERLAY_PADDING
        + OVERLAY_TITLE_BLOCK
        + used_rows as u32 * OVERLAY_LINE_HEIGHT
        + OVERLAY_FOOTER_BLOCK;
    let panel = LogicalRect::new(
        ((viewport_w.saturating_sub(panel_width)) / 2) as i32,
        ((viewport_h.saturating_sub(panel_height)) / 2) as i32,
        panel_width.min(viewport_w),
        panel_height.min(viewport_h),
    );

    let mut texts = Vec::with_capacity(total_slots.min(visible_slots) * 2 + 4);
    let mut title = String::from("Keybinds");
    title = truncate(
        &title,
        ((panel_width.saturating_sub(2 * OVERLAY_PADDING)) / OVERLAY_CHAR_ADVANCE) as usize,
    );
    texts.push(KeybindOverlayText {
        x: OVERLAY_PADDING,
        y: OVERLAY_PADDING,
        text: title,
        role: KeybindOverlayTextRole::Title,
    });

    let first_row_y = OVERLAY_PADDING + OVERLAY_TITLE_BLOCK;
    let description_x = |column: usize| {
        OVERLAY_PADDING
            + column as u32 * (column_width + OVERLAY_GUTTER)
            + chord_area
            + OVERLAY_COLUMN_GAP
    };
    let column_x =
        |column: usize| OVERLAY_PADDING + column as u32 * (column_width + OVERLAY_GUTTER);

    for slot in 0..visible_slots {
        let column = slot / rows_per_column;
        let row_in_column = slot % rows_per_column;
        let y = first_row_y + row_in_column as u32 * OVERLAY_LINE_HEIGHT;
        let (is_header, row_index) = slots[slot];
        if is_header {
            // Header slots carry the group of the following bind row.
            let group = slots[slot + 1]
                .1
                .map(|index| rows[index].group)
                .unwrap_or("");
            texts.push(KeybindOverlayText {
                x: column_x(column),
                y,
                text: truncate(group, (column_width / OVERLAY_CHAR_ADVANCE) as usize),
                role: KeybindOverlayTextRole::Group,
            });
            continue;
        }
        let row = &rows[row_index.expect("bind slot")];
        texts.push(KeybindOverlayText {
            x: column_x(column),
            y,
            text: truncate(
                &compact_chord(&row.chord),
                OVERLAY_CHORD_CHARS.min((chord_area / OVERLAY_CHAR_ADVANCE) as usize),
            ),
            role: KeybindOverlayTextRole::Chord,
        });
        texts.push(KeybindOverlayText {
            x: description_x(column),
            y,
            text: truncate(&row.description, description_chars),
            role: KeybindOverlayTextRole::Description,
        });
    }

    if hidden > 0 {
        let column = columns - 1;
        let row_in_column = visible_slots - column * rows_per_column;
        texts.push(KeybindOverlayText {
            x: column_x(column),
            y: first_row_y + row_in_column as u32 * OVERLAY_LINE_HEIGHT,
            text: format!("+{hidden} more"),
            role: KeybindOverlayTextRole::More,
        });
    }

    let hint = "Esc or click to close | Mod + Shift + Slash toggles";
    texts.push(KeybindOverlayText {
        x: OVERLAY_PADDING,
        y: OVERLAY_PADDING + OVERLAY_TITLE_BLOCK + used_rows as u32 * OVERLAY_LINE_HEIGHT + 4,
        text: truncate(
            hint,
            ((panel_width.saturating_sub(2 * OVERLAY_PADDING)) / OVERLAY_CHAR_ADVANCE) as usize,
        ),
        role: KeybindOverlayTextRole::Hint,
    });

    KeybindOverlayLayout {
        panel,
        texts,
        columns,
        rows_per_column,
        hidden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(count: usize) -> Vec<KeybindOverlayRow> {
        (0..count)
            .map(|index| {
                KeybindOverlayRow::new(
                    if index < 4 {
                        "Windows"
                    } else {
                        "Focus and movement"
                    },
                    format!("Mod + Shift + {index}"),
                    format!("Synthetic binding number {index}"),
                )
            })
            .collect()
    }

    fn viewport(width: u32, height: u32) -> LogicalViewport {
        LogicalViewport::new(width, height, 1.0)
    }

    #[test]
    fn wide_viewport_places_all_rows_in_two_columns() {
        let rows = rows(80);
        let layout = layout_keybind_overlay(&rows, viewport(1920, 1080));
        assert_eq!(layout.columns, 2);
        assert_eq!(layout.hidden, 0);
        assert_eq!(layout.visible_bind_count(), 80);
        assert_eq!(layout.visible_bind_count() + layout.hidden, rows.len());
        let mut column_origins: Vec<u32> = layout
            .texts
            .iter()
            .filter(|text| text.role == KeybindOverlayTextRole::Chord)
            .map(|text| text.x)
            .collect();
        column_origins.sort_unstable();
        column_origins.dedup();
        assert_eq!(column_origins.len(), 2);
    }

    #[test]
    fn narrow_viewport_reports_an_exact_more_count_and_keeps_every_visible_row() {
        let rows = rows(100);
        let layout = layout_keybind_overlay(&rows, viewport(800, 600));
        assert_eq!(layout.columns, 1);
        assert!(layout.hidden > 0);
        assert_eq!(
            layout.visible_bind_count() + layout.hidden,
            100,
            "every binding is either visible or counted in +N more"
        );
        assert!(layout.hidden > layout.visible_bind_count());
        let more = layout
            .texts
            .iter()
            .find(|text| text.role == KeybindOverlayTextRole::More)
            .expect("+N more row");
        assert_eq!(more.text, format!("+{} more", layout.hidden));
    }

    #[test]
    fn group_headers_are_emitted_once_per_group_in_order() {
        let layout = layout_keybind_overlay(&rows(6), viewport(1920, 1080));
        let groups: Vec<&str> = layout
            .texts
            .iter()
            .filter(|text| text.role == KeybindOverlayTextRole::Group)
            .map(|text| text.text.as_str())
            .collect();
        assert_eq!(groups, vec!["Windows", "Focus and movement"]);
    }

    #[test]
    fn panel_is_centered_and_never_leaves_the_viewport() {
        for (width, height) in [(1920, 1080), (1280, 720), (800, 600), (3840, 2160)] {
            let layout = layout_keybind_overlay(&rows(40), viewport(width, height));
            let panel = layout.panel;
            assert!(panel.x >= 0 && panel.y >= 0);
            assert!(panel.w <= width && panel.h <= height);
            assert!(panel.x * 2 + panel.w as i32 <= width as i32 + 1);
            assert!(panel.y * 2 + panel.h as i32 <= height as i32 + 1);
            for text in &layout.texts {
                assert!(
                    text.x < panel.w && text.y < panel.h,
                    "text {:?} outside {}x{} panel",
                    text.text,
                    panel.w,
                    panel.h
                );
            }
        }
    }

    #[test]
    fn every_visible_binding_appears_exactly_once() {
        let rows = rows(64);
        let layout = layout_keybind_overlay(&rows, viewport(1920, 1080));
        let mut seen: Vec<&str> = layout
            .texts
            .iter()
            .filter(|text| text.role == KeybindOverlayTextRole::Chord)
            .map(|text| text.text.as_str())
            .collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total);
        assert_eq!(total, 64);
    }

    #[test]
    fn compact_chords_remove_spaces_and_truncate_long_ones() {
        assert_eq!(compact_chord("Mod + Shift + Slash"), "Mod+Shift+Slash");
        let long =
            KeybindOverlayRow::new("Session", "Mod + Ctrl + Alt + Shift + Super + Slash", "x");
        let layout = layout_keybind_overlay(&[long], viewport(1920, 1080));
        let chord = layout
            .texts
            .iter()
            .find(|text| text.role == KeybindOverlayTextRole::Chord)
            .unwrap();
        assert!(chord.text.ends_with(".."));
        assert_eq!(chord.text.chars().count(), OVERLAY_CHORD_CHARS);
    }
}
