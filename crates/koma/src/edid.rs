#![forbid(unsafe_code)]
//! Pure-Rust, zero-unsafe VESA E-EDID base-block (128-byte) parser.
//!
//! Extracts vendor PNP ID (`make`), product code, physical millimeter dimensions (with Detailed
//! Timing Descriptor fallback when header centimeter bytes are zero), and 18-byte monitor
//! descriptors (`0xFC` monitor name, `0xFF` serial string, `0xFE` ASCII text) without any C FFI
//! or external EDID library dependencies.

use std::fmt;

const EDID_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
const EDID_BASE_BLOCK_LEN: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdidParseError {
    Truncated { len: usize },
    InvalidHeader,
    InvalidChecksum { sum: u8 },
    InvalidVendorId(u16),
}

impl fmt::Display for EdidParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { len } => {
                write!(
                    f,
                    "EDID blob truncated: expected >= {EDID_BASE_BLOCK_LEN} bytes, got {len}"
                )
            }
            Self::InvalidHeader => write!(f, "EDID base block has invalid 8-byte VESA header"),
            Self::InvalidChecksum { sum } => {
                write!(
                    f,
                    "EDID base block checksum invalid: modulo-256 sum = 0x{sum:02X}"
                )
            }
            Self::InvalidVendorId(raw) => {
                write!(f, "EDID vendor ID word 0x{raw:04X} is not valid PNP ASCII")
            }
        }
    }
}

impl std::error::Error for EdidParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEdid {
    pub vendor_id: String,
    pub product_code: u16,
    pub header_serial: u32,
    pub edid_version: u8,
    pub edid_revision: u8,
    pub width_mm: i32,
    pub height_mm: i32,
    pub monitor_name: Option<String>,
    pub serial_number: Option<String>,
    pub ascii_texts: Vec<String>,
}

impl ParsedEdid {
    /// Parses a 128-byte VESA EDID base block (ignoring trailing extension blocks if present).
    pub fn parse(raw: &[u8]) -> Result<Self, EdidParseError> {
        if raw.len() < EDID_BASE_BLOCK_LEN {
            return Err(EdidParseError::Truncated { len: raw.len() });
        }
        let block = &raw[..EDID_BASE_BLOCK_LEN];

        if block[..8] != EDID_HEADER {
            return Err(EdidParseError::InvalidHeader);
        }

        let sum = block.iter().fold(0_u8, |acc, &b| acc.wrapping_add(b));
        if sum != 0 {
            return Err(EdidParseError::InvalidChecksum { sum });
        }

        let vendor_word = u16::from_be_bytes([block[8], block[9]]);
        let vendor_id = decode_pnp_vendor_id(vendor_word)?;
        let product_code = u16::from_le_bytes([block[10], block[11]]);
        let header_serial = u32::from_le_bytes([block[12], block[13], block[14], block[15]]);
        let edid_version = block[18];
        let edid_revision = block[19];

        let header_w_cm = i32::from(block[21]);
        let header_h_cm = i32::from(block[22]);

        let mut dtd_mm: Option<(i32, i32)> = None;
        let mut monitor_name: Option<String> = None;
        let mut serial_number: Option<String> = None;
        let mut ascii_texts = Vec::new();

        for desc in block[54..126].as_chunks::<18>().0 {
            if desc[0] == 0x00 && desc[1] == 0x00 && desc[2] == 0x00 && desc[4] == 0x00 {
                match desc[3] {
                    0xFC => {
                        if monitor_name.is_none() {
                            monitor_name = parse_descriptor_text(&desc[5..18]);
                        }
                    }
                    0xFF => {
                        if serial_number.is_none() {
                            serial_number = parse_descriptor_text(&desc[5..18]);
                        }
                    }
                    0xFE => {
                        if let Some(txt) = parse_descriptor_text(&desc[5..18]) {
                            ascii_texts.push(txt);
                        }
                    }
                    _ => {}
                }
            } else if (desc[0] != 0x00 || desc[1] != 0x00) && dtd_mm.is_none() {
                let h_mm = (i32::from(desc[14] >> 4) << 8) | i32::from(desc[12]);
                let v_mm = (i32::from(desc[14] & 0x0F) << 8) | i32::from(desc[13]);
                if h_mm > 0 && v_mm > 0 {
                    dtd_mm = Some((h_mm, v_mm));
                }
            }
        }

        let (width_mm, height_mm) = if header_w_cm > 0 && header_h_cm > 0 {
            (header_w_cm * 10, header_h_cm * 10)
        } else {
            dtd_mm.unwrap_or((0, 0))
        };

        Ok(Self {
            vendor_id,
            product_code,
            header_serial,
            edid_version,
            edid_revision,
            width_mm,
            height_mm,
            monitor_name,
            serial_number,
            ascii_texts,
        })
    }

