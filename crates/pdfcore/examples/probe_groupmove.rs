//! Повтор переноса группы на настоящем файле — для охоты на отказ.
//!
//!     cargo run -p pdfcore --example probe_groupmove -- book.pdf 2

use anyhow::{Result, bail};
use pdfcore::geom::Rect;
use pdfcore::{BlockEdit, RenderEvent, Renderer};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_groupmove <файл.pdf> <страница с нуля>");
    };
    let page: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&path, tx)?;
    renderer.request_blocks(page);
    let blocks = loop {
        match rx.recv()? {
            RenderEvent::Blocks { blocks, .. } => break blocks,
            RenderEvent::Failed { message, .. } => bail!("блоки не разобрались: {message}"),
            _ => continue,
        }
    };
    println!("блоков: {}", blocks.len());
    for (i, b) in blocks.iter().enumerate().take(6) {
        println!(
            "{i}: y {:.1}..{:.1}  {:?}",
            b.bbox.bottom,
            b.bbox.top,
            b.text().chars().take(30).collect::<String>()
        );
    }

    let picked: Vec<usize> = args.map(|a| a.parse().unwrap()).collect::<Vec<usize>>();
    let picked = if picked.is_empty() {
        vec![1, 2]
    } else {
        picked
    };
    let (dx, dy) = (93.1, -233.1);
    let edits: Vec<BlockEdit> = picked
        .iter()
        .map(|i| &blocks[*i])
        .map(|block| {
            let b = block.bbox;
            BlockEdit::Transform(pdfcore::BlockTransform {
                page_number: page + 1,
                bbox: b,
                target: Rect::new(b.left + dx, b.bottom + dy, b.right + dx, b.top + dy),
                rotation: block.rotation,
                color: None,
                owner: block.mark,
            })
        })
        .collect();
    renderer.apply_edits(edits);
    loop {
        match rx.recv()? {
            RenderEvent::Edited { .. } => {
                println!("перенос прошёл");
                return Ok(());
            }
            RenderEvent::Failed { message, .. } => bail!("ОТКАЗ: {message}"),
            _ => continue,
        }
    }
}
