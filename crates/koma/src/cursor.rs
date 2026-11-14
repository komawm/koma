//! Cursor theme loading, HiDPI scaling, animation playback, embedded 24x24 fallback bitmap,
//! and front-of-scene `Kind::Cursor` render element generation (`Winit` & `DRM/KMS`).

use std::{
    cell::RefCell, cmp::Reverse, collections::HashMap, env, fs::File, io::Read as _, rc::Rc,
};

use koma_render::elements::{KomaRenderElement, KomaRenderTarget};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::element::{
            Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
    },
    input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive as _, Logical, Physical, Point, Scale, Transform},
    wayland::compositor::{SurfaceAttributes, with_states},
};
use tracing::{debug, warn};
use xcursor::{CursorTheme, parser::Image};

pub const DEFAULT_CURSOR_THEME: &str = "default";
pub const DEFAULT_CURSOR_SIZE: u32 = 24;
pub const FALLBACK_CURSOR_DIM: u32 = 24;
pub const FALLBACK_CURSOR_HOTSPOT: (u32, u32) = (1, 1);

/// 24x24 crisp pointer arrow sprite (tip hotspot at `(1, 1)`), rendered when no XCursor theme
/// asset is installed (e.g. minimal containers or headless VMs).
///
/// `.` = transparent, `#` = opaque black outline, `@` = opaque white interior.
const FALLBACK_ARROW_PATTERN: [&str; 24] = [
    "........................",
    ".##.....................",
    ".#@#....................",
    ".#@@#...................",
    ".#@@@#..................",
    ".#@@@@#.................",
    ".#@@@@@#................",
    ".#@@@@@@#...............",
    ".#@@@@@@@#..............",
    ".#@@@@@@@@#.............",
    ".#@@@@@@@@@#............",
    ".#@@@@@@@@@@#...........",
    ".#@@@@@@@@@@@#..........",
    ".#@@@@@@######..........",
    ".#@@@#@@#...............",
    ".#@@##@@#...............",
    ".#@#..#@@#..............",
    ".##...#@@#..............",
    ".#.....#@@#.............",
    ".......#@@#.............",
    "........##..............",
    "........................",
    "........................",
    "........................",
];

/// Generates the embedded `24x24` `Fourcc::Argb8888` (`[B, G, R, A]`) fallback arrow image.
#[must_use]
pub fn fallback_arrow_image() -> Image {
    let mut pixels_rgba =
        Vec::with_capacity((FALLBACK_CURSOR_DIM * FALLBACK_CURSOR_DIM * 4) as usize);
    for row in FALLBACK_ARROW_PATTERN {
        debug_assert_eq!(row.len(), FALLBACK_CURSOR_DIM as usize);
        for byte in row.bytes() {
            match byte {
                b'#' => pixels_rgba.extend_from_slice(&[0_u8, 0, 0, 255]),
                b'@' => pixels_rgba.extend_from_slice(&[255_u8, 255, 255, 255]),
                _ => pixels_rgba.extend_from_slice(&[0_u8, 0, 0, 0]),
            }
        }
    }

    Image {
        size: FALLBACK_CURSOR_DIM,
        width: FALLBACK_CURSOR_DIM,
        height: FALLBACK_CURSOR_DIM,
        xhot: FALLBACK_CURSOR_HOTSPOT.0,
        yhot: FALLBACK_CURSOR_HOTSPOT.1,
        delay: 0,
        pixels_argb: Vec::new(),
        pixels_rgba,
    }
}

/// Parses `$XCURSOR_SIZE` into a strictly positive nominal cursor size (defaulting to `24`).
#[must_use]
pub fn parse_cursor_size_str(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_CURSOR_SIZE)
}

/// Resolves the `$XCURSOR_THEME` name, defaulting to `"default"`.
#[must_use]
pub fn parse_cursor_theme_str(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_CURSOR_THEME)
        .to_owned()
}

