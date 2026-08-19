//! Встраивание системного шрифта в документ.
//!
//! Пока правка идёт прежним шрифтом абзаца, встраивать нечего. Но стоит
//! нажать «полужирный», сменить гарнитуру или набрать символ, которого нет во
//! встроенном подмножестве, — нужен настоящий файл шрифта внутри PDF.
//!
//! Собирается составной шрифт: `Type0` с кодировкой `Identity-H` и потомком
//! `CIDFontType2`. Кодировка выбрана не случайно — однобайтные кодировки
//! ограничены 256 знаками, и кириллица в них не помещается. При `Identity-H`
//! в строке лежат прямо номера глифов, а `CIDToGIDMap /Identity` говорит
//! читателю, что номер CID и есть номер глифа.
//!
//! Обязательная часть — `ToUnicode`: обратная таблица «номер глифа →
//! символ». Без неё текст в файле нельзя ни скопировать, ни найти поиском, ни
//! прочитать обратно — а обратно его читает и сам редактор, когда заново
//! разбирает страницу на блоки.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use lopdf::{Document, Object, ObjectId, Stream, dictionary};

/// Единицы, в которых PDF задаёт ширины глифов.
const PDF_EM: f32 = 1000.0;

/// Сколько записей `bfchar` помещается в один блок по спецификации CMap.
const BFCHAR_CHUNK: usize = 100;

/// Встраивает файл TrueType и возвращает идентификатор объекта шрифта.
///
/// Шрифт кладётся целиком, без выделения подмножества: так любой символ,
/// который пользователь наберёт потом, гарантированно найдётся. Файл от этого
/// тяжелеет на размер гарнитуры, что для правки книги — приемлемая плата за
/// предсказуемость.
pub fn embed_truetype_file(document: &mut Document, path: &Path) -> Result<ObjectId> {
    let data = std::fs::read(path)
        .with_context(|| format!("не удалось прочитать шрифт {}", path.display()))?;
    embed_truetype(document, data)
}

pub fn embed_truetype(document: &mut Document, data: Vec<u8>) -> Result<ObjectId> {
    let face = ttf_parser::Face::parse(&data, 0)
        .map_err(|e| anyhow!("файл не разобрался как TrueType: {e}"))?;

    let units_per_em = face.units_per_em() as f32;
    let scale = PDF_EM / units_per_em;
    let base_name = postscript_name(&face).unwrap_or_else(|| "EmbeddedFont".to_owned());

    let descriptor_id = build_descriptor(document, &face, scale, &base_name, &data)?;
    let cid_font_id = build_cid_font(document, &face, scale, &base_name, descriptor_id);
    let to_unicode_id = build_to_unicode(document, &face);

    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => Object::Name(base_name.into_bytes()),
        "Encoding" => Object::Name(b"Identity-H".to_vec()),
        "DescendantFonts" => vec![Object::Reference(cid_font_id)],
        "ToUnicode" => Object::Reference(to_unicode_id),
    });

    Ok(font_id)
}

/// Добавляет шрифт в ресурсы страницы и возвращает имя, под которым на него
/// можно ссылаться из потока содержимого.
pub fn register_on_page(
    document: &mut Document,
    page_id: ObjectId,
    font_id: ObjectId,
) -> Result<String> {
    // Имя должно быть свободным: в ресурсах уже лежат шрифты документа.
    let taken = existing_font_names(document, page_id);
    let name = (1..)
        .map(|n| format!("PdfEd{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("свободное имя обязано найтись");

    let resources_id = page_resources_id(document, page_id)?;
    let resources = document
        .get_object_mut(resources_id)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("ресурсы страницы недоступны для записи: {e}"))?;

    let fonts = match resources.get_mut(b"Font") {
        Ok(Object::Dictionary(dict)) => dict,
        _ => {
            resources.set("Font", Object::Dictionary(lopdf::Dictionary::new()));
            resources
                .get_mut(b"Font")
                .and_then(|o| o.as_dict_mut())
                .map_err(|e| anyhow!("не удалось создать словарь шрифтов: {e}"))?
        }
    };
    fonts.set(name.clone(), Object::Reference(font_id));

    Ok(name)
}

/// Идентификатор словаря ресурсов страницы.
///
/// Ресурсы могут быть унаследованы от узла дерева страниц, и тогда писать в
/// них нельзя — правка задела бы все страницы сразу. В этом случае страница
/// получает собственную копию.
fn page_resources_id(document: &mut Document, page_id: ObjectId) -> Result<ObjectId> {
    let own = document
        .get_object(page_id)
        .and_then(|o| o.as_dict())
        .map_err(|e| anyhow!("страница недоступна: {e}"))?
        .get(b"Resources")
        .ok()
        .cloned();

    match own {
        Some(Object::Reference(id)) => Ok(id),
        Some(Object::Dictionary(dict)) => {
            // Ресурсы лежат прямо в странице — выносим в отдельный объект,
            // чтобы на них можно было сослаться.
            let id = document.add_object(Object::Dictionary(dict));
            set_page_resources(document, page_id, id)?;
            Ok(id)
        }
        _ => {
            // Унаследованные ресурсы: копируем то, что видит страница, и
            // делаем копию собственной.
            let inherited = document
                .get_page_resources(page_id)
                .ok()
                .and_then(|(dict, _)| dict.cloned())
                .unwrap_or_default();
            let id = document.add_object(Object::Dictionary(inherited));
            set_page_resources(document, page_id, id)?;
            Ok(id)
        }
    }
}

fn set_page_resources(
    document: &mut Document,
    page_id: ObjectId,
    resources: ObjectId,
) -> Result<()> {
    document
        .get_object_mut(page_id)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("страница недоступна для записи: {e}"))?
        .set("Resources", Object::Reference(resources));
    Ok(())
}

