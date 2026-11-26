//! Files in other encodings open as the right text and save back unchanged.
use linework_storage::Document;
use std::{fs, io};

fn text(d: &Document) -> String {
    String::from_utf8(d.read(0, d.len() as usize).unwrap()).unwrap()
}

#[test]
fn legacy_files_are_guessed_and_saved_back() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    for (name, words, enc) in [
        ("ja.txt", "これは日本語の文章です。テキストエディタで正しく開けるか確認します。\r\n", encoding_rs::SHIFT_JIS),
        ("ru.txt", "Это обычный русский текст. Проверяем, что кодировка определяется верно.\r\n", encoding_rs::WINDOWS_1251),
        ("ko.txt", "이것은 한국어 문장입니다. 텍스트 편집기에서 올바르게 열리는지 확인합니다.\r\n", encoding_rs::EUC_KR),
    ] {
        let path = dir.path().join(name);
        let bytes = enc.encode(words).0.into_owned();
        fs::write(&path, &bytes)?;
        let mut d = Document::open(&path)?;
        assert_eq!(d.encoding_name(), enc.name(), "{name}");
        assert_eq!(text(&d), words, "{name}");
        d.replace(0, 0, words[..words.find(' ').unwrap_or(3)].as_bytes())?;
        assert!(d.undo());
        d.replace(d.len(), 0, b"!")?;
        d.save(&path)?;
        let mut expected = bytes.clone();
        expected.push(b'!');
        assert_eq!(fs::read(&path)?, expected, "{name}");
    }
    Ok(())
}

#[test]
fn dos_code_page_round_trips() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("box.txt");
    let bytes: Vec<u8> = (0x20..=0xffu8).collect();
    fs::write(&path, &bytes)?;
    let mut d = Document::open_as(&path, "cp437")?;
    assert_eq!(d.encoding_name(), "IBM437");
    assert!(text(&d).contains("╗╝") && text(&d).contains('╔'));
    d.replace(0, 1, "╚".as_bytes())?;
    d.save(&path)?;
    let mut expected = bytes.clone();
    expected[0] = 0xc8;
    assert_eq!(fs::read(&path)?, expected);
    Ok(())
}

#[test]
fn binary_files_open_as_hex_and_save_back() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("blob.bin");
    let mut bytes: Vec<u8> = (0..=255u8).cycle().take(300_017).collect();
    bytes[5] = 0;
    fs::write(&path, &bytes)?;
    assert!(Document::open(&path).err().unwrap().to_string().contains("binary data"));
    let mut d = Document::open_as(&path, "Hexadecimal")?;
    let shown = text(&d);
    assert!(shown.starts_with("0001 0203 0400 0607 0809 0a0b 0c0d 0e0f\n1011"));
    assert_eq!(shown.lines().count(), bytes.len().div_ceil(16));
    // Unedited, it saves back byte for byte.
    d.save(&path)?;
    assert_eq!(fs::read(&path)?, bytes);
    // Change the first byte to ff.
    d.replace(0, 2, b"ff")?;
    d.save(&path)?;
    bytes[0] = 0xff;
    assert_eq!(fs::read(&path)?, bytes);
    // A stray letter is refused and the file is left alone.
    d.replace(0, 1, b"z")?;
    assert!(d.save(&path).is_err());
    assert_eq!(fs::read(&path)?, bytes);
    Ok(())
}

#[test]
fn text_with_control_lines_first_opens_as_text() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    // A password list sorted by its bytes: lines of zero bytes and control
    // characters come first, then ordinary text.
    let mut bytes = Vec::new();
    for i in 0..4000u32 {
        bytes.extend_from_slice(&[0, (i % 31) as u8, 0, 1]);
        bytes.push(b'\n');
    }
    while bytes.len() < 600_000 {
        bytes.extend_from_slice(format!("password{}\n", bytes.len()).as_bytes());
    }
    let path = dir.path().join("sorted.txt");
    fs::write(&path, &bytes)?;
    let d = Document::open(&path)?;
    assert_eq!(d.encoding_name(), "UTF-8");
    assert_eq!(d.read(0, bytes.len())?, bytes);
    // Binary inside as well: still refused as text.
    let blob: Vec<u8> = (0..=255u8).cycle().take(600_000).collect();
    let path = dir.path().join("blob.bin");
    fs::write(&path, &blob)?;
    assert!(Document::open(&path).err().unwrap().to_string().contains("binary data"));
    Ok(())
}

#[test]
fn new_files_start_clean_in_their_encoding() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("new.txt");
    let mut d = Document::empty_as("UTF-8 BOM")?;
    assert!(!d.dirty());
    d.replace(0, 0, b"hi")?;
    d.save(&path)?;
    assert_eq!(fs::read(&path)?, b"\xef\xbb\xbfhi");
    Ok(())
}
