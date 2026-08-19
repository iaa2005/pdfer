//! Диагностика ядра на настоящем документе — без UI.
//!
//!     cargo run -p pdfcore --example inspect -- book.pdf [страница]
//!
//! Печатает геометрию документа, замеряет скорость растеризации и показывает,
//! как разобрались текстовые блоки указанной страницы. Это основной способ
//! проверить пороги [`pdfcore::BlockOptions`] на своей вёрстке: если абзацы
//! склеиваются или, наоборот, рассыпаются, крутить надо именно их.

use std::time::Instant;

use anyhow::{Result, bail};
use pdfcore::geom::Rotation;
use pdfcore::{RenderEvent, Renderer, TileKey, ZoomBucket};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: inspect <файл.pdf> [номер страницы, с нуля]");
    };
    let page: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    let (tx, rx) = flume::unbounded();

    let started = Instant::now();
    let (renderer, info) = Renderer::open(&path, tx)?;
    println!("документ:   {path}");
    println!("страниц:    {}", info.page_count);
    println!("геометрия:  прочитана за {:?}", started.elapsed());

    if let Some(size) = info.size(0) {
        println!("стр. 1:     {:.1} x {:.1} pt", size.width, size.height);
    }
    if page >= info.page_count {
        bail!("в документе нет страницы {page}");
    }

    // Растеризация окна вокруг запрошенной страницы — так же, как это делал бы
    // вьюпорт при прокрутке.
    let window: Vec<TileKey> = (page.saturating_sub(2)..(page + 3).min(info.page_count))
        .map(|p| TileKey {
            page: p,
            zoom: ZoomBucket::from_scale(1.0),
            rotation: Rotation::None,
        })
        .collect();
    let wanted = window.len();
    renderer.set_wanted_tiles(window);
    renderer.request_blocks(page);

    let started = Instant::now();
    let mut rendered = 0usize;
    let mut blocks_shown = false;

    while (rendered < wanted || !blocks_shown)
        && let Ok(event) = rx.recv_timeout(std::time::Duration::from_secs(60))
    {
        match event {
            RenderEvent::Tile { key, bitmap } => {
                rendered += 1;
                println!(
                    "растр:      стр. {} → {}x{} px, {:.1} МБ",
                    key.page,
                    bitmap.width,
                    bitmap.height,
                    bitmap.pixels.len() as f64 / (1024.0 * 1024.0)
                );
            }
            RenderEvent::Blocks { page, blocks } => {
                blocks_shown = true;
                println!("\nблоки страницы {page}: {}\n", blocks.len());
                for (i, block) in blocks.iter().enumerate() {
                    let style = block.dominant_style();
                    let leading = block
                        .leading()
                        .map(|l| format!("{l:.1}"))
                        .unwrap_or_else(|| "—".to_owned());
                    println!(
                        "[{i}] {} pt {}{}  строк: {}  интерлиньяж: {}  выключка: {:?}",
                        style.size,
                        style.clean_family(),
                        if style.is_bold() { " bold" } else { "" },
                        block.lines.len(),
                        leading,
                        block.align,
                    );
                    println!("    {}", preview(&block.text()));
                    // С аргументом --lines печатаем каждую строку с рамкой:
                    // так видно, что именно склеилось и почему.
                    if std::env::args().any(|a| a == "--lines") {
                        for line in &block.lines {
                            println!(
                                "      x {:.0}..{:.0}  y {:.0}  «{}»",
                                line.bbox.left,
                                line.bbox.right,
                                line.baseline,
                                preview(&line.text())
                            );
                            // С --runs дополнительно печатаем сами раны:
                            // видно, где проходят границы и какие зазоры.
                            if std::env::args().any(|a| a == "--runs") {
                                let mut previous_right: Option<f32> = None;
                                for run in &line.runs {
                                    let gap = previous_right
                                        .map(|right| run.bbox.left - right)
                                        .unwrap_or(0.0);
                                    println!(
                                        "         зазор {:>6.1}  x {:.0}..{:.0}  {:.1} pt  «{}»",
                                        gap,
                                        run.bbox.left,
                                        run.bbox.right,
                                        run.style.size,
                                        run.text
                                    );
                                    previous_right = Some(run.bbox.right);
                                }
                            }
                        }
                    }
                }
            }
            RenderEvent::Failed { page, message } => {
                match page {
                    Some(page) => println!("ошибка:     стр. {page} — {message}"),
                    None => println!("ошибка:     {message}"),
                }
                rendered += 1;
            }
            other => println!("событие:    {other:?}"),
        }
    }

    if rendered > 0 {
        println!(
            "\nрастеризация: {rendered} стр. за {:?} ({:.1} мс/стр.)",
            started.elapsed(),
            started.elapsed().as_secs_f64() * 1000.0 / rendered as f64
        );
    }
    Ok(())
}

fn preview(text: &str) -> String {
    const LIMIT: usize = 96;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= LIMIT {
        return flat;
    }
    let cut: String = flat.chars().take(LIMIT).collect();
    format!("{cut}…")
}
