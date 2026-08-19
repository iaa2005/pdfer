//! Модель документа, с которой работает редактор.
//!
//! Это *не* зеркало структуры PDF. В файле нет ни абзацев, ни стилей — только
//! позиционированные глифы. Модель восстанавливается при открытии (см.
//! [`crate::blocks`]) и при сохранении переписывается обратно в объекты
//! страницы. Такой промежуточный слой — единственный способ дать
//! предсказуемую правку текста: раскладку мы считаем сами, а значит то, что
//! на экране, и то, что в файле, всегда совпадает.

use crate::geom::Rect;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const BLACK: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };

    pub fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 255 }
    }
}

/// Оформление текстового рана.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    /// Имя семейства так, как его отдал pdfium (`FPDFFont_GetFamilyName`).
    /// Для встроенных подмножеств бывает с префиксом вида `ABCDEF+`.
    pub family: String,
    /// Кегль в пунктах.
    pub size: f32,
    /// Насыщенность в шкале CSS: 400 — обычный, 700 — полужирный.
    pub weight: u16,
    pub italic: bool,
    pub color: Rgba,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            family: "Helvetica".into(),
            size: 11.0,
            weight: 400,
            italic: false,
            color: Rgba::BLACK,
        }
    }
}

impl Style {
    pub fn is_bold(&self) -> bool {
        self.weight >= 600
    }

    /// Сравнение с допуском по кеглю: PDF часто хранит 11.999999 вместо 12.
    /// Точное равенство здесь дробило бы один абзац на несколько блоков.
    pub fn visually_eq(&self, other: &Style) -> bool {
        self.family == other.family
            && self.weight == other.weight
            && self.italic == other.italic
            && self.color == other.color
            && (self.size - other.size).abs() < 0.05
    }

    /// Имя семейства без префикса подмножества (`ABCDEF+Times` → `Times`).
    pub fn clean_family(&self) -> &str {
        match self.family.as_bytes() {
            [
                a @ b'A'..=b'Z',
                b'A'..=b'Z',
                b'A'..=b'Z',
                b'A'..=b'Z',
                b'A'..=b'Z',
                b'A'..=b'Z',
                b'+',
                ..,
            ] => {
                let _ = a;
                &self.family[7..]
            }
            _ => &self.family,
        }
    }
}

/// Выключка абзаца. Восстанавливается по геометрии строк, потому что в PDF
/// этой информации нет вообще.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
    Justify,
}

/// Именованный пресет оформления — то, что применяется к блоку одним кликом.
///
/// Ради этого и затевался промежуточный слой: «применить стиль» означает
/// пересчитать раскладку блока с новыми параметрами, а не пытаться
/// отредактировать глифы на месте.
#[derive(Clone, Debug, PartialEq)]
pub struct StyleTemplate {
    pub name: String,
    pub style: Style,
    /// Интерлиньяж как множитель кегля.
    pub line_height: f32,
    pub align: Align,
    /// Абзацный отступ первой строки в пунктах.
    pub first_line_indent: f32,
}

impl StyleTemplate {
    pub fn new(name: impl Into<String>, style: Style) -> Self {
        StyleTemplate {
            name: name.into(),
            style,
            line_height: 1.2,
            align: Align::Left,
            first_line_indent: 0.0,
        }
    }
}

/// Непрерывный кусок текста с одним оформлением.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub text: String,
    pub style: Style,
    /// Начало базовой линии рана.
    pub origin: (f32, f32),
    pub bbox: Rect,
}

impl TextRun {
    pub fn baseline(&self) -> f32 {
        self.origin.1
    }
}

/// Строка — раны, лежащие на одной базовой линии.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub runs: Vec<TextRun>,
    pub bbox: Rect,
    pub baseline: f32,
}

impl Line {
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// Преобладающий кегль строки — по самому длинному рану, а не по среднему:
    /// сноска или номер формулы не должны сдвигать оценку.
    pub fn dominant_size(&self) -> f32 {
        self.dominant_run().map(|r| r.style.size).unwrap_or(0.0)
    }

    /// Оформление самого длинного рана строки.
    pub fn dominant_style(&self) -> Option<&Style> {
        self.dominant_run().map(|r| &r.style)
    }

    fn dominant_run(&self) -> Option<&TextRun> {
        self.runs.iter().max_by_key(|r| r.text.chars().count())
    }
}