/// Integer ceiling HiDPI scale used for XCursor frame lookup and buffer caching.
#[must_use]
pub fn effective_cursor_scale(output_scale: f64) -> u32 {
    if !output_scale.is_finite() {
        return 1;
    }
    output_scale.ceil().max(1.0) as u32
}

/// Target cursor pixel size (`base_size * ceil(output_scale)`).
#[must_use]
pub fn target_cursor_pixel_size(base_size: u32, output_scale: f64) -> u32 {
    base_size
        .max(1)
        .saturating_mul(effective_cursor_scale(output_scale))
}

/// Selects all animation frames matching the XCursor size closest to `target_size`.
#[must_use]
pub fn select_nearest_size_frames(images: Vec<Image>, target_size: u32) -> Vec<Image> {
    let valid: Vec<Image> = images
        .into_iter()
        .filter(|img| img.width > 0 && img.height > 0)
        .collect();
    let Some(best) = valid
        .iter()
        .min_by_key(|img| (target_size.abs_diff(img.size), Reverse(img.size)))
    else {
        return Vec::new();
    };
    let chosen_w = best.width;
    let chosen_h = best.height;
    valid
        .into_iter()
        .filter(|img| img.width == chosen_w && img.height == chosen_h)
        .collect()
}

/// Computes the active animation frame index for `elapsed_millis`, wrapping at `total_duration_ms`.
#[must_use]
pub fn frame_index_for_millis(
    frame_delays_ms: &[u32],
    total_duration_ms: u32,
    elapsed_millis: u64,
) -> usize {
    if frame_delays_ms.len() <= 1 || total_duration_ms == 0 {
        return 0;
    }
    let mut rem = (elapsed_millis % u64::from(total_duration_ms)) as u32;
    for (idx, &delay) in frame_delays_ms.iter().enumerate() {
        if rem < delay {
            return idx;
        }
        rem -= delay;
    }
    frame_delays_ms.len() - 1
}

/// Computes milliseconds remaining until the next frame transition for an animated cursor.
#[must_use]
pub fn remaining_frame_delay_ms(
    frame_delays_ms: &[u32],
    total_duration_ms: u32,
    elapsed_millis: u64,
) -> Option<u64> {
    if frame_delays_ms.len() <= 1 || total_duration_ms == 0 {
        return None;
    }
    let mut rem = (elapsed_millis % u64::from(total_duration_ms)) as u32;
    for &delay in frame_delays_ms {
        if rem < delay {
            return Some(u64::from((delay - rem).max(1)));
        }
        rem -= delay;
    }
    Some(1)
}

#[derive(Debug, Clone)]
struct CursorFrameMeta {
    width: u32,
    height: u32,
    hotspot_logical: Point<f64, Logical>,
    pixels_argb_le: Vec<u8>,
}

#[derive(Debug, Clone)]
struct LoadedCursor {
    buffer_scale: i32,
    frames: Vec<CursorFrameMeta>,
    frame_delays_ms: Vec<u32>,
    total_duration_ms: u32,
}

impl LoadedCursor {
    fn from_images(images: Vec<Image>, base_size: u32) -> Option<Self> {
        if images.is_empty() {
            return None;
        }
        let nominal = images[0].size.max(images[0].width).max(1);
        let buffer_scale =
            ((f64::from(nominal) / f64::from(base_size.max(1))).round() as i32).max(1);
        let scale_f64 = f64::from(buffer_scale);

        let mut frames = Vec::with_capacity(images.len());
        let mut frame_delays_ms = Vec::with_capacity(images.len());
        let mut total_duration_ms: u32 = 0;

        for img in images {
            let expected_len = (img.width as usize)
                .saturating_mul(img.height as usize)
                .saturating_mul(4);
            if expected_len == 0 || img.pixels_rgba.len() < expected_len {
                continue;
            }
            // Note: XCursor `pixels_rgba` stores raw little-endian BGRA/ARGB8888 file bytes.
            let pixels_argb_le = img.pixels_rgba[..expected_len].to_vec();
            let hotspot_logical = Point::from((
                f64::from(img.xhot) / scale_f64,
                f64::from(img.yhot) / scale_f64,
            ));
            let delay = img.delay;
            frame_delays_ms.push(delay);
            total_duration_ms = total_duration_ms.saturating_add(delay);
            frames.push(CursorFrameMeta {
                width: img.width,
                height: img.height,
                hotspot_logical,
                pixels_argb_le,
            });
        }

        if frames.is_empty() {
            return None;
        }

        Some(Self {
            buffer_scale,
            frames,
            frame_delays_ms,
            total_duration_ms,
        })
    }
}

