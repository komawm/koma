//! Software `RGBA8 -> ARGB8888` (`wl_shm`) card compositor, bilinear/nearest thumbnail scaler,
//! semantic `WorkspaceLease` border painter, and crisp `5x7` ASCII status header renderer.

use koma_model::LogicalRect;

pub const ARGB_BACKDROP_DIM: u32 = 0xCC0D_0F14;
pub const ARGB_CARD_BG: u32 = 0xF418_1A20;
pub const ARGB_BORDER_SELECTED: u32 = 0xFF58_A6FF; // #58a6ff Focused / Selected
pub const ARGB_BORDER_AGENT: u32 = 0xFFD2_A8FF; // #d2a8ff AgentExclusive Lease
pub const ARGB_BORDER_PREEMPT: u32 = 0xFFF0_883E; // #f0883e HumanPreempted Takeover
pub const ARGB_BORDER_IDLE: u32 = 0xFF30_363D; // #30363d Settled / Frozen
pub const ARGB_TEXT_PRIMARY: u32 = 0xFFE6_EDF3;

#[derive(Debug, Clone)]
pub struct CardVisualMeta {
    pub title_label: String,
    pub is_selected: bool,
    pub is_agent_leased: bool,
    pub is_human_preempted: bool,
}

#[derive(Debug, Clone)]
pub struct RawRgbaFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Fills an entire `wl_shm::Format::Argb8888` canvas (`width * height * 4` little-endian bytes).
pub fn clear_argb8888_canvas(canvas: &mut [u8], argb: u32) {
    let px = argb.to_le_bytes();
    for chunk in canvas.as_chunks_mut::<4>().0 {
        chunk.copy_from_slice(&px);
    }
}

/// Renders a single workspace preview card (background, semantic border, `5x7` header text, and
/// scaled `SCM_RIGHTS` thumbnail) into an `Argb8888` shared-memory surface buffer.
#[allow(clippy::too_many_arguments)] // Explicit card/header/thumb layout rects avoid per-frame struct wrapper.
pub fn paint_workspace_card(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    card_rect: LogicalRect,
    header_rect: LogicalRect,
    thumb_rect: LogicalRect,
    meta: &CardVisualMeta,
    frame: Option<&RawRgbaFrame>,
) {
    // 1. Card container fill
    fill_rect_argb(canvas, canvas_w, canvas_h, card_rect, ARGB_CARD_BG);

    // 2. Semantic 2px border reflecting selection & WorkspaceLease state
    let border_col = if meta.is_human_preempted {
        ARGB_BORDER_PREEMPT
    } else if meta.is_selected {
        ARGB_BORDER_SELECTED
    } else if meta.is_agent_leased {
        ARGB_BORDER_AGENT
    } else {
        ARGB_BORDER_IDLE
    };
    stroke_rect_argb(canvas, canvas_w, canvas_h, card_rect, 2, border_col);

    // 3. Crisp 2x-scaled 5x7 pixel status label, strictly clipped to `header_rect`
    let label_start_x = header_rect.x + 6;
    let label_max_x = header_rect.x.saturating_add(header_rect.w as i32 - 6);
    draw_label_5x7(
        canvas,
        canvas_w,
        canvas_h,
        label_start_x,
        header_rect.y + 7,
        label_max_x,
        &meta.title_label,
        ARGB_TEXT_PRIMARY,
    );

    // 4. Downsample zero-copy `RGBA8` (`Abgr8888`) `memfd` frame into `thumb_rect`
    if let Some(src) = frame {
        blit_scaled_rgba_to_argb(canvas, canvas_w, canvas_h, thumb_rect, src);
    } else {
        fill_rect_argb(canvas, canvas_w, canvas_h, thumb_rect, 0xFF11_1216);
    }
}

pub fn fill_rect_argb(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    rect: LogicalRect,
    argb: u32,
) {
    let x0 = rect.x.max(0) as u32;
    let y0 = rect.y.max(0) as u32;
    let x1 = (rect.x.saturating_add(rect.w as i32)).max(0) as u32;
    let y1 = (rect.y.saturating_add(rect.h as i32)).max(0) as u32;
    let x_end = x1.min(canvas_w);
    let y_end = y1.min(canvas_h);
    let px = argb.to_le_bytes();

    for y in y0..y_end {
        let row_off = (y * canvas_w) as usize;
        for x in x0..x_end {
            let idx = (row_off + x as usize) * 4;
            canvas[idx..idx + 4].copy_from_slice(&px);
        }
    }
}

pub fn stroke_rect_argb(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    rect: LogicalRect,
    thickness: u32,
    argb: u32,
) {
    let t = thickness.min(rect.w / 2).min(rect.h / 2);
    fill_rect_argb(
        canvas,
        canvas_w,
        canvas_h,
        LogicalRect::new(rect.x, rect.y, rect.w, t),
        argb,
    );
    fill_rect_argb(
        canvas,
        canvas_w,
        canvas_h,
        LogicalRect::new(
            rect.x,
            rect.y + (rect.h.saturating_sub(t)) as i32,
            rect.w,
            t,
        ),
        argb,
    );
    fill_rect_argb(
        canvas,
        canvas_w,
        canvas_h,
        LogicalRect::new(rect.x, rect.y, t, rect.h),
        argb,
    );
    fill_rect_argb(
        canvas,
        canvas_w,
        canvas_h,
        LogicalRect::new(
            rect.x + (rect.w.saturating_sub(t)) as i32,
            rect.y,
            t,
            rect.h,
        ),
        argb,
    );
}

