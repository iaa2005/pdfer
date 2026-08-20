//! Метрики кириллического блока: насколько ширина по Encoder расходится с
//! настоящей шириной строк на странице.
//!
//!     cargo run -p pdfcore --example probe_ru_metrics -- book.pdf 0

use anyhow::{Result, bail};
use pdfcore::extract::extract_page_text;
use pdfcore::{BlockOptions, detect_blocks, stream_edit};
use pdfium_render::prelude::*;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_ru_metrics <файл.pdf> <страница с нуля>");
    };
    let page_index: i32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    let bindings = Pdfium::bind_to_library("vendor/pdfium/bin/pdfium.dll")
        .or_else(|_| Pdfium::bind_to_system_library())?;
    let pdfium = Pdfium::new(bindings);
    let document = pdfium.load_pdf_from_file(&path, None)?;
    let page = document.pages().get(page_index)?;
    let text = extract_page_text(&page)?;
    let blocks = detect_blocks(text.runs, &BlockOptions::default());

    let raw = lopdf::Document::load(&path)?;

    for block in &blocks {
        let sample: String = block.text().chars().take(28).collect();
        let cyrillic = sample
            .chars()
            .any(|ch| ('\u{0400}'..='\u{04FF}').contains(&ch));
        if !cyrillic {
            continue;
        }
        println!("=== блок «{sample}»  bbox_w={:.1}", block.bbox.width());
        let metrics = stream_edit::block_metrics(&raw, page_index as u32 + 1, block.bbox, None)?;
        for (family, encoder) in &metrics {
            for line in &block.lines {
                let line_text = line.text();
                let size = line.dominant_size();
                let real = line.bbox.width();
                let measured = encoder.width(&line_text, size);
                println!(
                    "  [{family}] size={size:.1} «{}…»  real={real:.1} measured={measured:.1} ({:+.1}%)",
                    line_text.chars().take(16).collect::<String>(),
                    (measured / real - 1.0) * 100.0,
                );
            }
        }
    }
    Ok(())
}
