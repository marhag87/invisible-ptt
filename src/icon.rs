//! The icon, drawn in code so the repo carries no binary asset.
//!
//! Two very different consumers want these shapes, which is why this file is
//! plain std Rust with no Windows in it:
//!
//! * the tray (`tray::make_icon`), which draws one per [`Status`] at 32x32 and
//!   swaps between them as the daemon's state changes;
//! * `build.rs`, which bakes [`APP`] into the exe as an `.ico` resource, so
//!   Explorer, the Start menu and Add/remove programs have something to show
//!   instead of the generic-application placeholder.
//!
//! build.rs compiles this file into itself rather than importing it - a build
//! script cannot depend on the crate it builds - so nothing here may name
//! `crate::`, and all of it has to compile for the *host* as readily as for
//! the target.
//!
//! Every shape is symmetric about both axes on purpose. `CreateIcon` wants its
//! rows one way round and a `.ico` DIB wants them the other, and a symmetric
//! image is correct either way without anyone having to be sure which is which.

/// What the icon is currently saying. The daemon has no window, so this and
/// the log are the only places its state is visible at all.
///
/// Deliberately three, and deliberately distinguished by *shape* as well as
/// colour: a 32x32 icon on a taskbar is small, and a colour-only difference is
/// no difference to anyone who cannot see it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
// Same as Controls: the Linux stub takes a Status and ignores it.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Status {
    /// No mouse. Either it has not enumerated yet (sign-in) or it went away.
    Waiting = 0,
    /// Configured and listening for the button.
    Ready = 1,
    /// The button is down and its action is asserted.
    Talking = 2,
}

/// The icon the *program* is, as opposed to the one the daemon happens to be
/// showing: what goes in the exe for the shell to find.
///
/// `Ready` rather than `Waiting`, because a shortcut showing the greyed-out
/// "no mouse" state would be claiming something about a program that is not
/// even running; and not `Talking`, which is a half-second condition.
// Read by build.rs, which the daemon's own dead-code analysis cannot see.
#[allow(dead_code)]
pub const APP: Status = Status::Ready;

impl Status {
    /// Mid-tones, so they stay legible on a light *and* a dark taskbar.
    fn rgb(self) -> (u8, u8, u8) {
        match self {
            Status::Waiting => (0x8a, 0x8a, 0x8a), // grey: nothing to talk to
            Status::Ready => (0x2e, 0x9b, 0xf0),   // blue: armed and waiting
            Status::Talking => (0x3d, 0xc2, 0x5f), // green: transmitting
        }
    }
}

/// Samples per pixel per axis. Coverage is coarse - 17 levels of alpha - but
/// the alternative at 16x16 is a circle with corners on it.
const SUPERSAMPLE: u32 = 4;

/// Draw `status` at `size`x`size`: a hollow ring while there is no mouse, a
/// dot inside that ring once it is armed, a filled disc while the button is
/// down.
///
/// Rows run top-down, and each pixel is B, G, R, A - the byte order both
/// `CreateIcon` and a 32bpp DIB read. Alpha is edge coverage, so the result is
/// antialiased; a consumer that cannot blend can threshold it at 128 and get
/// back what a plain point-sampled rasteriser would have produced.
///
/// The geometry is written for 32x32 and scaled, so every size is the same
/// picture rather than the same *numbers* at a different size.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn bgra(size: u32, status: Status) -> Vec<u8> {
    let (r, g, b) = status.rgb();
    let scale = size as f32 / 32.0;
    let centre = size as f32 / 2.0;
    let step = 1.0 / SUPERSAMPLE as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let mut hits = 0u32;
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let dx = x as f32 + (sx as f32 + 0.5) * step - centre;
                    let dy = y as f32 + (sy as f32 + 0.5) * step - centre;
                    // Back into 32x32 units, where the radii are written.
                    let dist = (dx * dx + dy * dy).sqrt() / scale;
                    let ring = (11.0..=14.5).contains(&dist);
                    let ink = match status {
                        Status::Waiting => ring,
                        Status::Ready => ring || dist <= 7.0,
                        Status::Talking => dist <= 14.5,
                    };
                    hits += u32::from(ink);
                }
            }
            if hits == 0 {
                continue;
            }
            let px = ((y * size + x) * 4) as usize;
            out[px] = b;
            out[px + 1] = g;
            out[px + 2] = r;
            out[px + 3] = (hits * 255 / (SUPERSAMPLE * SUPERSAMPLE)) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Status; 3] = [Status::Waiting, Status::Ready, Status::Talking];

    fn alpha(bits: &[u8], size: u32, x: u32, y: u32) -> u8 {
        bits[((y * size + x) * 4 + 3) as usize]
    }

    /// `CreateIcon` reads its rows one way round and the DIB inside an
    /// `RT_ICON` reads them the other, and neither caller flips anything.
    /// That is only correct while every shape is symmetric about both axes -
    /// so the symmetry is a property to hold, not a coincidence to admire.
    #[test]
    fn every_shape_is_symmetric() {
        for status in ALL {
            for size in [16, 32, 47] {
                let bits = bgra(size, status);
                for y in 0..size {
                    for x in 0..size {
                        let here = alpha(&bits, size, x, y);
                        assert_eq!(here, alpha(&bits, size, size - 1 - x, y));
                        assert_eq!(here, alpha(&bits, size, x, size - 1 - y));
                    }
                }
            }
        }
    }

    /// The three are meant to differ by *shape*, not only by colour - a
    /// taskbar icon is small and colour is no help to everyone. The middle is
    /// where they disagree: empty, a dot, and filled in.
    #[test]
    fn the_three_states_differ_by_shape() {
        const N: u32 = 32;
        // Centre, and a point in the gap between the dot and the ring.
        let gap = (N / 2, N / 2 - 9);
        for (status, centre, in_gap) in [
            (Status::Waiting, false, false),
            (Status::Ready, true, false),
            (Status::Talking, true, true),
        ] {
            let bits = bgra(N, status);
            assert_eq!(
                alpha(&bits, N, N / 2, N / 2) > 0,
                centre,
                "{status:?}: centre"
            );
            assert_eq!(alpha(&bits, N, gap.0, gap.1) > 0, in_gap, "{status:?}: gap");
        }
    }

    /// The edges are antialiased, which is the whole reason the coverage goes
    /// in the alpha channel rather than straight into an on/off mask.
    #[test]
    fn edges_are_antialiased() {
        let bits = bgra(64, APP);
        assert!(bits.chunks_exact(4).any(|px| px[3] > 0 && px[3] < 255));
    }
}
