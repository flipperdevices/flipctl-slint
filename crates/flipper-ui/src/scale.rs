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
///
/// `turn` rotates the capture before it is fitted. A portrait app that names a `size`
/// of its own - `288x512`, say - is not the panel transposed, so it takes this path;
/// without the turn the picture would come out upright in a frame whose status strip and
/// d-pad are already turned. The turn is applied by reading the source in the turned
/// order rather than by allocating a second buffer: `sw` and `sh` swap, and the sample a
/// destination pixel needs is read at the source coordinates the turn maps it to.
#[allow(clippy::too_many_arguments)]
pub fn fit_from_xrgb(
    src: &[u8],
    stride: u32,
    bgr: bool,
    sw: u32,
    sh: u32,
    dst: &mut [Gray8],
    dw: u32,
    dh: u32,
    turn: Option<bool>,
) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || dst.len() < (dw * dh) as usize {
        return;
    }
    if src.len() < (stride * sh) as usize {
        return;
    }
    // The capture's own extent, before any turn: the turn is undone per sample.
    let (src_w, src_h) = (sw, sh);
    // What is fitted is the shape the panel sees, which is the turned one.
    let (turned_w, turned_h) = match turn {
        Some(_) => (src_h, src_w),
        None => (src_w, src_h),
    };

    let (w, h) = if turned_w * dh > dw * turned_h {
        (dw, (turned_h * dw / turned_w).max(1))
    } else {
        ((turned_w * dh / turned_h).max(1), dh)
    };
    let (ox, oy) = ((dw - w) / 2, (dh - h) / 2);

    // Where a pixel of the fitted, turned image comes from in the capture. This is the
    // inverse of the mapping `turn_from_xrgb` writes with, and it has to be exactly that:
    // a turned fit and a turned copy of the same capture must put every pixel in the same
    // place, or a portrait app that names a size of its own comes out 180 degrees from the
    // status strip and the d-pad around it.
    //
    // The direct turn is `(x, y) -> (y, dh - 1 - x)` for `left`, and
    // `(x, y) -> (dw - 1 - y, x)` for right. Inverting either gives what follows.
    let sample = |ix: u32, iy: u32| -> (u32, u32) {
        match turn {
            None => (ix, iy),
            Some(left) => {
                if left {
                    (src_w - 1 - iy, ix)
                } else {
                    (iy, src_h - 1 - ix)
                }
            }
        }
    };

    for y in 0..h {
        let y0 = y * turned_h / h;
        let y1 = (((y + 1) * turned_h / h).max(y0 + 1)).min(turned_h);
        for x in 0..w {
            let x0 = x * turned_w / w;
            let x1 = (((x + 1) * turned_w / w).max(x0 + 1)).min(turned_w);
            let mut sum = 0u32;
            let mut n = 0u32;
            for ty in y0..y1 {
                for tx in x0..x1 {
                    let (sx, sy) = sample(tx, ty);
                    let p = (sy * stride + sx * 4) as usize;
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
        fit_from_xrgb(&src, 8, true, 2, 2, &mut dst, 2, 2, None);
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
        fit_from_xrgb(&src, 8, true, 40, 40, &mut dst, 2, 2, None);
        assert!(dst.iter().all(|p| p.0 == 0), "nothing written");
    }

    /// A turned app whose `size` is not the panel's is fitted, and the fit turns it.
    ///
    /// Without the turn the picture stands upright while the status strip and the d-pad
    /// are already turned: a 2x1 capture whose left pixel is dark would come out with the
    /// dark pixel still on the left, where a left turn puts it at the top.
    #[test]
    fn a_turned_capture_is_fitted_the_way_up_it_is_read() {
        use super::fit_from_xrgb;
        // 2 wide by 1 tall: a dark pixel then a light one.
        let mut src: Vec<u8> = Vec::new();
        for v in [0u8, 255u8] {
            src.extend_from_slice(&[v, v, v, 0]);
        }
        // A 2x1 capture fitted into a 2x2 destination. Unturned it is 2 wide and 1 tall,
        // so it fills the width and sits on one row; turned it is 1 wide and 2 tall, so
        // it fills the height and stands in one column. Which pixels are set says the
        // difference a reader sees: sideways, or upright.
        let mut straight = grey(&[128; 4]);
        fit_from_xrgb(&src, 8, true, 2, 1, &mut straight, 2, 2, None);
        let mut turned = grey(&[128; 4]);
        fit_from_xrgb(&src, 8, true, 2, 1, &mut turned, 2, 2, Some(true));

        // `grey(&[128; 4])` fills the letterbox, so the fitted pixels are the ones that
        // are not 128.
        // `128` is the letterbox the test filled, so a cell that is not 128 was written by
        // the fit. Cells are flat, `y * 2 + x`.
        let filled = |px: &[Gray8]| -> Vec<usize> { (0..4).filter(|i| px[*i].0 != 128).collect() };
        assert_eq!(filled(&straight), vec![0, 1], "unturned: one row, two wide");
        assert_eq!(filled(&turned), vec![0, 2], "turned: one column, two tall");

        // And it is a rotation rather than a relabelling. Which end the dark pixel lands
        // on is not a free choice: it is what `turn_from_xrgb` does to the same capture,
        // where a left turn sends source `(x, y)` to `(y, dh - 1 - x)` - so the light pixel
        // that was on the right comes up at the top of the column.
        assert_eq!(turned[0].0, 255, "the top of the turned column is the light pixel");
        assert_eq!(turned[2].0, 0, "and the bottom is the dark one");

        // The two paths must agree, or a size-naming portrait app comes out 180 degrees
        // from its own chrome. A 2x1 capture turned is 1x2, so a 1x2 destination is the one
        // shape where the fit and the whole turn cover exactly the same pixels and can be
        // compared without a letterbox in between.
        let mut whole = grey(&[128; 2]);
        super::turn_from_xrgb(&src, 8, true, 2, 1, &mut whole, true);
        let mut exact = grey(&[128; 2]);
        fit_from_xrgb(&src, 8, true, 2, 1, &mut exact, 1, 2, Some(true));
        assert_eq!(
            exact.iter().map(|p| p.0).collect::<Vec<_>>(),
            whole.iter().map(|p| p.0).collect::<Vec<_>>(),
            "a turned fit must land where the turned copy does"
        );
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
