//! Проверка хирургии потока на настоящем документе.
//!
//!     cargo run -p pdfcore --example probe_surgical -- book.pdf [страница] [подстрока]
//!
//! Заменяет текст одного абзаца и считает изменения **отдельно внутри рамки
//! блока и вне её**. Вне рамки не должно измениться ни одного пикселя — это и
//! есть определение неразрушающей правки.

use anyhow::{Result, bail};
use image::RgbaImage;
use lopdf::Document;
use pdfcore::stream_edit::{BlockRewrite, rewrite_block};
use pdfcore::{RenderEvent, Renderer};
use pdfium_render::prelude::*;

fn render(path: &std::path::Path, page: PdfPageIndex) -> Result<(u32, u32, Vec<u8>)> {
    let pdfium = pdfcore::engine::pdfium()?;
    let document = pdfium.load_pdf_from_file(path, None)?;
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

fn save_png(name: &str, raster: &(u32, u32, Vec<u8>)) -> Result<()> {
    let mut rgba = raster.2.clone();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    if let Some(image) = RgbaImage::from_raw(raster.0, raster.1, rgba) {
        let path = std::env::temp_dir().join(name);
        image.save(&path)?;
        println!("{}", path.display());
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_surgical <файл.pdf> [страница] [подстрока]")
    };
    let path = std::path::PathBuf::from(path);
    let page: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);
    let needle = args.next().unwrap_or_else(|| "deep dive".to_owned());
    let replacement = args
        .next()
        .unwrap_or_else(|| "A thorough tour of modern AI systems".to_owned());
    let replacement = replacement.as_str();
    // Пятым аргументом — гарнитура, которую надо встроить.
    let family = args.next();

    // Рамку блока и высоту страницы берём у pdfium — он же их и показывает.
    let (tx, rx) = flume::unbounded();
    let (renderer, info) = Renderer::open(&path, tx)?;
    let page_height = info.size(page).map(|s| s.height).unwrap_or(0.0);
    renderer.request_blocks(page);
    let blocks = loop {
        match rx.recv_timeout(std::time::Duration::from_secs(60))? {
            RenderEvent::Blocks { blocks, .. } => break blocks,
            RenderEvent::Failed { message, .. } => bail!("разбор не удался: {message}"),
            _ => continue,
        }
    };
    let Some(target) = blocks.iter().find(|b| b.text().contains(&needle)) else {
        bail!("блок с «{needle}» не найден")
    };
    let bbox = target.bbox;
    println!("правим блок: {:?}", target.text());
    drop(renderer);

    let before = render(&path, page as PdfPageIndex)?;

    let mut document = Document::load(&path)?;
    let outcome = rewrite_block(
        &mut document,
        &BlockRewrite {
            line_height: target.leading(),
            ..BlockRewrite::text_only(page + 1, bbox, replacement)
        }
        .with_font(
            family
                .as_deref()
                .map(|name| pdfcore::FontRequest::new(name, false, false)),
            None,
        ),
    )?;
    println!(
        "опустошено операторов: {}, создано строк: {}",
        outcome.cleared_ops, outcome.created_lines
    );

    let out = std::env::temp_dir().join("probe_surgical_out.pdf");
    document.save(&out)?;

    let after = render(&out, page as PdfPageIndex)?;
    if before.0 != after.0 || before.1 != after.1 {
        bail!("размеры растров разошлись");
    }

    // Рамка блока в координатах растра: масштаб 1.0, ось Y перевёрнута.
    // Берём с запасом — новый текст может занять больше строк.
    let margin = 6.0;
    let x0 = (bbox.left - margin).max(0.0) as u32;
    let x1 = ((bbox.right + margin) as u32).min(before.0);
    let y0 = ((page_height - bbox.top - margin).max(0.0)) as u32;
    let y1 = (((page_height - bbox.bottom + margin) + 60.0) as u32).min(before.1);

    let (mut inside, mut outside) = (0usize, 0usize);
    for y in 0..before.1 {
        for x in 0..before.0 {
            let offset = ((y * before.0 + x) * 4) as usize;
            if before.2[offset..offset + 4] == after.2[offset..offset + 4] {
                continue;
            }
            if x >= x0 && x < x1 && y >= y0 && y < y1 {
                inside += 1;
            } else {
                outside += 1;
            }
        }
    }

    println!("\nизменений внутри рамки блока: {inside}");
    println!("изменений ВНЕ рамки блока:    {outside}");
    if outside == 0 {
        println!("\nВЫВОД: правка не задела ничего постороннего");
    } else {
        println!("\nВЫВОД: правка ПОРТИТ страницу за пределами блока");
    }

    save_png("surgical_before.png", &before)?;
    save_png("surgical_after.png", &after)?;
    Ok(())
}
