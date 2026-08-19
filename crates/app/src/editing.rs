//! Правка прямо на странице.
//!
//! Поле ввода встаёт ровно на место абзаца, а над ним всплывает панель
//! формата. Обе накладки живут внутри элемента страницы и поэтому пользуются
//! той же системой координат, что и сам документ: смещение при прокрутке и
//! зуме получается само собой, пересчитывать ничего не нужно.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, Div, Focusable, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Point, Styled, Window, div, px, rgba,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Sizable};

use pdfcore::stream_edit::Script;

use crate::frame::{FrameDrag, Grip, Guide};
use crate::rich_text::RunStyle;
use crate::templates::FormatTemplate;
use crate::viewer::{FORMAT_BAR_HEIGHT, Viewer};

/// Отступ между блоком и панелью формата.
const BAR_GAP: f32 = 8.0;
/// Ширина панели формата. Фиксированная: только так её можно честно прижать
/// к краю страницы, не гадая о ширине после раскладки.
const BAR_WIDTH: f32 = 300.0;
/// Сторона маркера в пикселях. Меньше десяти по нему трудно попасть мышью.
const GRIP_SIZE: f32 = 11.0;
/// Толщина полосы рамки, за которую блок перетаскивают. Полоса идёт по обе
/// стороны контура, поэтому попасть по ней вдвое легче, чем кажется по рамке.
const EDGE: f32 = 11.0;
/// Насколько ручка поворота отстоит от верхнего края рамки.
const ROTATE_LIFT: f32 = 26.0;
/// Цвет рамки и маркеров.
const ACCENT: u32 = 0xEF44_44FF;
/// Цвет линий привязки — заметно другой, чтобы не путать их с рамкой.
const GUIDE_COLOR: u32 = 0xE011_D9FF;
/// Значок на ручке поворота.
const ROTATE_GLYPH: &str = "\u{27f3}";
/// На сколько пунктов документа рамка притягивается к соседям.
const SNAP_TOLERANCE: f32 = 4.0;

fn h_flex() -> Div {
    div().flex().flex_row()
}

impl Viewer {
    /// Накладки правки для указанной страницы. Пусто, если выделение на другой
    /// странице или его нет вовсе.
    pub(crate) fn render_editing(
        &mut self,
        page: u32,
        page_width: f32,
        page_height: f32,
        zoom: f32,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(selection) = self.selected.as_ref().filter(|s| s.page == page) else {
            return Vec::new();
        };
        let Some(widgets) = self.widgets.as_ref() else {
            return Vec::new();
        };

        let frame = selection.frame;
        let left = frame.left * zoom;
        let top = (page_height - frame.top) * zoom;
        let width = frame.width() * zoom;
        let height = (frame.height() * zoom).max(28.0);
        let editor = widgets.editor.clone();

        // Панель встаёт выше ручки поворота, а не на её место: иначе они
        // накрыли бы друг друга и повернуть блок стало бы нечем. Если сверху
        // места не хватает, панель уходит под блок. По горизонтали панель
        // прижимается к краям страницы: уехавшая за край панель — это кнопки,
        // до которых не добраться.
        let clearance = FORMAT_BAR_HEIGHT + BAR_GAP + ROTATE_LIFT;
        let bar_top = if top >= clearance {
            top - clearance
        } else {
            top + height + BAR_GAP
        };
        let bar_top = bar_top.clamp(0.0, (page_height * zoom - FORMAT_BAR_HEIGHT).max(0.0));
        let bar_left = left.clamp(0.0, (page_width * zoom - BAR_WIDTH).max(0.0));

        // Фона нет намеренно: под рамкой настоящая страница с настоящим
        // текстом, и закрашивать её нечем — правка идёт прямо в документе.
        //
        // Высота задана как наименьшая, а не точная: за нижний край рамку
        // тянут вручную, но если текста стало больше, чем в неё влезает, она
        // растёт сама — иначе строки вылезали бы наружу. Так же ведёт себя
        // самонастраивающаяся рамка в Acrobat.
        let editor = div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .min_h(px(frame.height() * zoom))
            .border_1()
            .border_color(rgba(ACCENT))
            // Всё нутро рамки — текстовая зона: без своего курсора здесь
            // просвечивала бы рука накладки выделения, лежащей ниже.
            .cursor(gpui::CursorStyle::IBeam)
            // Щелчок внутри рамки — работа с блоком, а не «мимо блока»: до
            // страницы, где он значил бы «применить», ему хода нет.
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
            )
            .child(editor)
            .children(self.render_grips(cx))
            .into_any_element();

