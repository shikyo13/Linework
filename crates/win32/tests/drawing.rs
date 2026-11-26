//! Draws through the Windows calls into an off-screen image and writes it to
//! the file named by the first argument, or to the temporary folder.
#[cfg(target_os = "macos")]
fn main() {
    use linework_win32::Win32::Foundation::RECT;
    use linework_win32::Win32::Graphics::Gdi::*;
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let path = std::env::args().nth(1).map(std::path::PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("linework-drawing.png"));
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, 320, 128);
        ReleaseDC(std::ptr::null_mut(), screen);
        let original = SelectObject(memory, bitmap);
        let fill = |r: RECT, color: u32| {
            SetDCBrushColor(memory, color);
            FillRect(memory, &r, GetStockObject(DC_BRUSH) as _);
        };
        fill(RECT { left: 0, top: 0, right: 320, bottom: 128 }, 0x1F1918);
        // A red block at the top left and a blue one at the bottom right say which way up the image is.
        fill(RECT { left: 0, top: 0, right: 40, bottom: 20 }, 0x0000FF);
        fill(RECT { left: 280, top: 108, right: 320, bottom: 128 }, 0xFF0000);
        let font = CreateFontW(-14, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 0, 0, wide("Segoe UI").as_ptr());
        let old = SelectObject(memory, font as _);
        SetBkMode(memory, 1);
        SetTextColor(memory, 0xF7EDF0);
        let mut area = RECT { left: 50, top: 0, right: 300, bottom: 40 };
        let text = wide("File  Edit  Selection");
        DrawTextW(memory, text.as_ptr(), -1, &mut area, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        SelectObject(memory, old);
        // A clipped pass, as the frame makes when part of it changes: what is
        // drawn inside the clip shows, what is outside does not, and drawing
        // after the pass is not held to the old clip.
        use linework_win32::Win32::Graphics::GdiPlus::*;
        let saved = SaveDC(memory);
        IntersectClipRect(memory, 100, 60, 200, 100);
        let mut graphics = std::ptr::null_mut();
        GdipCreateFromHDC(memory, &mut graphics);
        GdipSetClipRectI(graphics, 100, 60, 100, 40, CombineModeReplace);
        fill(RECT { left: 90, top: 50, right: 150, bottom: 110 }, 0x00FF00);
        GdipDeleteGraphics(graphics);
        RestoreDC(memory, saved);
        fill(RECT { left: 210, top: 60, right: 250, bottom: 100 }, 0x00FFFF);
        // Copy the left half onto a second image, shifted, as the frame is copied to its window.
        let second = CreateCompatibleDC(std::ptr::null_mut());
        let copy = CreateCompatibleBitmap(std::ptr::null_mut(), 320, 128);
        SelectObject(second, copy);
        BitBlt(second, 0, 0, 320, 128, memory, 0, 0, SRCCOPY);
        BitBlt(second, 160, 64, 100, 40, memory, 0, 0, SRCCOPY);
        // What is drawn after a copy must be in the next copy.
        fill(RECT { left: 0, top: 108, right: 20, bottom: 128 }, 0xFF00FF);
        BitBlt(second, 0, 108, 20, 20, memory, 0, 108, SRCCOPY);
        assert!(linework_win32::save_picture(second, &path));
        SelectObject(memory, original);
        println!("{}", path.display());
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
