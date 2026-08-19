//! Раскладка правимого абзаца по метрикам самого документа.
//!
//! Текст на экране рисует pdfium — это настоящая страница, а не копия поверх
//! неё. Редактор ничего не перерисовывает: он показывает только рамку, каретку
//! и подсветку выделения. Значит их положения обязаны совпадать с настоящими
//! буквами до доли пункта.
//!
//! Поэтому ширины берутся у **того же шрифта, которым набрана страница**
//! ([`pdfcore::stream_edit::Encoder`]), а не у системного. Системный дал бы
//! другие ширины, и каретка расходилась бы с текстом тем сильнее, чем длиннее
//! строка. По тем же метрикам переносятся строки — иначе перенос на экране
//! случился бы не там, где случится в документе.
//!
//! Все расчёты идут в пунктах документа и переводятся в пиксели только на
//! выходе, множителем текущего масштаба.

use std::ops::Range;

use gpui::{Hsla, Pixels, Point, point, px};
use pdfcore::stream_edit::{Encoder, Script};

use crate::rich_text::{RichText, RunStyle};

/// Оформление абзаца, от которого отсчитывается всё остальное.
#[derive(Clone)]
pub struct BaseStyle {
    /// Кегль абзаца в пунктах документа.
    pub size_points: f32,
    /// Интерлиньяж в пунктах.
    pub line_height_points: f32,
    /// Сколько экранных пикселей приходится на пункт при текущем масштабе.
    pub points_to_px: f32,
    /// Цвет каретки — тот же, которым набран абзац.
    pub color: Hsla,
    /// Метрики шрифта страницы. Приходят из документа с задержкой, поэтому
    /// поначалу пусто и раскладка держится на грубой оценке.
    pub metrics: Option<Encoder>,
    /// Метрики остальных шрифтов абзаца, по ключу гарнитуры. Пёстрый абзац —
    /// обычный, полужирный, курсив — меряется каждым куском по его же шрифту:
    /// полужирные буквы шире обычных, и одна общая линейка на всех гнула
    /// раскладку — слова «прыгали» относительно настоящей страницы.
    pub metrics_by_family: std::collections::HashMap<String, Encoder>,
}

/// Слово или промежуток между словами, уже обмеренные.
#[derive(Clone)]
struct Piece {
    text: String,
    /// Диапазон в тексте модели.
    range: Range<usize>,
    /// Ширина в пунктах.
    width: f32,
    /// Кегль куска в пунктах, уже с поправкой на индекс.
    size: f32,
    /// Пробельный кусок можно оставить висеть за правым полем.
    blank: bool,
}

#[derive(Clone)]
struct Placed {
    piece: usize,
    /// Отступ от левого края блока в пунктах.
    x: f32,
}

#[derive(Clone)]
struct LaidLine {
    placed: Vec<Placed>,
    range: Range<usize>,
    /// Отступ от верха блока в пунктах.
    top: f32,
    height: f32,
}

/// Готовая раскладка абзаца.
#[derive(Clone)]
pub struct TextLayout {
    pieces: Vec<Piece>,
    lines: Vec<LaidLine>,
    metrics: Option<Encoder>,
    scale: f32,
    /// Ширина самой длинной строки в пикселях. По ней рамка садится по тексту,
    /// а не растягивается на всю исходную ширину блока.
    pub width: Pixels,
    pub height: Pixels,
}

impl TextLayout {
    /// Верстает текст по ширине блока, заданной в пикселях.
    pub fn build(text: &RichText, base: &BaseStyle, wrap_width: Pixels) -> TextLayout {
        let scale = base.points_to_px.max(0.01);
        let pieces = measure_pieces(text, base);
        let line_height = if base.line_height_points > 0.0 {
            base.line_height_points
        } else {
            base.size_points * 1.2
        };
        let lines = wrap(
            &pieces,
            (f32::from(wrap_width) / scale).max(1.0),
            line_height,
        );

        let width = lines
            .iter()
            .map(|line| {
                line.placed
                    .last()
                    .map(|placed| placed.x + pieces[placed.piece].width)
                    .unwrap_or(0.0)
            })
            .fold(0.0f32, f32::max);
        let height = lines
            .last()
            .map(|line| line.top + line.height)
            .unwrap_or(line_height);

        TextLayout {
            pieces,
            lines,
            metrics: base.metrics.clone(),
            scale,
            width: px(width * scale),
            height: px(height * scale),
        }
    }

    /// Положение каретки: точка у верха строки и её высота.
    pub fn caret(&self, offset: usize) -> (Point<Pixels>, Pixels) {
        let Some(line) = self.line_for_offset(offset) else {
            return (point(px(0.0), px(0.0)), self.height);
        };
        let x = self.x_in_line(line, offset);
        (self.to_px(x, line.top), px(line.height * self.scale))
    }

