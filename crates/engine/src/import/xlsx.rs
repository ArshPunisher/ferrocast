//! Reads the first sheet of an `.xlsx` workbook as rows of text. Just enough of the format
//! for the spreadsheets Streamlabs Chatbot exports: shared strings, inline strings, numbers.
//! Every entry is size-capped, so a crafted file can't unpack into gigabytes.

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;

/// Largest single part of the workbook once unpacked.
const MAX_PART: u64 = 64 * 1024 * 1024;
/// Most cells read from a sheet.
const MAX_CELLS: usize = 2_000_000;

/// Why a workbook couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum XlsxError {
    /// Not a workbook at all.
    NotAWorkbook,
    /// A workbook, but damaged or too big to unpack.
    Damaged(String),
}

fn part(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Option<String>, XlsxError> {
    let mut f = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(XlsxError::Damaged(e.to_string())),
    };
    if f.size() > MAX_PART {
        return Err(XlsxError::Damaged("the sheet is too big to open".into()));
    }
    let mut buf = Vec::new();
    f.by_ref().take(MAX_PART + 1).read_to_end(&mut buf).map_err(|e| XlsxError::Damaged(e.to_string()))?;
    if buf.len() as u64 > MAX_PART {
        return Err(XlsxError::Damaged("the sheet is too big to open".into()));
    }
    String::from_utf8(buf).map(Some).map_err(|_| XlsxError::Damaged("its text isn't UTF-8".into()))
}

fn attr(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    e.try_get_attribute(name)
        .ok()
        .flatten()
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

/// Text of an entity reference (`&amp;`, `&#233;`).
fn entity(r: &quick_xml::events::BytesRef<'_>) -> String {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return c.to_string();
    }
    quick_xml::escape::resolve_predefined_entity(r).map(String::from).unwrap_or_default()
}

fn xml_err(e: quick_xml::Error) -> XlsxError {
    XlsxError::Damaged(format!("its XML can't be read ({e})"))
}

