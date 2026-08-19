//! Служебный дамп меток блоков: trailer-флаг, метки, операторы BDC/EMC.
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap();
    let doc = lopdf::Document::load(&path)?;
    println!(
        "trailer has flag: {:?}",
        doc.trailer.get(b"PdfEdMarked").is_ok()
    );
    let marks = pdfcore::stream_edit::page_marks(&doc, 1)?;
    println!("marks: {marks:?}");
    let page_id = doc.page_iter().next().unwrap();
    let content = doc.get_page_content(page_id);
    let parsed = lopdf::content::Content::decode(&content)?;
    for op in &parsed.operations {
        if matches!(op.operator.as_str(), "BDC" | "BMC" | "EMC") {
            println!("op: {} {:?}", op.operator, op.operands);
        }
    }

    // Путь повёрнутого варианта: сколько ранов и что за блоки получаются.
    for variant in pdfcore::stream_edit::mark_variants(&doc, 1)? {
        let mut bytes = Vec::new();
        let mut vdoc = variant.document;
        vdoc.save_to(&mut bytes)?;
        let pdfium = pdfcore::engine::pdfium()?;
        let loaded = pdfium.load_pdf_from_byte_vec(bytes, None)?;
        let page = loaded.pages().get(0)?;
        let text = pdfcore::extract::extract_page_text_with(&page, true)?;
        println!(
            "variant owner={:?} rotation={} runs={} rotated_skipped={}",
            variant.owner,
            variant.rotation,
            text.runs.len(),
            text.rotated_chars
        );
        for run in text.runs.iter().take(6) {
            println!("  run {:?} origin={:?}", run.text, run.origin);
        }

        if variant.rotation.abs() > 0.5 {
            // Повторяем распрямление из blocks_by_owner и смотрим на детект.
            let whole = pdfcore::geom::union_all(text.runs.iter().map(|run| &run.bbox)).unwrap();
            let centre = (whole.center_x(), whole.center_y());
            let upright: Vec<pdfcore::model::TextRun> = text
                .runs
                .into_iter()
                .map(|mut run| {
                    run.origin =
                        pdfcore::stream_edit::rotate_about(run.origin, centre, -variant.rotation);
                    let turned = pdfcore::stream_edit::rotate_about(
                        (run.bbox.center_x(), run.bbox.center_y()),
                        centre,
                        -variant.rotation,
                    );
                    let (w, h) = (run.bbox.width(), run.bbox.height());
                    run.bbox = pdfcore::geom::Rect::new(
                        turned.0 - w / 2.0,
                        turned.1 - h / 2.0,
                        turned.0 + w / 2.0,
                        turned.1 + h / 2.0,
                    );
                    run
                })
                .collect();
            for run in upright.iter().take(6) {
                println!(
                    "  upright {:?} origin={:?} size={}",
                    run.text, run.origin, run.style.size
                );
            }
            let blocks = pdfcore::detect_blocks(upright, &pdfcore::BlockOptions::default());
            println!("  detected {} blocks", blocks.len());
            for block in &blocks {
                println!("    text: {:?}", block.text());
            }
        }
    }
    Ok(())
}
