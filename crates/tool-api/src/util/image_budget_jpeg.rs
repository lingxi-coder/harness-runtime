//! Only the JPEG metadata needed by the image budget processor.
//!
//! Scan bounded marker payloads before the first entropy-coded scan. EXIF
//! orientation is a single inline SHORT in TIFF IFD0; no nested IFDs, thumbnails,
//! or external offsets are followed. The image decoder validates image content.

use image::DynamicImage;

pub(super) struct Metadata {
    pub orientation: u16,
    pub coefficient_bytes: u64,
    pub can_scale: bool,
}

pub(super) fn metadata(bytes: &[u8]) -> Metadata {
    let mut metadata = Metadata {
        orientation: 1,
        coefficient_bytes: 0,
        can_scale: false,
    };
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return metadata;
    }
    let mut offset = 2;
    let mut orientation_found = false;
    while offset < bytes.len() {
        // Match jpeg-decoder's tolerance for padding/junk between markers.
        while bytes.get(offset).is_some_and(|byte| *byte != 0xff) {
            offset += 1;
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let Some(&marker) = bytes.get(offset) else {
            break;
        };
        offset += 1;
        match marker {
            0xda | 0xd9 => break, // SOS / EOI: never inspect compressed image data.
            0x01 | 0xd0..=0xd8 => continue, // Standalone markers have no length.
            0x00 => continue,
            _ => {}
        }
        let Some(length_bytes) = bytes.get(offset..offset.saturating_add(2)) else {
            break;
        };
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            break;
        }
        let Some(payload) = bytes.get(offset + 2..offset.saturating_add(length)) else {
            break;
        };
        if marker == 0xe1 && !orientation_found {
            if let Some(orientation) = payload.strip_prefix(b"Exif\0\0").and_then(orientation) {
                metadata.orientation = orientation;
                orientation_found = true;
            }
        } else if matches!(marker, 0xc0..=0xc2) {
            metadata.can_scale = true;
            if marker == 0xc2 {
                // Progressive JPEG keeps 64 i16 coefficients per original DCT block.
                metadata.coefficient_bytes = progressive_coefficient_bytes(payload).unwrap_or(0);
            }
        }
        offset += length;
    }
    metadata
}

fn orientation(tiff: &[u8]) -> Option<u16> {
    let little_endian = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let read_short = |offset: usize| -> Option<u16> {
        let value: [u8; 2] = tiff.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
        Some(if little_endian {
            u16::from_le_bytes(value)
        } else {
            u16::from_be_bytes(value)
        })
    };
    let read_long = |offset: usize| -> Option<u32> {
        let value: [u8; 4] = tiff.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
        Some(if little_endian {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        })
    };
    if read_short(2)? != 42 {
        return None;
    }
    let ifd = usize::try_from(read_long(4)?).ok()?;
    if ifd < 8 {
        return None;
    }
    let count = usize::from(read_short(ifd)?);
    let entries_start = ifd.checked_add(2)?;
    let entries_end = entries_start.checked_add(count.checked_mul(12)?)?;
    // Validate the complete table plus its next-IFD offset without following it.
    tiff.get(entries_start..entries_end.checked_add(4)?)?;
    for entry in (entries_start..entries_end).step_by(12) {
        if read_short(entry)? == 0x0112 && read_short(entry + 2)? == 3 && read_long(entry + 4)? == 1
        {
            let value = read_short(entry + 8)?;
            if (1..=8).contains(&value) {
                return Some(value);
            }
        }
    }
    None
}

fn progressive_coefficient_bytes(frame: &[u8]) -> Option<u64> {
    let height = u64::from(u16::from_be_bytes(frame.get(1..3)?.try_into().ok()?));
    let width = u64::from(u16::from_be_bytes(frame.get(3..5)?.try_into().ok()?));
    let component_count = usize::from(*frame.get(5)?);
    let components = frame.get(6..6 + component_count * 3)?;
    let max_horizontal = components.chunks_exact(3).map(|c| c[1] >> 4).max()?;
    let max_vertical = components.chunks_exact(3).map(|c| c[1] & 15).max()?;
    if max_horizontal == 0 || max_vertical == 0 {
        return None;
    }
    let mcu_width = width.div_ceil(u64::from(max_horizontal) * 8);
    let mcu_height = height.div_ceil(u64::from(max_vertical) * 8);
    let sampling_blocks: u64 = components
        .chunks_exact(3)
        .map(|c| u64::from(c[1] >> 4) * u64::from(c[1] & 15))
        .sum();
    Some(mcu_width * mcu_height * sampling_blocks * 64 * 2)
}

