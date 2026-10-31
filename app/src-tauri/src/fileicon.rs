//! The icon the shell already shows for a kind of file.
//!
//! A download listed as `setup.exe` should carry the same picture Explorer
//! gives it, because that is the one the person has already learnt. Asking
//! the shell is also the only way to be right about it: the association is
//! per-machine and changes when they install something.
//!
//! Raw pixels go back to the window rather than a PNG. Encoding one here
//! would mean an image crate and a base64 crate to undo it on the other
//! side, when the window already has a canvas that turns pixels into an
//! image in two lines. The window asks once per extension and keeps it.

/// Pixels for one icon: RGBA, top row first, ready for `ImageData`.
#[derive(serde::Serialize)]
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[cfg(not(windows))]
pub fn for_extension(_extension: &str) -> Result<Icon, String> {
    Err("file icons are only read from the Windows shell".into())
}

#[cfg(windows)]
pub fn for_extension(extension: &str) -> Result<Icon, String> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, GetDIBits, GetObjectW,
    };
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Shell::{
        SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};
    use windows::core::PCWSTR;

    // Only ever letters and digits reach the shell. The extension arrives
    // from a filename the server chose, and a name is not a thing to hand
    // to an API that resolves paths.
    let clean: String =
        extension.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect::<String>();
    if clean.is_empty() {
        return Err("not an extension".into());
    }

    // USEFILEATTRIBUTES means the shell answers from the name alone, so no
    // such file has to exist and nothing touches the disk.
    let name: Vec<u16> =
        std::ffi::OsStr::new(&format!("a.{clean}")).encode_wide().chain([0]).collect();

    const FILE_ATTRIBUTE_NORMAL: FILE_FLAGS_AND_ATTRIBUTES = FILE_FLAGS_AND_ATTRIBUTES(0x80);

    let mut info = SHFILEINFOW::default();
    let got = unsafe {
        SHGetFileInfoW(
            PCWSTR(name.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if got == 0 || info.hIcon.is_invalid() {
        return Err(format!("the shell has no icon for .{clean}"));
    }

    // Everything from here owns something the OS gave us, so each exit has
    // to give it back. The work is done in a closure to keep the cleanup in
    // one place rather than on every early return.
    let result = (|| unsafe {
        let mut parts = ICONINFO::default();
        GetIconInfo(info.hIcon, &mut parts).map_err(|error| error.to_string())?;

        let mut shape = BITMAP::default();
        let read = GetObjectW(
            parts.hbmColor.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut shape as *mut _ as *mut _),
        );
        if read == 0 {
            let _ = DeleteObject(parts.hbmColor.into());
            let _ = DeleteObject(parts.hbmMask.into());
            return Err("the icon had no colour bitmap".to_owned());
        }

        let (width, height) = (shape.bmWidth.max(0) as u32, shape.bmHeight.max(0) as u32);
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative so the rows come back top down, the order
                // `ImageData` wants, rather than bottom up as DIBs default.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let dc = CreateCompatibleDC(None);
        let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        let lines = GetDIBits(
            dc,
            parts.hbmColor,
            0,
            height,
            Some(pixels.as_mut_ptr().cast()),
            &mut header,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        let _ = DeleteObject(parts.hbmColor.into());
        let _ = DeleteObject(parts.hbmMask.into());

        if lines == 0 {
            return Err("the icon's pixels could not be read".to_owned());
        }

        // Windows hands these over as BGRA.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }

        // An older icon carries its transparency in a separate mask and
        // leaves this channel at zero throughout, which would otherwise
        // come out as a square of nothing.
        if pixels.chunks_exact(4).all(|pixel| pixel[3] == 0) {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
        }

        Ok(Icon { width, height, rgba: pixels })
    })();

    let _ = unsafe { DestroyIcon(info.hIcon) };
    result
}
