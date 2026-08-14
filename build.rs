//! Bake the app icon into the exe as a Win32 resource.
//!
//! Without one, the exe has no icon at all: the Start menu shortcut, the
//! Explorer entry and Add/remove programs all fall back to the generic
//! application placeholder. The tray icon does not help - that one is built at
//! runtime with `CreateIcon`, and nothing but the running process can see it.
//!
//! The shapes come from `src/icon.rs`, compiled straight into this build
//! script - a build script cannot depend on the crate it builds, so it takes
//! the source rather than the module. One definition of the icon, two
//! consumers; the alternative is a checked-in `.ico` that silently stops
//! matching the tray.
//!
//! ## Why there is a `.res` writer in here
//!
//! `link.exe` takes `.res` files as input directly and turns them into the
//! PE's `.rsrc` section itself, so the only thing missing is something to
//! produce one. The usual answers are a crate that shells out to `rc.exe`
//! (another build-time toolchain to have installed, in CI too) or a checked-in
//! binary. The format is a flat list of length-prefixed records; writing it is
//! the smaller of the three, and matches how `src/hidpp.rs` treats HID++.
//!
//! Format, per record, all little-endian and each record starting on a DWORD
//! boundary: data length, header length, type, name, then DataVersion,
//! MemoryFlags, LanguageId, Version, Characteristics, then the data padded out
//! to a DWORD. Type and name are each either a NUL-terminated UTF-16 string,
//! or `0xFFFF` followed by an id - which is all this needs, every id here
//! being a number. The file opens with a record that is entirely zeroes apart
//! from its own 32-byte header length.

// Only APP and one of the three shapes are reachable from here.
#[allow(dead_code)]
#[path = "src/icon.rs"]
mod icon;

use std::path::PathBuf;

/// Predefined resource types. An icon *file* is a directory plus its images;
/// in a PE those become one `RT_GROUP_ICON` naming N `RT_ICON`s, and the shell
/// asks for the group. `LoadIcon`-style lookup picks the numerically lowest
/// group, which is why the group below is id 1.
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;

/// Every size the shell is likely to ask for. It will scale whatever is
/// nearest, but scaling is exactly what makes an icon look like a placeholder,
/// and these are circles: the whole set costs a few hundred KB uncompressed
/// and almost nothing once the installer's LZMA has seen a flat-coloured disc.
const SIZES: [u32; 8] = [16, 20, 24, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");

    // Both of these describe the *target*, not the machine we are running on:
    // resources are a PE thing, and a `.res` is only understood by link.exe.
    // The Linux build (smoke tests, and CI builds it so it cannot rot) and any
    // -gnu target simply get no icon rather than a broken link.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let res = out.join("invisible-ptt.res");
    std::fs::write(&res, resource_file()).expect("writing the icon resource");

    // -bins, not the blanket form: the icon belongs on the daemon, and there
    // is no reason for every test harness to carry a copy of it.
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

/// The whole `.res`: one `RT_ICON` per size, then the group tying them
/// together.
fn resource_file() -> Vec<u8> {
    let images: Vec<Vec<u8>> = SIZES.iter().map(|&size| dib(size)).collect();

    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes()); // reserved
    group.extend_from_slice(&1u16.to_le_bytes()); // type: icon, not cursor
    group.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, (&size, image)) in SIZES.iter().zip(&images).enumerate() {
        // Width and height are single bytes, so 256 - the largest size there
        // is - is written as 0.
        let side = if size >= 256 { 0 } else { size as u8 };
        group.push(side);
        group.push(side);
        group.push(0); // palette entries: none, this is truecolour
        group.push(0); // reserved
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        group.extend_from_slice(&(image.len() as u32).to_le_bytes());
        group.extend_from_slice(&(i as u16 + 1).to_le_bytes()); // RT_ICON id
    }

    let mut out = Vec::new();
    // The leading empty record. link.exe expects a file to open with one; it
    // carries no data and its own header length is the only non-zero field.
    record(&mut out, 0, 0, 0, 0, &[]);
    for (i, image) in images.iter().enumerate() {
        // 0x1010 and 0x1030 are MOVEABLE|DISCARDABLE and MOVEABLE|PURE|
        // DISCARDABLE: 16-bit memory-management hints that nothing has read
        // since Win32, kept only because every other .res has them.
        record(&mut out, RT_ICON, i as u16 + 1, 0x1010, LANG_EN_US, image);
    }
    record(&mut out, RT_GROUP_ICON, 1, 0x1030, LANG_EN_US, &group);
    out
}

/// US English, which is what a resource compiler stamps on a resource with no
/// `LANGUAGE` statement. The icon has no language, but the field is not
/// optional and matching the convention is free.
const LANG_EN_US: u16 = 0x0409;

/// One `.res` record, with an ordinal type and an ordinal name.
fn record(out: &mut Vec<u8>, ty: u16, name: u16, flags: u16, lang: u16, data: &[u8]) {
    // 32 bytes: the two lengths, two ordinals of four bytes each (so the
    // header needs no alignment padding), and the five trailing fields.
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    out.extend_from_slice(&0xffffu16.to_le_bytes()); // "an ordinal follows"
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&0xffffu16.to_le_bytes());
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    out.extend_from_slice(&lang.to_le_bytes()); // LanguageId
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// One icon image, in the form an `RT_ICON` holds it: a BITMAPINFOHEADER, the
/// colour bits, and a 1bpp AND mask - i.e. an `.ico` entry with no file header
/// in front of it.
///
/// Two things about this layout catch people out. The header claims *twice*
/// the real height, because it is describing colour and mask as one bitmap.
/// And the rows run bottom-up, as in every other DIB; the shapes being
/// symmetric means the order cannot be got wrong here, but the mask has to be
/// flipped to match the colour bits regardless.
fn dib(size: u32) -> Vec<u8> {
    let bgra = icon::bgra(size, icon::APP);
    let row = (size * 4) as usize;

    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(2 * size as i32).to_le_bytes()); // colour + mask
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression: BI_RGB
    out.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage: implied
    for _ in 0..4 {
        // Pixels-per-metre and the two palette counts, none of which apply.
        out.extend_from_slice(&0u32.to_le_bytes());
    }

    for y in (0..size).rev() {
        let start = y as usize * row;
        out.extend_from_slice(&bgra[start..start + row]);
    }

    // The AND mask: 1 leaves the background alone, 0 draws the pixel. Alpha
    // has already said everything this can say, and Windows blends 32bpp icons
    // by alpha; the mask is here because the format has nowhere to say it is
    // absent, and because it is what a pre-alpha caller would fall back to.
    let stride = (size.div_ceil(32) * 4) as usize;
    for y in (0..size).rev() {
        let mut bits = vec![0xffu8; stride];
        for x in 0..size as usize {
            if bgra[y as usize * row + x * 4 + 3] != 0 {
                bits[x / 8] &= !(0x80 >> (x % 8));
            }
        }
        out.extend_from_slice(&bits);
    }
    out
}
