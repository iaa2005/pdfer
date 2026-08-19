//! Модель текста с оформлением по кускам.
//!
//! Это то, что редактируется на странице: последовательность ранов, у каждого
//! своя гарнитура, кегль, цвет, начертание и положение относительно базовой
//! линии. Плюс каретка и выделение.
//!
//! Модель ничего не знает ни про отрисовку, ни про раскладку по строкам —
//! только про текст и оформление. Поэтому она проверяется обычными тестами, а
//! не глазами, и на ней держится весь элемент.
//!
//! Смещения — байтовые, как принято для `str`, но всегда выровнены по границам
//! символов: кириллица занимает два байта, и каретка между ними означала бы
//! разрезанную букву.

use pdfcore::stream_edit::{Script, StyledSpan};

/// Оформление куска текста.
///
/// `None` в гарнитуре, кегле и цвете значит «как у абзаца»: пока пользователь
/// не трогал оформление, правка идёт исходным шрифтом и в файл ничего не
/// добавляется.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunStyle {
    pub family: Option<String>,
    pub size: Option<f32>,
    pub color: Option<[f32; 3]>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub script: Script,
    /// Гарнитура, кегль и цвет куска в самом документе. Это не правка, а
    /// память: пёстрый абзац приходит в модель со своими стилями по кускам,
    /// и перенабор воспроизводит их как были. Пользовательские поля выше
    /// всегда главнее документных.
    pub document_family: Option<String>,
    pub document_size: Option<f32>,
    pub document_color: Option<[f32; 3]>,
    /// Индекс, каким кусок набран в документе: подстрочный знак приходит в
    /// модель уже помеченным и перенабирается индексом, а не мелкой буквой
    /// на основной линии. Пользовательский `script` выше всегда главнее.
    pub document_script: Script,
    /// Начертание куска в самом документе. `bold`/`italic` выше сеются этими
    /// же значениями при выделении блока — кнопки Ж и К загораются у жирного
    /// и курсивного текста, — а правкой считается только расхождение с ними.
    pub document_bold: bool,
    pub document_italic: bool,
}

impl RunStyle {
    /// Оставляет ли это оформление набор прежним.
    ///
    /// Цвет сюда не входит намеренно: перекрасить текст можно, не трогая ни
    /// одной буквы, а вот гарнитура, кегль, начертание, индекс и подчёркивание
    /// требуют выложить строку заново.
    pub fn keeps_typesetting(&self) -> bool {
        // Документные поля не в счёт: они описывают то, что и так напечатано.
        // Начертание сравнивается с документным: жирный кусок с горящей «Ж» —
        // это не правка, а честное отражение страницы; правка — расхождение.
        self.family.is_none()
            && self.size.is_none()
            && self.bold == self.document_bold
            && self.italic == self.document_italic
            && !self.underline
            && self.script == Script::Baseline
    }

    /// Стиль куска, каким он был в документе, — заготовка для модели.
    pub fn from_document(family: &str, size: f32, color: Option<[f32; 3]>) -> RunStyle {
        RunStyle {
            document_family: Some(family.to_owned()),
            document_size: Some(size),
            document_color: color,
            ..RunStyle::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    pub style: RunStyle,
}

/// Куда двигать каретку.
///
/// `Start` и `End` пока не вызываются: их свяжет с клавишами Home и End
/// текстовый элемент, который пишется следующим.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Movement {
    Left,
    Right,
    WordLeft,
    WordRight,
    Start,
    End,
}

#[derive(Clone, Debug)]
pub struct RichText {
    runs: Vec<Run>,
    caret: usize,
    /// Второй конец выделения. Совпадает с кареткой, когда выделения нет.
    anchor: usize,
    /// Оформление, выбранное кнопкой до набора: нажали «полужирный» и печатают.
    /// Сбрасывается, как только каретка ушла в другое место.
    pending: Option<RunStyle>,
}

impl Default for RichText {
    fn default() -> Self {
        RichText::new(String::new(), RunStyle::default())
    }
}

impl RichText {
    pub fn new(text: impl Into<String>, style: RunStyle) -> RichText {
        let text = text.into();
        let caret = text.len();
        let runs = if text.is_empty() {
            Vec::new()
        } else {
            vec![Run { text, style }]
        };
        RichText {
            runs,
            caret,
            anchor: caret,
            pending: None,
        }
    }

