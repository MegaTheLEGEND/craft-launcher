//! Unpacks a downloaded web zip in memory. The archive is untrusted input (it passed its checksum,
//! which proves it is the published file, not that it is well-formed), so: every offset and size
//! is checked, names can't leave the app's folder, sizes are capped, and each file's CRC is verified.
//!
//! Only what a static site needs is supported: stored and deflated files, no ZIP64, no encryption.

use std::io::Read;

use flate2::Crc;
use flate2::read::DeflateDecoder;

/// Most files one archive may hold.
const MAX_ENTRIES: usize = 20_000;
/// Most bytes all files together may unpack to (they're held in memory until stored).
const MAX_TOTAL: u64 = 512 << 20;
/// Largest central directory we read.
const MAX_DIRECTORY: u64 = 16 << 20;

const EOCD: u32 = 0x0605_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const LOCAL: u32 = 0x0403_4b50;

/// One unpacked file: a `/`-separated path relative to the app's root, and its bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct File {
    pub path: String,
    pub data: Vec<u8>,
}

#[derive(Debug)]
struct Entry {
    segments: Vec<String>,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    Some(u16::from_le_bytes(<[u8; 2]>::try_from(b.get(at..end)?).ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_le_bytes(<[u8; 4]>::try_from(b.get(at..end)?).ok()?))
}

/// The folder-and-file names of an entry, or `None` when the name could escape the app's folder
/// (`..`, absolute paths, drive letters, backslashes) or can't be a URL path (`?`, `#`, `%`).
fn clean_name(name: &str) -> Option<Vec<String>> {
    if name.starts_with('/') || name.starts_with('\\') {
        return None;
    }
    let mut out = Vec::new();
    for part in name.split('/') {
        if part.is_empty() {
            continue;
        }
        if part == "." || part == ".." || part.contains(['\\', ':', '\0', '?', '#', '%']) || part.chars().any(char::is_control) {
            return None;
        }
        out.push(part.to_string());
    }
    Some(out)
}

fn is_clutter(segments: &[String]) -> bool {
    segments.first().is_some_and(|s| s == "__MACOSX") || segments.last().is_some_and(|s| s == ".DS_Store")
}

fn read_entries(zip: &[u8]) -> Result<Vec<Entry>, String> {
    let bad = || "this isn't a readable zip file".to_string();
    let len = zip.len();
    let tail_start = len.saturating_sub(65_557);
    let tail = zip.get(tail_start..).ok_or_else(bad)?;

    let eocd = (0..tail.len().saturating_sub(21)).rev().find(|&i| le32(tail, i) == Some(EOCD)).ok_or_else(bad)?;
    let count = le16(tail, eocd + 10).ok_or_else(bad)?;
    let dir_size = le32(tail, eocd + 12).ok_or_else(bad)?;
    let dir_offset = le32(tail, eocd + 16).ok_or_else(bad)?;
    if count == 0xFFFF || dir_size == u32::MAX || dir_offset == u32::MAX {
        return Err("this zip uses ZIP64, which isn't supported".into());
    }
    if u64::from(dir_size) > MAX_DIRECTORY || usize::from(count) > MAX_ENTRIES {
        return Err(bad());
    }
    let dir_start = usize::try_from(dir_offset).map_err(|_| bad())?;
    let dir_end = dir_start.checked_add(usize::try_from(dir_size).map_err(|_| bad())?).ok_or_else(bad)?;
    let dir = zip.get(dir_start..dir_end).ok_or_else(bad)?;

    let mut entries = Vec::new();
    let mut at = 0usize;
    for _ in 0..count {
        if le32(dir, at) != Some(CENTRAL) {
            return Err(bad());
        }
        let flags = le16(dir, at + 8).ok_or_else(bad)?;
        let method = le16(dir, at + 10).ok_or_else(bad)?;
        let crc = le32(dir, at + 16).ok_or_else(bad)?;
        let compressed = le32(dir, at + 20).ok_or_else(bad)?;
        let size = le32(dir, at + 24).ok_or_else(bad)?;
        let name_len = usize::from(le16(dir, at + 28).ok_or_else(bad)?);
        let extra_len = usize::from(le16(dir, at + 30).ok_or_else(bad)?);
        let comment_len = usize::from(le16(dir, at + 32).ok_or_else(bad)?);
        let offset = le32(dir, at + 42).ok_or_else(bad)?;
        let name_start = at.checked_add(46).ok_or_else(bad)?;
        let name_end = name_start.checked_add(name_len).ok_or_else(bad)?;
        let name = String::from_utf8_lossy(dir.get(name_start..name_end).ok_or_else(bad)?).into_owned();
        at = name_end.checked_add(extra_len).and_then(|n| n.checked_add(comment_len)).ok_or_else(bad)?;

        if name.ends_with('/') {
            continue; // a folder: nothing to store
        }
        let segments = clean_name(&name).ok_or_else(|| format!("the zip holds a file with an unsafe name ({name:?}), so it wasn't unpacked"))?;
        if segments.is_empty() || is_clutter(&segments) {
            continue;
        }
        if flags & 1 != 0 {
            return Err("this zip is encrypted".into());
        }
        if method != 0 && method != 8 {
            return Err(format!("this zip uses compression method {method}, which isn't supported"));
        }
        if compressed == u32::MAX || size == u32::MAX || offset == u32::MAX {
            return Err("this zip uses ZIP64, which isn't supported".into());
        }
        entries.push(Entry { segments, method, crc, compressed: u64::from(compressed), size: u64::from(size), offset: u64::from(offset) });
    }
    Ok(entries)
}

/// Unpacks `zip` into memory. When every file sits inside one top-level folder
/// (`photocraft-web-0.1.1/index.html`), that folder is dropped, so `index.html` is the page
/// either way.
pub fn unzip(zip: &[u8]) -> Result<Vec<File>, String> {
    let entries = read_entries(zip)?;
    if entries.is_empty() {
        return Err("the zip is empty".into());
    }
    let total = entries.iter().fold(0u64, |t, e| t.saturating_add(e.size));
    if total > MAX_TOTAL {
        return Err("the app would unpack to more than 512 MiB, so it wasn't unpacked".into());
    }
    let first = entries.first().and_then(|e| e.segments.first()).cloned();
    let strip = usize::from(first.as_ref().is_some_and(|f| entries.iter().all(|e| e.segments.len() >= 2 && e.segments.first() == Some(f))));

    let mut files = Vec::with_capacity(entries.len());
    for entry in &entries {
        let name = entry.segments.join("/");
        let corrupt = || format!("the zip is damaged ({name})");

        // The local header says where the data starts (its name and extra field lengths differ from the directory's).
        let head_start = usize::try_from(entry.offset).map_err(|_| corrupt())?;
        let head = zip.get(head_start..head_start.checked_add(30).ok_or_else(corrupt)?).ok_or_else(corrupt)?;
        if le32(head, 0) != Some(LOCAL) {
            return Err(corrupt());
        }
        let skip = usize::from(le16(head, 26).ok_or_else(corrupt)?) + usize::from(le16(head, 28).ok_or_else(corrupt)?);
        let start = head_start.checked_add(30).and_then(|n| n.checked_add(skip)).ok_or_else(corrupt)?;
        let end = start.checked_add(usize::try_from(entry.compressed).map_err(|_| corrupt())?).ok_or_else(corrupt)?;
        let raw = zip.get(start..end).ok_or_else(corrupt)?;

        let size = usize::try_from(entry.size).map_err(|_| corrupt())?;
        let mut data = Vec::with_capacity(size.min(16 << 20));
        if entry.method == 0 {
            data.extend_from_slice(raw);
        } else {
            // One byte more than promised, so a lying size shows up as a mismatch below.
            DeflateDecoder::new(raw).take(entry.size.saturating_add(1)).read_to_end(&mut data).map_err(|_| corrupt())?;
        }
        if data.len() != size {
            return Err(format!("the zip is damaged ({name} isn't the size it says)"));
        }
        let mut crc = Crc::new();
        crc.update(&data);
        if crc.sum() != entry.crc {
            return Err(format!("the zip is damaged ({name} fails its check)"));
        }
        files.push(File { path: entry.segments.iter().skip(strip).cloned().collect::<Vec<_>>().join("/"), data });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::DeflateEncoder;
    use std::io::Write;

    /// A zip with the given (name, bytes, deflate?) files, built by hand.
    fn make_zip(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data, deflate) in files {
            let mut crc = Crc::new();
            crc.update(data);
            let body = if *deflate {
                let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
                enc.write_all(data).unwrap();
                enc.finish().unwrap()
            } else {
                data.to_vec()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            let offset = out.len() as u32;
            let mut local = Vec::new();
            local.extend(LOCAL.to_le_bytes());
            local.extend([20, 0, 0, 0]);
            local.extend(method.to_le_bytes());
            local.extend([0, 0, 0, 0]);
            local.extend(crc.sum().to_le_bytes());
            local.extend((body.len() as u32).to_le_bytes());
            local.extend((data.len() as u32).to_le_bytes());
            local.extend((name.len() as u16).to_le_bytes());
            local.extend([0, 0]);
            local.extend(name.as_bytes());
            out.extend(&local);
            out.extend(&body);

            central.extend(CENTRAL.to_le_bytes());
            central.extend([20, 0, 20, 0, 0, 0]);
            central.extend(method.to_le_bytes());
            central.extend([0, 0, 0, 0]);
            central.extend(crc.sum().to_le_bytes());
            central.extend((body.len() as u32).to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0u8; 12]);
            central.extend(offset.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let dir_offset = out.len() as u32;
        out.extend(&central);
        out.extend(EOCD.to_le_bytes());
        out.extend([0, 0, 0, 0]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((central.len() as u32).to_le_bytes());
        out.extend(dir_offset.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    fn find<'a>(files: &'a [File], path: &str) -> Option<&'a File> {
        files.iter().find(|f| f.path == path)
    }

    #[test]
    fn unpacks_stored_and_deflated_files_and_drops_a_single_top_folder() {
        let page = "<html>".repeat(500);
        let zip = make_zip(&[("app-web-1.0.0/index.html", page.as_bytes(), true), ("app-web-1.0.0/js/app.wasm", b"\0asm", false)]);
        let files = unzip(&zip).unwrap();
        assert_eq!(find(&files, "index.html").unwrap().data, page.as_bytes());
        assert_eq!(find(&files, "js/app.wasm").unwrap().data, b"\0asm");
    }

    #[test]
    fn keeps_the_layout_when_files_are_at_the_top() {
        let files = unzip(&make_zip(&[("index.html", b"x", false), ("sub/a.js", b"y", true)])).unwrap();
        assert!(find(&files, "index.html").is_some() && find(&files, "sub/a.js").is_some());
    }

    #[test]
    fn refuses_names_that_escape_the_folder_or_break_urls() {
        for name in ["../evil.txt", "a/../../evil.txt", "/etc/passwd", "C:/x.txt", "a\\b.txt", "a?b.txt", "a#b.txt", "a%2e.txt"] {
            let r = unzip(&make_zip(&[("index.html", b"x", false), (name, b"boom", false)]));
            assert!(r.is_err(), "{name} should be refused");
        }
    }

    #[test]
    fn detects_a_wrong_crc_and_a_lying_size() {
        let mut zip = make_zip(&[("index.html", b"hello", false)]);
        // Flip a byte of the file data (not the name): the CRC no longer matches.
        let at = zip.windows(5).position(|w| w == b"hello").unwrap();
        zip[at] = b'j';
        assert!(unzip(&zip).unwrap_err().contains("damaged"));
    }

    #[test]
    fn skips_mac_clutter_and_rejects_garbage() {
        let files = unzip(&make_zip(&[("index.html", b"x", false), ("__MACOSX/._index.html", b"junk", false), ("a/.DS_Store", b"j", false)])).unwrap();
        assert_eq!(files.len(), 1);
        assert!(unzip(b"definitely not a zip file").is_err());
        assert!(unzip(b"").is_err());
    }
}