type NamedCursorCache = HashMap<(CursorIcon, u32), Option<Rc<LoadedCursor>>>;
type CursorBufferCache = HashMap<(CursorIcon, u32), Vec<MemoryRenderBuffer>>;

/// Manages XCursor theme loading, HiDPI/animated frame caching, Wayland cursor surfaces,
/// and top Z-order `Kind::Cursor` render element construction.
pub struct CursorManager {
    base_size: u32,
    theme: CursorTheme,
    status: CursorImageStatus,
    named_cache: RefCell<NamedCursorCache>,
    buffer_cache: RefCell<CursorBufferCache>,
}

impl CursorManager {
    /// Initializes cursor manager from `$XCURSOR_THEME` (default `"default"`) and
    /// `$XCURSOR_SIZE` (default `24`).
    #[must_use]
    pub fn new() -> Self {
        let theme_name = parse_cursor_theme_str(env::var("XCURSOR_THEME").ok().as_deref());
        let base_size = parse_cursor_size_str(env::var("XCURSOR_SIZE").ok().as_deref());
        debug!(theme = %theme_name, base_size, "initialized XCursor manager");
        let theme = CursorTheme::load(&theme_name);
        Self {
            base_size,
            theme,
            status: CursorImageStatus::default_named(),
            named_cache: RefCell::new(HashMap::new()),
            buffer_cache: RefCell::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn status(&self) -> &CursorImageStatus {
        &self.status
    }

    /// Updates active cursor image status.
    pub fn set_status(&mut self, status: CursorImageStatus) {
        self.status = status;
    }

    /// Drops an expired client `wl_surface` cursor back to `CursorImageStatus::default_named()`.
    pub fn refresh_alive(&mut self) {
        if let CursorImageStatus::Surface(surface) = &self.status
            && !surface.alive()
        {
            self.status = CursorImageStatus::default_named();
        }
    }

    /// Handles a Wayland commit on `surface`, applying any cursor `buffer_delta` hotspot shift
    /// and returning `true` when `surface` is the currently active cursor surface.
    pub fn on_surface_commit(&mut self, surface: &WlSurface, root_surface: &WlSurface) -> bool {
        self.refresh_alive();
        let is_active = matches!(
            &self.status,
            CursorImageStatus::Surface(active) if active == root_surface
        );
        if !is_active {
            return false;
        }

        if surface == root_surface {
            with_states(surface, |states| {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                let buffer_delta = cached.current().buffer_delta.take();
                if let Some(delta) = buffer_delta
                    && let Some(attrs) = states.data_map.get::<CursorImageSurfaceData>()
                    && let Ok(mut guard) = attrs.lock()
                {
                    guard.hotspot -= delta;
                }
            });
        }
        true
    }

    /// Returns milliseconds until the next animation frame if the active cursor is animated.
    #[must_use]
    pub fn next_frame_delay_ms(&self, output_scale: f64, elapsed_millis: u64) -> Option<u64> {
        let icon = match &self.status {
            CursorImageStatus::Named(icon) => *icon,
            CursorImageStatus::Hidden | CursorImageStatus::Surface(_) => return None,
        };
        let (_, cursor) = self.resolve_named_cursor(icon, output_scale);
        remaining_frame_delay_ms(
            &cursor.frame_delays_ms,
            cursor.total_duration_ms,
            elapsed_millis,
        )
    }

    fn resolve_named_cursor(
        &self,
        icon: CursorIcon,
        output_scale: f64,
    ) -> (CursorIcon, Rc<LoadedCursor>) {
        if let Some(loaded) = self.load_cached_icon(icon, output_scale) {
            return (icon, loaded);
        }
        if icon != CursorIcon::Default
            && let Some(loaded) = self.load_cached_icon(CursorIcon::Default, output_scale)
        {
            return (CursorIcon::Default, loaded);
        }
        let fallback = LoadedCursor::from_images(vec![fallback_arrow_image()], self.base_size)
            .expect("embedded 24x24 fallback arrow bitmap is valid");
        (CursorIcon::Default, Rc::new(fallback))
    }

    fn load_cached_icon(&self, icon: CursorIcon, output_scale: f64) -> Option<Rc<LoadedCursor>> {
        let int_scale = effective_cursor_scale(output_scale);
        let target_size = target_cursor_pixel_size(self.base_size, output_scale);
        if let Some(entry) = self.named_cache.borrow().get(&(icon, int_scale)) {
            return entry.clone();
        }

        let loaded = self
            .load_xcursor_icon(icon, target_size)
            .or_else(|| {
                if icon == CursorIcon::Default {
                    LoadedCursor::from_images(vec![fallback_arrow_image()], self.base_size)
                } else {
                    None
                }
            })
            .map(Rc::new);

        self.named_cache
            .borrow_mut()
            .insert((icon, int_scale), loaded.clone());
        loaded
    }

    fn load_xcursor_icon(&self, icon: CursorIcon, target_size: u32) -> Option<LoadedCursor> {
        let candidates = std::iter::once(icon.name()).chain(icon.alt_names().iter().copied());
        for name in candidates {
            let Some(path) = self.theme.load_icon(name) else {
                continue;
            };
            let mut file = match File::open(&path) {
                Ok(f) => f,
                Err(err) => {
                    warn!(path = %path.display(), "failed opening xcursor file: {err}");
                    continue;
                }
            };
            let mut bytes = Vec::new();
            if let Err(err) = file.read_to_end(&mut bytes) {
                warn!(path = %path.display(), "failed reading xcursor file: {err}");
                continue;
            }
            let Some(images) = xcursor::parser::parse_xcursor(&bytes) else {
                warn!(path = %path.display(), "failed parsing xcursor file");
                continue;
            };
            let selected = select_nearest_size_frames(images, target_size);
            if let Some(loaded) = LoadedCursor::from_images(selected, self.base_size) {
                return Some(loaded);
            }
        }
        None
    }

    fn frame_buffer(
        &self,
        icon: CursorIcon,
        int_scale: u32,
        cursor: &LoadedCursor,
        frame_idx: usize,
    ) -> MemoryRenderBuffer {
        let mut cache = self.buffer_cache.borrow_mut();
        let buffers = cache.entry((icon, int_scale)).or_insert_with(|| {
            cursor
                .frames
                .iter()
                .map(|frame| {
                    MemoryRenderBuffer::from_slice(
                        &frame.pixels_argb_le,
                        Fourcc::Argb8888,
                        (frame.width as i32, frame.height as i32),
                        cursor.buffer_scale,
                        Transform::Normal,
                        None,
                    )
                })
                .collect()
        });
        buffers[frame_idx].clone()
    }

    /// Produces front-most `Kind::Cursor` render elements for the active cursor state.
    pub fn render_elements<R>(
        &mut self,
        renderer: &mut R,
        pointer_location: Point<f64, Logical>,
        output_scale: Scale<f64>,
        elapsed_millis: u64,
    ) -> Vec<KomaRenderElement<R>>
    where
        R: KomaRenderTarget,
        R::TextureId: Clone + Send + 'static,
    {
        self.refresh_alive();

        match &self.status {
            CursorImageStatus::Hidden => Vec::new(),
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .and_then(|attrs| attrs.lock().ok().map(|guard| guard.hotspot))
                        .unwrap_or_default()
                });
                let loc_phys: Point<i32, Physical> =
                    (pointer_location - hotspot.to_f64()).to_physical_precise_round(output_scale);
                let elems: Vec<WaylandSurfaceRenderElement<R>> = render_elements_from_surface_tree(
                    renderer,
                    surface,
                    loc_phys,
                    output_scale,
                    1.0,
                    Kind::Cursor,
                );
                elems.into_iter().map(KomaRenderElement::from).collect()
            }
            CursorImageStatus::Named(icon) => self.render_named_cursor(
                renderer,
                *icon,
                pointer_location,
                output_scale,
                elapsed_millis,
            ),
        }
    }

    fn render_named_cursor<R>(
        &self,
        renderer: &mut R,
        icon: CursorIcon,
        pointer_location: Point<f64, Logical>,
        output_scale: Scale<f64>,
        elapsed_millis: u64,
    ) -> Vec<KomaRenderElement<R>>
    where
        R: KomaRenderTarget,
        R::TextureId: Clone + Send + 'static,
    {
        let int_scale = effective_cursor_scale(output_scale.x);
        let (resolved_icon, cursor) = self.resolve_named_cursor(icon, output_scale.x);
        let frame_idx = frame_index_for_millis(
            &cursor.frame_delays_ms,
            cursor.total_duration_ms,
            elapsed_millis,
        );
        let frame = &cursor.frames[frame_idx];
        let buffer = self.frame_buffer(resolved_icon, int_scale, &cursor, frame_idx);
        let loc_phys: Point<i32, Physical> =
            (pointer_location - frame.hotspot_logical).to_physical_precise_round(output_scale);

        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            loc_phys.to_f64(),
            &buffer,
            None,
            None,
            None,
            Kind::Cursor,
        ) {
            Ok(elem) => vec![KomaRenderElement::CursorBuffer(elem)],
            Err(err) => {
                warn!("failed importing cursor memory buffer into renderer: {err}");
                Vec::new()
            }
        }
    }
}

