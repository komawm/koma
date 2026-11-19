//! Compositor-rendered keybind overview (`Mod + Shift + Slash`).
//!
//! The overlay is not a shell-out: it draws a dimmed scrim and a rounded multi-column panel on
//! the focused output from the same effective-keymap merge that backs `komactl keys`. While it is
//! open it owns the keyboard grab, so `Escape` and the bound chord reach the overlay instead of
//! the focused client, and pointer buttons dismiss it without reaching clients.

use std::cell::RefCell;

use koma_lua::{KEYBIND_CATALOG, KEYBIND_GROUP_ORDER, merge_binds};
use koma_model::{
    KeybindOverlayLayout, KeybindOverlayRow, KeybindOverlayTextRole, LogicalPoint, LogicalViewport,
    layout_keybind_overlay_page,
};
use koma_render::elements::{
    KomaRenderElement, KomaRenderTarget, KomaSolidColorElement, TextPanelLine, TextPanelSpec,
    rasterize_text_panel_buffer, render_text_panel_buffer,
};
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Point, Rectangle, Scale, Size};

use crate::state::KomaState;

/// Full-output scrim drawn behind the panel.
pub const COLOR_KEYBIND_OVERLAY_SCRIM: [f32; 4] = [0.035, 0.043, 0.063, 0.62];
const COLOR_TITLE: [u8; 4] = [240, 246, 252, 255];
const COLOR_HINT: [u8; 4] = [139, 148, 158, 255];
const COLOR_GROUP: [u8; 4] = [88, 166, 255, 255];
const COLOR_CHORD: [u8; 4] = [240, 246, 252, 255];
const COLOR_DESCRIPTION: [u8; 4] = [139, 148, 158, 255];
const COLOR_MORE: [u8; 4] = [210, 168, 255, 255];
const PANEL_CORNER_RADIUS: u32 = 14;

#[derive(Debug, Clone)]
struct CachedOverlayPanel {
    viewport_w: u32,
    viewport_h: u32,
    page: usize,
    int_scale: i32,
    panel_loc: LogicalPoint,
    buffer: MemoryRenderBuffer,
}

/// Open overlay state: the output it was requested on, active page, and cached rasterized buffer.
#[derive(Debug, Clone)]
pub struct KeybindOverlay {
    /// Focused output at open time; `None` means no output was focused and every output draws it.
    pub output: Option<String>,
    pub rows: Vec<KeybindOverlayRow>,
    pub page: usize,
    cached_panel: RefCell<Option<CachedOverlayPanel>>,
}

impl KeybindOverlay {
    #[must_use]
    pub fn should_draw_on(&self, output_name: &str) -> bool {
        self.output
            .as_deref()
            .is_none_or(|name| name == output_name)
    }
}

fn group_rank(group: &str) -> usize {
    KEYBIND_GROUP_ORDER
        .iter()
        .position(|known| *known == group)
        .unwrap_or(KEYBIND_GROUP_ORDER.len())
}

/// Builds overlay rows from the live configuration registry, in catalogue group order.
#[must_use]
pub fn effective_overlay_rows(state: &KomaState) -> Vec<KeybindOverlayRow> {
    let registry = state.lua_sandbox.config_snapshot();
    let config_path = state.config_path.active_init_path();
    let mut rows: Vec<KeybindOverlayRow> = merge_binds(&registry, &config_path, KEYBIND_CATALOG)
        .into_iter()
        .filter(|bind| bind.enabled)
        .map(|bind| KeybindOverlayRow {
            group: bind.group,
            chord: bind.chord,
            description: bind.description,
        })
        .collect();
    rows.sort_by_key(|row| group_rank(row.group));
    rows
}

impl KomaState {
    #[must_use]
    pub fn keybind_overlay_is_open(&self) -> bool {
        self.keybind_overlay.is_some()
    }

