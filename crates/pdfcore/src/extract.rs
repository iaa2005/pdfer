//! Извлечение текстовых ранов из страницы.
//!
//! Работа идёт на уровне отдельных символов, а не текстовых объектов страницы.
//! Так надёжнее: один текстовый объект в реальных файлах сплошь и рядом
//! содержит куски разных строк, а иногда и целый абзац, — границы объектов
//! отражают то, как файл был сгенерирован, а не то, как он выглядит.
//! Символы же несут точные координаты и оформление, и раны из них собираются
//! по тем же правилам, по которым читатель видит слова.

use anyhow::{Context, Result};
use pdfium_render::prelude::*;

use crate::geom::Rect;
use crate::model::{Rgba, Style, TextRun};

/// Отклонение угла от горизонтали, при котором символ ещё считается обычным.
const MAX_ANGLE_DEGREES: f32 = 1.0;

/// Результат разбора текстового слоя страницы.
#[derive(Debug, Default)]
pub struct PageText {
    pub runs: Vec<TextRun>,
    /// Сколько символов пропущено из-за поворота. Модель абзацев рассчитана на
    /// горизонтальный текст; повёрнутые надписи (колонтитулы вдоль корешка,
    /// водяные знаки) в неё не укладываются и правке пока не подлежат.
    pub rotated_chars: usize,
}

pub fn extract_page_text(page: &PdfPage) -> Result<PageText> {
    extract_page_text_with(page, false)
}

/// То же, но с возможностью забрать и повёрнутые символы.
///
/// Обычный разбор их пропускает: модель абзацев горизонтальна, и случайные
/// повёрнутые надписи книги (колонтитул вдоль корешка) только мешали бы. Но
/// блок, повёрнутый самим редактором, обязан остаться выделяемым — его
/// вариант страницы разбирается с этим флагом, а угол известен из метки.
pub fn extract_page_text_with(page: &PdfPage, include_rotated: bool) -> Result<PageText> {
    let text = page
        .text()
        .context("не удалось получить текстовый слой страницы")?;

    let mut out = PageText::default();
    let mut current: Option<TextRun> = None;

    for ch in text.chars().iter() {
        let Some(c) = ch.unicode_char() else { continue };
        // pdfium возвращает перевод строки и прочие служебные символы как
        // обычные символы с нулевой геометрией — они только мешают.
        if c.is_control() {
            flush(&mut current, &mut out.runs);
            continue;
        }

        if !include_rotated && ch.angle_degrees().unwrap_or(0.0).abs() > MAX_ANGLE_DEGREES {
            out.rotated_chars += 1;
            flush(&mut current, &mut out.runs);
            continue;
        }

        let Ok(bounds) = ch.loose_bounds() else {
            flush(&mut current, &mut out.runs);
            continue;
        };
        let Ok((origin_x, origin_y)) = ch.origin() else {
            flush(&mut current, &mut out.runs);
            continue;
        };

        let bbox = Rect::new(
            bounds.left().value,
            bounds.bottom().value,
            bounds.right().value,
            bounds.top().value,
        );
        let style = char_style(&ch);
        let origin = (origin_x.value, origin_y.value);

        match current.as_mut() {
            Some(run) if continues(run, &style, origin.1, bbox.left) => {
                run.text.push(c);
                run.bbox = run.bbox.union(&bbox);
            }
            _ => {
                flush(&mut current, &mut out.runs);
                current = Some(TextRun {
                    text: c.to_string(),
                    style,
                    origin,
                    bbox,
                });
            }
        }
    }
    flush(&mut current, &mut out.runs);

    Ok(out)
}

/// Продолжает ли символ текущий ран: то же оформление, та же базовая линия и
/// отсутствие большого разрыва по горизонтали.
fn continues(run: &TextRun, style: &Style, baseline: f32, left: f32) -> bool {
    if !run.style.visually_eq(style) {
        return false;
    }
    if (run.baseline() - baseline).abs() > 0.1 {
        return false;
    }
    // Разрыв шире четырёх кеглей — это уже не пробел, а другая колонка либо
    // выключка по формату; такие места режем, дальше их разберёт `blocks`.
    left - run.bbox.right <= style.size * 4.0
}

fn flush(current: &mut Option<TextRun>, out: &mut Vec<TextRun>) {
    if let Some(run) = current.take()
        && !run.text.is_empty()
    {
        out.push(run);
    }
}

fn char_style(ch: &PdfPageTextChar) -> Style {
    let color = ch
        .fill_color()
        .map(|c| Rgba {
            r: c.red(),
            g: c.green(),
            b: c.blue(),
            a: c.alpha(),
        })
        .unwrap_or(Rgba::BLACK);

    Style {
        family: ch.font_name(),
        size: ch.scaled_font_size().value,
        weight: weight_to_css(ch.font_weight()),
        // Наклон бывает задан как флагом шрифта, так и ненулевым italic angle
        // в его дескрипторе; для нас это одно и то же начертание.
        italic: ch.font_is_italic() || ch.font_is_cursive(),
        color,
    }
}

fn weight_to_css(weight: Option<PdfFontWeight>) -> u16 {
    match weight {
        Some(PdfFontWeight::Weight100) => 100,
        Some(PdfFontWeight::Weight200) => 200,
        Some(PdfFontWeight::Weight300) => 300,
        Some(PdfFontWeight::Weight400Normal) => 400,
        Some(PdfFontWeight::Weight500) => 500,
        Some(PdfFontWeight::Weight600) => 600,
        Some(PdfFontWeight::Weight700Bold) => 700,
        Some(PdfFontWeight::Weight800) => 800,
        Some(PdfFontWeight::Weight900) => 900,
        Some(PdfFontWeight::Custom(v)) => (v as u16).clamp(1, 1000),
        None => 400,
    }
}
