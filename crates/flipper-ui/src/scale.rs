//! An app's own size, fitted to the panel.
//!
//! A hosted app draws at whatever size it insists on: Doom renders 320x200 and
//! cannot be told otherwise, since the port sizes its window to its render buffer.
//! When flipctl composites the app rather than letting the compositor put it on the
//! panel directly, that fitting becomes ours to do.
//!
//! Box average rather than nearest, because dropping rows and columns of a 320x200
//! frame loses whole pixels of a game that has few to spare, and averaging costs one
//! pass over the source. Aspect ratio is kept and the remainder is left as it was,
//! so a caller that clears once gets letterbox bars for free.

use crate::pixel::Gray8;

/// Turn a `sw` by `sh` capture a quarter turn and grey it onto the panel, without
/// scaling.
///
/// A portrait app's frame is the panel's size the other way up: 144 by 256 against 256
/// by 144. Resampling it would be a mistake twice over -- it is the same rectangle, so
/// there is nothing to fit, and a box average over a pixel font turns every glyph edge
/// into a grey the design has no token for. So this is a rotation of the source, one
/// destination pixel per source pixel.
///
/// `left` is the direction a `rotate = "left"` manifest promises: the app's top edge
/// becomes the panel's left edge. `(x, y)` from the app lands at panel `(y, sh - 1 - x)`;
/// turning the other way leaves the page upside down.
pub fn turn_from_xrgb(
    src: &[u8],
    stride: u32,
    bgr: bool,
    sw: u32,
    sh: u32,
    dst: &mut [Gray8],
    left: bool,
) {
    let (dw, dh) = (sh, sw);
    if sw == 0 || sh == 0 || dst.len() < (dw * dh) as usize || src.len() < (stride * sh) as usize {
        return;
    }
    // The panel is grey, so whatever is not covered is paper rather than the black a
    // letterboxed fit uses: a turned page fills the frame, and a row that came up short
    // should show as blank rather than as an ink bar.
    dst[..(dw * dh) as usize].fill(Gray8::WHITE);

    for y in 0..sh {
        let row = (y * stride) as usize;
        for x in 0..sw {
            let p = row + (x * 4) as usize;
            let (first, third) = (u32::from(src[p]), u32::from(src[p + 2]));
            let (r, g, b) = if bgr {
                (third, u32::from(src[p + 1]), first)
            } else {
                (first, u32::from(src[p + 1]), third)
            };
            let grey = ((r * 299 + g * 587 + b * 114) / 1000).min(255) as u8;
            // A quarter turn into a `dw` by `dh` frame: left puts the source's top edge
            // on the frame's left, right puts it on the frame's right. Both swap the
            // axes, so the two are mirror images of each other and only one can be right
            // for a given manifest.
            let (dx, dy) = if left { (y, dh - 1 - x) } else { (dw - 1 - y, x) };
            dst[(dy * dw + dx) as usize] = Gray8(grey);
        }
    }
}

/// Scale `src` to fit `dw` by `dh`, centred, keeping its aspect ratio.
///
/// `dst` is not cleared: the fitted image lands in the middle and whatever the
/// caller put in the margins stays there.
pub fn fit(src: &[Gray8], sw: u32, sh: u32, dst: &mut [Gray8], dw: u32, dh: u32) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }
    if src.len() < (sw * sh) as usize || dst.len() < (dw * dh) as usize {
        return;
    }

    // Whole-pixel arithmetic: the largest box that fits, then the offset that
    // centres it. Anything else puts the image half a pixel off and blurs a panel
    // whose pixels are individually visible.
    let (w, h) =
        if sw * dh > dw * sh { (dw, (sh * dw / sw).max(1)) } else { ((sw * dh / sh).max(1), dh) };
    let (ox, oy) = ((dw - w) / 2, (dh - h) / 2);

    for y in 0..h {
        // The source rows this destination row covers, at least one.
        let y0 = y * sh / h;
        let y1 = (((y + 1) * sh / h).max(y0 + 1)).min(sh);
        for x in 0..w {
            let x0 = x * sw / w;
            let x1 = (((x + 1) * sw / w).max(x0 + 1)).min(sw);
            let mut sum = 0u32;
            let mut n = 0u32;
            for sy in y0..y1 {
                let row = (sy * sw) as usize;
                for sx in x0..x1 {
                    sum += u32::from(src[row + sx as usize].0);
                    n += 1;
                }
            }
            dst[((oy + y) * dw + ox + x) as usize] = Gray8((sum / n.max(1)) as u8);
        }
    }
}