impl Default for CursorManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_arrow_bitmap_is_24x24_with_hotspot_1_1_and_opaque_outline() {
        let img = fallback_arrow_image();
        assert_eq!(img.width, 24);
        assert_eq!(img.height, 24);
        assert_eq!(img.size, 24);
        assert_eq!((img.xhot, img.yhot), (1, 1));
        assert_eq!(img.pixels_rgba.len(), 24 * 24 * 4);

        let pixel_at = |x: usize, y: usize| -> [u8; 4] {
            let off = (y * 24 + x) * 4;
            [
                img.pixels_rgba[off],
                img.pixels_rgba[off + 1],
                img.pixels_rgba[off + 2],
                img.pixels_rgba[off + 3],
            ]
        };

        // Hotspot (1, 1) must be solid black outline pixel; (0, 0) transparent; (2, 2) white interior
        assert_eq!(pixel_at(1, 1), [0, 0, 0, 255]);
        assert_eq!(pixel_at(0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel_at(2, 2), [255, 255, 255, 255]);

        let mut opaque_black = 0_usize;
        let mut opaque_white = 0_usize;
        let mut transparent = 0_usize;
        for px in img.pixels_rgba.as_chunks::<4>().0 {
            match px {
                &[0, 0, 0, 255] => opaque_black += 1,
                &[255, 255, 255, 255] => opaque_white += 1,
                &[0, 0, 0, 0] => transparent += 1,
                other => panic!("unexpected pixel value in fallback arrow: {other:?}"),
            }
        }
        assert!(opaque_black > 20);
        assert!(opaque_white > 40);
        assert_eq!(opaque_black + opaque_white + transparent, 24 * 24);
    }

