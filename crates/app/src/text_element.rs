//! Правка прямо в тексте страницы.
//!
//! Элемент **не рисует текст**. Это главное в нём. Буквы на экране —
//! настоящие, их рисует pdfium из настоящего документа, перерисовывая страницу
//! на каждое нажатие. Своя копия текста поверх страницы не годилась в принципе:
//! pdfium и типографика gpui — два разных растеризатора, они не совпадут ни
//! ширинами, ни хинтингом, ни округлением, сколько их ни подгоняй. Именно так
//! это устроено в Acrobat: правишь сам документ, а не изображение над ним.
//!
//! Что остаётся элементу: рамка, каретка, подсветка выделения, мышь и
//! клавиатура. Все положения он берёт из [`crate::text_layout`], который меряет
//! строки метриками шрифта самого документа, — поэтому каретка встаёт ровно
//! между теми буквами, между которыми её поставили.

use std::ops::Range;

use gpui::{
    App, Bounds, Context, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, Hitbox, HitboxBehavior, Hsla,
    InspectorElementId, InteractiveElement, IntoElement, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, Style,
    UTF16Selection, Window, div, fill, point, px, size,
};

use crate::rich_text::{Movement, RichText, RunStyle};
use crate::text_layout::{BaseStyle, TextLayout};

/// О чём редактор сообщает наружу.
pub enum EditorEvent {
    /// Текст или оформление изменились.
    Changed,
    /// Пользователь отказался от правки.
    Cancel,
}

pub struct RichTextEditor {
    pub text: RichText,
    pub base: BaseStyle,
    pub wrap_width: Pixels,
    pub selection_color: Hsla,
    focus: FocusHandle,
    /// Незавершённый ввод: пока идёт набор через IME, система показывает
    /// промежуточный текст и сама решает, когда он станет окончательным.
    marked: Option<Range<usize>>,
    /// Выделение мышью началось внутри текста.
    ///
    /// Без этого признака протягивание по всему окну считалось бы выделением
    /// откуда угодно — в том числе когда тянут за маркер рамки, и текст тогда
    /// выделялся заодно с переносом блока.
    selecting: bool,
    /// Последняя раскладка и место элемента на экране. Нужны обработчику
    /// ввода: система спрашивает, где показать окно подсказок, и в какой
    /// символ попала точка.
    last_layout: Option<TextLayout>,
    last_bounds: Bounds<Pixels>,
}

impl EventEmitter<EditorEvent> for RichTextEditor {}

impl Focusable for RichTextEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl RichTextEditor {
    pub fn new(
        text: RichText,
        base: BaseStyle,
        wrap_width: Pixels,
        selection_color: Hsla,
        cx: &mut Context<Self>,
    ) -> RichTextEditor {
        RichTextEditor {
            text,
            base,
            wrap_width,
            selection_color,
            focus: cx.focus_handle(),
            marked: None,
            selecting: false,
            last_layout: None,
            last_bounds: Bounds::default(),
        }
    }

    /// Меняет оформление выделения или задаёт его для следующего ввода.
    pub fn restyle(&mut self, change: impl Fn(&mut RunStyle), cx: &mut Context<Self>) {
        self.text.restyle(change);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let shift = keystroke.modifiers.shift;
        let control = keystroke.modifiers.control;

        match keystroke.key.as_str() {
            "backspace" => self.text.backspace(),
            "delete" => self.text.delete(),
            "left" => self.text.move_caret(
                if control {
                    Movement::WordLeft
                } else {
                    Movement::Left
                },
                shift,
            ),
            "right" => self.text.move_caret(
                if control {
                    Movement::WordRight
                } else {
                    Movement::Right
                },
                shift,
            ),
            "home" => self.text.move_caret(Movement::Start, shift),
            "end" => self.text.move_caret(Movement::End, shift),
            "a" if control => self.text.select_all(),
            // Enter переносит строку, как в любом текстовом редакторе.
            // Применение правки — клик по пустому месту страницы.
            "enter" => self.text.insert("\n"),
            "escape" => {
                cx.emit(EditorEvent::Cancel);
                return;
            }
            // Печатный текст сюда не доходит: его приносит обработчик ввода
            // (см. `EntityInputHandler` ниже). Через него же работает IME, без
            // которого набор во многих раскладках невозможен.
            _ => return,
        }

        window.focus(&self.focus);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }
}

