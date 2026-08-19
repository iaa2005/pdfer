//! Правая панель свойств — как в Acrobat.
//!
//! Здесь собрано всё, что известно о выделенном абзаце: где он стоит, каким
//! шрифтом набран, что из этого прочитано из документа, а что задано вручную.
//! Плавающая панель у блока остаётся для быстрых действий, а сюда вынесено
//! то, что требует места и точности: геометрия, поворот, выключка,
//! интерлиньяж, цвет.
//!
//! Ни одно поле не дублирует состояние: каждая величина показывается прямо из
//! выделения и меняется тем же путём, каким её меняет мышь. Поэтому панель
//! никогда не расходится с рамкой на странице.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, Div, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme, Sizable, StyledExt};

use pdfcore::model::Align;
use pdfcore::stream_edit::Script;

use crate::rich_text::RunStyle;
use crate::viewer::{Viewer, align_label};

impl Viewer {
    /// Кнопка выбора шрифта: показывает текущую гарнитуру её же начертанием и
    /// открывает раскрывающийся список — семейства с начертаниями внутри.
    fn font_choice_button(
        &self,
        id: &'static str,
        target: crate::viewer::FontTarget,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let (family, face) = match target {
            crate::viewer::FontTarget::Editor => {
                (self.chosen_family.clone(), self.chosen_face.clone())
            }
            crate::viewer::FontTarget::Multi => (self.multi_family.clone(), None),
        };
        let shown = family.clone().unwrap_or_else(|| "Гарнитура…".to_owned());
        let title = match &face {
            Some(face) => format!("{shown} · {face}"),
            None => shown.clone(),
        };
        h_flex()
            .id(id)
            .w_full()
            .items_center()
            .justify_between()
            .gap_1()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .cursor_pointer()
            .hover(|el| el.bg(cx.theme().accent))
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .truncate()
                    .when(family.is_some(), |el| {
                        el.font_family(gpui::SharedString::from(shown))
                    })
                    .when(family.is_none(), |el| {
                        el.text_color(cx.theme().muted_foreground)
                    })
                    .child(title),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("▾"),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                    this.toggle_font_picker(target, window, cx);
                    cx.stop_propagation();
                }),
            )
    }
}

/// Ширина панели. Хватает на две колонки полей и не съедает страницу.
pub(crate) const PANEL_WIDTH: f32 = 268.0;

/// Шаг изменения интерлиньяжа в пунктах.
const LEADING_STEP: f32 = 0.5;

/// Готовые цвета для окна стилей — те же, без имён.
pub(crate) const STYLE_SWATCHES: [u32; 8] = [
    0x0000_00, 0x4B_4B4B, 0x9E_9E9E, 0xFF_FFFF, 0xD3_2F2F, 0xF5_7C00, 0x2E_7D32, 0x1E_66F5,
];

/// Цвет свотча в долях единицы — общий пересчёт для панели и окна стилей.
pub(crate) fn unpack_rgb(value: u32) -> [f32; 3] {
    unpack(value)
}

/// Готовые цвета текста. Первым идёт чёрный: им набрано почти всё.
const SWATCHES: [(&str, u32); 8] = [
    ("Чёрный", 0x0000_00),
    ("Тёмно-серый", 0x4B_4B4B),
    ("Серый", 0x8C_8C8C),
    ("Белый", 0xFFFF_FF),
    ("Красный", 0xD1_2E2E),
    ("Оранжевый", 0xD1_7A2E),
    ("Зелёный", 0x2E_8B4F),
    ("Синий", 0x2563_EB),
];

fn v_flex() -> Div {
    div().flex().flex_col()
}

fn h_flex() -> Div {
    div().flex().flex_row()
}

