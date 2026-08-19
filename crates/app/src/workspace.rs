//! Корневое представление: переключает стартовую страницу и открытый документ,
//! владеет списком недавних и отвечает за открытие файлов.
//!
//! Просмотрщик ничего не знает ни про недавние, ни про файловые диалоги — он
//! умеет показывать один открытый документ и сообщать наружу, что пользователь
//! попросился обратно на стартовую страницу.

use std::path::{Path, PathBuf};

use gpui::{
    AppContext, Context, Entity, ExternalPaths, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, Render, Styled, Window, div,
};
use gpui_component::ActiveTheme;

use crate::recents::Recents;
use crate::viewer::{Viewer, ViewerEvent};

pub struct Workspace {
    pub(crate) recents: Recents,
    viewer: Option<Entity<Viewer>>,
    pub(crate) error: Option<String>,
    focus: FocusHandle,
}

impl Workspace {
    pub fn new(path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut workspace = Workspace {
            recents: Recents::load(),
            viewer: None,
            error: None,
            focus: cx.focus_handle(),
        };
        if let Some(path) = path {
            workspace.open_document(&path, cx);
        }
        workspace
    }

    pub(crate) fn open_document(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        let viewer = cx.new(|cx| Viewer::new(path.clone(), cx));

        match viewer.read(cx).page_count() {
            Some(pages) => {
                self.recents.touch(&path, pages);
                cx.subscribe(&viewer, |this, _, event, cx| match event {
                    ViewerEvent::GoHome => this.show_home(cx),
                })
                .detach();
                self.viewer = Some(viewer);
                self.error = None;
            }
            None => {
                // Не открылся — остаёмся на стартовой странице и показываем
                // причину. В недавние такой документ не попадает.
                self.error = Some(
                    viewer
                        .read(cx)
                        .error()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("не удалось открыть {}", path.display())),
                );
                self.viewer = None;
            }
        }
        cx.notify();
    }

    pub(crate) fn show_home(&mut self, cx: &mut Context<Self>) {
        self.viewer = None;
        cx.notify();
    }

    /// Системный диалог выбора файла.
    pub(crate) fn prompt_open(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Открыть".into()),
        });

        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| this.open_document(&path, cx))
                .ok();
        })
        .detach();
    }

    pub(crate) fn forget(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.recents.remove(path);
        cx.notify();
    }

    fn on_files_dropped(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let pdf = paths
            .paths()
            .iter()
            .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")));

        match pdf {
            Some(path) => self.open_document(path, cx),
            None => {
                self.error = Some("Перетащить можно только файл PDF".to_owned());
                cx.notify();
            }
        }
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.viewer.clone() {
            Some(viewer) => viewer.into_any_element(),
            None => self.render_home(window, cx),
        };

        div()
            .id("workspace")
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&self.focus)
            // Перетаскивание работает всегда, а не только на стартовой
            // странице: бросить файл поверх открытого документа — привычный жест.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.on_files_dropped(paths, cx)
            }))
            .child(body)
    }
}
