use std::error::Error;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use calamine::{Reader, Xlsx, open_workbook};
use quick_xml::Reader as XmlReader;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use zip::ZipArchive;

#[derive(Clone, Copy)]
enum XmlTextMode {
    Word,
    Presentation,
}

pub fn extract_searchable_text(path: &Path) -> Result<Option<String>, Box<dyn Error>> {
    let Some(extension) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
    else {
        return Ok(None);
    };

    if is_office_lockfile(path) {
        return Ok(Some(String::new()));
    }

    match extension.as_str() {
        "docx" => Ok(Some(extract_docx(path)?)),
        "pdf" => Ok(Some(clean_extracted_text(pdf_extract::extract_text(path)?))),
        "pptx" => Ok(Some(extract_pptx(path)?)),
        "xlsx" => Ok(Some(extract_xlsx(path)?)),
        _ => Ok(None),
    }
}

fn is_office_lockfile(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.starts_with("~$"))
}

fn extract_docx(path: &Path) -> Result<String, Box<dyn Error>> {
    extract_zip_xml_text(path, |name| {
        name == "word/document.xml"
            || (name.starts_with("word/header") && name.ends_with(".xml"))
            || (name.starts_with("word/footer") && name.ends_with(".xml"))
            || (name.starts_with("word/footnotes") && name.ends_with(".xml"))
            || (name.starts_with("word/endnotes") && name.ends_with(".xml"))
            || (name.starts_with("word/comments") && name.ends_with(".xml"))
    }, XmlTextMode::Word)
}

fn extract_pptx(path: &Path) -> Result<String, Box<dyn Error>> {
    extract_zip_xml_text(path, |name| {
        (name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
            || (name.starts_with("ppt/notesSlides/notesSlide") && name.ends_with(".xml"))
    }, XmlTextMode::Presentation)
}

fn extract_zip_xml_text<F>(
    path: &Path,
    predicate: F,
    mode: XmlTextMode,
) -> Result<String, Box<dyn Error>>
where
    F: Fn(&str) -> bool,
{
    let mut archive = ZipArchive::new(File::open(path)?)?;
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

fn extract_xlsx(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut workbook: Xlsx<_> = open_workbook(path)?;
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
                    XmlTextMode::Word => {
                        if name.as_slice() == b"t" {
                            in_text = true;
                        }
                    }
                    XmlTextMode::Presentation => {
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
                    XmlTextMode::Word => {
                        if name.as_slice() == b"t" {
                            in_text = false;
                        } else if name.as_slice() == b"p" {
                            push_line_break(&mut output);
                        }
                    }
                    XmlTextMode::Presentation => {
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
