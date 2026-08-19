//! Раны одной страницы с координатами — для разбора сложной вёрстки.
//!
//!     cargo run -p pdfcore --example probe_runs -- book.pdf 67 "Llama"
//!
//! Печатает каждый ран с рамкой, кеглем и шрифтом. Третьим доводом можно дать
//! подстроку — тогда показываются только раны рядом с ней по вертикали.

use anyhow::{Result, bail};
use pdfcore::extract::extract_page_text;
use pdfium_render::prelude::*;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_runs <файл.pdf> <страница с нуля> [подстрока]");
    };
    let page: i32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);
    let needle = args.next();

    let bindings = Pdfium::bind_to_library("vendor/pdfium/bin/pdfium.dll")
        .or_else(|_| Pdfium::bind_to_system_library())?;
    let pdfium = Pdfium::new(bindings);
    let document = pdfium.load_pdf_from_file(&path, None)?;
    let page = document.pages().get(page)?;
    let text = extract_page_text(&page)?;

    // Полоса вокруг искомого текста: по вертикали ±30 пунктов.
    let band = needle.as_ref().and_then(|needle| {
        text.runs
            .iter()
            .find(|run| run.text.contains(needle.as_str()))
            .map(|run| run.baseline())
    });

    for run in &text.runs {
        if let Some(band) = band
            && (run.baseline() - band).abs() > 30.0
        {
            continue;
        }
        println!(
            "x {:7.1}..{:7.1}  y {:7.1}..{:7.1}  {:5.2} pt  {:22}  {:?}",
            run.bbox.left,
            run.bbox.right,
            run.bbox.bottom,
            run.bbox.top,
            run.style.size,
            run.style.family.as_str(),
            run.text
        );
    }
    Ok(())
}