impl Viewer {
    /// Панель свойств. Пуста по содержанию, пока ничего не выделено, — но
    /// остаётся на месте: панель, которая появляется и исчезает, дёргает всю
    /// раскладку окна.
    pub(crate) fn render_properties(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // В режиме сетки правят не текст, а состав страниц: панель свойств
        // там пуста и только отнимает место.
        if !self.show_properties || self.organise.is_some() {
            return div().into_any_element();
        }

        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        // Групповое выделение показывает свою панель: общие свойства и
        // «Сгруппировать». Несовпавшие значения — пустое поле, как в Acrobat.
        if self.multi.len() > 1 {
            return self.render_multi_properties(border, muted, cx);
        }

        let Some(selection) = self.selected.as_ref() else {
            return v_flex()
                .w(px(PANEL_WIDTH))
                .h_full()
                .border_l_1()
                .border_color(border)
                .p_3()
                .child(heading("СВОЙСТВА", muted))
                .child(
                    div()
                        .mt_2()
                        .text_xs()
                        .text_color(muted)
                        .child("Выберите абзац на странице — здесь появятся его свойства."),
                )
                .into_any_element();
        };

        let align = selection.align;
        let leading = selection.line_height.unwrap_or(selection.style.size * 1.2);
        let lines = selection.lines;
        let page_font = selection.page_font.as_ref();
        let document_font = page_font.map(|font| font.base_font.clone());
        let document_size = page_font
            .map(|font| font.size)
            .unwrap_or(selection.style.size);
        let document_color = page_font.and_then(|font| font.color);

        let at_caret: RunStyle = self
            .widgets
            .as_ref()
            .map(|widgets| widgets.editor.read(cx).text.style_at_caret())
            .unwrap_or_default();

        v_flex()
            .w(px(PANEL_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(border)
            .id("properties")
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p_3()
                    .gap_3()
                    .child(heading("РАСПОЛОЖЕНИЕ", muted))
                    // Координаты показываются и правятся так, как их привык
                    // видеть человек: от левого верхнего угла страницы и вниз.
                    // В самом PDF ось ординат смотрит вверх, но показывать это
                    // наружу значило бы заставить пересчитывать в уме.
                    // Значения в поля сеет `sync_property_inputs`; правка
                    // применяется по Enter и уходу фокуса.
                    .children(self.widgets.as_ref().map(|widgets| {
                        let g = &widgets.geometry;
                        v_flex()
                            .gap_2()
                            .child(pair(
                                input_field("X, пт", &g.x, muted),
                                input_field("Y, пт", &g.y, muted),
                            ))
                            .child(pair(
                                input_field("Ширина, пт", &g.w, muted),
                                input_field("Высота, пт", &g.h, muted),
                            ))
                            .child(input_field("Поворот, °", &g.angle, muted))
                    })),
            )
            .child(divider(border))
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(heading("ФОРМАТ", muted))
                    // Гарнитура и кегль живут здесь, как в Acrobat: панель
                    // всегда на месте, а плавающая у блока остаётся короткой.
                    .children(self.widgets.as_ref().map(|_| {
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(div().flex_1().child(self.font_choice_button(
                                "editor-font",
                                crate::viewer::FontTarget::Editor,
                                cx,
                            )))
                            .child(
                                Button::new("refresh-fonts")
                                    .small()
                                    .ghost()
                                    .label("\u{27f3}")
                                    .tooltip("Перечитать шрифты системы — после установки нового")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.refresh_fonts(window, cx)
                                    })),
                            )
                    }))
                    .children(self.widgets.as_ref().map(|widgets| {
                        let size_input = widgets.size.clone();
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(72.0)).child(Input::new(&size_input).small()))
                            .child(label("пт", muted))
                    }))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(style_toggle(cx, "prop-bold", "Ж", at_caret.bold, {
                                let value = !at_caret.bold;
                                move |this, cx| this.set_bold(value, cx)
                            }))
                            .child(style_toggle(cx, "prop-italic", "К", at_caret.italic, {
                                let value = !at_caret.italic;
                                move |this, cx| this.set_italic(value, cx)
                            }))
                            .child(style_toggle(
                                cx,
                                "prop-underline",
                                "Ч",
                                at_caret.underline,
                                {
                                    let value = !at_caret.underline;
                                    move |this, cx| this.set_underline(value, cx)
                                },
                            ))
                            .child(style_toggle(
                                cx,
                                "prop-super",
                                "x²",
                                at_caret.script == Script::Superscript,
                                {
                                    let value = toggled(at_caret.script, Script::Superscript);
                                    move |this, cx| this.set_script(value, cx)
                                },
                            ))
                            .child(style_toggle(
                                cx,
                                "prop-sub",
                                "x₂",
                                at_caret.script == Script::Subscript,
                                {
                                    let value = toggled(at_caret.script, Script::Subscript);
                                    move |this, cx| this.set_script(value, cx)
                                },
                            )),
                    )
                    .children(self.widgets.as_ref().map(|widgets| {
                        let g = &widgets.geometry;
                        pair(
                            input_field("Межбуквенный, пт", &g.char_spacing, muted),
                            input_field("Гор. масштаб, %", &g.h_scale, muted),
                        )
                    }))
                    .child(label("Цвет", muted))
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(SWATCHES.iter().enumerate().map(|(index, (name, value))| {
                                let value = *value;
                                let selected = at_caret.color == Some(unpack(value));
                                div()
                                    .id(("swatch", index))
                                    .w(px(20.0))
                                    .h(px(20.0))
                                    .rounded_sm()
                                    .bg(rgb(value))
                                    .border_2()
                                    .border_color(if selected {
                                        cx.theme().primary
                                    } else {
                                        cx.theme().border
                                    })
                                    .cursor_pointer()
                                    .tooltip({
                                        let name = SharedString::from(*name);
                                        move |window, cx| {
                                            gpui_component::tooltip::Tooltip::new(name.clone())
                                                .build(window, cx)
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_color(unpack(value), cx)
                                    }))
                            })),
                    )
                    // Свой цвет — из палитры: свотчей мало, а книги красят
                    // текст как хотят.
                    .children(self.widgets.as_ref().map(|widgets| {
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(label("Свой цвет", muted))
                            .child(picker_cage(gpui_component::color_picker::ColorPicker::new(
                                &widgets.color,
                            )))
                    }))
                    // Заливка — фон за буквами. «Нет» убирает его: снятая
                    // заливка при перенаборе просто не рисуется.
                    .children(self.widgets.as_ref().map(|widgets| {
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(label("Заливка", muted))
                            .child(picker_cage(gpui_component::color_picker::ColorPicker::new(
                                &widgets.fill,
                            )))
                            .child(
                                Button::new("fill-none")
                                    .small()
                                    .ghost()
                                    .label("Нет")
                                    .tooltip("Убрать фон за буквами")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.set_fill(None, cx)),
                                    ),
                            )
                    })),
            )
            .child(divider(border))
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(heading("АБЗАЦ", muted))
                    .child(h_flex().gap_1().children(
                        [Align::Left, Align::Center, Align::Right, Align::Justify].map(|value| {
                            Button::new(("align", value as usize))
                                .small()
                                .when(align == value, |b| b.primary())
                                .when(align != value, |b| b.ghost())
                                .icon(gpui_component::Icon::empty().path(align_icon(value)))
                                .tooltip(align_label(value))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.set_align(value, cx)),
                                )
                        }),
                    ))
                    .children(self.widgets.as_ref().map(|widgets| {
                        input_field("Отбивка абзацев, пт", &widgets.geometry.para_spacing, muted)
                    }))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(label("Интерлиньяж", muted))
                            .child(
                                Button::new("leading-minus")
                                    .small()
                                    .ghost()
                                    .label("−")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_line_height(leading - LEADING_STEP, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .min_w(px(46.0))
                                    .text_xs()
                                    .child(format!("{leading:.1} пт")),
                            )
                            .child(
                                Button::new("leading-plus")
                                    .small()
                                    .ghost()
                                    .label("+")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_line_height(leading + LEADING_STEP, cx)
                                    })),
                            ),
                    ),
            )
            .child(divider(border))
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    // Всё, что прочитано из самого файла. Меняться отсюда
                    // ничего не может — это и есть ценность: видно, с чем
                    // имеешь дело, прежде чем что-то трогать.
                    .child(heading("ИЗ ДОКУМЕНТА", muted))
                    .child(row(
                        "Шрифт",
                        document_font.unwrap_or_else(|| "читается…".into()),
                        muted,
                    ))
                    .child(row("Кегль", format!("{document_size:.2} пт"), muted))
                    .child(row(
                        "Цвет",
                        document_color
                            .map(describe_color)
                            .unwrap_or_else(|| "—".into()),
                        muted,
                    ))
                    .child(row("Строк", lines.to_string(), muted))
                    .child(row(
                        "Выключка",
                        align_label(selection.align).to_owned(),
                        muted,
                    ))
                    .child(row("Страница", format!("{}", selection.page + 1), muted)),
            )
            .into_any_element()
    }
}