    /// Смещение, в которое попал курсор.
    pub fn offset_at(&self, position: Point<Pixels>) -> usize {
        let (x, y) = (
            f32::from(position.x) / self.scale,
            f32::from(position.y) / self.scale,
        );

        let Some(line) = self
            .lines
            .iter()
            .find(|line| y < line.top + line.height)
            .or_else(|| self.lines.last())
        else {
            return 0;
        };

        let mut best = line.range.start;
        for placed in &line.placed {
            let piece = &self.pieces[placed.piece];
            if x < placed.x {
                return piece.range.start;
            }
            if x <= placed.x + piece.width {
                return piece.range.start + self.index_within(piece, x - placed.x);
            }
            best = piece.range.end;
        }
        best
    }

    /// Прямоугольники выделения — по одному на строку.
    pub fn selection_rects(&self, range: Range<usize>) -> Vec<(Point<Pixels>, Pixels, Pixels)> {
        if range.is_empty() {
            return Vec::new();
        }

        let mut rects = Vec::new();
        for line in &self.lines {
            let from = range.start.max(line.range.start);
            let to = range.end.min(line.range.end);
            if from >= to {
                continue;
            }
            // Оба края меряются внутри одной и той же строки. Искать строку по
            // смещению здесь нельзя: на стыке одно и то же смещение принадлежит
            // и концу предыдущей строки, и началу следующей, и подсветка
            // ложилась кусками не на свои места.
            let start = self.x_in_line(line, from);
            let end = self.x_in_line(line, to);
            rects.push((
                self.to_px(start, line.top),
                px((end - start) * self.scale),
                px(line.height * self.scale),
            ));
        }
        rects
    }

    fn to_px(&self, x: f32, y: f32) -> Point<Pixels> {
        point(px(x * self.scale), px(y * self.scale))
    }

    /// Строка, которой принадлежит смещение.
    ///
    /// На стыке предпочитается **следующая**: каретка в конце перенесённой
    /// строки визуально стоит в начале следующей.
    fn line_for_offset(&self, offset: usize) -> Option<&LaidLine> {
        self.lines
            .iter()
            .find(|line| offset < line.range.end)
            .or_else(|| self.lines.last())
    }

    /// Горизонтальное положение смещения внутри заданной строки, в пунктах.
    fn x_in_line(&self, line: &LaidLine, offset: usize) -> f32 {
        let mut x = line.placed.first().map(|placed| placed.x).unwrap_or(0.0);
        for placed in &line.placed {
            let piece = &self.pieces[placed.piece];
            if offset <= piece.range.start {
                return placed.x;
            }
            if offset <= piece.range.end {
                let prefix = &piece.text[..offset - piece.range.start];
                return placed.x + self.measure(prefix, piece.size);
            }
            x = placed.x + piece.width;
        }
        x
    }

    /// Сколько байтов слова остаётся левее заданного отступа.
    fn index_within(&self, piece: &Piece, offset_x: f32) -> usize {
        let mut previous = 0;
        for (index, _) in piece.text.char_indices().skip(1) {
            let width = self.measure(&piece.text[..index], piece.size);
            if width > offset_x {
                // Каретка встаёт к ближайшему стыку букв, а не к левому краю
                // той, на которую пришёлся клик.
                let left = self.measure(&piece.text[..previous], piece.size);
                return if offset_x - left <= width - offset_x {
                    previous
                } else {
                    index
                };
            }
            previous = index;
        }
        let left = self.measure(&piece.text[..previous], piece.size);
        if offset_x - left <= piece.width - offset_x {
            previous
        } else {
            piece.text.len()
        }
    }

    fn measure(&self, text: &str, size: f32) -> f32 {
        measure_with(self.metrics.as_ref(), text, size)
    }
}

/// Ширина строки в пунктах. Пока метрики документа не пришли, остаётся грубая
/// оценка: полкегля на букву. Каретка при этом неточна, но раскладка уже есть,
/// и первый кадр не приходится ждать.
fn measure_with(metrics: Option<&Encoder>, text: &str, size: f32) -> f32 {
    match metrics {
        Some(metrics) => metrics.width(text, size),
        None => text.chars().count() as f32 * size * 0.5,
    }
}

fn measure_pieces(text: &RichText, base: &BaseStyle) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut offset = 0;

    for run in text.runs() {
        let size = effective_size(&run.style, base);
        let metrics = metrics_for_run(&run.style, base);
        for token in tokenize(&run.text) {
            let slice = &run.text[token.start..token.end];
            pieces.push(Piece {
                text: slice.to_owned(),
                range: offset + token.start..offset + token.end,
                width: measure_with(metrics, slice, size),
                size,
                blank: slice.chars().all(char::is_whitespace),
            });
        }
        offset += run.text.len();
    }
    pieces
}