    /// Opens the overlay unless the session is locked/abandoned or it is already open.
    pub fn open_keybind_overlay(&mut self) {
        if self.is_locked_or_abandoned() || self.keybind_overlay.is_some() {
            return;
        }
        let rows = effective_overlay_rows(self);
        if rows.is_empty() {
            return;
        }
        self.keybind_overlay = Some(KeybindOverlay {
            output: self
                .focused_output_name
                .clone()
                .or_else(|| self.focused_output_name()),
            rows,
            page: 0,
            cached_panel: RefCell::new(None),
        });
        self.keybinds.reset_scroll_trackers();
        self.queue_redraw();
    }

    /// Closes the overlay, returning whether it was open.
    pub fn dismiss_keybind_overlay(&mut self) -> bool {
        let dismissed = self.keybind_overlay.take().is_some();
        if dismissed {
            self.keybinds.reset_scroll_trackers();
            self.queue_redraw();
        }
        dismissed
    }

    /// Toggles the overlay; the bound chord uses this so pressing it again closes.
    pub fn toggle_keybind_overlay(&mut self) {
        if self.keybind_overlay.is_some() {
            self.dismiss_keybind_overlay();
        } else {
            self.open_keybind_overlay();
        }
    }

    /// Handles a key while the overlay owns the keyboard grab. Returns `true` when the key was
    /// consumed by the overlay (always, while open); `Escape` also dismisses it.
    pub fn handle_keybind_overlay_key(&mut self, escape_pressed: bool) -> bool {
        if self.keybind_overlay.is_none() {
            return false;
        }
        if escape_pressed {
            self.dismiss_keybind_overlay();
        }
        true
    }

    /// Advances or rewinds the open keybind overlay page by `delta` (`+1` / `-1`), wrapping cleanly
    /// across `total_pages`.
    pub fn step_keybind_overlay_page(&mut self, delta: isize) -> bool {
        let Some(overlay) = self.keybind_overlay.as_ref() else {
            return false;
        };
        let rows = overlay.rows.clone();
        let current_page = overlay.page;
        let viewport = self
            .focused_output()
            .map(|o| self.viewport_for_output(&o))
            .unwrap_or_else(|| LogicalViewport::new(1920, 1080, 1.0));
        let total_pages = layout_keybind_overlay_page(&rows, viewport, current_page)
            .total_pages
            .max(1);
        if total_pages <= 1 {
            return false;
        }
        let next_page = ((current_page as isize + delta).rem_euclid(total_pages as isize)) as usize;
        if let Some(overlay_mut) = self.keybind_overlay.as_mut() {
            overlay_mut.page = next_page;
            *overlay_mut.cached_panel.borrow_mut() = None;
        }
        self.queue_redraw();
        true
    }

    /// No-op unless the overlay is open on this output and the session is unlocked.
    #[must_use]
    pub fn keybind_overlay_layout(
        &self,
        output_name: &str,
        viewport: LogicalViewport,
    ) -> Option<KeybindOverlayLayout> {
        if self.is_locked_or_abandoned() {
            return None;
        }
        let overlay = self.keybind_overlay.as_ref()?;
        if !overlay.should_draw_on(output_name) {
            return None;
        }
        Some(layout_keybind_overlay_page(
            &overlay.rows,
            viewport,
            overlay.page,
        ))
    }
}

fn role_color(role: KeybindOverlayTextRole) -> [u8; 4] {
    match role {
        KeybindOverlayTextRole::Title => COLOR_TITLE,
        KeybindOverlayTextRole::Hint => COLOR_HINT,
        KeybindOverlayTextRole::Group => COLOR_GROUP,
        KeybindOverlayTextRole::Chord => COLOR_CHORD,
        KeybindOverlayTextRole::Description => COLOR_DESCRIPTION,
        KeybindOverlayTextRole::More => COLOR_MORE,
    }
}