impl Viewer {
    fn render_multi_properties(
        &mut self,
        border: Hsla,
        muted: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = self.multi.len();
        let shared_align = {
            let mut aligns = self.multi.iter().map(|target| target.align);
            let first = aligns.next();
            first.filter(|value| aligns.all(|other| other == *value))
        };

        v_flex()
            .w(px(PANEL_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(border)
            .p_3()
            .gap_3()
            .child(heading("ВЫДЕЛЕНО БЛОКОВ", muted))
            .child(div().text_xl().child(format!("{count}")))
            .child(divider(border))
            .child(heading("ОБЩИЕ СВОЙСТВА", muted))
            .children(self.multi_size.as_ref().map(|input| {
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(label("Кегль", muted))
                    .child(div().w(px(72.0)).child(Input::new(input).small()))
                    .child(label("пт — пусто, если разный", muted))
            }))
            .child(
                h_flex()
                    .gap_1()
                    .child(style_toggle(cx, "multi-bold", "Ж", false, |this, cx| {
                        this.multi_restyle(|style| style.bold = true, cx)
                    }))
                    .child(style_toggle(cx, "multi-italic", "К", false, |this, cx| {
                        this.multi_restyle(|style| style.italic = true, cx)
                    }))
                    .child(style_toggle(
                        cx,
                        "multi-underline",
                        "Ч",
                        false,
                        |this, cx| this.multi_restyle(|style| style.underline = true, cx),
                    )),
            )
            .child(
                h_flex().gap_1().flex_wrap().children(SWATCHES.iter().enumerate().map(
                    |(index, (name, value))| {
                        let value = *value;
                        div()
                            .id(("multi-swatch", index))
                            .w(px(20.0))
                            .h(px(20.0))
                            .rounded_sm()
                            .bg(rgb(value))
                            .border_2()
                            .border_color(cx.theme().border)
                            .cursor_pointer()
                            .tooltip({
                                let name = SharedString::from(*name);
                                move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(name.clone())
                                        .build(window, cx)
                                }
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.multi_set_color(unpack(value), cx)
                            }))
                    },
                )),
            )
            .child(
                h_flex().gap_1().children(
                    [Align::Left, Align::Center, Align::Right, Align::Justify].map(|value| {
                        Button::new(("multi-align", value as usize))
                            .small()
                            .when(shared_align == Some(value), |b| b.primary())
                            .when(shared_align != Some(value), |b| b.ghost())
                            .label(align_glyph(value))
                            .tooltip(align_label(value))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.multi_set_align(value, cx)),
                            )
                    }),
                ),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .child(label("Гарнитура", muted))
                    .child(self.font_choice_button(
                        "multi-font",
                        crate::viewer::FontTarget::Multi,
                        cx,
                    )),
            )
            .children(self.multi_color.as_ref().map(|color| {
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(label("Свой цвет", muted))
                    .child(picker_cage(
                        gpui_component::color_picker::ColorPicker::new(color),
                    ))
            }))
            .child(divider(border))
            // Выравнивание блоков по общей рамке — как в Figma: три кнопки по
            // горизонтали, три по вертикали.
            .child(label("Выравнивание", muted))
            .child(
                h_flex().gap_1().children(
                    [
                        (0u8, "icons/align-left.svg", "По левым краям"),
                        (1, "icons/align-horizontal-center.svg", "По центрам, по горизонтали"),
                        (2, "icons/align-right.svg", "По правым краям"),
                        (3, "icons/align-top.svg", "По верхним краям"),
                        (4, "icons/align-vertical-center.svg", "По серединам, по вертикали"),
                        (5, "icons/align-bottom.svg", "По нижним краям"),
                    ]
                    .map(|(mode, icon, title)| {
                        Button::new(("multi-frame-align", mode as usize))
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path(icon))
                            .tooltip(title)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.multi_align(mode, cx)
                            }))
                    }),
                ),
            )
            .child(divider(border))
            .child(
                Button::new("group-blocks")
                    .small()
                    .primary()
                    .label("Сгруппировать")
                    .tooltip(
                        "Слить выбранные блоки в один абзац — когда детектор \
                         разрезал единый текст",
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.group_multi(cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Рамка слева направо берёт целиком накрытые блоки, справа налево — задетые. Ctrl+клик добавляет блок в группу."),
            )
            .into_any_element()
    }
}