impl Render for RichTextEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            // Перехват, а не всплытие: библиотека виджетов вешает на Esc свои
            // привязки — закрыть список, свернуть подсказку, — и они срабатывают
            // раньше обычных обработчиков, так что до отмены правки дело не
            // доходило вовсе. В фазе перехвата первым получает событие тот, кто
            // в фокусе, а это как раз правимый абзац.
            .capture_key_down(cx.listener(Self::on_key))
            .child(RichTextElement {
                id: "rich-text".into(),
                editor: cx.entity(),
            })
    }
}

/// Смещение в байтах по смещению в единицах UTF-16.
///
/// Система оперирует UTF-16 — так исторически устроен ввод на всех платформах,
/// — а модель, как принято в Rust, байтами. Без честного перевода кириллица
/// разъезжается: в UTF-16 её буква занимает одну единицу, в UTF-8 — два байта.
fn utf16_to_byte(text: &str, target: usize) -> usize {
    let mut utf16 = 0;
    for (index, ch) in text.char_indices() {
        if utf16 >= target {
            return index;
        }
        utf16 += ch.len_utf16();
    }
    text.len()
}

fn byte_to_utf16(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())]
        .chars()
        .map(char::len_utf16)
        .sum()
}

impl EntityInputHandler for RichTextEditor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.text.text();
        let from = utf16_to_byte(&text, range.start);
        let to = utf16_to_byte(&text, range.end).max(from);
        *adjusted = Some(byte_to_utf16(&text, from)..byte_to_utf16(&text, to));
        Some(text[from..to].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let text = self.text.text();
        let range = self.text.selection();
        Some(UTF16Selection {
            range: byte_to_utf16(&text, range.start)..byte_to_utf16(&text, range.end),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let text = self.text.text();
        self.marked
            .clone()
            .map(|range| byte_to_utf16(&text, range.start)..byte_to_utf16(&text, range.end))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_utf16(range);
        self.text.insert(new_text);
        self.marked = None;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_utf16(range);
        let start = self.text.selection().start;
        self.text.insert(new_text);
        // Помеченный участок — это то, что система ещё может заменить.
        self.marked = Some(start..start + new_text.len());
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let text = self.text.text();
        let (position, height) = layout.caret(utf16_to_byte(&text, range_utf16.start));
        Some(Bounds {
            origin: point(
                element_bounds.origin.x + position.x,
                element_bounds.origin.y + position.y,
            ),
            size: size(px(1.0), height),
        })
    }

    fn character_index_for_point(
        &mut self,
        position: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        let local = point(
            position.x - self.last_bounds.origin.x,
            position.y - self.last_bounds.origin.y,
        );
        let text = self.text.text();
        Some(byte_to_utf16(&text, layout.offset_at(local)))
    }
}

impl RichTextEditor {
    /// Ставит выделение по диапазону в единицах UTF-16.
    fn select_utf16(&mut self, range: Option<Range<usize>>) {
        let text = self.text.text();
        // Пустой диапазон — система хочет заменить помеченный участок, а если
        // и его нет, то текущее выделение.
        let range = range.or_else(|| {
            self.marked
                .clone()
                .map(|range| byte_to_utf16(&text, range.start)..byte_to_utf16(&text, range.end))
        });
        let Some(range) = range else { return };

        let from = utf16_to_byte(&text, range.start);
        let to = utf16_to_byte(&text, range.end);
        self.text.set_caret(from);
        self.text.extend_to(to);
    }
}

pub struct RichTextElement {
    id: ElementId,
    editor: Entity<RichTextEditor>,
}

impl IntoElement for RichTextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RichTextElement {
    type RequestLayoutState = TextLayout;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let editor = self.editor.read(cx);
        let (text, base, wrap_width) =
            (editor.text.clone(), editor.base.clone(), editor.wrap_width);

        let layout = TextLayout::build(&text, &base, wrap_width);

        // Рамка садится по тексту, а не по исходной ширине блока: пустое
        // место справа выглядело как часть выделенного абзаца. Переносить
        // строки при этом продолжаем по ширине блока — так вёрстка страницы
        // остаётся прежней.
        let mut style = Style::default();
        style.size.width = layout.width.max(px(24.0)).into();
        style.size.height = layout.height.into();

        (window.request_layout(style, [], cx), layout)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Значения снимаем заранее и отпускаем заимствование: отрисовка
        // требует `cx` изменяемым.
        let (selection, caret, focused, selection_color, caret_color) = {
            let editor = self.editor.read(cx);
            (
                editor.text.selection(),
                editor.text.caret(),
                editor.focus.is_focused(window),
                editor.selection_color,
                editor.base.color,
            )
        };

        // Текст здесь не рисуется: под нами настоящая страница, и буквы на ней
        // настоящие. Подсветка выделения полупрозрачная — сквозь неё видно
        // выделенное.
        for (origin, width, height) in layout.selection_rects(selection.clone()) {
            let rect = Bounds {
                origin: point(bounds.origin.x + origin.x, bounds.origin.y + origin.y),
                size: size(width, height),
            };
            window.paint_quad(fill(rect, selection_color));
        }

        if focused && selection.is_empty() {
            let (position, height) = layout.caret(caret);
            let rect = Bounds {
                origin: point(bounds.origin.x + position.x, bounds.origin.y + position.y),
                size: size(px(1.5), height),
            };
            window.paint_quad(fill(rect, caret_color));
        }

        // Над самим текстом курсор текстовый — им целятся между буквами.
        window.set_cursor_style(gpui::CursorStyle::IBeam, hitbox);

        // Обработчику ввода нужна свежая раскладка: система спрашивает, где
        // показать окно подсказок IME и в какой символ попала точка.
        self.editor.update(cx, |editor, _| {
            editor.last_layout = Some(layout.clone());
            editor.last_bounds = bounds;
        });

        if focused {
            window.handle_input(
                &self.editor.read(cx).focus.clone(),
                ElementInputHandler::new(bounds, self.editor.clone()),
                cx,
            );
        }

        self.handle_mouse(bounds, layout.clone(), hitbox.clone(), window);
    }
}

