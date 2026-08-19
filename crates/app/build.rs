//! Иконка приложения: `icon.svg` из корня проекта превращается в `.ico` и
//! вшивается в исполняемый файл ресурсом с идентификатором 1 — именно его
//! gpui загружает для окна и панели задач.
//!
//! Растрируется на месте, при сборке: держать в репозитории производный `.ico`
//! рядом с исходным `.svg` — верный способ однажды поправить один и забыть про
//! другой.

use std::path::{Path, PathBuf};

/// Размеры, которые Windows берёт из иконки: список, панель задач, крупные
/// плитки проводника.
const SIZES: [u32; 6] = [16, 24, 32, 48, 128, 256];

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("корень воркспейса")
        .to_path_buf();
    let svg = root.join("icon.svg");
    println!("cargo:rerun-if-changed={}", svg.display());
    println!("cargo:rerun-if-changed=build.rs");

    if !cfg!(target_os = "windows") {
        return;
    }
    if !svg.exists() {
        println!("cargo:warning=icon.svg не найден — окно останется без иконки");
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
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