/// Абзац: строки, объединённые по интерлиньяжу, выравниванию и кеглю.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub lines: Vec<Line>,
    pub bbox: Rect,
    pub align: Align,
    /// Угол поворота в градусах против часовой стрелки. У обычного текста
    /// ноль; ненулевой приходит только из меток редактора — обычный повёрнутый
    /// текст книги (колонтитули вдоль корешка) в блоки не попадает вовсе.
    pub rotation: f32,
    /// Владелец блока: номер метки редактора либо `None` у нетронутого текста.
    /// По нему правка отличает свой текст от чужого, когда рамки блоков
    /// пересекаются.
    pub mark: Option<i64>,
    /// Номер стиля документа, за которым следует блок.
    pub style: Option<i64>,
}

impl Block {
    /// Текст абзаца со строками, склеенными пробелами. Перенос по дефису
    /// сшивается обратно — иначе при повторной раскладке в середине слова
    /// останется дефис.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            let t = line.text();
            let t = t.trim_end();
            if i > 0 {
                if out.ends_with('-') {
                    out.pop();
                } else {
                    out.push(' ');
                }
            }
            out.push_str(t);
        }
        out
    }

    pub fn dominant_style(&self) -> Style {
        self.lines
            .iter()
            .flat_map(|l| l.runs.iter())
            .max_by_key(|r| r.text.chars().count())
            .map(|r| r.style.clone())
            .unwrap_or_default()
    }

    /// Средний шаг базовых линий. `None`, если строка одна — тогда
    /// интерлиньяж неизвестен и берётся из шаблона.
    pub fn leading(&self) -> Option<f32> {
        if self.lines.len() < 2 {
            return None;
        }
        let first = self.lines.first()?.baseline;
        let last = self.lines.last()?.baseline;
        Some((first - last) / (self.lines.len() - 1) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> TextRun {
        TextRun {
            text: text.into(),
            style: Style::default(),
            origin: (0.0, 0.0),
            bbox: Rect::ZERO,
        }
    }

    fn line(text: &str, baseline: f32) -> Line {
        Line {
            runs: vec![run(text)],
            bbox: Rect::ZERO,
            baseline,
        }
    }

    #[test]
    fn subset_prefix_is_stripped() {
        let s = Style {
            family: "ABCDEF+TimesNewRoman".into(),
            ..Default::default()
        };
        assert_eq!(s.clean_family(), "TimesNewRoman");

        let plain = Style {
            family: "Arial".into(),
            ..Default::default()
        };
        assert_eq!(plain.clean_family(), "Arial");
    }

    #[test]
    fn near_identical_sizes_compare_equal() {
        let a = Style {
            size: 12.0,
            ..Default::default()
        };
        let b = Style {
            size: 11.999_998,
            ..Default::default()
        };
        assert!(a.visually_eq(&b));

        let c = Style {
            size: 12.5,
            ..Default::default()
        };
        assert!(!a.visually_eq(&c));
    }

    #[test]
    fn block_text_rejoins_hyphenated_words() {
        let block = Block {
            lines: vec![line("Anforde-", 100.0), line("rungsniveau", 88.0)],
            bbox: Rect::ZERO,
            align: Align::Left,
            rotation: 0.0,
            mark: None,
            style: None,
        };
        assert_eq!(block.text(), "Anforderungsniveau");
    }

    #[test]
    fn block_text_joins_plain_lines_with_space() {
        let block = Block {
            lines: vec![line("Wirtschaft", 100.0), line("und Recht", 88.0)],
            bbox: Rect::ZERO,
            align: Align::Left,
            rotation: 0.0,
            mark: None,
            style: None,
        };
        assert_eq!(block.text(), "Wirtschaft und Recht");
    }

    #[test]
    fn leading_is_average_baseline_step() {
        let block = Block {
            lines: vec![line("a", 100.0), line("b", 86.0), line("c", 72.0)],
            bbox: Rect::ZERO,
            align: Align::Left,
            rotation: 0.0,
            mark: None,
            style: None,
        };
        assert_eq!(block.leading(), Some(14.0));
        assert_eq!(
            Block {
                lines: vec![line("a", 100.0)],
                bbox: Rect::ZERO,
                align: Align::Left,
                rotation: 0.0,
                mark: None,
                style: None
            }
            .leading(),
            None
        );
    }
}