    #[must_use]
    pub const fn physical_size_mm(&self) -> (i32, i32) {
        (self.width_mm, self.height_mm)
    }

    #[must_use]
    pub fn display_make(&self) -> &str {
        &self.vendor_id
    }

    #[must_use]
    pub fn display_model(&self, fallback: &str) -> String {
        self.monitor_name
            .clone()
            .or_else(|| self.ascii_texts.first().cloned())
            .unwrap_or_else(|| fallback.to_owned())
    }

    #[must_use]
    pub fn display_serial(&self) -> String {
        if let Some(serial) = &self.serial_number {
            return serial.clone();
        }
        if self.header_serial != 0 {
            return self.header_serial.to_string();
        }
        "Unknown".to_owned()
    }
}

fn decode_pnp_vendor_id(word: u16) -> Result<String, EdidParseError> {
    if (word & 0x8000) != 0 {
        return Err(EdidParseError::InvalidVendorId(word));
    }
    let c1 = ((word >> 10) & 0x1F) as u8;
    let c2 = ((word >> 5) & 0x1F) as u8;
    let c3 = (word & 0x1F) as u8;
    if !(1..=26).contains(&c1) || !(1..=26).contains(&c2) || !(1..=26).contains(&c3) {
        return Err(EdidParseError::InvalidVendorId(word));
    }
    let mut out = String::with_capacity(3);
    out.push(char::from(b'A' + (c1 - 1)));
    out.push(char::from(b'A' + (c2 - 1)));
    out.push(char::from(b'A' + (c3 - 1)));
    Ok(out)
}

