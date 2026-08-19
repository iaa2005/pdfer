//! Блоки одной страницы: сколько их и что внутри — для настройки детектора.
//!
//!     cargo run -p pdfcore --example probe_blocks -- book.pdf 28
//!
//! Печатает каждый блок с рамкой и началом текста. Именно это видит
//! пользователь как синие рамки в режиме «Блоки».

use anyhow::{Result, bail};
use pdfcore::extract::extract_page_text;
use pdfcore::{BlockOptions, detect_blocks};
use pdfium_render::prelude::*;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_blocks <файл.pdf> <страница с нуля>");
    };
    let page: i32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    let bindings = Pdfium::bind_to_library("vendor/pdfium/bin/pdfium.dll")
        .or_else(|_| Pdfium::bind_to_system_library())?;
    let pdfium = Pdfium::new(bindings);
    let document = pdfium.load_pdf_from_file(&path, None)?;
    let page = document.pages().get(page)?;
    let text = extract_page_text(&page)?;

    let blocks = detect_blocks(text.runs, &BlockOptions::default());
    println!("блоков: {}", blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        let mut preview: String = block.text().chars().take(60).collect();
        if block.text().chars().count() > 60 {
            preview.push('…');
        }
        println!(
            "{index:3}  x {:6.1}..{:6.1}  y {:6.1}..{:6.1}  строк {:2}  {preview:?}",
            block.bbox.left,
            block.bbox.right,
            block.bbox.bottom,
            block.bbox.top,
            block.lines.len(),
        );
    }
    Ok(())
}
