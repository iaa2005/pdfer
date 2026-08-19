//! Сколько стоит показать правку.
//!
//!     cargo run -p pdfcore --release --example probe_latency -- book.pdf [страница]
//!
//! Правка в стиле Acrobat означает, что страница перерисовывается прямо по
//! ходу набора. Возможно это или нет — решают четыре числа: разбор структуры,
//! переписывание потока, сериализация документа и его повторное чтение
//! растеризатором. Первое делается один раз, остальные — на каждое обновление.

use std::time::Instant;

use anyhow::{Result, bail};
use lopdf::Document;
use pdfcore::stream_edit::{BlockRewrite, rewrite_block};
use pdfcore::{RenderEvent, Renderer};
use pdfium_render::prelude::*;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_latency <файл.pdf> [страница]")
    };
    let path = std::path::PathBuf::from(path);
    let page: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    // Рамку блока берём обычным путём.
    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&path, tx)?;
    renderer.request_blocks(page);
    let blocks = loop {
        match rx.recv_timeout(std::time::Duration::from_secs(60))? {
            RenderEvent::Blocks { blocks, .. } => break blocks,
            RenderEvent::Failed { message, .. } => bail!("разбор не удался: {message}"),
            _ => continue,
        }
    };
    let target = blocks
        .iter()
        .max_by_key(|b| b.text().len())
        .expect("на странице нет текста");
    let bbox = target.bbox;
    let original = target.text();
    drop(renderer);

    let size = std::fs::metadata(&path)?.len();
    println!(
        "файл: {:.1} МБ, страниц-блок: {:?}\n",
        size as f64 / (1024.0 * 1024.0),
        &original[..original.len().min(40)]
    );

    let started = Instant::now();
    let mut document = Document::load(&path)?;
    println!(
        "разбор структуры (однократно):  {:>8.1} мс",
        started.elapsed().as_secs_f64() * 1000.0
    );

    let pdfium = pdfcore::engine::pdfium()?;

    // Три круга: первый прогревает кэши, дальше видно установившееся время.
    for round in 1..=3 {
        // Текст тот же: важна стоимость перезаписи, а не содержание.
        let text = original.clone();

        let started = Instant::now();
        rewrite_block(
            &mut document,
            &BlockRewrite::text_only(page + 1, bbox, &text),
        )?;
        let rewrite = started.elapsed();

        let started = Instant::now();
        let mut bytes = Vec::new();
        document.save_to(&mut bytes)?;
        let serialise = started.elapsed();

        let started = Instant::now();
        let reloaded = pdfium.load_pdf_from_byte_vec(bytes, None)?;
        let reload = started.elapsed();

        let started = Instant::now();
        let target_page = reloaded.pages().get(page as PdfPageIndex)?;
        let config = PdfRenderConfig::new()
            .scale_page_by_factor(1.0)
            .set_reverse_byte_order(false);
        let _ = target_page.render_with_config(&config)?;
        let render = started.elapsed();

        let total = rewrite + serialise + reload + render;
        println!(
            "круг {round}: поток {:>6.1} · сериализация {:>7.1} · чтение {:>6.1} · растр {:>6.1}  =  {:>7.1} мс",
            rewrite.as_secs_f64() * 1000.0,
            serialise.as_secs_f64() * 1000.0,
            reload.as_secs_f64() * 1000.0,
            render.as_secs_f64() * 1000.0,
            total.as_secs_f64() * 1000.0,
        );
    }

    // Вариант с отдельным одностраничным документом: собирается один раз при
    // выделении блока, а дальше сериализуется только он.
    println!("\n— одностраничный документ для показа —");

    let started = Instant::now();
    let mut preview = pdfcore::stream_edit::extract_page(&document, page + 1)?;
    let build = started.elapsed();

    let mut bytes = Vec::new();
    preview.save_to(&mut bytes)?;
    println!(
        "сборка (однократно): {:.0} мс,  размер {:.2} МБ",
        build.as_secs_f64() * 1000.0,
        bytes.len() as f64 / (1024.0 * 1024.0)
    );

    for round in 1..=3 {
        let started = Instant::now();
        rewrite_block(&mut preview, &BlockRewrite::text_only(1, bbox, &original))?;
        let rewrite = started.elapsed();

        let started = Instant::now();
        let mut bytes = Vec::new();
        preview.save_to(&mut bytes)?;
        let serialise = started.elapsed();

        let started = Instant::now();
        let reloaded = pdfium.load_pdf_from_byte_vec(bytes, None)?;
        let target_page = reloaded.pages().get(0)?;
        let config = PdfRenderConfig::new()
            .scale_page_by_factor(1.0)
            .set_reverse_byte_order(false);
        let _ = target_page.render_with_config(&config)?;
        let show = started.elapsed();

        println!(
            "круг {round}: поток {:>5.1} · сериализация {:>6.1} · чтение и растр {:>6.1}  =  {:>6.1} мс",
            rewrite.as_secs_f64() * 1000.0,
            serialise.as_secs_f64() * 1000.0,
            show.as_secs_f64() * 1000.0,
            (rewrite + serialise + show).as_secs_f64() * 1000.0,
        );
    }

    Ok(())
}