/// Метрики для куска: его собственный шрифт, если он известен, иначе общий.
fn metrics_for_run<'a>(style: &RunStyle, base: &'a BaseStyle) -> Option<&'a Encoder> {
    let family = style.family.as_deref().or(style.document_family.as_deref());
    family
        .map(pdfcore::fonts::family_key)
        .and_then(|key| base.metrics_by_family.get(&key))
        .or(base.metrics.as_ref())
}

fn effective_size(style: &RunStyle, base: &BaseStyle) -> f32 {
    // Кегль куска: сперва правка, затем документный, затем блочный — тем же
    // порядком, каким его выберет запись в файл.
    style
        .size
        .or(style.document_size)
        .unwrap_or(base.size_points)
        * style.script.scale_ratio()
}

/// Разбивает строку на чередующиеся слова и промежутки.
fn tokenize(text: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut blank = None;

    for (index, ch) in text.char_indices() {
        let is_blank = ch.is_whitespace();
        match blank {
            Some(previous) if previous == is_blank => {}
            Some(_) => {
                tokens.push(start..index);
                start = index;
            }
            None => start = index,
        }
        blank = Some(is_blank);
    }
    if start < text.len() {
        tokens.push(start..text.len());
    }
    tokens
}

fn wrap(pieces: &[Piece], wrap_width: f32, line_height: f32) -> Vec<LaidLine> {
    let mut lines: Vec<LaidLine> = Vec::new();
    let mut placed: Vec<Placed> = Vec::new();
    let mut x = 0.0f32;
    let mut top = 0.0f32;

    let close = |placed: &mut Vec<Placed>, lines: &mut Vec<LaidLine>, top: &mut f32| {
        if placed.is_empty() {
            return;
        }
        let range = pieces[placed[0].piece].range.start
            ..pieces[placed.last().expect("непусто").piece].range.end;
        lines.push(LaidLine {
            placed: std::mem::take(placed),
            range,
            top: *top,
            height: line_height,
        });
        *top += line_height;
    };

    for (index, piece) in pieces.iter().enumerate() {
        // Пробел в конце строки не переносится: иначе он уезжал бы вниз и
        // следующая строка начиналась бы с отступа.
        if x + piece.width > wrap_width && !placed.is_empty() && !piece.blank {
            close(&mut placed, &mut lines, &mut top);
            x = 0.0;
        }
        placed.push(Placed { piece: index, x });
        x += piece.width;
    }
    close(&mut placed, &mut lines, &mut top);

    if lines.is_empty() {
        lines.push(LaidLine {
            placed: Vec::new(),
            range: 0..0,
            top: 0.0,
            height: line_height,
        });
    }
    lines
}

/// Пропорции индексов держим рядом с раскладкой: в PDF они те же, но здесь
/// нужны для обмера.
trait ScriptMetrics {
    fn scale_ratio(self) -> f32;
}

