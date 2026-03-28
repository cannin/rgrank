use std::error::Error;
use std::fs;
use std::io::{self, Cursor, Read, Seek};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use calamine::{Reader, Xlsx, open_workbook_from_rs};
use flate2::read::MultiGzDecoder;
use quick_xml::Reader as XmlReader;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use tar::Archive;
use zip::ZipArchive;

const MAX_ARCHIVE_DEPTH: usize = 4;
const MAX_ARCHIVE_ENTRY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
enum XmlTextMode {
    Word,
    Presentation,
}

#[derive(Clone, Copy)]
enum SupportedKind {
    Docx,
    Pdf,
    Pptx,
    Xlsx,
    Zip,
    Tar,
    TarGz,
}

pub fn extract_searchable_text(path: &Path) -> Result<Option<String>, Box<dyn Error>> {
    let display_name = path.to_string_lossy();
    if is_office_lockfile_name(display_name.as_ref()) {
        return Ok(Some(String::new()));
    }

    let Some(_) = detect_supported_kind(display_name.as_ref()) else {
        return Ok(None);
    };

    let bytes = fs::read(path)?;
    extract_supported_bytes(display_name.as_ref(), &bytes, 0).map(Some)
}

fn extract_supported_bytes(
    name: &str,
    bytes: &[u8],
    depth: usize,
) -> Result<String, Box<dyn Error>> {
    let Some(kind) = detect_supported_kind(name) else {
        return Ok(String::new());
    };

    guard_panic(kind.operation_name(), || match kind {
        SupportedKind::Docx => extract_docx_from_bytes(bytes),
        SupportedKind::Pdf => extract_pdf_from_bytes(bytes),
        SupportedKind::Pptx => extract_pptx_from_bytes(bytes),
        SupportedKind::Xlsx => extract_xlsx_from_bytes(bytes),
        SupportedKind::Zip => extract_zip_archive(bytes, depth),
        SupportedKind::Tar => extract_tar_archive(Cursor::new(bytes), depth),
        SupportedKind::TarGz => {
            let decoder = MultiGzDecoder::new(Cursor::new(bytes));
            extract_tar_archive(decoder, depth)
        }
    })
}

fn extract_archive_member_text(
    name: &str,
    bytes: &[u8],
    depth: usize,
) -> Result<Option<String>, Box<dyn Error>> {
    if is_office_lockfile_name(name) {
        return Ok(Some(String::new()));
    }
    if let Some(kind) = detect_supported_kind(name) {
        if depth >= MAX_ARCHIVE_DEPTH && matches!(
            kind,
            SupportedKind::Zip | SupportedKind::Tar | SupportedKind::TarGz
        ) {
            return Ok(None);
        }
        return Ok(Some(extract_supported_bytes(name, bytes, depth + 1)?));
    }
    Ok(extract_plain_text(bytes))
}

fn detect_supported_kind(name: &str) -> Option<SupportedKind> {
    let lower_name = name.to_ascii_lowercase();
    if lower_name.ends_with(".tar.gz") || lower_name.ends_with(".tgz") {
        return Some(SupportedKind::TarGz);
    }
    if lower_name.ends_with(".docx") {
        return Some(SupportedKind::Docx);
    }
    if lower_name.ends_with(".pdf") {
        return Some(SupportedKind::Pdf);
    }
    if lower_name.ends_with(".pptx") {
        return Some(SupportedKind::Pptx);
    }
    if lower_name.ends_with(".xlsx") {
        return Some(SupportedKind::Xlsx);
    }
    if lower_name.ends_with(".zip") {
        return Some(SupportedKind::Zip);
    }
    if lower_name.ends_with(".tar") {
        return Some(SupportedKind::Tar);
    }
    None
}

impl SupportedKind {
    fn operation_name(self) -> &'static str {
        match self {
            SupportedKind::Docx => "docx extraction",
            SupportedKind::Pdf => "pdf extraction",
            SupportedKind::Pptx => "pptx extraction",
            SupportedKind::Xlsx => "xlsx extraction",
            SupportedKind::Zip => "zip extraction",
            SupportedKind::Tar => "tar extraction",
            SupportedKind::TarGz => "tar.gz extraction",
        }
    }
}