/// Renders the overlay scene elements in front-to-back order (panel, then scrim).
///
/// Returns an empty vector when the overlay is closed, the session is locked/abandoned, or the
/// overlay was requested on another output.
#[must_use]
pub fn keybind_overlay_elements<R>(
    renderer: &mut R,
    state: &KomaState,
    output_name: &str,
    viewport: LogicalViewport,
    output_scale: Scale<f64>,
) -> Vec<KomaRenderElement<R>>
where
    R: KomaRenderTarget,
    R::TextureId: Send + Clone + 'static,
{
    let Some(layout) = state.keybind_overlay_layout(output_name, viewport) else {
        return Vec::new();
    };
    let Some(overlay) = state.keybind_overlay.as_ref() else {
        return Vec::new();
    };
    let int_scale = output_scale.x.ceil().clamp(1.0, 4.0) as i32;

    let mut cached = overlay.cached_panel.borrow_mut();
    let cache_hit = cached.as_ref().is_some_and(|c| {
        c.viewport_w == viewport.width
            && c.viewport_h == viewport.height
            && c.page == layout.page
            && c.int_scale == int_scale
    });
    if !cache_hit {
        let mut spec = TextPanelSpec::new(layout.panel.w, layout.panel.h, PANEL_CORNER_RADIUS);
        spec.lines = layout
            .texts
            .into_iter()
            .map(|text| TextPanelLine {
                x: text.x,
                y: text.y,
                text: text.text,
                color: role_color(text.role),
            })
            .collect();
        if let Some(buffer) = rasterize_text_panel_buffer(&spec, output_scale) {
            *cached = Some(CachedOverlayPanel {
                viewport_w: viewport.width,
                viewport_h: viewport.height,
                page: layout.page,
                int_scale,
                panel_loc: LogicalPoint {
                    x: f64::from(layout.panel.x),
                    y: f64::from(layout.panel.y),
                },
                buffer,
            });
        }
    }

    let mut elements = Vec::with_capacity(2);
    if let Some(entry) = cached.as_ref()
        && let Some(panel) =
            render_text_panel_buffer(renderer, &entry.buffer, entry.panel_loc, output_scale)
    {
        elements.push(panel);
    }
    let scrim = Rectangle::new(
        Point::from((0.0, 0.0)),
        Size::from((f64::from(viewport.width), f64::from(viewport.height))),
    );
    elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
        scrim,
        COLOR_KEYBIND_OVERLAY_SCRIM,
    )));
    elements
}

/// Prepends the overlay scene elements above the workspace but below cursor elements.
pub fn prepend_keybind_overlay_elements<R>(
    renderer: &mut R,
    state: &KomaState,
    output_name: &str,
    viewport: LogicalViewport,
    output_scale: Scale<f64>,
    elements: &mut Vec<KomaRenderElement<R>>,
) where
    R: KomaRenderTarget,
    R::TextureId: Send + Clone + 'static,
{
    let overlay = keybind_overlay_elements(renderer, state, output_name, viewport, output_scale);
    elements.splice(0..0, overlay);
}

#[cfg(test)]
mod tests {
    use koma_lua::KeybindCatalogEntry;
    use koma_model::LogicalViewport;

    use super::*;
    use crate::state::tests::make_headless_test_state;

    fn state() -> KomaState {
        let (_event_loop, _display, state) = make_headless_test_state();
        state
    }

    #[test]
    fn builtin_toggle_is_in_the_catalogue_and_listed_by_the_overlay() {
        assert!(KEYBIND_CATALOG.iter().any(|entry: &KeybindCatalogEntry| {
            entry.chord == "Mod + Shift + Slash"
                && entry.lua_action == "koma.act.overlay.keybinds_toggle()"
                && entry.description == "Show the keybind overview"
        }));
        let rows = effective_overlay_rows(&state());
        let row = rows
            .iter()
            .find(|row| row.chord == "Mod + Shift + Slash")
            .expect("overlay lists its own toggle");
        assert_eq!(row.description, "Show the keybind overview");
    }

    #[test]
    fn escape_dismisses_the_open_overlay() {
        let mut state = state();
        state.open_keybind_overlay();
        assert!(state.keybind_overlay_is_open());
        assert!(state.handle_keybind_overlay_key(true));
        assert!(!state.keybind_overlay_is_open());
    }

    #[test]
    fn pressing_the_bound_chord_again_closes_the_overlay() {
        let mut state = state();
        state.toggle_keybind_overlay();
        assert!(state.keybind_overlay_is_open());
        state.toggle_keybind_overlay();
        assert!(!state.keybind_overlay_is_open());
    }