impl ScriptMetrics for Script {
    fn scale_ratio(self) -> f32 {
        match self {
            Script::Baseline => 1.0,
            _ => 0.6,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(scale: f32) -> BaseStyle {
        BaseStyle {
            metrics_by_family: std::collections::HashMap::new(),
            size_points: 10.0,
            line_height_points: 12.0,
            points_to_px: scale,
            color: gpui::black(),
            metrics: None,
        }
    }

    fn model(text: &str) -> RichText {
        RichText::new(text.to_owned(), RunStyle::default())
    }

    #[test]
    fn tokenizer_alternates_words_and_gaps() {
        let text = "раз два";
        let tokens = tokenize(text);
        let pieces: Vec<&str> = tokens.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(pieces, vec!["раз", " ", "два"]);
    }

    #[test]
    fn tokenizer_keeps_leading_and_trailing_gaps() {
        let text = "  край  ";
        let tokens = tokenize(text);
        let pieces: Vec<&str> = tokens.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(pieces, vec!["  ", "край", "  "]);
    }

    #[test]
    fn tokenizer_covers_the_whole_string_without_gaps() {
        for text in ["", "одно", " ", "а б в  г "] {
            let tokens = tokenize(text);
            let restored: String = tokens.iter().map(|range| &text[range.clone()]).collect();
            assert_eq!(restored, text, "разбор потерял символы: {text:?}");
        }
    }

    #[test]
    fn long_text_wraps_by_the_block_width() {
        // Запасная оценка — полкегля на букву. Десять букв при кегле 10 дают
        // 50 пунктов, значит в 60 умещается ровно одно слово.
        let layout = TextLayout::build(&model("аааааааааа бббббббббб"), &base(1.0), px(60.0));
        assert_eq!(layout.lines.len(), 2, "текст обязан перенестись");
        assert_eq!(f32::from(layout.height), 24.0);
    }

    #[test]
    fn frame_hugs_the_text_and_not_the_block() {
        // Ширина рамки — по самой длинной строке, а не по ширине переноса.
        let layout = TextLayout::build(&model("аб"), &base(1.0), px(500.0));
        assert_eq!(f32::from(layout.width), 10.0);
    }

    #[test]
    fn caret_starts_at_the_left_edge() {
        let layout = TextLayout::build(&model("что-нибудь"), &base(1.0), px(500.0));
        let (position, _) = layout.caret(0);
        assert_eq!(f32::from(position.x), 0.0);
    }

    #[test]
    fn caret_moves_right_as_the_offset_grows() {
        let text = model("абвгде");
        let layout = TextLayout::build(&text, &base(1.0), px(500.0));
        let mut previous = -1.0;
        for (offset, _) in text.text().char_indices() {
            let x = f32::from(layout.caret(offset).0.x);
            assert!(
                x > previous,
                "каретка обязана двигаться вправо: {offset} → {x}"
            );
            previous = x;
        }
    }

    #[test]
    fn a_click_returns_the_offset_the_caret_was_drawn_at() {
        let text = model("раз два три");
        let layout = TextLayout::build(&text, &base(1.0), px(500.0));
        for (offset, _) in text.text().char_indices() {
            let (position, _) = layout.caret(offset);
            assert_eq!(
                layout.offset_at(position),
                offset,
                "клик по каретке сместил её"
            );
        }
    }

    #[test]
    fn selection_of_one_line_gives_one_rectangle() {
        let text = model("раз два три");
        let layout = TextLayout::build(&text, &base(1.0), px(500.0));
        let rects = layout.selection_rects(0..text.len());
        assert_eq!(rects.len(), 1);
        assert!(
            f32::from(rects[0].1) > 0.0,
            "ширина выделения обязана быть положительной"
        );
    }

    #[test]
    fn selection_across_lines_gives_a_rectangle_per_line() {
        let text = model("аааааааааа бббббббббб");
        let layout = TextLayout::build(&text, &base(1.0), px(60.0));
        let rects = layout.selection_rects(0..text.len());
        assert_eq!(rects.len(), 2, "на каждую строку — свой прямоугольник");
        // И каждый начинается у левого края своей строки, а не там, где
        // кончился предыдущий.
        for (origin, _, _) in &rects {
            assert_eq!(f32::from(origin.x), 0.0);
        }
        assert_eq!(f32::from(rects[1].0.y), 12.0);
    }

    #[test]
    fn empty_selection_paints_nothing() {
        let layout = TextLayout::build(&model("раз два"), &base(1.0), px(500.0));
        assert!(layout.selection_rects(3..3).is_empty());
    }

    #[test]
    fn zoom_scales_everything_together() {
        let text = model("раз два");
        let one = TextLayout::build(&text, &base(1.0), px(500.0));
        let two = TextLayout::build(&text, &base(2.0), px(1000.0));

        let (at_one, height_one) = one.caret(4);
        let (at_two, height_two) = two.caret(4);
        assert!((f32::from(at_two.x) - f32::from(at_one.x) * 2.0).abs() < 0.01);
        assert!((f32::from(height_two) - f32::from(height_one) * 2.0).abs() < 0.01);
        assert!((f32::from(two.width) - f32::from(one.width) * 2.0).abs() < 0.01);
    }

    #[test]
    fn wrapping_is_measured_in_points_not_pixels() {
        // Одна и та же ширина блока в пунктах при разном масштабе обязана дать
        // одинаковый перенос: иначе на экране строки рвались бы не там, где
        // порвутся в документе.
        let text = model("аааааааааа бббббббббб");
        let one = TextLayout::build(&text, &base(1.0), px(60.0));
        let three = TextLayout::build(&text, &base(3.0), px(180.0));
        assert_eq!(one.lines.len(), three.lines.len());
    }

    #[test]
    fn superscript_is_measured_smaller() {
        let mut raised = RunStyle::default();
        raised.script = Script::Superscript;
        let plain = TextLayout::build(&model("22"), &base(1.0), px(500.0));
        let small = TextLayout::build(
            &RichText::new("22".to_owned(), raised),
            &base(1.0),
            px(500.0),
        );
        assert!(
            f32::from(small.width) < f32::from(plain.width),
            "надындекс обязан быть уже основного текста"
        );
    }
}