fn existing_font_names(document: &Document, page_id: ObjectId) -> Vec<String> {
    let Ok((dict, inherited)) = document.get_page_resources(page_id) else {
        return Vec::new();
    };

    let mut names = Vec::new();
    let mut collect = |resources: &lopdf::Dictionary| {
        if let Ok(Object::Dictionary(fonts)) = resources.get(b"Font") {
            names.extend(
                fonts
                    .iter()
                    .map(|(key, _)| String::from_utf8_lossy(key).into_owned()),
            );
        }
    };
    if let Some(dict) = dict {
        collect(dict);
    }
    for id in inherited {
        if let Ok(Object::Dictionary(resources)) = document.get_object(id) {
            collect(resources);
        }
    }
    names
}

fn build_descriptor(
    document: &mut Document,
    face: &ttf_parser::Face,
    scale: f32,
    base_name: &str,
    data: &[u8],
) -> Result<ObjectId> {
    let length = data.len() as i64;
    let mut stream = Stream::new(dictionary! { "Length1" => length }, data.to_vec());
    // Шрифт — самая тяжёлая часть правки; сжатие уменьшает её в разы.
    let _ = stream.compress();
    let file_id = document.add_object(stream);

    let bbox = face.global_bounding_box();
    let flags = descriptor_flags(face);

    Ok(document.add_object(dictionary! {
        "Type" => "FontDescriptor",
        "FontName" => Object::Name(base_name.as_bytes().to_vec()),
        "Flags" => flags,
        "FontBBox" => vec![
            Object::Integer((bbox.x_min as f32 * scale) as i64),
            Object::Integer((bbox.y_min as f32 * scale) as i64),
            Object::Integer((bbox.x_max as f32 * scale) as i64),
            Object::Integer((bbox.y_max as f32 * scale) as i64),
        ],
        "ItalicAngle" => face.italic_angle() as i64,
        "Ascent" => (face.ascender() as f32 * scale) as i64,
        "Descent" => (face.descender() as f32 * scale) as i64,
        "CapHeight" => face
            .capital_height()
            .map(|v| (v as f32 * scale) as i64)
            .unwrap_or((face.ascender() as f32 * scale) as i64),
        // Толщина основного штриха: точного значения в TrueType нет, а
        // читатели используют его только для подстановки отсутствующего
        // шрифта. Берём общепринятую оценку.
        "StemV" => if face.is_bold() { 160 } else { 80 },
        "FontFile2" => Object::Reference(file_id),
    }))
}

/// Флаги дескриптора по спецификации PDF.
fn descriptor_flags(face: &ttf_parser::Face) -> i64 {
    let mut flags = 0i64;
    if face.is_monospaced() {
        flags |= 1; // FixedPitch
    }
    // Символьным шрифт объявлять нельзя: тогда читатель проигнорирует
    // кодировку. Ставим Nonsymbolic.
    flags |= 1 << 5;
    if face.is_italic() {
        flags |= 1 << 6;
    }
    flags
}

fn build_cid_font(
    document: &mut Document,
    face: &ttf_parser::Face,
    scale: f32,
    base_name: &str,
    descriptor_id: ObjectId,
) -> ObjectId {
    // Ширины всех глифов подряд: при Identity-H номер CID совпадает с номером
    // глифа, поэтому одного диапазона с нуля достаточно.
    let widths: Vec<Object> = (0..face.number_of_glyphs())
        .map(|gid| {
            let advance = face
                .glyph_hor_advance(ttf_parser::GlyphId(gid))
                .unwrap_or(0);
            Object::Integer((advance as f32 * scale).round() as i64)
        })
        .collect();

    document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "CIDFontType2",
        "BaseFont" => Object::Name(base_name.as_bytes().to_vec()),
        "CIDSystemInfo" => dictionary! {
            "Registry" => Object::string_literal("Adobe"),
            "Ordering" => Object::string_literal("Identity"),
            "Supplement" => 0,
        },
        "FontDescriptor" => Object::Reference(descriptor_id),
        "DW" => 1000,
        "W" => vec![Object::Integer(0), Object::Array(widths)],
        "CIDToGIDMap" => Object::Name(b"Identity".to_vec()),
    })
}