fn shared_strings(xml: &str) -> Result<Vec<String>, XlsxError> {
    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let (mut in_si, mut in_t, mut in_rph) = (false, false, false);
    let mut cur = String::new();
    loop {
        match r.read_event().map_err(xml_err)? {
            Event::Start(e) => match e.local_name().as_ref() {
                "si" => {
                    in_si = true;
                    cur.clear();
                }
                "t" if in_si => in_t = true,
                "rPh" => in_rph = true,
                _ => {}
            },
            Event::Empty(e) if e.local_name().as_ref() == "si" => out.push(String::new()),
            Event::Text(t) if in_t && !in_rph => cur.push_str(&t.xml10_content()),
            Event::GeneralRef(g) if in_t && !in_rph => cur.push_str(&entity(&g)),
            Event::End(e) => match e.local_name().as_ref() {
                "si" => {
                    in_si = false;
                    out.push(std::mem::take(&mut cur));
                }
                "t" => in_t = false,
                "rPh" => in_rph = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        if out.len() > MAX_CELLS {
            return Err(XlsxError::Damaged("it has too many cells".into()));
        }
    }
    Ok(out)
}

/// Column index from a cell reference: "C12" → 2.
fn column(r: &str) -> Option<usize> {
    let mut n: usize = 0;
    let mut any = false;
    for b in r.bytes().take_while(u8::is_ascii_alphabetic) {
        n = n.checked_mul(26)?.checked_add(usize::from(b.to_ascii_uppercase() - b'A') + 1)?;
        any = true;
    }
    (any && n <= 16_384).then(|| n - 1)
}

/// Which sheet comes first in the workbook (its path inside the zip).
fn first_sheet(zip: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Result<String, XlsxError> {
    let fallback = "xl/worksheets/sheet1.xml".to_string();
    let Some(wb) = part(zip, "xl/workbook.xml")? else { return Err(XlsxError::NotAWorkbook) };
    let mut rid = None;
    let mut r = Reader::from_str(&wb);
    loop {
        match r.read_event().map_err(xml_err)? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "sheet" => {
                rid = attr(&e, "r:id");
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let (Some(rid), Some(rels)) = (rid, part(zip, "xl/_rels/workbook.xml.rels")?) else { return Ok(fallback) };
    let mut r = Reader::from_str(&rels);
    loop {
        match r.read_event().map_err(xml_err)? {
            Event::Start(e) | Event::Empty(e)
                if e.local_name().as_ref() == "Relationship" && attr(&e, "Id").as_deref() == Some(rid.as_str()) =>
            {
                let Some(target) = attr(&e, "Target") else { return Ok(fallback) };
                let target = target.trim_start_matches('/');
                let path = if target.starts_with("xl/") { target.to_string() } else { format!("xl/{target}") };
                // Only a sheet inside the workbook's own folder.
                return Ok(if path.contains("..") { fallback } else { path });
            }
            Event::Eof => return Ok(fallback),
            _ => {}
        }
    }
}

/// The first sheet's rows, each a list of cell texts (empty cells as "").
pub(crate) fn rows(bytes: &[u8]) -> Result<Vec<Vec<String>>, XlsxError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| XlsxError::NotAWorkbook)?;
    let sheet_path = first_sheet(&mut zip)?;
    let strings = match part(&mut zip, "xl/sharedStrings.xml")? {
        Some(xml) => shared_strings(&xml)?,
        None => Vec::new(),
    };
    let Some(sheet) = part(&mut zip, &sheet_path)? else {
        return Err(XlsxError::Damaged("it has no sheet".into()));
    };
    let mut r = Reader::from_str(&sheet);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell_col: usize = 0;
    let mut cell_type = String::new();
    let mut value = String::new();
    let (mut in_v, mut in_t) = (false, false);
    let mut cells = 0usize;
    loop {
        match r.read_event().map_err(xml_err)? {
            Event::Start(e) => match e.local_name().as_ref() {
                "row" => row = Vec::new(),
                "c" => {
                    cell_col = attr(&e, "r").as_deref().and_then(column).unwrap_or(row.len());
                    cell_type = attr(&e, "t").unwrap_or_default();
                    value.clear();
                }
                "v" => in_v = true,
                "t" => in_t = true,
                _ => {}
            },
            Event::Text(t) if in_v || in_t => value.push_str(&t.xml10_content()),
            Event::GeneralRef(g) if in_v || in_t => value.push_str(&entity(&g)),
            Event::End(e) => match e.local_name().as_ref() {
                "v" => in_v = false,
                "t" => in_t = false,
                "c" => {
                    cells += 1;
                    if cells > MAX_CELLS {
                        return Err(XlsxError::Damaged("it has too many cells".into()));
                    }
                    let text = match cell_type.as_str() {
                        "s" => {
                            value.trim().parse::<usize>().ok().and_then(|i| strings.get(i).cloned()).unwrap_or_default()
                        }
                        "b" => {
                            if value.trim() == "1" {
                                "TRUE".into()
                            } else {
                                "FALSE".into()
                            }
                        }
                        _ => value.clone(),
                    };
                    if cell_col >= row.len() {
                        // Empty cells padded in count too: one cell far to the right must
                        // not cost thousands of allocations per row.
                        cells += cell_col + 1 - row.len();
                        if cells > MAX_CELLS {
                            return Err(XlsxError::Damaged("it has too many cells".into()));
                        }
                        row.resize(cell_col + 1, String::new());
                    }
                    row[cell_col] = text;
                }
                "row" => rows.push(std::mem::take(&mut row)),
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(rows)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal workbook with shared strings, an inline string and numbers.
    pub(crate) fn workbook(rows: &[&[&str]]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut sheet = String::from(
            r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
        );
        for (ri, row) in rows.iter().enumerate() {
            sheet.push_str(&format!("<row r=\"{}\">", ri + 1));
            for (ci, cell) in row.iter().enumerate() {
                let r = format!("{}{}", (b'A' + ci as u8) as char, ri + 1);
                if cell.parse::<f64>().is_ok() {
                    sheet.push_str(&format!("<c r=\"{r}\"><v>{cell}</v></c>"));
                } else {
                    strings.push(cell.replace('&', "&amp;").replace('<', "&lt;"));
                    sheet.push_str(&format!("<c r=\"{r}\" t=\"s\"><v>{}</v></c>", strings.len() - 1));
                }
            }
            sheet.push_str("</row>");
        }
        sheet.push_str("</sheetData></worksheet>");
        let sst = format!(
            "<sst xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">{}</sst>",
            strings.iter().map(|s| format!("<si><t>{s}</t></si>")).collect::<String>()
        );
        let wb = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Currency" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<Relationships><Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let mut out = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut out);
            let o = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("xl/workbook.xml", wb.to_string()),
                ("xl/_rels/workbook.xml.rels", rels.to_string()),
                ("xl/sharedStrings.xml", sst),
                ("xl/worksheets/sheet1.xml", sheet),
            ] {
                z.start_file(name, o).unwrap();
                z.write_all(body.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        out.into_inner()
    }

    #[test]
    fn reads_the_first_sheet() {
        let wb = workbook(&[&["Name", "Rank", "Points", "Hours"], &["Sam & Co", "Viewer", "1500", "12.5"]]);
        let rows = rows(&wb).unwrap();
        assert_eq!(rows, vec![vec!["Name", "Rank", "Points", "Hours"], vec!["Sam & Co", "Viewer", "1500", "12.5"]]);
        assert_eq!(column("A1"), Some(0));
        assert_eq!(column("AB7"), Some(27));
        assert_eq!(column("12"), None);
        // A far-right cell on every row is counted with its padding, so it can't blow up memory.
        let wide: String = (1..=200).map(|i| format!("<row><c r=\"XFD{i}\"><v>1</v></c></row>")).collect();
        let sheet = format!("<worksheet><sheetData>{wide}</sheetData></worksheet>");
        let mut out = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut out);
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("xl/workbook.xml", o).unwrap();
            z.write_all(b"<workbook><sheets><sheet name='x'/></sheets></workbook>").unwrap();
            z.start_file("xl/worksheets/sheet1.xml", o).unwrap();
            z.write_all(sheet.as_bytes()).unwrap();
            z.finish().unwrap();
        }
        assert!(matches!(super::rows(&out.into_inner()), Err(XlsxError::Damaged(_))));
        assert_eq!(super::rows(b"PK\x03\x04 not really"), Err(XlsxError::NotAWorkbook));
    }

    #[test]
    fn inline_strings_and_gaps() {
        let sheet = r#"<worksheet><sheetData><row><c r="A1" t="inlineStr"><is><t>Name</t></is></c><c r="C1" t="inlineStr"><is><t>Points</t></is></c></row></sheetData></worksheet>"#;
        let mut out = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut out);
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("xl/workbook.xml", o).unwrap();
            z.write_all(b"<workbook><sheets><sheet name='x'/></sheets></workbook>").unwrap();
            z.start_file("xl/worksheets/sheet1.xml", o).unwrap();
            z.write_all(sheet.as_bytes()).unwrap();
            z.finish().unwrap();
        }
        assert_eq!(rows(&out.into_inner()).unwrap(), vec![vec!["Name".to_string(), String::new(), "Points".into()]]);
    }
}
