use super::*;

fn append_sample(bytes: &mut Vec<u8>, sample: i16) {
    bytes.extend_from_slice(&sample.to_le_bytes());
}

fn stereo_input(frames: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(frames * 4);
    for frame in 0..frames {
        let frame = i32::try_from(frame).unwrap();
        append_sample(&mut bytes, ((frame * 97 % 60_001) - 30_000) as i16);
        append_sample(&mut bytes, ((frame * 131 % 58_001) - 29_000) as i16);
    }
    bytes
}

#[test]
fn conversion_is_independent_of_whole_frame_chunk_boundaries() {
    let input = stereo_input(4_410);

    let mut contiguous = Pcm16StreamConverter::new(44_100, 2, 48_000, 1).unwrap();
    let mut expected = contiguous.push(&input).unwrap();
    expected.extend(contiguous.finish().unwrap());

    let mut chunked = Pcm16StreamConverter::new(44_100, 2, 48_000, 1).unwrap();
    let mut actual = Vec::new();
    let mut frame_offset = 0;
    for chunk_frames in [1, 17, 239, 3, 481, 997, 61, 2_611] {
        if frame_offset == 4_410 {
            break;
        }
        let frames = chunk_frames.min(4_410 - frame_offset);
        let start = frame_offset * 4;
        let end = (frame_offset + frames) * 4;
        actual.extend(chunked.push(&input[start..end]).unwrap());
        frame_offset += frames;
    }
    if frame_offset < 4_410 {
        actual.extend(chunked.push(&input[frame_offset * 4..]).unwrap());
    }
    actual.extend(chunked.finish().unwrap());

    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 4_800 * 2);
}

#[test]
fn channel_conversion_and_extreme_supported_rates_keep_complete_frames() {
    let mut converter = Pcm16StreamConverter::new(8_000, 1, 192_000, 2).unwrap();
    let input: Vec<u8> = (0..80)
        .flat_map(|sample| i16::try_from(sample * 100).unwrap().to_le_bytes())
        .collect();
    let mut output = converter.push(&input).unwrap();
    output.extend(converter.finish().unwrap());

    assert_eq!(output.len(), 1_920 * 2 * 2);
    assert_eq!(output.len() % 4, 0);
    for frame in output.chunks_exact(4) {
        assert_eq!(&frame[..2], &frame[2..]);
    }

    assert!(Pcm16StreamConverter::new(7_999, 1, 48_000, 1).is_err());
    assert!(Pcm16StreamConverter::new(48_000, 1, 192_001, 1).is_err());
    assert!(Pcm16StreamConverter::new(48_000, 3, 48_000, 1).is_err());

    let mut stereo = Pcm16StreamConverter::new(192_000, 2, 8_000, 2).unwrap();
    let stereo_input = stereo_input(1_920);
    let mut stereo_output = stereo.push(&stereo_input).unwrap();
    stereo_output.extend(stereo.finish().unwrap());
    assert_eq!(stereo_output.len(), 80 * 2 * 2);
}

#[test]
fn input_alignment_chunk_bound_and_finish_are_enforced() {
    let mut converter = Pcm16StreamConverter::new(8_000, 1, 16_000, 1).unwrap();
    assert_eq!(
        converter.push(&[0]),
        Err(Pcm16StreamError::IncompleteInputFrame)
    );
    assert_eq!(
        converter.push(&vec![0; (8_001usize) * 2]),
        Err(Pcm16StreamError::InputChunkTooLarge)
    );
    assert!(converter.push(&[0; 80 * 2]).is_ok());
    let tail = converter.finish().unwrap();
    assert_eq!(tail.len() % 2, 0);
    assert_eq!(converter.push(&[]), Err(Pcm16StreamError::StreamFinished));
}
