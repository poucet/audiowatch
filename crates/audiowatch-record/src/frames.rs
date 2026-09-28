//! Picking a take's channels out of a device's interleaved frames.
//!
//! A device is opened at its full channel count — the only way to reach
//! channels 13–14 of a fourteen-channel mixer is to ask for fourteen — and a
//! take keeps one or two of them. [`extract`] does that in place of a copy the
//! callback would have to allocate, so it can run on the audio thread.
//!
//! Nothing here measures the audio: judging what a file holds is the job of
//! whatever reads it (an analysis server). The recorder promises the file is
//! complete and says where it is.

use crate::spec::Channels;

/// Copy the selected channels of every whole frame in `interleaved` (a buffer
/// of `device_channels`-wide frames) into `out`, frame after frame, and return
/// how many frames were copied. Stops when either side runs out, so `out` may
/// be shorter than the input. Allocation-free; safe on the audio thread.
///
/// A trailing partial frame — which a well-behaved backend never delivers —
/// is ignored rather than read past.
pub fn extract(
    interleaved: &[f32],
    device_channels: u16,
    keep: Channels,
    out: &mut [f32],
) -> usize {
    let width = device_channels as usize;
    let first = keep.first() as usize - 1;
    let count = keep.count() as usize;
    if width == 0 || first + count > width {
        return 0;
    }
    let frames = (interleaved.len() / width).min(out.len() / count);
    for (frame, dst) in
        interleaved.chunks_exact(width).zip(out.chunks_exact_mut(count)).take(frames)
    {
        dst.copy_from_slice(&frame[first..first + count]);
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four-channel frames whose samples are `frame*10 + channel`, so a wrong
    /// index shows up as a wrong digit.
    fn frames(n: usize, width: usize) -> Vec<f32> {
        (0..n).flat_map(|f| (0..width).map(move |c| (f * 10 + c + 1) as f32)).collect()
    }

    #[test]
    fn a_pair_is_picked_out_of_interleaved_frames() {
        let input = frames(3, 4);
        let mut out = [0.0; 6];
        assert_eq!(extract(&input, 4, Channels::Pair(3), &mut out), 3);
        assert_eq!(out, [3.0, 4.0, 13.0, 14.0, 23.0, 24.0]);
    }

    #[test]
    fn a_mono_channel_is_picked_out_and_the_output_bounds_the_copy() {
        let input = frames(5, 4);
        let mut out = [0.0; 2];
        assert_eq!(extract(&input, 4, Channels::Mono(2), &mut out), 2);
        assert_eq!(out, [2.0, 12.0]);
    }

    #[test]
    fn a_selection_past_the_device_copies_nothing() {
        let input = frames(2, 2);
        let mut out = [0.0; 4];
        assert_eq!(extract(&input, 2, Channels::Pair(2), &mut out), 0);
        assert_eq!(extract(&input, 0, Channels::Mono(1), &mut out), 0);
    }

    #[test]
    fn a_trailing_partial_frame_is_not_read() {
        let mut input = frames(2, 4);
        input.push(99.0);
        let mut out = [0.0; 8];
        assert_eq!(extract(&input, 4, Channels::Mono(1), &mut out), 2);
    }
}