fn blit_scaled_rgba_to_argb(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    dst: LogicalRect,
    src: &RawRgbaFrame,
) {
    if src.width == 0 || src.height == 0 || dst.w == 0 || dst.h == 0 {
        return;
    }
    let expected_len = (src.width as usize) * (src.height as usize) * 4;
    if src.pixels.len() < expected_len {
        return;
    }

    for dy in 0..dst.h {
        let cy = dst.y + dy as i32;
        if cy < 0 || cy >= canvas_h as i32 {
            continue;
        }
        let sy = ((u64::from(dy) * u64::from(src.height)) / u64::from(dst.h)) as u32;
        let src_row = (sy * src.width) as usize;
        let dst_row = (cy as u32 * canvas_w) as usize;

        for dx in 0..dst.w {
            let cx = dst.x + dx as i32;
            if cx < 0 || cx >= canvas_w as i32 {
                continue;
            }
            let sx = ((u64::from(dx) * u64::from(src.width)) / u64::from(dst.w)) as u32;
            let s_idx = (src_row + sx as usize) * 4;
            let r = src.pixels[s_idx];
            let g = src.pixels[s_idx + 1];
            let b = src.pixels[s_idx + 2];

            // Wayland `wl_shm::Format::Argb8888` in little-endian memory is `[B, G, R, A]`
            let d_idx = (dst_row + cx as usize) * 4;
            canvas[d_idx] = b;
            canvas[d_idx + 1] = g;
            canvas[d_idx + 2] = r;
            canvas[d_idx + 3] = 0xFF;
        }
    }
}

/// Minimal crisp `5x7` uppercase/digit/symbol glyph renderer (2x pixel scale -> `10x14` px)
/// clipped strictly inside `[start_x, max_x)` so long labels never bleed into neighboring cards.
#[allow(clippy::too_many_arguments)]
fn draw_label_5x7(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    start_x: i32,
    start_y: i32,
    max_x: i32,
    text: &str,
    argb: u32,
) {
    let avail_px = max_x.saturating_sub(start_x).max(0);
    // Each character advances 12 px and draws 10 px wide (`+2 px` trailing gap).
    let max_chars = if avail_px < 10 {
        0
    } else {
        ((avail_px - 10) / 12 + 1) as usize
    };
    if max_chars == 0 {
        return;
    }

    let char_count = text.chars().count();
    let truncated: String = if char_count > max_chars {
        if max_chars >= 3 {
            let mut s: String = text.chars().take(max_chars - 3).collect();
            s.push_str("...");
            s
        } else {
            text.chars().take(max_chars).collect()
        }
    } else {
        text.to_owned()
    };

    let px = argb.to_le_bytes();
    let mut cursor_x = start_x;
    for ch in truncated.chars() {
        let rows = glyph_5x7(ch.to_ascii_uppercase());
        for (ry, row_bits) in rows.iter().enumerate() {
            for rx in 0..5 {
                if (row_bits >> (4 - rx)) & 1 == 1 {
                    let gx = cursor_x + (rx * 2);
                    let gy = start_y + (ry as i32 * 2);
                    for sub_y in 0..2 {
                        for sub_x in 0..2 {
                            let x = gx + sub_x;
                            let y = gy + sub_y;
                            if x >= start_x
                                && x < max_x
                                && y >= 0
                                && (x as u32) < canvas_w
                                && (y as u32) < canvas_h
                            {
                                let idx = ((y as u32 * canvas_w) + x as u32) as usize * 4;
                                canvas[idx..idx + 4].copy_from_slice(&px);
                            }
                        }
                    }
                }
            }
        }
        cursor_x += 12;
    }
}

const fn glyph_5x7(c: char) -> [u8; 7] {
    match c {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        ':' => [
            0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000,
        ],
        '-' | '_' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        '|' => [
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ],
        _ => [0b00000; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_5x7_covers_full_alphanumeric_set_without_blanks() {
        for c in ('A'..='Z').chain('0'..='9').chain(['.', ':', '-', '|']) {
            let rows = glyph_5x7(c);
            assert_ne!(rows, [0; 7], "missing 5x7 glyph bitmap for '{c}'");
        }
    }

    #[test]
    fn issue_120_long_card_label_is_clipped_strictly_inside_header_rect() {
        let canvas_w = 500_u32;
        let canvas_h = 200_u32;
        let mut canvas = vec![0_u8; (canvas_w * canvas_h * 4) as usize];
        clear_argb8888_canvas(&mut canvas, ARGB_BACKDROP_DIM);

        let card_rect = LogicalRect::new(20, 20, 220, 140);
        let header_rect = LogicalRect::new(26, 26, 208, 30);
        let thumb_rect = LogicalRect::new(26, 56, 208, 98);
        let meta = CardVisualMeta {
            title_label: "AGENT:WORK | VERY-LONG-NAME-THAT-WOULD-OVERFLOW-CARD".to_owned(),
            is_selected: false,
            is_agent_leased: true,
            is_human_preempted: false,
        };
        paint_workspace_card(
            &mut canvas,
            canvas_w,
            canvas_h,
            card_rect,
            header_rect,
            thumb_rect,
            &meta,
            None,
        );

        let right_edge = (card_rect.x + card_rect.w as i32) as u32;
        let bg = ARGB_BACKDROP_DIM.to_le_bytes();
        for y in 0..canvas_h {
            for x in right_edge..canvas_w {
                let idx = ((y * canvas_w) + x) as usize * 4;
                assert_eq!(
                    &canvas[idx..idx + 4],
                    &bg,
                    "overview card pixel leaked past card right edge at ({x}, {y})"
                );
            }
        }
    }
}