    #[test]
    fn cursor_size_and_hidpi_scale_selection_work_cleanly() {
        assert_eq!(parse_cursor_size_str(None), 24);
        assert_eq!(parse_cursor_size_str(Some("")), 24);
        assert_eq!(parse_cursor_size_str(Some("0")), 24);
        assert_eq!(parse_cursor_size_str(Some("invalid")), 24);
        assert_eq!(parse_cursor_size_str(Some(" 32 ")), 32);

        assert_eq!(parse_cursor_theme_str(None), "default");
        assert_eq!(parse_cursor_theme_str(Some("   ")), "default");
        assert_eq!(
            parse_cursor_theme_str(Some(" Bibata-Modern-Ice ")),
            "Bibata-Modern-Ice"
        );

        assert_eq!(effective_cursor_scale(1.0), 1);
        assert_eq!(effective_cursor_scale(1.25), 2);
        assert_eq!(effective_cursor_scale(1.5), 2);
        assert_eq!(effective_cursor_scale(2.0), 2);
        assert_eq!(effective_cursor_scale(0.5), 1);
        assert_eq!(effective_cursor_scale(f64::NAN), 1);

        assert_eq!(target_cursor_pixel_size(24, 1.0), 24);
        assert_eq!(target_cursor_pixel_size(24, 1.5), 48);
        assert_eq!(target_cursor_pixel_size(24, 2.0), 48);

        let dummy_img = |size: u32, delay: u32| Image {
            size,
            width: size,
            height: size,
            xhot: 0,
            yhot: 0,
            delay,
            pixels_argb: Vec::new(),
            pixels_rgba: vec![0_u8; (size * size * 4) as usize],
        };
        let picked_48 = select_nearest_size_frames(
            vec![
                dummy_img(24, 50),
                dummy_img(48, 60),
                dummy_img(48, 70),
                dummy_img(96, 80),
            ],
            48,
        );
        assert_eq!(picked_48.len(), 2);
        assert!(picked_48.iter().all(|img| img.size == 48));

        // On tie (e.g. target 36 between 24 and 48), prefer sharper larger size
        let picked_tie = select_nearest_size_frames(vec![dummy_img(24, 0), dummy_img(48, 0)], 36);
        assert_eq!(picked_tie.len(), 1);
        assert_eq!(picked_tie[0].size, 48);
    }