fn is_office_lockfile_name(name: &str) -> bool {
    name.rsplit(['/', '\\'])
        .next()
        .is_some_and(|value| value.starts_with("~$"))
}

fn extract_docx_from_bytes(bytes: &[u8]) -> Result<String, Box<dyn Error>> {
    extract_zip_xml_text_from_reader(
        Cursor::new(bytes),
        |name| {
            name == "word/document.xml"
                || (name.starts_with("word/header") && name.ends_with(".xml"))
                || (name.starts_with("word/footer") && name.ends_with(".xml"))
                || (name.starts_with("word/footnotes") && name.ends_with(".xml"))
                || (name.starts_with("word/endnotes") && name.ends_with(".xml"))
                || (name.starts_with("word/comments") && name.ends_with(".xml"))
        },
        XmlTextMode::Word,
    )
}

fn extract_pptx_from_bytes(bytes: &[u8]) -> Result<String, Box<dyn Error>> {
    extract_zip_xml_text_from_reader(
        Cursor::new(bytes),
        |name| {
            (name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
                || (name.starts_with("ppt/notesSlides/notesSlide") && name.ends_with(".xml"))
        },
        XmlTextMode::Presentation,
    )
}

fn extract_pdf_from_bytes(bytes: &[u8]) -> Result<String, Box<dyn Error>> {
    Ok(clean_extracted_text(pdf_extract::extract_text_from_mem(bytes)?))
}

fn extract_zip_xml_text_from_reader<R, F>(
    reader: R,
    predicate: F,
    mode: XmlTextMode,
) -> Result<String, Box<dyn Error>>
where
    R: Read + Seek,
    F: Fn(&str) -> bool,
{
    let mut archive = ZipArchive::new(reader)?;
    let mut parts = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let entry_name = entry.name().to_owned();
        if !predicate(&entry_name) {
            continue;
        }
        let mut xml = String::new();
        entry.read_to_string(&mut xml)?;
        let extracted = extract_xml_text(&xml, mode)?;
        if !extracted.is_empty() {
            parts.push(extracted);
        }
    }

    Ok(parts.join("\n"))
}

fn extract_xlsx_from_bytes(bytes: &[u8]) -> Result<String, Box<dyn Error>> {
    let mut workbook: Xlsx<_> = open_workbook_from_rs(Cursor::new(bytes.to_vec()))?;
    let mut output = String::new();

    for sheet_name in workbook.sheet_names().to_owned() {
        let range = workbook.worksheet_range(&sheet_name)?;
        let mut wrote_sheet = false;
        for row in range.rows() {
            let cells = row
                .iter()
                .map(|cell| cell.to_string())
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>();
            if cells.is_empty() {
                continue;
            }
            if !wrote_sheet {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("Sheet: ");
                output.push_str(&sheet_name);
                output.push('\n');
                wrote_sheet = true;
            }
            output.push_str(&cells.join("\t"));
            output.push('\n');
        }
    }

    Ok(clean_extracted_text(output))
}

fn extract_zip_archive(bytes: &[u8], depth: usize) -> Result<String, Box<dyn Error>> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut output = String::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.is_file() {
            continue;
        }
        let entry_name = entry.name().to_owned();
        let Some(member_bytes) = read_limited_bytes(&mut entry)? else {
            continue;
        };
        let Ok(Some(text)) = extract_archive_member_text(&entry_name, &member_bytes, depth) else {
            continue;
        };
        append_archive_section(&mut output, &entry_name, &text);
    }

    Ok(clean_extracted_text(output))
}

fn extract_tar_archive<R>(reader: R, depth: usize) -> Result<String, Box<dyn Error>>
where
    R: Read,
{
    let mut archive = Archive::new(reader);
    let mut output = String::new();

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_name = entry.path()?.to_string_lossy().into_owned();
        let Some(member_bytes) = read_limited_bytes(&mut entry)? else {
            continue;
        };
        let Ok(Some(text)) = extract_archive_member_text(&entry_name, &member_bytes, depth) else {
            continue;
        };
        append_archive_section(&mut output, &entry_name, &text);
    }

    Ok(clean_extracted_text(output))
}