impl RichTextElement {
    /// Клик ставит каретку, протягивание — выделяет.
    fn handle_mouse(
        &self,
        bounds: Bounds<Pixels>,
        layout: TextLayout,
        hitbox: Hitbox,
        window: &mut Window,
    ) {
        let editor = self.editor.clone();
        let inside = move |position: gpui::Point<Pixels>| {
            point(position.x - bounds.origin.x, position.y - bounds.origin.y)
        };

        window.on_mouse_event({
            let editor = editor.clone();
            let layout = layout.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                if !hitbox.is_hovered(window) {
                    return;
                }
                let offset = layout.offset_at(inside(event.position));
                editor.update(cx, |editor, cx| {
                    editor.text.set_caret(offset);
                    editor.selecting = true;
                    window.focus(&editor.focus);
                    cx.notify();
                });
                // Дальше страницы щелчок не идёт: там он значил бы «применить
                // правку», а здесь он всего лишь переставил каретку.
                cx.stop_propagation();
            }
        });

        window.on_mouse_event({
            let editor = editor.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if !phase.bubble() || event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                // Протягивание считается выделением, только если началось в
                // тексте. Иначе перенос рамки за маркер заодно выделял бы её
                // содержимое.
                if !editor.read(cx).selecting {
                    return;
                }
                let offset = layout.offset_at(inside(event.position));
                editor.update(cx, |editor, cx| {
                    editor.text.extend_to(offset);
                    cx.notify();
                });
            }
        });

        window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
            if !phase.bubble() {
                return;
            }
            if editor.read(cx).selecting {
                editor.update(cx, |editor, _| editor.selecting = false);
            }
        });
    }
}
