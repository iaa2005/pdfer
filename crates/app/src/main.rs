//! Точка входа редактора.
//!
//!     cargo run -p app -- book.pdf

// Готовое приложение запускается без чёрного окна консоли: на Windows это
// решает подсистема, выбранная при сборке. В отладочной сборке консоль
// остаётся — там она и есть окно журнала.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod assets;
mod editing;
mod frame;
mod home;
mod properties;
mod recents;
mod rich_text;
mod templates;
mod text_element;
mod text_layout;
mod viewer;
mod workspace;

use gpui::{
    AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use gpui_component::Root;

use workspace::Workspace;

/// Куда писать журнал.
///
/// В отладочной сборке — в консоль, как и раньше. В готовой консоли нет, и
/// строки уходили бы в никуда: там журнал ложится файлом рядом с настройками,
/// заново на каждый запуск, чтобы не рос без конца.
fn init_tracing() {
    let builder = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false);

    if cfg!(debug_assertions) {
        builder.init();
        return;
    }

    let file = recents::data_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("pdfer.log");
        std::fs::File::create(&path).ok().map(|file| (path, file))
    });

    match file {
        Some((path, file)) => {
            let file = std::sync::Arc::new(std::sync::Mutex::new(file));
            builder
                .with_ansi(false)
                .with_writer(move || FileWriter(file.clone()))
                .init();
            tracing::info!(path = %path.display(), "журнал");
        }
        // Каталога данных нет или он закрыт на запись — тогда без журнала:
        // ронять запуск редактора из-за него было бы совсем нелепо.
        None => builder.with_writer(std::io::sink).init(),
    }
}

/// Обёртка над файлом журнала: `tracing` просит писателя на каждую запись, а
/// файл один на всё приложение и делится замком.
struct FileWriter(std::sync::Arc<std::sync::Mutex<std::fs::File>>);

impl std::io::Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut file) => file.write(buf),
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock() {
            Ok(mut file) => file.flush(),
            Err(_) => Ok(()),
        }
    }
}

fn main() {
    init_tracing();

    let path = std::env::args().nth(1).map(std::path::PathBuf::from);

    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            // Тёмная тема — родная для этого редактора: страницы книги и так
            // белые, и тёмная рама вокруг них глазу заметно легче.
            gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);

            // Шрифт интерфейса — вкомпилированный Geist Mono. Регистрация идёт
            // после смены темы: та переписывает семейство целиком, и порядок
            // наоборот стёр бы наш выбор.
            let fonts = assets::FONTS
                .iter()
                .map(|bytes| std::borrow::Cow::Borrowed(*bytes))
                .collect();
            match cx.text_system().add_fonts(fonts) {
                Ok(()) => {
                    let theme = gpui_component::Theme::global_mut(cx);
                    theme.font_family = assets::UI_FONT.into();
                    theme.mono_font_family = assets::UI_FONT.into();
                }
                Err(e) => tracing::warn!("не удалось загрузить Geist Mono: {e:#}"),
            }

            let path = path.clone();
            cx.spawn(async move |cx| {
                let options = WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(80.0), px(60.0)),
                        size: size(px(1440.0), px(920.0)),
                    })),
                    titlebar: Some(TitlebarOptions {
                        title: Some("PDFer".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                };

                cx.open_window(options, |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new(path, cx));
                    // Первым уровнем внутри окна обязан быть Root — на нём держатся
                    // всплывающие панели, диалоги и уведомления gpui-component.
                    cx.new(|cx| Root::new(workspace, window, cx))
                })?;

                Ok::<_, anyhow::Error>(())
            })
            .detach();
        });
}
