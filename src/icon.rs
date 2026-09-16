//! The tray icon: one of two pre-rendered `.ico` files, picked by
//! configuration and WSL2 state.
//!
//! Both files (`assets/icon-color.ico`, `assets/icon-mono.ico`) are embedded
//! with `include_bytes!` and parsed directly: [`load`] reads the `.ico`
//! container's `ICONDIR`/`ICONDIRENTRY` structures to find the embedded image
//! closest to the requested pixel size, then hands its raw bytes to
//! `CreateIconFromResourceEx`. That function already understands both the
//! classic DIB layout and the PNG-compressed payload a modern `.ico` uses for
//! its largest entry, so no image decoding of our own is needed either way.

use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, HICON, LR_DEFAULTCOLOR,
};

/// Which of the two embedded icons to load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Color,
    Mono,
}

static ICON_COLOR: &[u8] = include_bytes!("../assets/icon-color.ico");
static ICON_MONO: &[u8] = include_bytes!("../assets/icon-mono.ico");

impl Kind {
    fn bytes(self) -> &'static [u8] {
        match self {
            Kind::Color => ICON_COLOR,
            Kind::Mono => ICON_MONO,
        }
    }
}

/// Loads `kind` as an `HICON` at (as close as the `.ico` allows to) `size`
/// pixels square. The caller owns the returned handle and must destroy it
/// with `DestroyIcon`.
pub fn load(kind: Kind, size: usize) -> Result<HICON, String> {
    let data = kind.bytes();
    let entry = best_entry(data, size)?;
    let image = data
        .get(entry.offset..entry.offset + entry.len)
        .ok_or("icon directory entry points outside the file")?;
    let hicon = unsafe {
        CreateIconFromResourceEx(
            image.as_ptr(),
            image.len() as u32,
            1,          // fIcon: TRUE, an icon rather than a cursor
            0x00030000, // dwVer: the only value current Windows versions expect
            0,
            0, // cxDesired/cyDesired: 0 with no LR_DEFAULTSIZE uses the entry's own size
            LR_DEFAULTCOLOR,
        )
    };
    if hicon.is_null() {
        Err("CreateIconFromResourceEx failed".to_string())
    } else {
        Ok(hicon)
    }
}

/// One `ICONDIRENTRY`: the pixel width of that image and where its bits live
/// in the file.
struct Entry {
    width: u32,
    offset: usize,
    len: usize,
}

/// Parses the `ICONDIR` header and `ICONDIRENTRY` array (see
/// [the icon resource format](https://learn.microsoft.com/windows/win32/menurc/resource-file-formats#icon-resource-format))
/// and returns the entry whose (square) size is closest to `size`, biased
/// towards the larger one on a tie so the shell downscales rather than
/// upscales.
fn best_entry(data: &[u8], size: usize) -> Result<Entry, String> {
    let count = read_u16(data, 4).ok_or("truncated ICO header")? as usize;
    let mut best: Option<Entry> = None;
    for i in 0..count {
        let rec = 6 + i * 16;
        let width = match data.get(rec).copied() {
            Some(0) => 256,
            Some(w) => w as u32,
            None => return Err("truncated ICONDIRENTRY".to_string()),
        };
        let len = read_u32(data, rec + 8).ok_or("truncated ICONDIRENTRY")? as usize;
        let offset = read_u32(data, rec + 12).ok_or("truncated ICONDIRENTRY")? as usize;
        let entry = Entry { width, offset, len };
        let better = match &best {
            None => true,
            Some(b) => {
                let d = width.abs_diff(size as u32);
                let bd = b.width.abs_diff(size as u32);
                d < bd || (d == bd && width > b.width)
            }
        };
        if better {
            best = Some(entry);
        }
    }
    best.ok_or_else(|| "ICO file has no images".to_string())
}

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_icons_load_at_every_tray_size() {
        for kind in [Kind::Color, Kind::Mono] {
            for size in [16, 20, 24, 32, 48] {
                let hicon = load(kind, size).unwrap();
                assert!(!hicon.is_null());
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(hicon);
                }
            }
        }
    }

    #[test]
    fn best_entry_picks_the_closest_size() {
        // Both embedded files carry 16, 20, 24, 32, 40, 48, 64 and 256.
        assert_eq!(best_entry(ICON_COLOR, 16).unwrap().width, 16);
        assert_eq!(best_entry(ICON_COLOR, 24).unwrap().width, 24);
        assert_eq!(best_entry(ICON_COLOR, 256).unwrap().width, 256);
        // 100 is closer to 64 than to 256, and closer sizes should never
        // round up to the huge one.
        assert_eq!(best_entry(ICON_COLOR, 100).unwrap().width, 64);
    }

    #[test]
    fn rejects_a_truncated_file() {
        assert!(best_entry(&ICON_COLOR[..4], 16).is_err());
    }
}