pub(super) fn orient(image: DynamicImage, orientation: u16) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate90().flipv(),
        8 => image.rotate270(),
        _ => image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exif(orientation: u16, little_endian: bool) -> Vec<u8> {
        let mut data = b"Exif\0\0".to_vec();
        let short = |v: u16| {
            if little_endian {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let long = |v: u32| {
            if little_endian {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        data.extend_from_slice(if little_endian { b"II" } else { b"MM" });
        data.extend(short(42));
        data.extend(long(8));
        data.extend(short(1));
        data.extend(short(0x112));
        data.extend(short(3));
        data.extend(long(1));
        data.extend(short(orientation));
        data.extend(short(0));
        data.extend(long(0));
        data
    }

    fn jpeg_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0xff, 0xd8, 0xff, marker];
        data.extend(((payload.len() + 2) as u16).to_be_bytes());
        data.extend(payload);
        data.extend([0xff, 0xd9]);
        data
    }

    #[test]
    fn reads_all_orientations_in_both_byte_orders() {
        for little_endian in [true, false] {
            for value in 1..=8 {
                assert_eq!(
                    metadata(&jpeg_segment(0xe1, &exif(value, little_endian))).orientation,
                    value
                );
            }
        }
    }

    #[test]
    fn transforms_every_orientation_including_mirrors() {
        let img = DynamicImage::ImageLuma8(
            image::GrayImage::from_raw(2, 3, vec![1, 2, 3, 4, 5, 6]).unwrap(),
        );
        let expected = [
            vec![1, 2, 3, 4, 5, 6],
            vec![2, 1, 4, 3, 6, 5],
            vec![6, 5, 4, 3, 2, 1],
            vec![5, 6, 3, 4, 1, 2],
            vec![1, 3, 5, 2, 4, 6],
            vec![5, 3, 1, 6, 4, 2],
            vec![6, 4, 2, 5, 3, 1],
            vec![2, 4, 6, 1, 3, 5],
        ];
        for (index, expected) in expected.into_iter().enumerate() {
            let output = orient(img.clone(), index as u16 + 1).into_luma8();
            assert_eq!(output.dimensions(), if index < 4 { (2, 3) } else { (3, 2) });
            assert_eq!(output.into_raw(), expected, "orientation {}", index + 1);
        }
    }

    #[test]
    fn malformed_exif_is_ignored_without_following_offsets() {
        let valid = exif(6, true);
        for end in 0..valid.len() {
            assert_eq!(metadata(&jpeg_segment(0xe1, &valid[..end])).orientation, 1);
        }
        let mut malformed = Vec::new();
        for (range, value) in [
            (6..8, vec![b'X', b'X']), // Unknown byte order.
            (8..10, vec![0, 0]),      // Invalid TIFF magic.
            (10..14, vec![255; 4]),   // Out-of-range IFD offset.
            (10..14, vec![0; 4]),     // IFD points into its own header.
            (14..16, vec![255; 2]),   // Truncated IFD table.
            (18..20, vec![4, 0]),     // Wrong tag type.
            (20..24, vec![255; 4]),   // Wrong element count.
            (24..26, vec![9, 0]),     // Invalid orientation.
        ] {
            let mut item = valid.clone();
            item[range].copy_from_slice(&value);
            malformed.push(item);
        }
        for item in malformed {
            assert_eq!(metadata(&jpeg_segment(0xe1, &item)).orientation, 1);
        }
    }

    #[test]
    fn marker_scan_is_bounded_and_skips_unrelated_metadata() {
        let valid = jpeg_segment(0xe1, &exif(6, false));
        for end in 0..valid.len() - 2 {
            assert_eq!(metadata(&valid[..end]).orientation, 1);
        }
        for bytes in [
            vec![],
            vec![0xff],
            vec![0xff, 0xd8, 0xff],
            vec![0xff, 0xd8, 0xff, 0xe1, 0, 1],
        ] {
            assert_eq!(metadata(&bytes).orientation, 1);
        }
        let mut multiple = jpeg_segment(0xe1, b"http://ns.adobe.com/xap/1.0/\0not EXIF");
        multiple.truncate(multiple.len() - 2);
        multiple.extend_from_slice(&valid[2..]);
        assert_eq!(metadata(&multiple).orientation, 6);
        let mut padded = vec![0xff, 0xd8, 0x12, 0x34, 0xff, 0x00, 0xff];
        padded.extend_from_slice(&valid[2..]);
        assert_eq!(metadata(&padded).orientation, 6);
        // A fake APP1 in entropy-coded bytes must never be interpreted as metadata.
        let mut scanned = vec![0xff, 0xd8, 0xff, 0xda];
        scanned.extend_from_slice(&valid[2..]);
        assert_eq!(metadata(&scanned).orientation, 1);
    }

    #[test]
    fn progressive_coefficients_account_for_sampling_and_mcu_padding() {
        // 17x9 4:2:0: 2x1 MCUs, (4+1+1) blocks per MCU, 128 bytes per block.
        let frame = [8, 0, 9, 0, 17, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1];
        assert_eq!(
            metadata(&jpeg_segment(0xc2, &frame)).coefficient_bytes,
            1536
        );
        assert_eq!(metadata(&jpeg_segment(0xc0, &frame)).coefficient_bytes, 0);
        for marker in 0xc0..=0xc2 {
            assert!(metadata(&jpeg_segment(marker, &frame)).can_scale);
        }
        assert!(!metadata(&jpeg_segment(0xc3, &frame)).can_scale);
        for end in 0..frame.len() {
            assert_eq!(
                metadata(&jpeg_segment(0xc2, &frame[..end])).coefficient_bytes,
                0
            );
        }
    }
}