    /// Собирает модель из кусков, пришедших из документа.
    ///
    /// Пока вызывается только тестом обратного преобразования: разбор блоков
    /// отдаёт абзац одним куском. Понадобится, когда оформление ранов начнёт
    /// доходить из документа до редактора.
    #[allow(dead_code)]
    /// Собирает модель из готовых ранов — так выделенный блок приходит в
    /// редактор со стилями по кускам, а не одним усреднённым.
    pub fn from_runs(runs: Vec<Run>) -> RichText {
        let mut model = RichText {
            runs: runs
                .into_iter()
                .filter(|run| !run.text.is_empty())
                .collect(),
            caret: 0,
            anchor: 0,
            pending: None,
        };
        model.normalise();
        let end = model.len();
        model.caret = end;
        model.anchor = end;
        model
    }

    #[cfg(test)]
    pub fn from_spans(spans: &[StyledSpan]) -> RichText {
        let mut model = RichText {
            runs: Vec::new(),
            caret: 0,
            anchor: 0,
            pending: None,
        };
        for span in spans {
            if span.text.is_empty() {
                continue;
            }
            model.runs.push(Run {
                text: span.text.clone(),
                style: RunStyle {
                    family: span.font.as_ref().map(|font| font.family.clone()),
                    size: span.size,
                    color: span.color,
                    bold: span.font.as_ref().is_some_and(|font| font.bold),
                    italic: span.font.as_ref().is_some_and(|font| font.italic),
                    underline: span.underline,
                    script: span.script,
                    document_family: span.page_family.clone(),
                    document_size: None,
                    document_color: None,
                    document_script: Script::Baseline,
                    document_bold: false,
                    document_italic: false,
                },
            });
        }
        model.normalise();
        let end = model.len();
        model.caret = end;
        model.anchor = end;
        model
    }