/// Заголовок раздела.
fn heading(text: &'static str, color: Hsla) -> Div {
    div().text_xs().font_bold().text_color(color).child(text)
}

/// Иконка выключки — те же, что в текстовых редакторах.
fn align_icon(align: Align) -> &'static str {
    match align {
        Align::Left => "icons/text-align-left.svg",
        Align::Center => "icons/text-align-center.svg",
        Align::Right => "icons/text-align-right.svg",
        Align::Justify => "icons/text-align-justify-center.svg",
    }
}

/// Подписанное числовое поле панели.
fn input_field(
    title: &'static str,
    state: &gpui::Entity<gpui_component::input::InputState>,
    muted: Hsla,
) -> Div {
    v_flex()
        .flex_1()
        .gap_0p5()
        .child(label(title, muted))
        .child(Input::new(state).small())
}

/// Клетка для чужого ColorPicker.
///
/// Внутри пикера лежит `absolute().size_full()` без явного угла — в gpui
/// такой элемент встаёт в статическую позицию и накрывает невидимой областью
/// всё, что нарисовано ниже: кнопки под пикером переставали ловить мышь.
/// Жёсткая рамка с обрезкой держит его при себе.
fn picker_cage(picker: impl IntoElement) -> Div {
    div()
        .relative()
        .w(px(30.0))
        .h(px(26.0))
        .overflow_hidden()
        .child(picker)
}