fn append_archive_section(output: &mut String, entry_name: &str, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if !output.is_empty() {
        output.push('\n');
        output.push('\n');
    }
    output.push_str("Archive entry: ");
    output.push_str(entry_name);
    output.push('\n');
    output.push_str(trimmed);
}

fn read_limited_bytes<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let mut limited = reader.take((MAX_ARCHIVE_ENTRY_BYTES + 1) as u64);
    limited.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ARCHIVE_ENTRY_BYTES {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn extract_plain_text(bytes: &[u8]) -> Option<String> {
    if !looks_like_text(bytes) {
        return None;
    }
    let text = clean_extracted_text(String::from_utf8_lossy(bytes).into_owned());
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

fn guard_panic<T, F>(operation: &str, callback: F) -> Result<T, Box<dyn Error>>
where
    F: FnOnce() -> Result<T, Box<dyn Error>>,
{
    panic::catch_unwind(AssertUnwindSafe(callback))
        .map_err(|payload| {
            Box::<dyn Error>::from(io::Error::other(format!(
                "{operation} panicked: {}",
                panic_payload_message(payload)
            )))
        })?
}

fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let sample = &bytes[..bytes.len().min(4096)];
    if std::str::from_utf8(sample).is_ok() {
        return true;
    }

    let mut suspicious = 0usize;
    for &byte in sample {
        if byte == 0 {
            return false;
        }
        if matches!(byte, b'\n' | b'\r' | b'\t' | 0x20..=0x7e) {
            continue;
        }
        if byte < 0x09 || (0x0e..=0x1f).contains(&byte) || byte == 0x7f {
            suspicious += 1;
        }
    }

    suspicious * 20 <= sample.len()
}

fn extract_xml_text(xml: &str, mode: XmlTextMode) -> Result<String, Box<dyn Error>> {
    let mut reader = XmlReader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut output = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(event) => {
                let name = event.local_name().as_ref().to_vec();
                match mode {
                    XmlTextMode::Word | XmlTextMode::Presentation => {
                        if name.as_slice() == b"t" {
                            in_text = true;
                        }
                    }
                }
            }
            Event::Empty(event) => {
                let name = event.local_name().as_ref().to_vec();
                match mode {
                    XmlTextMode::Word => {
                        if name.as_slice() == b"tab" {
                            output.push('\t');
                        } else if name.as_slice() == b"br" || name.as_slice() == b"cr" {
                            push_line_break(&mut output);
                        }
                    }
                    XmlTextMode::Presentation => {
                        if name.as_slice() == b"br" {
                            push_line_break(&mut output);
                        }
                    }
                }
            }
            Event::End(event) => {
                let name = event.local_name().as_ref().to_vec();
                match mode {
                    XmlTextMode::Word | XmlTextMode::Presentation => {
                        if name.as_slice() == b"t" {
                            in_text = false;
                        } else if name.as_slice() == b"p" {
                            push_line_break(&mut output);
                        }
                    }
                }
            }
            Event::Text(event) => {
                if in_text {
                    output.push_str(&decode_xml_bytes(&reader, event.as_ref())?);
                }
            }
            Event::CData(event) => {
                if in_text {
                    output.push_str(&reader.decoder().decode(event.as_ref())?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }

    Ok(clean_extracted_text(output))
}

fn decode_xml_bytes(
    reader: &XmlReader<&[u8]>,
    bytes: &[u8],
) -> Result<String, Box<dyn Error>> {
    let decoded = reader.decoder().decode(bytes)?;
    Ok(unescape(decoded.as_ref())?.into_owned())
}

fn push_line_break(output: &mut String) {
    if !output.ends_with('\n') {
        output.push('\n');
    }
}

fn clean_extracted_text(text: String) -> String {
    let mut cleaned_lines = Vec::new();
    let mut previous_blank = true;

    for line in text.lines() {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            if !previous_blank {
                cleaned_lines.push(String::new());
            }
            previous_blank = true;
        } else {
            cleaned_lines.push(collapsed);
            previous_blank = false;
        }
    }

    cleaned_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_panic_converts_panics_to_errors() {
        let error = guard_panic("pdf extraction", || -> Result<(), Box<dyn Error>> {
            panic!("boom");
        })
        .expect_err("panic should become error");
        assert!(error.to_string().contains("pdf extraction panicked: boom"));
    }
}