    /// Отдаёт куски в том виде, в каком их принимает запись в PDF.
    ///
    /// `base_family` — гарнитура абзаца. Она нужна для кусков, где сменили
    /// только начертание: «полужирный» без имени семейства не описать, а
    /// потерять его нельзя.
    pub fn to_spans(&self, base_family: &str) -> Vec<StyledSpan> {
        self.runs
            .iter()
            .map(|run| {
                let face_changed = run.style.bold != run.style.document_bold
                    || run.style.italic != run.style.document_italic;
                let font = (run.style.family.is_some() || face_changed).then(|| {
                    // Смена начертания отталкивается от того, что видит
                    // пользователь: у куска из документа это его же
                    // гарнитура, а не базовая гарнитура абзаца.
                    let family = run
                        .style
                        .family
                        .as_deref()
                        .or(run.style.document_family.as_deref())
                        .unwrap_or(base_family);
                    pdfcore::FontRequest::new(family, run.style.bold, run.style.italic)
                });
                StyledSpan {
                    text: run.text.clone(),
                    // Документная гарнитура держит кусок при его же шрифте
                    // страницы; явная смена гарнитуры её перекрывает.
                    page_family: font
                        .is_none()
                        .then(|| run.style.document_family.clone())
                        .flatten(),
                    font,
                    size: run.style.size.or(run.style.document_size),
                    color: run.style.color.or(run.style.document_color),
                    script: if run.style.script != Script::Baseline {
                        run.style.script
                    } else {
                        run.style.document_script
                    },
                    underline: run.style.underline,
                    bold: run.style.bold,
                    italic: run.style.italic,
                }
            })
            .collect()
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    pub fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.runs.iter().map(|run| run.text.len()).sum()
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Выделенный участок; пустой, когда выделения нет.
    pub fn selection(&self) -> std::ops::Range<usize> {
        let (from, to) = if self.caret <= self.anchor {
            (self.caret, self.anchor)
        } else {
            (self.anchor, self.caret)
        };
        from..to
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    /// Ставит каретку, снимая выделение.
    pub fn set_caret(&mut self, offset: usize) {
        let offset = self.clamp(offset);
        self.caret = offset;
        self.anchor = offset;
        self.pending = None;
    }

    /// Расширяет выделение до указанного смещения.
    pub fn extend_to(&mut self, offset: usize) {
        self.caret = self.clamp(offset);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.len();
    }

    pub fn move_caret(&mut self, movement: Movement, extend: bool) {
        let target = match movement {
            Movement::Left => self.prev_boundary(self.caret),
            Movement::Right => self.next_boundary(self.caret),
            Movement::WordLeft => self.prev_word(self.caret),
            Movement::WordRight => self.next_word(self.caret),
            Movement::Start => 0,
            Movement::End => self.len(),
        };

        // Без Shift выделение сначала схлопывается к своему краю: так ведут
        // себя все текстовые поля.
        if !extend && self.has_selection() {
            let range = self.selection();
            let collapsed = match movement {
                Movement::Left | Movement::WordLeft | Movement::Start => range.start,
                _ => range.end,
            };
            if matches!(movement, Movement::Left | Movement::Right) {
                self.set_caret(collapsed);
                return;
            }
        }

        self.caret = target;
        if !extend {
            self.anchor = target;
        }
    }

    /// Вставляет текст на место каретки или выделения.
    pub fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let style = self.style_for_insert();
        self.delete_selection();

        let at = self.caret;
        self.split_at(at);

        // Ищем ран, который заканчивается ровно в точке вставки, и с тем же
        // оформлением — тогда текст просто дописывается к нему.
        let mut offset = 0;
        let mut index = self.runs.len();
        for (position, run) in self.runs.iter().enumerate() {
            if offset == at {
                index = position;
                break;
            }
            offset += run.text.len();
        }
        if offset != at {
            index = self.runs.len();
        }

        self.runs.insert(
            index,
            Run {
                text: text.to_owned(),
                style,
            },
        );
        self.caret = at + text.len();
        self.anchor = self.caret;
        // Оформление уже применено к набранному, дальше действует обычное.
        self.pending = None;
        self.normalise();
    }

    /// Удаляет символ слева от каретки либо выделение.
    pub fn backspace(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        if self.caret == 0 {
            return;
        }
        let from = self.prev_boundary(self.caret);
        self.remove(from..self.caret);
        self.set_caret(from);
    }

    /// Удаляет символ справа от каретки либо выделение.
    pub fn delete(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        let to = self.next_boundary(self.caret);
        if to == self.caret {
            return;
        }
        let at = self.caret;
        self.remove(at..to);
        self.set_caret(at);
    }

    pub fn delete_selection(&mut self) {
        if !self.has_selection() {
            return;
        }
        let range = self.selection();
        self.remove(range.clone());
        self.set_caret(range.start);
    }

    /// Меняет оформление выделенного участка.
    ///
    /// Если выделения нет, оформление запоминается для следующего ввода —
    /// именно так ведёт себя кнопка «полужирный» в текстовом редакторе, когда
    /// её нажимают перед набором.
    pub fn restyle(&mut self, change: impl Fn(&mut RunStyle)) {
        // Без выделения оформление меняется у всего абзаца — как в Acrobat:
        // выделил блок, нажал «Ж» — весь блок пожирнел. Стиль для следующего
        // ввода обновляется заодно, чтобы набор продолжился так же.
        if !self.has_selection() {
            for run in &mut self.runs {
                change(&mut run.style);
            }
            let mut style = self.style_for_insert();
            change(&mut style);
            self.pending = Some(style);
            self.normalise();
            return;
        }

        let range = self.selection();
        self.split_at(range.start);
        self.split_at(range.end);

        let mut offset = 0;
        for run in &mut self.runs {
            let end = offset + run.text.len();
            if offset >= range.start && end <= range.end {
                change(&mut run.style);
            }
            offset = end;
        }
        self.normalise();
    }

    /// Оформление в точке каретки — то, что покажет панель формата.
    ///
    /// Понадобится элементу: панель обязана показывать состояние там, где
    /// стоит каретка, а не то, что нажимали последним.
    #[allow(dead_code)]
    pub fn style_at_caret(&self) -> RunStyle {
        self.style_for_insert()
    }

    fn style_for_insert(&self) -> RunStyle {
        if let Some(style) = &self.pending {
            return style.clone();
        }
        // Берём оформление слева от каретки: так продолжается начатое слово.
        let mut offset = 0;
        let mut last = RunStyle::default();
        for run in &self.runs {
            if offset >= self.caret {
                break;
            }
            last = run.style.clone();
            offset += run.text.len();
        }
        last
    }

    fn remove(&mut self, range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.split_at(range.start);
        self.split_at(range.end);

        let mut offset = 0;
        self.runs.retain(|run| {
            let end = offset + run.text.len();
            let keep = end <= range.start || offset >= range.end;
            offset = end;
            keep
        });
        self.normalise();
    }

    /// Разрезает ран, если смещение попадает внутрь него.
    fn split_at(&mut self, at: usize) {
        let mut offset = 0;
        for index in 0..self.runs.len() {
            let length = self.runs[index].text.len();
            if at > offset && at < offset + length {
                let tail = self.runs[index].text.split_off(at - offset);
                let style = self.runs[index].style.clone();
                self.runs.insert(index + 1, Run { text: tail, style });
                return;
            }
            offset += length;
        }
    }

    /// Выкидывает пустые раны и склеивает соседние с одинаковым оформлением.
    fn normalise(&mut self) {
        self.runs.retain(|run| !run.text.is_empty());

        let mut index = 1;
        while index < self.runs.len() {
            if self.runs[index - 1].style == self.runs[index].style {
                let tail = std::mem::take(&mut self.runs[index].text);
                self.runs[index - 1].text.push_str(&tail);
                self.runs.remove(index);
            } else {
                index += 1;
            }
        }

        let length = self.len();
        self.caret = self.caret.min(length);
        self.anchor = self.anchor.min(length);
    }

    fn clamp(&self, offset: usize) -> usize {
        let text = self.text();
        let offset = offset.min(text.len());
        // Сдвигаемся влево до ближайшей границы символа: между байтами одной
        // буквы каретке не место.
        (0..=offset)
            .rev()
            .find(|candidate| text.is_char_boundary(*candidate))
            .unwrap_or(0)
    }

    fn prev_boundary(&self, from: usize) -> usize {
        let text = self.text();
        text[..from.min(text.len())]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_boundary(&self, from: usize) -> usize {
        let text = self.text();
        let from = from.min(text.len());
        text[from..]
            .chars()
            .next()
            .map(|ch| from + ch.len_utf8())
            .unwrap_or(from)
    }

    fn prev_word(&self, from: usize) -> usize {
        let text = self.text();
        let head = &text[..from.min(text.len())];
        let trimmed = head.trim_end();
        match trimmed.rfind(char::is_whitespace) {
            Some(index) => {
                index
                    + text[index..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(1)
            }
            None => 0,
        }
    }

    fn next_word(&self, from: usize) -> usize {
        let text = self.text();
        let from = from.min(text.len());
        let tail = &text[from..];
        let skipped = tail.len() - tail.trim_start().len();
        match tail[skipped..].find(char::is_whitespace) {
            Some(index) => from + skipped + index,
            None => text.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bold() -> RunStyle {
        RunStyle {
            bold: true,
            ..Default::default()
        }
    }

    fn model(text: &str) -> RichText {
        RichText::new(text, RunStyle::default())
    }

    #[test]
    fn typing_extends_the_run_it_continues() {
        let mut text = model("Прив");
        text.insert("ет");

        assert_eq!(text.text(), "Привет");
        assert_eq!(
            text.runs().len(),
            1,
            "одинаковое оформление не должно плодить раны"
        );
        assert_eq!(text.caret(), text.len());
    }

    #[test]
    fn caret_never_lands_inside_a_letter() {
        let mut text = model("ще");
        // «щ» занимает два байта: смещение 1 разрезало бы букву.
        text.set_caret(1);
        assert_eq!(
            text.caret(),
            0,
            "каретка обязана съехать на границу символа"
        );

        text.move_caret(Movement::Right, false);
        assert_eq!(text.caret(), 2);
    }

    #[test]
    fn styling_a_selection_splits_into_three_runs() {
        let mut text = model("раз два три");
        // Смещения считаем по строке: в кириллице буква занимает два байта, и
        // арифметика «на глаз» промахивается мимо границы символа.
        let start = text.text().find("два").unwrap();
        let end = start + "два".len();
        text.set_caret(start);
        text.extend_to(end);

        text.restyle(|style| style.bold = true);

        let runs = text.runs();
        assert_eq!(
            runs.len(),
            3,
            "должно получиться до/выделено/после: {runs:#?}"
        );
        assert_eq!(runs[1].text, "два");
        assert!(runs[1].style.bold);
        assert!(!runs[0].style.bold && !runs[2].style.bold);
        assert_eq!(text.text(), "раз два три", "текст меняться не должен");
    }

    #[test]
    fn removing_the_difference_merges_neighbours_back() {
        let mut text = model("раз два три");
        let start = text.text().find("два").unwrap();
        let end = start + "два".len();
        text.set_caret(start);
        text.extend_to(end);
        text.restyle(|style| style.bold = true);
        assert_eq!(text.runs().len(), 3);

        text.set_caret(start);
        text.extend_to(end);
        text.restyle(|style| style.bold = false);

        assert_eq!(text.runs().len(), 1, "одинаковые соседи обязаны склеиться");
        assert_eq!(text.text(), "раз два три");
    }

    #[test]
    fn typing_inherits_the_style_to_the_left() {
        let mut text = RichText::new("жирный", bold());
        text.insert(" хвост");

        assert_eq!(text.runs().len(), 1);
        assert!(
            text.runs()[0].style.bold,
            "продолжение слова наследует оформление"
        );
    }

    #[test]
    fn style_button_without_selection_styles_the_whole_block() {
        // Как в Acrobat: блок выделен, текст внутри — нет, нажали «Ж» — весь
        // блок пожирнел, и набор продолжается жирным.
        let mut text = model("обычный ");
        text.restyle(|style| style.bold = true);
        text.insert("и дальше");

        assert!(
            text.runs().iter().all(|run| run.style.bold),
            "жирным обязан стать весь блок"
        );
        assert_eq!(text.text(), "обычный и дальше");
    }

    #[test]
    fn style_button_on_a_selection_styles_only_it() {
        let mut text = model("обычный жирный");
        let start = "обычный ".len();
        text.set_caret(start);
        text.extend_to(text.len());
        text.restyle(|style| style.bold = true);

        let runs = text.runs();
        assert_eq!(runs.len(), 2);
        assert!(!runs[0].style.bold);
        assert!(runs[1].style.bold);
        assert_eq!(runs[1].text, "жирный");
    }

    #[test]
    fn backspace_deletes_one_letter_not_one_byte() {
        let mut text = model("Привет");
        text.backspace();
        assert_eq!(text.text(), "Приве");
        assert_eq!(text.caret(), text.len());
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut text = model("раз два три");
        let start = text.text().find("два").unwrap();
        text.set_caret(start);
        text.extend_to(start + "два".len());
        text.insert("ноль");

        assert_eq!(text.text(), "раз ноль три");
        assert!(!text.has_selection());
    }

    #[test]
    fn delete_removes_forward_and_keeps_the_caret() {
        let mut text = model("абв");
        text.set_caret(0);
        text.delete();
        assert_eq!(text.text(), "бв");
        assert_eq!(text.caret(), 0);
    }

    #[test]
    fn word_movement_jumps_over_whole_words() {
        let mut text = model("раз два три");
        text.set_caret(0);
        text.move_caret(Movement::WordRight, false);
        assert_eq!(&text.text()[..text.caret()], "раз");

        text.move_caret(Movement::End, false);
        text.move_caret(Movement::WordLeft, false);
        assert_eq!(&text.text()[text.caret()..], "три");
    }

    #[test]
    fn plain_arrow_collapses_the_selection() {
        let mut text = model("раз два три");
        let start = text.text().find("два").unwrap();
        text.set_caret(start);
        text.extend_to(start + "два".len());
        text.move_caret(Movement::Left, false);

        assert!(!text.has_selection());
        assert_eq!(
            text.caret(),
            start,
            "каретка обязана встать на левый край выделения"
        );
    }

    #[test]
    fn shift_arrow_grows_the_selection() {
        let mut text = model("абв");
        text.set_caret(0);
        text.move_caret(Movement::Right, true);
        text.move_caret(Movement::Right, true);

        assert!(text.has_selection());
        assert_eq!(text.selection(), 0..4, "два двухбайтных символа");
    }

    #[test]
    fn bold_without_an_explicit_family_keeps_the_base_one() {
        let mut text = model("обычный жирный");
        let start = "обычный ".len();
        text.set_caret(start);
        text.extend_to(text.len());
        text.restyle(|style| style.bold = true);

        let spans = text.to_spans("Georgia");
        let bold = spans
            .iter()
            .find(|span| span.text == "жирный")
            .expect("жирный кусок");
        let font = bold
            .font
            .as_ref()
            .expect("начертание обязано дойти до записи");
        assert_eq!(font.family, "Georgia");
        assert!(font.bold);

        // А обычный кусок гарнитуру не навязывает: значит шрифт абзаца
        // останется прежним и встраивать ничего не придётся.
        let plain = spans
            .iter()
            .find(|span| span.text == "обычный ")
            .expect("обычный кусок");
        assert!(plain.font.is_none());
    }

    #[test]
    fn spans_survive_the_round_trip() {
        let mut text = model("формула H2O");
        let start = "формула H".len();
        text.set_caret(start);
        text.extend_to(start + 1);
        text.restyle(|style| style.script = Script::Subscript);
        text.set_caret(text.len());

        let spans = text.to_spans("Arial");
        let restored = RichText::from_spans(&spans);

        assert_eq!(restored.text(), "формула H2O");
        assert_eq!(restored.runs().len(), 3);
        assert_eq!(restored.runs()[1].style.script, Script::Subscript);
    }

    #[test]
    fn deleting_everything_leaves_a_usable_model() {
        let mut text = model("что-нибудь");
        text.select_all();
        text.delete_selection();

        assert_eq!(text.len(), 0);
        assert_eq!(text.caret(), 0);
        assert!(text.runs().is_empty());

        text.insert("заново");
        assert_eq!(text.text(), "заново");
    }
}
