//! Каталог именованных стилей, живущий в самом документе.
//!
//! Стиль — это набор «гарнитура, кегль, цвет, начертание» с именем. Блоки
//! ссылаются на стиль через свою метку (`/St` в словаре `BDC`), а каталог
//! лежит отдельным объектом, на который указывает ключ `PdfEdStyles` в
//! трейлере. Правка стиля перенабирает все его блоки; удаление стиля просто
//! рвёт связь — блоки остаются как есть, только больше не следуют за стилем.

use lopdf::{Dictionary, Document, Object, dictionary};

/// Именованный стиль документа.
#[derive(Clone, Debug, PartialEq)]
pub struct StyleDef {
    /// Номер стиля — ключ связи с блоками. Никогда не переиспользуется.
    pub id: i64,
    pub name: String,
    /// Гарнитура. `None` — блоки сохраняют свою.
    pub family: Option<String>,
    pub size: Option<f32>,
    pub color: Option<[f32; 3]>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl StyleDef {
    /// Новый пустой стиль с очередным свободным номером.
    pub fn fresh(taken: &[StyleDef]) -> StyleDef {
        let id = taken.iter().map(|def| def.id).max().unwrap_or(0) + 1;
        StyleDef {
            id,
            name: format!("Стиль {id}"),
            family: None,
            size: None,
            color: None,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

const TRAILER_KEY: &str = "PdfEdStyles";

/// Читает каталог стилей документа. Пусто — если каталога нет.
pub fn read_styles(document: &Document) -> Vec<StyleDef> {
    let Ok(Object::Reference(id)) = document.trailer.get(TRAILER_KEY.as_bytes()) else {
        return Vec::new();
    };
    let Ok(catalog) = document.get_object(*id).and_then(|o| o.as_dict()) else {
        return Vec::new();
    };

    let mut styles = Vec::new();
    for (key, value) in catalog.iter() {
        let Ok(id) = String::from_utf8_lossy(key).parse::<i64>() else {
            continue;
        };
        let entry = match value {
            Object::Dictionary(entry) => entry,
            Object::Reference(reference) => {
                let Ok(entry) = document.get_object(*reference).and_then(|o| o.as_dict()) else {
                    continue;
                };
                entry
            }
            _ => continue,
        };
        styles.push(decode(id, entry));
    }
    // Порядок стабильный: по номеру, а не по прихоти словаря.
    styles.sort_by_key(|def| def.id);
    styles
}

/// Записывает каталог стилей, заменяя прежний.
pub fn write_styles(document: &mut Document, styles: &[StyleDef]) {
    let mut catalog = Dictionary::new();
    for def in styles {
        catalog.set(def.id.to_string(), Object::Dictionary(encode(def)));
    }

    match document.trailer.get(TRAILER_KEY.as_bytes()) {
        Ok(Object::Reference(id)) => {
            let id = *id;
            document.objects.insert(id, Object::Dictionary(catalog));
        }
        _ => {
            let id = document.add_object(Object::Dictionary(catalog));
            document.trailer.set(TRAILER_KEY, Object::Reference(id));
        }
    }
}

fn encode(def: &StyleDef) -> Dictionary {
    let mut entry = dictionary! {
        "Name" => Object::string_literal(def.name.as_str()),
    };
    if let Some(family) = &def.family {
        entry.set("Family", Object::string_literal(family.as_str()));
    }
    if let Some(size) = def.size {
        entry.set("Size", Object::Real(size));
    }
    if let Some([r, g, b]) = def.color {
        entry.set(
            "Color",
            Object::Array(vec![Object::Real(r), Object::Real(g), Object::Real(b)]),
        );
    }
    if def.bold {
        entry.set("Bold", Object::Boolean(true));
    }
    if def.italic {
        entry.set("Italic", Object::Boolean(true));
    }
    if def.underline {
        entry.set("Underline", Object::Boolean(true));
    }
    entry
}

fn decode(id: i64, entry: &Dictionary) -> StyleDef {
    let text = |key: &str| {
        entry
            .get(key.as_bytes())
            .ok()
            .and_then(|o| o.as_str().ok())
            .map(|raw| String::from_utf8_lossy(raw).into_owned())
    };
    let flag = |key: &str| matches!(entry.get(key.as_bytes()), Ok(Object::Boolean(true)));
    let number = |key: &str| match entry.get(key.as_bytes()) {
        Ok(Object::Real(value)) => Some(*value),
        Ok(Object::Integer(value)) => Some(*value as f32),
        _ => None,
    };
    let color = entry
        .get(b"Color")
        .ok()
        .and_then(|o| o.as_array().ok())
        .and_then(|parts| {
            let mut channels = parts.iter().filter_map(|part| match part {
                Object::Real(value) => Some(*value),
                Object::Integer(value) => Some(*value as f32),
                _ => None,
            });
            Some([channels.next()?, channels.next()?, channels.next()?])
        });

    StyleDef {
        id,
        name: text("Name").unwrap_or_else(|| format!("Стиль {id}")),
        family: text("Family"),
        size: number("Size"),
        color,
        bold: flag("Bold"),
        italic: flag("Italic"),
        underline: flag("Underline"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_survive_a_write_read_cycle() {
        let mut doc = Document::with_version("1.5");
        let styles = vec![
            StyleDef {
                id: 1,
                name: "Заголовок".into(),
                family: Some("PT Serif".into()),
                size: Some(18.0),
                color: Some([0.8, 0.1, 0.1]),
                bold: true,
                italic: false,
                underline: false,
            },
            StyleDef {
                id: 2,
                name: "Подпись".into(),
                family: None,
                size: Some(8.5),
                color: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ];
        write_styles(&mut doc, &styles);
        assert_eq!(read_styles(&doc), styles);

        // Перезапись заменяет каталог целиком: удалённый стиль исчезает.
        write_styles(&mut doc, &styles[1..]);
        assert_eq!(read_styles(&doc), styles[1..]);
    }

    #[test]
    fn a_fresh_style_never_reuses_an_id() {
        let mut existing = vec![StyleDef::fresh(&[])];
        assert_eq!(existing[0].id, 1);
        let second = StyleDef::fresh(&existing);
        assert_eq!(second.id, 2);
        existing.remove(0);
        existing.push(second);
        // Первый номер освободился, но новый стиль его не занимает.
        assert_eq!(StyleDef::fresh(&existing).id, 3);
    }
}
