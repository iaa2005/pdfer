//! Насколько разрушительна пересборка потока содержимого сама по себе.
//!
//!     cargo run -p pdfcore --example probe_regen -- book.pdf [страница]
//!
//! Открывает документ, вызывает `FPDFPage_GenerateContent` **без единой
//! правки**, сохраняет и сравнивает отрисовку до и после. Если страница
//! изменилась, значит пересборка потока не сохраняет исходное содержимое, и
//! правка через объектную модель pdfium портит документ независимо от того,
//! что именно мы редактируем.

use anyhow::{Result, bail};
use image::RgbaImage;
use pdfium_render::prelude::*;

fn render(document: &PdfDocument<'_>, page: PdfPageIndex) -> Result<(u32, u32, Vec<u8>)> {
    let page = document.pages().get(page)?;
    let config = PdfRenderConfig::new()
        .scale_page_by_factor(1.0)
        .set_reverse_byte_order(false);
    let bitmap = page.render_with_config(&config)?;
    Ok((
        bitmap.width() as u32,
        bitmap.height() as u32,
        bitmap.as_raw_bytes(),
    ))
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_regen <файл.pdf> [страница]")
    };
    let page: PdfPageIndex = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    let pdfium = pdfcore::engine::pdfium()?;

    let before = {
        let document = pdfium.load_pdf_from_file(&path, None)?;
        render(&document, page)?
    };
    println!("исходная страница: {}x{}", before.0, before.1);

    let out = std::env::temp_dir().join("probe_regen_out.pdf");
    {
        let document = pdfium.load_pdf_from_file(&path, None)?;
        let mut target = document.pages().get(page)?;
        // Единственное действие: пересборка потока. Ничего не добавляем и не
        // удаляем.
        target.regenerate_content()?;
        drop(target);
        document.save_to_file(&out)?;
    }
    println!("сохранено без правок: {}", out.display());

    let after = {
        let document = pdfium.load_pdf_from_file(&out, None)?;
        render(&document, page)?
    };

    if before.0 != after.0 || before.1 != after.1 {
        bail!(
            "размеры растров разошлись: {:?} против {:?}",
            (before.0, before.1),
            (after.0, after.1)
        );
    }

    let differing = before
        .2
        .chunks_exact(4)
        .zip(after.2.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    let total = (before.0 * before.1) as usize;
    let percent = differing as f64 * 100.0 / total as f64;

    println!("\nразличий: {differing} пикселей из {total} ({percent:.2}%)");
    if percent < 0.01 {
        println!("ВЫВОД: пересборка потока безвредна");
    } else {
        println!("ВЫВОД: пересборка потока МЕНЯЕТ страницу сама по себе");
    }

    // Оба варианта на диск, чтобы посмотреть глазами.
    for (name, (w, h, pixels)) in [
        ("probe_regen_before.png", before),
        ("probe_regen_after.png", after),
    ] {
        let mut rgba = pixels;
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2); // BGRA -> RGBA для PNG
        }
        if let Some(image) = RgbaImage::from_raw(w, h, rgba) {
            let target = std::env::temp_dir().join(name);
            image.save(&target)?;
            println!("{}", target.display());
        }
    }
    Ok(())
}
