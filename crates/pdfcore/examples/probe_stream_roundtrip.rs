//! Замкнётся ли круг «разобрать поток содержимого — собрать обратно».
//!
//!     cargo run -p pdfcore --example probe_stream_roundtrip -- book.pdf [страница]
//!
//! На этом допущении стоит вся хирургия потока: если lopdf разбирает операторы
//! страницы и пишет их обратно без потерь, то замену текста можно делать
//! точечно, не трогая ни цвета, ни разрядку, ни соседние объекты. Проба
//! ничего не меняет по смыслу — только разбирает и собирает — и сравнивает
//! отрисовку до и после попиксельно.

use anyhow::{Result, bail};
use lopdf::Document;
use lopdf::content::Content;
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

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_stream_roundtrip <файл.pdf> [страница]")
    };
    let path = std::path::PathBuf::from(path);
    let page_number: usize = args.next().map(|s| s.parse()).transpose()?.unwrap_or(1);

    let before = render(&path, (page_number - 1) as PdfPageIndex)?;
    println!("эталон: {}x{}", before.0, before.1);

    let mut document = Document::load(&path)?;
    let pages = document.get_pages();
    let Some(&page_id) = pages.get(&(page_number as u32)) else {
        bail!("в документе нет страницы {page_number}");
    };

    let raw = document.get_page_content(page_id);
    println!("поток содержимого: {} байт", raw.len());

    let content = Content::decode(&raw)?;
    println!("операторов: {}", content.operations.len());

    let reencoded = content.encode()?;
    println!("после сборки:      {} байт", reencoded.len());

    document.change_page_content(page_id, reencoded)?;

    let out = std::env::temp_dir().join("probe_stream_roundtrip.pdf");
    document.save(&out)?;
    println!("сохранено: {}", out.display());

    let after = render(&out, (page_number - 1) as PdfPageIndex)?;
    if before.0 != after.0 || before.1 != after.1 {
        bail!("размеры растров разошлись");
    }

    let differing = before
        .2
        .chunks_exact(4)
        .zip(after.2.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    let total = (before.0 * before.1) as usize;
    let percent = differing as f64 * 100.0 / total as f64;

    println!("\nразличий: {differing} px из {total} ({percent:.3}%)");
    if percent < 0.01 {
        println!("ВЫВОД: круг замыкается — хирургия потока возможна");
    } else {
        println!("ВЫВОД: разбор/сборка сами по себе меняют страницу");
    }
    Ok(())
}