/// Scale a 32-bit capture straight into greyscale, fitted to `dw` by `dh`.
///
/// One pass where there used to be four. A hosted app's frame arrived as XRGB, was
/// converted to grey at full size, scaled in a second pass, expanded back to XRGB for
/// the dumb buffer and reduced to gray8 again by the driver. Nothing about that is
/// necessary: the only greyscale anyone needs is the panel-sized one, and this writes
/// it directly from the capture.
///
/// `stride` is the capture's own row length in bytes, `bgr` says its first channel is
/// blue, and the luma is the kernel's own 299/587/114 so a frame we grey and a frame
/// `drm_fb_xrgb8888_to_gray8` greys agree.
pub fn fit_from_xrgb(
    src: &[u8],
    stride: u32,
    bgr: bool,
    sw: u32,
    sh: u32,
    dst: &mut [Gray8],
    dw: u32,
    dh: u32,
) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || dst.len() < (dw * dh) as usize {
        return;
    }
    if src.len() < (stride * sh) as usize {
        return;
    }

    let (w, h) =
        if sw * dh > dw * sh { (dw, (sh * dw / sw).max(1)) } else { ((sw * dh / sh).max(1), dh) };
    let (ox, oy) = ((dw - w) / 2, (dh - h) / 2);

    for y in 0..h {
        let y0 = y * sh / h;
        let y1 = (((y + 1) * sh / h).max(y0 + 1)).min(sh);
        for x in 0..w {
            let x0 = x * sw / w;
            let x1 = (((x + 1) * sw / w).max(x0 + 1)).min(sw);
            let mut sum = 0u32;
            let mut n = 0u32;
            for sy in y0..y1 {
                let row = (sy * stride) as usize;
                for sx in x0..x1 {
                    let p = row + (sx * 4) as usize;
                    let (first, third) = (u32::from(src[p]), u32::from(src[p + 2]));
                    let (r, g, b) = if bgr {
                        (third, u32::from(src[p + 1]), first)
                    } else {
                        (first, u32::from(src[p + 1]), third)
                    };
                    sum += ((r * 299 + g * 587 + b * 114) / 1000).min(255);
                    n += 1;
                }
            }
            dst[((oy + y) * dw + ox + x) as usize] = Gray8((sum / n.max(1)) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fit;
    use crate::pixel::Gray8;

    fn grey(v: &[u8]) -> Vec<Gray8> {
        v.iter().copied().map(Gray8).collect()
    }

    #[test]
    fn a_frame_of_one_value_keeps_it() {
        // Doom's size onto the panel's, which is the case this exists for. The panel
        // is named rather than written out, both because it is the rule here and
        // because the fit has to hold if it ever changes.
        let (pw, ph) = (u32::from(crate::PANEL_W), u32::from(crate::PANEL_H));
        let src = grey(&vec![200; 320 * 200]);
        let mut dst = grey(&vec![0; (pw * ph) as usize]);
        fit(&src, 320, 200, &mut dst, pw, ph);
        let middle = (ph / 2 * pw + pw / 2) as usize;
        assert_eq!(dst[middle].0, 200, "the middle is the image");
        assert_eq!(dst[(ph / 2 * pw) as usize].0, 0, "the left margin is untouched");
    }

    #[test]
    fn averaging_halves_a_checkerboard() {
        // 4x4 alternating 0 and 255 down to 2x2: every box holds one of each.
        let src = grey(&[0, 255, 0, 255, 255, 0, 255, 0, 0, 255, 0, 255, 255, 0, 255, 0]);
        let mut dst = grey(&[9; 4]);
        fit(&src, 4, 4, &mut dst, 2, 2);
        assert!(
            dst.iter().all(|p| p.0 == 127),
            "each pixel is the average of its box, got {:?}",
            dst.iter().map(|p| p.0).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_frame_the_size_of_the_panel_is_copied_across() {
        let mut src = grey(&[0; 8 * 4]);
        src[10] = Gray8(64);
        let mut dst = grey(&[0; 8 * 4]);
        fit(&src, 8, 4, &mut dst, 8, 4);
        assert_eq!(dst[10].0, 64, "no scaling, no offset, no averaging");
    }

    #[test]
    fn a_capture_greys_as_the_driver_would() {
        use super::fit_from_xrgb;
        // One 2x2 capture of a single colour, XRGB order, into a 2x2 destination: no
        // scaling, so each pixel is its own luma and can be checked against the
        // kernel's formula directly.
        let (r, g, b) = (200u8, 100u8, 50u8);
        let src: Vec<u8> = std::iter::repeat([b, g, r, 0]).take(4).flatten().collect();
        let mut dst = grey(&[0; 4]);
        fit_from_xrgb(&src, 8, true, 2, 2, &mut dst, 2, 2);
        let want = Gray8::from_rgb(r, g, b);
        assert!(
            dst.iter().all(|p| p.0.abs_diff(want.0) <= 1),
            "expected about {}, got {:?}",
            want.0,
            dst.iter().map(|p| p.0).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_capture_of_the_wrong_size_is_refused() {
        use super::fit_from_xrgb;
        let src = vec![255u8; 16];
        let mut dst = grey(&[0; 4]);
        fit_from_xrgb(&src, 8, true, 40, 40, &mut dst, 2, 2);
        assert!(dst.iter().all(|p| p.0 == 0), "nothing written");
    }

    /// A portrait app's frame is the panel's size the other way up, so the turn is a
    /// rotation of the pixels rather than a fit, and the corners say which way round.
    #[test]
    fn a_portrait_capture_turns_onto_the_panel() {
        use super::turn_from_xrgb;
        // 4 wide by 2 tall, one distinct luma per corner of the source.
        let (sw, sh) = (4u32, 2u32);
        let luma = |v: u8| [v, v, v, 0];
        let px: Vec<[u8; 4]> = vec![
            luma(10),
            luma(20),
            luma(30),
            luma(40), // top row
            luma(50),
            luma(60),
            luma(70),
            luma(80), // bottom row
        ];
        let src: Vec<u8> = px.concat();
        let mut dst = grey(&[0; 8]);

        // Turned left: the source's top edge becomes the result's left edge. The result
        // is 2 wide by 4 tall, and `(x, y)` lands at `(y, sw - 1 - x)`.
        turn_from_xrgb(&src, sw * 4, true, sw, sh, &mut dst, true);
        let at = |x: usize, y: usize| dst[y * sh as usize + x].0;
        assert_eq!(at(0, 3), 10, "the source's top-left is the result's bottom-left");
        assert_eq!(at(0, 0), 40, "its top-right is the result's top-left");
        assert_eq!(at(1, 3), 50, "and its bottom-left is the result's bottom-right");
        assert_eq!(at(1, 0), 80, "with the bottom-right at the top-right");
    }

    /// The other manifest value turns the other way, and the same pixel ends up in a
    /// different corner: that is what makes `rotate = "right"` a different thing.
    #[test]
    fn a_right_turn_is_not_a_left_turn() {
        use super::turn_from_xrgb;
        let (sw, sh) = (4u32, 2u32);
        let mut src = vec![0u8; (sw * sh * 4) as usize];
        src[0..4].copy_from_slice(&[10, 10, 10, 0]);
        let (mut left, mut right) = (grey(&[0; 8]), grey(&[0; 8]));
        turn_from_xrgb(&src, sw * 4, true, sw, sh, &mut left, true);
        turn_from_xrgb(&src, sw * 4, true, sw, sh, &mut right, false);
        assert_ne!(left, right, "the two turns are the same picture");
        assert_eq!(left.iter().filter(|p| p.0 == 10).count(), 1, "one pixel moves");
        assert_eq!(right.iter().filter(|p| p.0 == 10).count(), 1);
    }

    /// Nothing but the capture's own pixels: a portrait page drawn in the panel's greys
    /// comes out with exactly the greys it went in with. A box average would invent
    /// shades between them, which on this panel is a smudge rather than an edge.
    #[test]
    fn a_turn_invents_no_greys() {
        use super::turn_from_xrgb;
        let (sw, sh) = (8u32, 6u32);
        let src: Vec<u8> = (0..sw * sh)
            .flat_map(|i| {
                let v = if i % 3 == 0 { 0u8 } else { 255 };
                [v, v, v, 0]
            })
            .collect();
        let mut dst = grey(&vec![0; (sw * sh) as usize]);
        turn_from_xrgb(&src, sw * 4, true, sw, sh, &mut dst, true);
        let odd: Vec<u8> = dst.iter().map(|p| p.0).filter(|v| *v != 0 && *v != 255).collect();
        assert!(
            odd.is_empty(),
            "the turn produced {} greys that were not in the capture",
            odd.len()
        );
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_panicking() {
        let src = grey(&[1; 4]);
        let mut dst = grey(&[0; 4]);
        fit(&src, 100, 100, &mut dst, 2, 2);
        assert!(dst.iter().all(|p| p.0 == 0), "nothing written");
    }
}
