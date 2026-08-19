//! Встроенные ресурсы приложения: иконки Hugeicons, логотип и шрифт Geist
//! Mono.
//!
//! Набор скачан пользователем и лежит в репозитории проекта; сюда попадают
//! только реально используемые файлы, вкомпилированные в исполняемый файл, —
//! никакого чтения с диска на старте и никаких потерянных ассетов при
//! переносе бинаря. gpui рисует SVG одноцветной маской, поэтому зашитый в
//! файлах цвет обводки роли не играет: кнопка красит иконку своим цветом.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

macro_rules! icons {
    ($($name:literal),+ $(,)?) => {
        &[$(($name, include_bytes!(concat!("../assets/", $name)) as &[u8])),+]
    };
}

/// Логотип для стартовой страницы. Именно растр, а не SVG: gpui рисует
/// векторные ресурсы одноцветной маской, и знак — розовый квадрат с белой
/// буквой и светлым словом рядом — слипся бы в сплошной силуэт. Растр
/// готовится при сборке из `logo.svg` (см. `build.rs`), поэтому править
/// по-прежнему нужно только исходный вектор.
const IMAGES: &[(&str, &[u8])] = &[(
    "images/logo.png",
    include_bytes!(concat!(env!("OUT_DIR"), "/logo.png")),
)];

const ASSETS: &[(&str, &[u8])] = icons![
    "icons/add-01.svg",
    "icons/alert-02.svg",
    "icons/align-bottom.svg",
    "icons/align-horizontal-center.svg",
    "icons/align-left.svg",
    "icons/align-right.svg",
    "icons/align-top.svg",
    "icons/align-vertical-center.svg",
    "icons/clipboard.svg",
    "icons/copy-01.svg",
    "icons/cursor-01.svg",
    "icons/delete-02.svg",
    "icons/file-export.svg",
    "icons/fit-height.svg",
    "icons/fit-width.svg",
    "icons/floppy-disk.svg",
    "icons/magnet-02.svg",
    "icons/page-spread.svg",
    "icons/paint-board.svg",
    "icons/redo-03.svg",
    "icons/rotate-left-01.svg",
    "icons/rotate-right-01.svg",
    "icons/text-align-center.svg",
    "icons/text-align-justify-center.svg",
    "icons/text-align-left.svg",
    "icons/text-align-right.svg",
    "icons/text-font.svg",
    "icons/undo-03.svg",
    "icons/zoom-in-area.svg",
    "icons/zoom-out-area.svg",
];

/// Начертания Geist Mono, вкомпилированные в бинарь.
///
/// Системе шрифт не ставится: интерфейс не должен зависеть от того, что у
/// пользователя установлено. Четырёх начертаний хватает на всё — обычный
/// текст, подписи, полужирные заголовки и акценты кнопок. Лицензия — SIL OFL,
/// её текст лежит рядом со шрифтами.
pub const FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
    include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Bold.ttf"),
];

/// Имя семейства, каким его видит система шрифтов gpui.
pub const UI_FONT: &str = "Geist Mono";

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS
            .iter()
            .chain(IMAGES)
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ASSETS
            .iter()
            .chain(IMAGES)
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}
