//! Стартовая страница: недавние документы и открытие новых.
//!
//! Сетка превью не открывает ни одного PDF — миниатюры берутся готовыми
//! файлами из кэша (см. [`crate::recents::thumbnail_path`]), поэтому экран
//! появляется мгновенно независимо от того, сколько документов в списке и
//! насколько они тяжёлые.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, Div, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, img, px, white,
};
use gpui_component::ActiveTheme;
use gpui_component::button::{Button, ButtonVariants};

use crate::recents::{RecentDoc, thumbnail_path};
use crate::workspace::Workspace;

const CARD_WIDTH: f32 = 164.0;
const THUMB_WIDTH: f32 = 148.0;
const THUMB_HEIGHT: f32 = 196.0;

fn v_flex() -> Div {
    div().flex().flex_col()
}

fn h_flex() -> Div {
    div().flex().flex_row()
}

impl Workspace {
    pub(crate) fn render_home(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entries: Vec<RecentDoc> = self.recents.entries().to_vec();

        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .when_some(self.error.clone(), |el, error| {
                el.child(self.render_error(error, cx))
            })
            .child(self.render_dropzone(cx))
            .child(
                div()
                    .px_8()
                    .pt_6()
                    .pb_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(if entries.is_empty() {
                        "Недавние документы".to_owned()
                    } else {
                        format!("Недавние документы · {}", entries.len())
                    }),
            )
            .child(self.render_grid(entries, cx))
            .into_any_element()
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> Div {
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .px_8()
            .pt_8()
            .pb_2()
            .child(div().text_2xl().child("PDFer"))
            .child(
                Button::new("open-document")
                    .primary()
                    .label("Открыть документ")
                    .on_click(cx.listener(|this, _, _, cx| this.prompt_open(cx))),
            )
    }

    fn render_error(&mut self, error: String, cx: &mut Context<Self>) -> Div {
        div()
            .mx_8()
            .mt_4()
            .px_4()
            .py_3()
            .rounded_md()
            .bg(cx.theme().danger.opacity(0.12))
            .text_sm()
            .text_color(cx.theme().danger)
            .child(error)
    }

    fn render_dropzone(&mut self, cx: &mut Context<Self>) -> Div {
        div()
            .mx_8()
            .mt_4()
            .h(px(104.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_lg()
            .border_1()
            .border_dashed()
            .border_color(cx.theme().border)
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("Перетащите сюда PDF, чтобы открыть")
    }

    fn render_grid(&mut self, entries: Vec<RecentDoc>, cx: &mut Context<Self>) -> AnyElement {
        if entries.is_empty() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Пока пусто — открытые документы появятся здесь")
                .into_any_element();
        }

        div()
            .id("recents-grid")
            .flex_1()
            .overflow_y_scroll()
            .px_8()
            .pb_8()
            .child(
                div().flex().flex_wrap().gap_5().children(
                    entries
                        .into_iter()
                        .enumerate()
                        .map(|(ix, doc)| self.render_card(ix, doc, cx)),
                ),
            )
            .into_any_element()
    }

    fn render_card(&mut self, ix: usize, doc: RecentDoc, cx: &mut Context<Self>) -> AnyElement {
        let available = doc.is_available();
        let thumbnail = thumbnail_path(&doc.path);
        let has_thumbnail = available && thumbnail.is_file();
        let path = doc.path.clone();

        let meta = if available {
            format!("{} стр. · {}", doc.pages, doc.opened_ago())
        } else {
            "файл не найден".to_owned()
        };

        v_flex()
            .id(("recent", ix))
            .w(px(CARD_WIDTH))
            .gap_2()
            .cursor_pointer()
            // Недоступную запись клик убирает из списка: чинить нечего, а
            // мёртвые плитки копятся сами собой.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    if available {
                        this.open_document(&path, cx);
                    } else {
                        this.forget(&path, cx);
                    }
                }),
            )
            .child(
                div()
                    .w(px(THUMB_WIDTH))
                    .h(px(THUMB_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().border)
                    .when(available, |el| el.bg(white()))
                    .when(!available, |el| el.bg(cx.theme().muted))
                    // ObjectFit::Contain в gpui задан по умолчанию, поэтому
                    // страница вписывается в рамку без искажения пропорций.
                    .when(has_thumbnail, |el| {
                        el.child(img(thumbnail).w(px(THUMB_WIDTH)).h(px(THUMB_HEIGHT)))
                    })
                    .when(!has_thumbnail, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("PDF"),
                        )
                    }),
            )
            .child(
                div()
                    .w_full()
                    .text_sm()
                    .truncate()
                    .when(!available, |el| el.text_color(cx.theme().muted_foreground))
                    .child(doc.title.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(meta),
            )
            .into_any_element()
    }
}