fn parse_descriptor_text(bytes: &[u8]) -> Option<String> {
    let mut text = String::new();
    for &b in bytes {
        if b == 0x0A || b == 0x00 {
            break;
        }
        if (0x20..=0x7E).contains(&b) {
            text.push(char::from(b));
        } else {
            text.push(' ');
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_pnp_vendor(chars: [u8; 3]) -> [u8; 2] {
        let c1 = u16::from(chars[0] - b'A' + 1);
        let c2 = u16::from(chars[1] - b'A' + 1);
        let c3 = u16::from(chars[2] - b'A' + 1);
        ((c1 << 10) | (c2 << 5) | c3).to_be_bytes()
    }

    fn write_monitor_descriptor(slot: &mut [u8], tag: u8, text: &str) {
        slot.fill(0x20);
        slot[0] = 0x00;
        slot[1] = 0x00;
        slot[2] = 0x00;
        slot[3] = tag;
        slot[4] = 0x00;
        let bytes = text.as_bytes();
        let copy_len = bytes.len().min(13);
        slot[5..5 + copy_len].copy_from_slice(&bytes[..copy_len]);
        if copy_len < 13 {
            slot[5 + copy_len] = 0x0A;
        }
    }

    fn write_dtd(slot: &mut [u8], px_w: u16, px_h: u16, mm_w: u16, mm_h: u16) {
        slot.fill(0);
        // Non-zero pixel clock (e.g. 148.50 MHz -> 14850 in 10 kHz units).
        slot[0..2].copy_from_slice(&14850_u16.to_le_bytes());
        slot[2] = (px_w & 0xFF) as u8;
        slot[4] = ((px_w >> 8) as u8 & 0x0F) << 4;
        slot[5] = (px_h & 0xFF) as u8;
        slot[7] = ((px_h >> 8) as u8 & 0x0F) << 4;
        slot[12] = (mm_w & 0xFF) as u8;
        slot[13] = (mm_h & 0xFF) as u8;
        slot[14] = ((((mm_w >> 8) & 0x0F) << 4) | ((mm_h >> 8) & 0x0F)) as u8;
    }

    fn finalize_edid_checksum(block: &mut [u8; 128]) {
        let sum_127 = block[..127]
            .iter()
            .fold(0_u8, |acc, &b| acc.wrapping_add(b));
        block[127] = 0_u8.wrapping_sub(sum_127);
    }

    fn build_synthetic_desktop_edid() -> [u8; 128] {
        let mut block = [0_u8; 128];
        block[..8].copy_from_slice(&EDID_HEADER);
        block[8..10].copy_from_slice(&encode_pnp_vendor(*b"DEL"));
        block[10..12].copy_from_slice(&0x4123_u16.to_le_bytes());
        block[12..16].copy_from_slice(&98765_u32.to_le_bytes());
        block[18] = 1;
        block[19] = 4;
        // 60cm x 34cm -> 600mm x 340mm (typical 27" 4K monitor)
        block[21] = 60;
        block[22] = 34;

        write_dtd(&mut block[54..72], 3840, 2160, 597, 336);
        write_monitor_descriptor(&mut block[72..90], 0xFC, "DELL U2723QE");
        write_monitor_descriptor(&mut block[90..108], 0xFF, "CN0ABC123");
        write_monitor_descriptor(&mut block[108..126], 0xFE, "DisplayPort");
        finalize_edid_checksum(&mut block);
        block
    }

    fn build_apple_retina_panel_edid() -> [u8; 128] {
        let mut block = [0_u8; 128];
        block[..8].copy_from_slice(&EDID_HEADER);
        block[8..10].copy_from_slice(&encode_pnp_vendor(*b"APP"));
        block[10..12].copy_from_slice(&0xA050_u16.to_le_bytes());
        block[18] = 1;
        block[19] = 4;
        // Header cm left 0 so DTD physical mm (302mm x 196mm for 14.2" MacBook Pro) provides geometry.
        block[21] = 0;
        block[22] = 0;

        write_dtd(&mut block[54..72], 3024, 1964, 302, 196);
        write_monitor_descriptor(&mut block[72..90], 0xFC, "Color LCD");
        write_monitor_descriptor(&mut block[90..108], 0xFE, "Apple Silicon");
        write_monitor_descriptor(&mut block[108..126], 0xFF, "FVF0000001");
        finalize_edid_checksum(&mut block);
        block
    }

    #[test]
    fn parses_synthetic_desktop_monitor_edid_fields_and_header_dimensions() {
        let raw = build_synthetic_desktop_edid();
        let edid = ParsedEdid::parse(&raw).expect("valid desktop EDID");
        assert_eq!(edid.vendor_id, "DEL");
        assert_eq!(edid.product_code, 0x4123);
        assert_eq!(edid.header_serial, 98765);
        assert_eq!(edid.physical_size_mm(), (600, 340));
        assert_eq!(edid.monitor_name.as_deref(), Some("DELL U2723QE"));
        assert_eq!(edid.serial_number.as_deref(), Some("CN0ABC123"));
        assert_eq!(edid.ascii_texts, vec!["DisplayPort".to_owned()]);
        assert_eq!(edid.display_make(), "DEL");
        assert_eq!(edid.display_model("DP-1"), "DELL U2723QE");
        assert_eq!(edid.display_serial(), "CN0ABC123");
    }

    #[test]
    fn parses_apple_retina_internal_panel_edid_with_dtd_mm_fallback_and_hidpi_geometry() {
        let raw = build_apple_retina_panel_edid();
        let edid = ParsedEdid::parse(&raw).expect("valid Apple Retina EDID");
        assert_eq!(edid.vendor_id, "APP");
        assert_eq!(edid.product_code, 0xA050);
        assert_eq!(edid.physical_size_mm(), (302, 196));
        assert_eq!(edid.display_model("eDP-1"), "Color LCD");
        assert_eq!(edid.ascii_texts, vec!["Apple Silicon".to_owned()]);
        assert_eq!(edid.display_serial(), "FVF0000001");

        // Verify high-DPI physical geometry exceeds Retina threshold (>= 192 DPI).
        let dpi_x = 3024.0 / (f64::from(edid.width_mm) / 25.4);
        let dpi_y = 1964.0 / (f64::from(edid.height_mm) / 25.4);
        assert!(dpi_x > 250.0 && dpi_y > 250.0);
    }

    #[test]
    fn rejects_invalid_checksum_and_truncated_or_malformed_inputs() {
        let mut bad_checksum = build_synthetic_desktop_edid();
        bad_checksum[127] ^= 0x01;
        assert!(matches!(
            ParsedEdid::parse(&bad_checksum),
            Err(EdidParseError::InvalidChecksum { .. })
        ));

        let truncated = &bad_checksum[..127];
        assert_eq!(
            ParsedEdid::parse(truncated),
            Err(EdidParseError::Truncated { len: 127 })
        );

        let mut bad_header = build_synthetic_desktop_edid();
        bad_header[0] = 0x01;
        finalize_edid_checksum(&mut bad_header);
        assert_eq!(
            ParsedEdid::parse(&bad_header),
            Err(EdidParseError::InvalidHeader)
        );
    }
}