    #[test]
    fn animation_frame_selection_and_wraparound_are_exact() {
        // Static or degenerate animations always select index 0 and emit no timer delay
        assert_eq!(frame_index_for_millis(&[0], 0, 999), 0);
        assert_eq!(frame_index_for_millis(&[100], 100, 50), 0);
        assert_eq!(remaining_frame_delay_ms(&[100], 100, 50), None);
        assert_eq!(frame_index_for_millis(&[0, 0], 0, 25), 0);

        let delays = [40_u32, 60, 100];
        let total = 200_u32;
        assert_eq!(frame_index_for_millis(&delays, total, 0), 0);
        assert_eq!(remaining_frame_delay_ms(&delays, total, 0), Some(40));

        assert_eq!(frame_index_for_millis(&delays, total, 39), 0);
        assert_eq!(remaining_frame_delay_ms(&delays, total, 39), Some(1));

        assert_eq!(frame_index_for_millis(&delays, total, 40), 1);
        assert_eq!(remaining_frame_delay_ms(&delays, total, 40), Some(60));

        assert_eq!(frame_index_for_millis(&delays, total, 100), 2);
        assert_eq!(remaining_frame_delay_ms(&delays, total, 100), Some(100));

        // Wrap across period boundary (200ms -> 0, 245ms -> 45ms -> frame 1 with 55ms left)
        assert_eq!(frame_index_for_millis(&delays, total, 200), 0);
        assert_eq!(frame_index_for_millis(&delays, total, 245), 1);
        assert_eq!(remaining_frame_delay_ms(&delays, total, 245), Some(55));
    }
}