        // Панель непрозрачна для мыши. Иначе нажатие проходит сквозь неё в
        // накладку блока, лежащего выше по странице, и вместо кнопки панели
        // перевыделяется чужой абзац — панель выглядела «нетыкабельной».
        let bar = div()
            .absolute()
            .left(px(bar_left))
            .top(px(bar_top))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
            )
            .child(self.render_format_bar(cx))
            .into_any_element();

        vec![editor, bar]
    }

    /// Обвязка рамки: полосы переноса, восемь маркеров размера и поворот.
    ///
    /// Полосы лежат по контуру рамки, а не внутри неё: внутри работает правка
    /// текста, и нажатие там обязано ставить каретку, а не хватать блок.
    /// Маркеры нарисованы поверх полос и перехватывают нажатие у своих углов и
    /// середин сторон.
    fn render_grips(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut parts = Vec::new();

        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            let strip = div()
                .absolute()
                .occlude()
                .cursor(Grip::Move.cursor())
                .on_mouse_down(MouseButton::Left, grab(cx, Grip::Move));
            parts.push(edge.place(strip).into_any_element());
        }

        for grip in Grip::RESIZE {
            // Квадратные, с белой заливкой и цветным контуром: такой маркер
            // виден на любом фоне страницы, в том числе на чёрной иллюстрации.
            let marker = div()
                .absolute()
                .occlude()
                .w(px(GRIP_SIZE))
                .h(px(GRIP_SIZE))
                .bg(gpui::white())
                .border_1()
                .border_color(rgba(ACCENT))
                .cursor(grip.cursor())
                .on_mouse_down(MouseButton::Left, grab(cx, grip));
            parts.push(place_marker(marker, grip).into_any_element());
        }

        // Ручка поворота вынесена над рамкой: на самом контуре ей места нет,
        // там уже стоят маркеры размера.
        parts.push(
            div()
                .absolute()
                .top(px(-ROTATE_LIFT))
                .left(gpui::relative(0.5))
                .ml(px(-GRIP_SIZE))
                .w(px(GRIP_SIZE * 2.0))
                .h(px(GRIP_SIZE * 2.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(gpui::white())
                .border_1()
                .border_color(rgba(ACCENT))
                .text_xs()
                .text_color(rgba(ACCENT))
                .occlude()
                .cursor(Grip::Rotate.cursor())
                .child(ROTATE_GLYPH)
                .on_mouse_down(MouseButton::Left, grab(cx, Grip::Rotate))
                .into_any_element(),
        );

        parts
    }

    /// Линии привязки, показанные, пока рамку тянут вровень с соседями.
    ///
    /// Линия проводится через всю страницу, а не по длине совпадения: так
    /// сразу видно, чему именно рамка стала вровень.
    pub(crate) fn render_guides(&self, page_height: f32, zoom: f32) -> Vec<AnyElement> {
        self.guides
            .iter()
            .map(|guide| match *guide {
                Guide::Vertical(x) => div()
                    .absolute()
                    .left(px(x * zoom))
                    .top_0()
                    .bottom_0()
                    .w(px(1.0))
                    .bg(rgba(GUIDE_COLOR)),
                Guide::Horizontal(y) => div()
                    .absolute()
                    .top(px((page_height - y) * zoom))
                    .left_0()
                    .right_0()
                    .h(px(1.0))
                    .bg(rgba(GUIDE_COLOR)),
            })
            .map(IntoElement::into_any_element)
            .collect()
    }

    fn render_format_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (Some(selection), Some(widgets)) = (self.selected.as_ref(), self.widgets.as_ref())
        else {
            return div().into_any_element();
        };

        // Состояние кнопок читаем там, где стоит каретка: панель обязана
        // показывать оформление текущего места, а не последнее нажатие.
        let at_caret = widgets.editor.read(cx).text.style_at_caret();
        let (bold, italic, underline, script) = (
            at_caret.bold,
            at_caret.italic,
            at_caret.underline,
            at_caret.script,
        );
        let templates: Vec<FormatTemplate> = self.templates.items().to_vec();
        let can_add_template = !self.templates.is_full();

        // Две короткие строки вместо одной длинной: узкая панель не выезжает
        // за страницу и не заслоняет пол-абзаца. Шрифт, кегль и цвет живут в
        // правой панели свойств — как в Acrobat.
        div()
            .w(px(BAR_WIDTH))
            .rounded_md()
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_md()
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .pt_1()
                    .child(toggle(cx, "fmt-bold", "Ж", bold, move |this, cx| {
                        this.set_bold(!bold, cx)
                    }))
                    .child(toggle(cx, "fmt-italic", "К", italic, move |this, cx| {
                        this.set_italic(!italic, cx)
                    }))
                    .child(toggle(
                        cx,
                        "fmt-underline",
                        "Ч",
                        underline,
                        move |this, cx| this.set_underline(!underline, cx),
                    ))
                    .child(toggle(
                        cx,
                        "fmt-superscript",
                        "x²",
                        script == Script::Superscript,
                        move |this, cx| {
                            let next = if script == Script::Superscript {
                                Script::Baseline
                            } else {
                                Script::Superscript
                            };
                            this.set_script(next, cx)
                        },
                    ))
                    .child(toggle(
                        cx,
                        "fmt-subscript",
                        "x₂",
                        script == Script::Subscript,
                        move |this, cx| {
                            let next = if script == Script::Subscript {
                                Script::Baseline
                            } else {
                                Script::Subscript
                            };
                            this.set_script(next, cx)
                        },
                    ))
                    .children(
                        selection
                            .page_font
                            .as_ref()
                            .and_then(|font| font.color)
                            .map(|color| {
                                div()
                                    .w(px(16.0))
                                    .h(px(16.0))
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .bg(color)
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("erase-block")
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path("icons/delete-02.svg"))
                            .tooltip("Удалить блок со страницы")
                            .on_click(cx.listener(|this, _, _, cx| this.erase_selected(cx))),
                    )
                    .child(
                        Button::new("cancel-edit")
                            .small()
                            .ghost()
                            .label("×")
                            .tooltip("Отменить правку (Esc). Применение — щелчок мимо блока")
                            .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .pb_1()
                    .children(templates.into_iter().enumerate().map(|(index, template)| {
                        let hint = format!("{}\nAlt+клик — удалить шаблон", template.summary());
                        Button::new(("template", index))
                            .small()
                            .ghost()
                            .label(template.name.clone())
                            .tooltip(hint)
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                // Список шаблонов ограничен, и без способа
                                // удалить лишнее он упёрся бы в потолок.
                                if event.modifiers().alt {
                                    this.forget_template(&template.name, cx);
                                } else {
                                    this.apply_template(&template, window, cx);
                                }
                            }))
                    }))
                    .child(
                        Button::new("save-template")
                            .small()
                            .ghost()
                            .label("＋")
                            .tooltip(if can_add_template {
                                "Сохранить оформление как шаблон"
                            } else {
                                "Список шаблонов заполнен"
                            })
                            .on_click(
                                cx.listener(|this, _, _, cx| this.save_current_as_template(cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Запоминает начало перетаскивания рамки.
    pub(crate) fn begin_drag(
        &mut self,
        grip: Grip,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.selected.as_ref() else {
            return;
        };
        self.drag = Some(FrameDrag {
            grip,
            origin,
            start: selection.frame,
            start_rotation: selection.rotation,
        });

        // Клавиатура остаётся у текста: рамку двигают и тут же продолжают
        // печатать, а Esc обязан отменять правку и после перетаскивания.
        if let Some(widgets) = self.widgets.as_ref() {
            window.focus(&widgets.editor.read(cx).focus_handle(cx));
        }
        cx.notify();
    }

    /// Двигает рамку вслед за курсором.
    pub(crate) fn drag_frame(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        let zoom = self
            .doc
            .as_ref()
            .map(|doc| doc.zoom)
            .unwrap_or(1.0)
            .max(0.01);

        if drag.grip == Grip::Rotate {
            self.rotate_frame(position, cx);
            return;
        }

        // Смещение переводится в пункты документа: рамка живёт в его
        // координатах, а не в экранных, и от масштаба не зависит.
        let dx = f32::from(position.x - drag.origin.x) / zoom;
        let dy = f32::from(position.y - drag.origin.y) / zoom;
        let (grip, start) = (drag.grip, drag.start);
        let moved = crate::frame::dragged(start, grip, dx, dy);

        // Привязка — по соседним абзацам той же страницы. Сам правимый блок в
        // список не входит: притягиваться к себе бессмысленно.
        let (frame, guides) = if self.snapping {
            let targets = self.snap_targets();
            crate::frame::snap(moved, grip, &targets, SNAP_TOLERANCE)
        } else {
            (moved, Vec::new())
        };

        let changed = self.guides != guides;
        self.guides = guides;

        let Some(selection) = self.selected.as_mut() else {
            return;
        };
        if selection.frame == frame {
            if changed {
                cx.notify();
            }
            return;
        }
        selection.frame = frame;

        // Новая ширина сразу меняет перенос строк.
        if let Some(widgets) = self.widgets.as_ref() {
            widgets.editor.update(cx, |editor, cx| {
                editor.wrap_width = px(frame.width() * zoom);
                cx.notify();
            });
        }
        let _ = window;
        // И страница перерисовывается с текстом на новом месте.
        self.request_preview(cx);
    }

    /// Прямоугольники, к которым притягивается рамка: соседние абзацы страницы.
    fn snap_targets(&self) -> Vec<pdfcore::geom::Rect> {
        let (Some(selection), Some(doc)) = (self.selected.as_ref(), self.doc.as_ref()) else {
            return Vec::new();
        };
        doc.blocks
            .get(&selection.page)
            .map(|blocks| {
                blocks
                    .iter()
                    .map(|block| block.bbox)
                    .filter(|bbox| *bbox != selection.bbox)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Поворачивает блок вслед за ручкой поворота.
    fn rotate_frame(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        let zoom = self
            .doc
            .as_ref()
            .map(|doc| doc.zoom)
            .unwrap_or(1.0)
            .max(0.01);
        let Some(page_height) = self
            .selected
            .as_ref()
            .and_then(|s| self.doc.as_ref()?.info.size(s.page))
            .map(|size| size.height)
        else {
            return;
        };

        // Центр рамки в экранных координатах: вокруг него и крутится блок.
        let start = drag.start;
        let centre = gpui::point(
            px(start.center_x() * zoom),
            px((page_height - (start.top + start.bottom) * 0.5) * zoom),
        );
        // Мышь приходит в координатах окна, а центр посчитан в координатах
        // страницы. Углы считаются от одной точки, поэтому обе точки надо
        // привести к одной системе — иначе поворот пошёл бы вокруг чужого
        // места. Положение страницы в окне запоминается при её отрисовке.
        // Угол выбирается по той же точке, за которую тянут: страница в
        // ленте раскладывается не единожды, и «просто взять последний» значило
        // бы иногда считать поворот от чужого места.
        let origin = self
            .selected
            .as_ref()
            .and_then(|selection| self.origin_for(selection.page, position))
            .unwrap_or(self.page_origin);
        let local = |value: Point<Pixels>| gpui::point(value.x - origin.x, value.y - origin.y);
        let rotation = crate::frame::rotated(
            drag.start_rotation,
            centre,
            local(drag.origin),
            local(position),
        );

        let Some(selection) = self.selected.as_mut() else {
            return;
        };
        if (selection.rotation - rotation).abs() < 0.05 {
            return;
        }
        selection.rotation = rotation;
        self.request_preview(cx);
    }

    /// Меняет цвет текста в выделении или для следующего ввода.
    pub(crate) fn set_color(&mut self, color: [f32; 3], cx: &mut Context<Self>) {
        self.restyle(move |style| style.color = Some(color), cx);
    }

    /// Задаёт выключку абзаца.
    pub(crate) fn set_align(&mut self, align: pdfcore::model::Align, cx: &mut Context<Self>) {
        let Some(selection) = self.selected.as_mut() else {
            return;
        };
        if selection.align == align {
            return;
        }
        selection.align = align;
        selection.needs_retypeset = true;
        self.request_preview(cx);
    }

    /// Задаёт интерлиньяж. Меньше кегля строки налезали бы друг на друга,
    /// поэтому ниже разумного предела он не опускается.
    pub(crate) fn set_line_height(&mut self, points: f32, cx: &mut Context<Self>) {
        let Some(selection) = self.selected.as_mut() else {
            return;
        };
        let smallest = (selection.style.size * 0.6).max(1.0);
        selection.line_height = Some(points.max(smallest));
        selection.needs_retypeset = true;
        self.request_preview(cx);
    }

    /// Задаёт поворот блока числом — из панели свойств.
    pub(crate) fn set_rotation(&mut self, degrees: f32, cx: &mut Context<Self>) {
        let Some(selection) = self.selected.as_mut() else {
            return;
        };
        selection.rotation = crate::frame::normalise(degrees);
        self.request_preview(cx);
    }

    /// Применяет изменение оформления к выделению в тексте.
    fn restyle(&mut self, change: impl Fn(&mut RunStyle) + 'static, cx: &mut Context<Self>) {
        let Some(widgets) = self.widgets.as_ref() else {
            return;
        };
        let editor = widgets.editor.clone();
        editor.update(cx, |editor, cx| editor.restyle(change, cx));
        cx.notify();
    }

    pub(crate) fn set_bold(&mut self, value: bool, cx: &mut Context<Self>) {
        self.restyle(move |style| style.bold = value, cx);
    }

    pub(crate) fn set_italic(&mut self, value: bool, cx: &mut Context<Self>) {
        self.restyle(move |style| style.italic = value, cx);
    }

    pub(crate) fn set_underline(&mut self, value: bool, cx: &mut Context<Self>) {
        self.restyle(move |style| style.underline = value, cx);
    }

    /// Нижний и верхний индекс — как кнопки в текстовом редакторе.
    pub(crate) fn set_script(&mut self, script: Script, cx: &mut Context<Self>) {
        self.restyle(move |style| style.script = script, cx);
    }

    /// Применяет шаблон к панели формата. Текст при этом не трогается —
    /// пользователь ещё может передумать и нажать «×».
    pub(crate) fn apply_template(
        &mut self,
        template: &FormatTemplate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(widgets) = self.widgets.as_ref() else {
            return;
        };

        let families = pdfcore::system_fonts().families();
        if let Some(index) = families
            .iter()
            .position(|f| f.eq_ignore_ascii_case(&template.family))
        {
            self.chosen_family = Some(families[index].clone());
            self.chosen_face = None;
        }

        let size_input = widgets.size.clone();
        let size = template.size;
        size_input.update(cx, |state, cx| {
            state.set_value(format!("{size:.1}"), window, cx);
        });

        let (bold, italic, underline) = (template.bold, template.italic, template.underline);
        let family = template.family.clone();
        self.restyle(
            move |style| {
                style.bold = bold;
                style.italic = italic;
                style.underline = underline;
                style.family = Some(family.clone());
            },
            cx,
        );
        self.set_status(format!("Шаблон «{}» применён", template.name), cx);
    }

    pub(crate) fn forget_template(&mut self, name: &str, cx: &mut Context<Self>) {
        self.templates.remove(name);
        self.set_status(format!("Шаблон «{name}» удалён"), cx);
    }

    pub(crate) fn save_current_as_template(&mut self, cx: &mut Context<Self>) {
        if self.templates.is_full() {
            self.set_status("Больше шаблонов не помещается — удалите ненужные", cx);
            return;
        }
        let Some((family, size, bold, italic, underline)) = self.current_format(cx) else {
            return;
        };

        let name = self.templates.suggest_name();
        self.templates.save(FormatTemplate {
            name: name.clone(),
            family,
            size,
            bold,
            italic,
            underline,
        });
        self.set_status(format!("Шаблон «{name}» сохранён"), cx);
    }
}

/// Обработчик нажатия на маркер или полосу рамки.
///
/// Событие обязано остановиться здесь. Области рамки перекрываются — маркер
/// лежит на полосе, полоса на рамке найденного абзаца, — а в gpui нажатие
/// доходит до **всех** обработчиков под курсором. Без остановки каждый
/// следующий переписывал бы захват предыдущего: нажатие на угловой маркер
/// оборачивалось переносом блока, а сам перенос сбивался повторным выделением
/// того же абзаца, и блок стоял на месте.
fn grab(
    cx: &mut Context<Viewer>,
    grip: Grip,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
        this.begin_drag(grip, event.position, window, cx);
        cx.stop_propagation();
    })
}

/// Ставит маркер на своё место: по углам и посередине сторон.
///
/// Смещение на половину маркера нужно, чтобы он сидел **на** линии рамки, а не
/// рядом с ней: иначе половина его площади оказалась бы вне контура и попасть
/// по нему было бы вдвое труднее.
fn place_marker(marker: Div, grip: Grip) -> Div {
    let half = px(-GRIP_SIZE / 2.0);
    let centre = gpui::relative(0.5);
    match grip {
        Grip::TopLeft => marker.left(half).top(half),
        Grip::Top => marker.left(centre).ml(half).top(half),
        Grip::TopRight => marker.right(half).top(half),
        Grip::Right => marker.right(half).top(centre).mt(half),
        Grip::BottomRight => marker.right(half).bottom(half),
        Grip::Bottom => marker.left(centre).ml(half).bottom(half),
        Grip::BottomLeft => marker.left(half).bottom(half),
        Grip::Left => marker.left(half).top(centre).mt(half),
        Grip::Move | Grip::Rotate => marker,
    }
}

/// Сторона рамки, за которую её тянут.
#[derive(Clone, Copy)]
enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

impl Edge {
    /// Растягивает полосу вдоль своей стороны рамки.
    ///
    /// Полоса вынесена наружу и заходит внутрь всего на пару пикселей. Иначе
    /// широкая полоса съедала бы начало первой строки: щелчок по первым буквам
    /// хватал бы блок вместо того, чтобы поставить каретку.
    ///
    /// Углы полосам не принадлежат: там лежат маркеры, и они перекрывают
    /// полосу, потому что нарисованы позже.
    fn place(self, strip: Div) -> Div {
        let thickness = px(EDGE);
        let inset = px(-(EDGE - 2.0));
        match self {
            Edge::Top => strip.left_0().right_0().top(inset).h(thickness),
            Edge::Bottom => strip.left_0().right_0().bottom(inset).h(thickness),
            Edge::Left => strip.top_0().bottom_0().left(inset).w(thickness),
            Edge::Right => strip.top_0().bottom_0().right(inset).w(thickness),
        }
    }
}

fn toggle(
    cx: &mut Context<Viewer>,
    id: &'static str,
    label: &'static str,
    active: bool,
    action: impl Fn(&mut Viewer, &mut Context<Viewer>) + 'static,
) -> Button {
    Button::new(id)
        .small()
        .when(active, |b| b.primary())
        .when(!active, |b| b.ghost())
        .label(label)
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
}