fn label(text: &'static str, color: Hsla) -> Div {
    div().text_xs().text_color(color).child(text)
}

fn divider(color: Hsla) -> Div {
    div().h(px(1.0)).bg(color)
}

fn pair(left: Div, right: Div) -> Div {
    h_flex().gap_3().child(left).child(right)
}

/// Строка «подпись — значение» для сведений, которые только читают.
fn row(name: &'static str, value: String, color: Hsla) -> Div {
    h_flex()
        .justify_between()
        .gap_2()
        .child(label(name, color))
        .child(div().text_xs().child(value))
}

fn align_glyph(align: Align) -> &'static str {
    match align {
        Align::Left => "\u{2261}L",
        Align::Center => "\u{2261}C",
        Align::Right => "\u{2261}R",
        Align::Justify => "\u{2261}J",
    }
}

/// Повторное нажатие на кнопку индекса возвращает текст на базовую линию.
fn toggled(current: Script, wanted: Script) -> Script {
    if current == wanted {
        Script::Baseline
    } else {
        wanted
    }
}

fn unpack(rgb: u32) -> [f32; 3] {
    [
        ((rgb >> 16) & 0xFF) as f32 / 255.0,
        ((rgb >> 8) & 0xFF) as f32 / 255.0,
        (rgb & 0xFF) as f32 / 255.0,
    ]
}

/// Цвет в том виде, в каком его пишут в PDF, — долями единицы.
fn describe_color(color: Hsla) -> String {
    let rgba: gpui::Rgba = color.into();
    format!("{:.2} {:.2} {:.2}", rgba.r, rgba.g, rgba.b)
}

fn style_toggle(
    cx: &mut Context<Viewer>,
    id: &'static str,
    text: &'static str,
    active: bool,
    action: impl Fn(&mut Viewer, &mut Context<Viewer>) + 'static,
) -> Button {
    Button::new(id)
        .small()
        .when(active, |b| b.primary())
        .when(!active, |b| b.ghost())
        .label(text)
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
}