/// Обратная таблица «номер глифа → символ».
fn build_to_unicode(document: &mut Document, face: &ttf_parser::Face) -> ObjectId {
    let mapping = glyph_to_unicode(face);

    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n\
         12 dict begin\n\
         begincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n\
         /CMapType 2 def\n\
         1 begincodespacerange\n\
         <0000> <FFFF>\n\
         endcodespacerange\n",
    );

    let entries: Vec<(u16, u32)> = mapping.into_iter().collect();
    for chunk in entries.chunks(BFCHAR_CHUNK) {
        cmap.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (gid, code) in chunk {
            cmap.push_str(&format!("<{:04X}> <{}>\n", gid, utf16_be_hex(*code)));
        }
        cmap.push_str("endbfchar\n");
    }

    cmap.push_str(
        "endcmap\n\
         CMapName currentdict /CMap defineresource pop\n\
         end\n\
         end\n",
    );

    let mut stream = Stream::new(lopdf::Dictionary::new(), cmap.into_bytes());
    let _ = stream.compress();
    document.add_object(stream)
}

/// Символ в шестнадцатеричном UTF-16BE — формате, которого требует CMap.
fn utf16_be_hex(code: u32) -> String {
    let Some(ch) = char::from_u32(code) else {
        return "FFFD".to_owned();
    };
    let mut buffer = [0u16; 2];
    ch.encode_utf16(&mut buffer)
        .iter()
        .map(|unit| format!("{unit:04X}"))
        .collect()
}

/// Соответствие «глиф → символ», собранное из таблицы cmap шрифта.
fn glyph_to_unicode(face: &ttf_parser::Face) -> BTreeMap<u16, u32> {
    let mut mapping = BTreeMap::new();
    let Some(cmap) = face.tables().cmap else {
        return mapping;
    };

    for subtable in cmap.subtables {
        if !subtable.is_unicode() {
            continue;
        }
        // Кодовые точки собираем отдельно: замыкание уже занимает подтаблицу.
        let mut codepoints = Vec::new();
        subtable.codepoints(|code| codepoints.push(code));

        for code in codepoints {
            if let Some(gid) = subtable.glyph_index(code) {
                mapping.entry(gid.0).or_insert(code);
            }
        }
    }
    mapping
}

fn postscript_name(face: &ttf_parser::Face) -> Option<String> {
    // Идентификатор 6 — PostScript-имя, именно оно ожидается в BaseFont.
    face.names()
        .into_iter()
        .filter(|name| name.name_id == 6)
        .find_map(|name| name.to_string())
        .map(|name| name.replace(' ', ""))
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any_system_face() -> Option<Vec<u8>> {
        let fonts = crate::fonts::system_fonts();
        let request = crate::fonts::FontRequest::new("Arial", false, false);
        let face = fonts.resolve(&request)?;
        std::fs::read(&face.path).ok()
    }

    #[test]
    fn utf16_encoding_covers_the_basic_plane_and_beyond() {
        assert_eq!(utf16_be_hex('A' as u32), "0041");
        assert_eq!(utf16_be_hex('я' as u32), "044F");
        // Символ за пределами базовой плоскости кодируется суррогатной парой.
        assert_eq!(utf16_be_hex('𝄞' as u32).len(), 8);
    }

    #[test]
    fn embedded_font_has_every_part_a_reader_needs() {
        let Some(data) = any_system_face() else {
            return;
        };
        let mut document = Document::with_version("1.5");
        let font_id = embed_truetype(&mut document, data).expect("встраивание");

        let font = document.get_object(font_id).unwrap().as_dict().unwrap();
        assert_eq!(font.get(b"Subtype").unwrap().as_name().unwrap(), b"Type0");
        assert_eq!(
            font.get(b"Encoding").unwrap().as_name().unwrap(),
            b"Identity-H"
        );
        assert!(
            font.get(b"ToUnicode").is_ok(),
            "без ToUnicode текст не прочитать обратно"
        );

        let descendants = font.get(b"DescendantFonts").unwrap().as_array().unwrap();
        let Object::Reference(cid_id) = descendants[0] else {
            panic!("ожидалась ссылка")
        };
        let cid = document.get_object(cid_id).unwrap().as_dict().unwrap();
        assert_eq!(
            cid.get(b"Subtype").unwrap().as_name().unwrap(),
            b"CIDFontType2"
        );
        assert_eq!(
            cid.get(b"CIDToGIDMap").unwrap().as_name().unwrap(),
            b"Identity"
        );
        assert!(cid.get(b"W").is_ok(), "без ширин текст разъедется");

        let Object::Reference(descriptor_id) = cid.get(b"FontDescriptor").unwrap() else {
            panic!("ожидалась ссылка на дескриптор")
        };
        let descriptor = document
            .get_object(*descriptor_id)
            .unwrap()
            .as_dict()
            .unwrap();
        assert!(
            descriptor.get(b"FontFile2").is_ok(),
            "программа шрифта обязана быть внутри"
        );
    }

    #[test]
    fn nonsymbolic_flag_is_always_set() {
        let Some(data) = any_system_face() else {
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        // Пятый бит — Nonsymbolic. Со сброшенным читатель игнорирует
        // кодировку, и текст превращается в мусор.
        assert_ne!(descriptor_flags(&face) & (1 << 5), 0);
    }
}
