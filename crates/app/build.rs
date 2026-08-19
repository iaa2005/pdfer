//! Производные картинки собираются здесь же, при сборке.
//!
//! `icon.svg` из корня проекта превращается в `.ico` и вшивается в
//! исполняемый файл ресурсом с идентификатором 1 — именно его gpui загружает
//! для окна и панели задач. `logo.svg` растрируется в `.png` для стартовой
//! страницы: gpui рисует SVG одноцветной маской, и разноцветный знак в ней
//! слипся бы в силуэт.
//!
//! Держать в репозитории производные файлы рядом с исходными — верный способ
//! однажды поправить один и забыть про другой.

use std::path::{Path, PathBuf};

/// Размеры, которые Windows берёт из иконки: список, панель задач, крупные
/// плитки проводника.
const SIZES: [u32; 6] = [16, 24, 32, 48, 128, 256];

/// Во сколько раз растр логотипа крупнее его собственных размеров. Экран
/// бывает и вдвое плотнее обычного, и знак на нём не должен мылиться.
const LOGO_SCALE: f32 = 4.0;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("корень воркспейса")
        .to_path_buf();
    let svg = root.join("icon.svg");
    let logo = root.join("logo.svg");
    println!("cargo:rerun-if-changed={}", svg.display());
    println!("cargo:rerun-if-changed={}", logo.display());
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // Файл создаётся всегда, даже пустой: его подхватывает `include_bytes!`,
    // и без него не соберётся сам крейт, а не только картинка.
    let png = out.join("logo.png");
    match render_png(&logo, &png) {
        Ok(()) => {}
        Err(e) => {
            println!("cargo:warning=логотип не растрирован: {e}");
            std::fs::write(&png, EMPTY_PNG).expect("заглушка логотипа");
        }
    }

    if !cfg!(target_os = "windows") {
        return;
    }
    if !svg.exists() {
        println!("cargo:warning=icon.svg не найден — окно останется без иконки");
        return;
    }

    let ico = out.join("icon.ico");
    match render_ico(&svg, &ico) {
        Ok(()) => {
            let mut resource = winresource::WindowsResource::new();
            resource.set_icon(&ico.to_string_lossy());
            // Свойства файла в проводнике: без них exe подписан именем крейта.
            resource.set("ProductName", "PDFer");
            resource.set("FileDescription", "PDFer — редактор PDF");
            resource.set("OriginalFilename", "PDFer.exe");
            resource.set("CompanyName", "IAA Labs");
            resource.set(
                "LegalCopyright",
                "MIT © IAA Labs · pdfium © The Chromium Authors",
            );
            if let Err(e) = resource.compile() {
                println!("cargo:warning=иконка не вшита: {e}");
            }
        }
        Err(e) => println!("cargo:warning=не удалось растрировать icon.svg: {e}"),
    }
}

/// Прозрачная точка: подставляется вместо логотипа, если растрировать его не
/// вышло, — стартовая страница не покажет знак, но программа соберётся.
const EMPTY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// Растрирует SVG в PNG, увеличив его в [`LOGO_SCALE`] раз.
fn render_png(svg: &Path, png: &Path) -> Result<(), String> {
    let data = std::fs::read(svg).map_err(|e| e.to_string())?;
    let mut options = usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_data(&data, &options).map_err(|e| e.to_string())?;

    let width = (tree.size().width() * LOGO_SCALE).ceil().max(1.0) as u32;
    let height = (tree.size().height() * LOGO_SCALE).ceil().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("пустой холст")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(LOGO_SCALE, LOGO_SCALE),
        &mut pixmap.as_mut(),
    );
    let encoded = pixmap.encode_png().map_err(|e| e.to_string())?;
    std::fs::write(png, encoded).map_err(|e| e.to_string())
}

/// Растрирует SVG в несколько размеров и складывает их в один `.ico`.
fn render_ico(svg: &Path, ico: &Path) -> Result<(), String> {
    let data = std::fs::read(svg).map_err(|e| e.to_string())?;
    let mut options = usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_data(&data, &options).map_err(|e| e.to_string())?;

    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
    for side in SIZES {
        let mut pixmap = tiny_skia::Pixmap::new(side, side).ok_or("пустой холст")?;
        let scale = side as f32 / tree.size().width().max(1.0);
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let image = ico::IconImage::from_rgba_data(side, side, pixmap.data().to_vec());
        icon.add_entry(ico::IconDirEntry::encode(&image).map_err(|e| e.to_string())?);
    }

    let file = std::fs::File::create(ico).map_err(|e| e.to_string())?;
    icon.write(file).map_err(|e| e.to_string())
}
