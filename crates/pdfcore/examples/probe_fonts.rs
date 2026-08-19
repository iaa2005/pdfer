//! Что можно сделать с каждым абзацем страницы и почему.
//!
//!     cargo run -p pdfcore --release --example probe_fonts -- book.pdf [страница]
//!
//! Отвечает на три вопроса разом: каким шрифтом набран абзац, есть ли этот
//! шрифт под рукой — встроенным в документ или установленным в системе, — и
//! возьмётся ли редактор его перенабрать. Отказ в перенаборе не поломка: он
//! означает, что в рамке лежит текст разного оформления и выкладывать его
//! заново одним шрифтом нельзя. Такой абзац по-прежнему можно двигать,
//! поворачивать и перекрашивать — это делается без перенабора.

use anyhow::{Result, bail};
use lopdf::Document;
use pdfcore::stream_edit::{BlockRewrite, block_font, rewrite_block};
use pdfcore::{RenderEvent, Renderer};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_fonts <файл.pdf> [страница]")
    };
    let path = std::path::PathBuf::from(path);
    let page: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

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
    drop(renderer);

    let document = Document::load(&path)?;
    println!("страница {}, абзацев: {}\n", page + 1, blocks.len());

    for (index, block) in blocks.iter().enumerate() {
        let text = block.text();
        let head: String = text.chars().take(46).collect();
        println!("[{index}] {head:?}");

        let font = match block_font(&document, page + 1, block.bbox, None) {
            Ok(font) => font,
            Err(e) => {
                println!("     оформление не прочитано: {e}\n");
                continue;
            }
        };

        let installed = pdfcore::system_fonts().has_family(&font.base_font);
        let family = pdfcore::fonts::system_fonts()
            .find_family(&font.base_font)
            .map(|name| name.to_owned());

        println!(
            "     шрифт: {} · кегль {:.2} · встроен: {} · в системе: {}",
            font.base_font,
            font.size,
            yes_no(font.program.is_some()),
            match (&family, installed) {
                (Some(name), _) => name.clone(),
                (None, true) => "да".to_owned(),
                (None, false) => "нет".to_owned(),
            }
        );

        // Чего во встроенном подмножестве не хватает для собственного же
        // текста. Пусто — значит текст можно перенабрать как есть.
        if let Some(metrics) = &font.metrics {
            let missing = metrics.missing(&text);
            if !missing.is_empty() {
                let list: String = missing.iter().collect();
                println!("     нет глифов для: {list:?}");
            }
        }

        // Самая честная проверка: пробуем перенабрать тем же текстом на копии.
        let probe = BlockRewrite::text_only(page + 1, block.bbox, text.clone());
        match rewrite_block(&mut document.clone(), &probe) {
            Ok(_) => println!("     перенабор: можно"),
            Err(e) => println!("     перенабор: нельзя — {e}"),
        }
        println!();
    }

    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "да" } else { "нет" }
}