    #[test]
    fn the_action_descriptor_toggles_through_the_dispatcher() {
        let mut state = state();
        crate::keybind_dispatcher::execute_action_descriptor(
            &mut state,
            koma_lua::ActionDescriptor::OverlayKeybindsToggle,
            false,
        );
        assert!(state.keybind_overlay_is_open());
        crate::keybind_dispatcher::execute_action_descriptor(
            &mut state,
            koma_lua::ActionDescriptor::OverlayKeybindsToggle,
            false,
        );
        assert!(!state.keybind_overlay_is_open());
    }

    #[test]
    fn a_pointer_click_dismisses_the_overlay() {
        let mut state = state();
        state.open_keybind_overlay();
        assert!(state.dismiss_keybind_overlay());
        assert!(!state.keybind_overlay_is_open());
        // A second dismissal is a no-op, not a panic.
        assert!(!state.dismiss_keybind_overlay());
    }

    #[test]
    fn a_locked_or_abandoned_session_refuses_to_open_the_overlay() {
        let mut state = state();
        state.session_lock.phase = crate::state::SessionLockPhase::Abandoned;
        state.open_keybind_overlay();
        assert!(!state.keybind_overlay_is_open());
        assert!(
            state
                .keybind_overlay_layout("primary", LogicalViewport::new(1920, 1080, 1.0))
                .is_none()
        );

        state.session_lock.phase = crate::state::SessionLockPhase::Unlocked;
        state.open_keybind_overlay();
        assert!(state.keybind_overlay_is_open());
        assert!(
            state
                .keybind_overlay_layout("primary", LogicalViewport::new(1920, 1080, 1.0))
                .is_some()
        );
    }

    #[test]
    fn opening_twice_does_not_stack_or_replace_the_overlay() {
        let mut state = state();
        state.open_keybind_overlay();
        let first_len = state.keybind_overlay.as_ref().expect("open").rows.len();
        state.open_keybind_overlay();
        assert_eq!(
            state
                .keybind_overlay
                .as_ref()
                .expect("still open")
                .rows
                .len(),
            first_len
        );
    }

    #[test]
    fn overlay_is_restricted_to_the_output_it_was_opened_on() {
        let mut state = state();
        state.focused_output_name = Some("DP-1".to_owned());
        state.open_keybind_overlay();
        assert!(
            state
                .keybind_overlay_layout("DP-1", LogicalViewport::new(1920, 1080, 1.0))
                .is_some()
        );
        assert!(
            state
                .keybind_overlay_layout("HDMI-A-1", LogicalViewport::new(1920, 1080, 1.0))
                .is_none()
        );
    }

    #[test]
    fn issue_123_step_page_wraps_and_updates_layout_page() {
        let mut state = state();
        state.open_keybind_overlay();
        assert!(state.keybind_overlay_is_open());
        let vp = LogicalViewport::new(800, 600, 1.0);
        let first = state.keybind_overlay_layout("DP-1", vp).unwrap();
        assert_eq!(first.page, 0);
        assert!(first.total_pages > 1);

        // Add enough synthetic rows so `step_keybind_overlay_page` spans multiple pages even on
        // a 1920x1080 headless output.
        if let Some(ov) = state.keybind_overlay.as_mut() {
            let extra: Vec<KeybindOverlayRow> = (0..220)
                .map(|i| {
                    KeybindOverlayRow::new("Session", format!("Mod + {i}"), format!("Act {i}"))
                })
                .collect();
            ov.rows = extra;
        }
        assert!(state.step_keybind_overlay_page(1));
        assert_eq!(state.keybind_overlay.as_ref().unwrap().page, 1);
        assert!(state.step_keybind_overlay_page(-1));
        assert_eq!(state.keybind_overlay.as_ref().unwrap().page, 0);
        // Wrapping backward from page 0 reaches the last page.
        assert!(state.step_keybind_overlay_page(-1));
        assert!(state.keybind_overlay.as_ref().unwrap().page >= 1);
    }
}
