//! Правка «туда-сюда» не должна разъедать текст.
//!
//!     cargo run -p pdfcore --release --example probe_ligatures -- book.pdf [страница]
//!
//! Порча выглядела так: после пары смен кегля «artificial» превращался в
//! «artiffiffiiicial» — буква «f» записывалась глифом лигатуры «ﬃ», а тот при
//! следующем чтении разворачивался обратно в три буквы, и текст рос лавиной.
//!
//! Проба гоняет самый жёсткий круг: перенабрать абзац его же текстом, извлечь
//! текст заново, перенабрать снова — пять раз подряд. Текст обязан сойтись с
//! исходным после каждого круга.

use anyhow::{Result, bail};
use lopdf::Document;
use pdfcore::stream_edit::{BlockRewrite, rewrite_block};
use pdfcore::{BlockOptions, RenderEvent, Renderer, detect_blocks};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_ligatures <файл.pdf> [страница]")
    };
    let path = std::path::PathBuf::from(path);
    let page: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    // Исходный абзац — через обычный конвейер.
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

    let target = blocks
        .iter()
        .filter(|b| b.text().contains('f'))
        .max_by_key(|b| b.text().len())
        .ok_or_else(|| anyhow::anyhow!("на странице нет абзаца с буквой f"))?;
    let bbox = target.bbox;
    let original = target.text();
    println!(
        "абзац ({} знаков): {:?}…\n",
        original.len(),
        &original[..original.len().min(60)]
    );

    let mut document = Document::load(&path)?;
    let mut text = original.clone();
    let pdfium = pdfcore::engine::pdfium()?;

    for round in 1..=5 {
        rewrite_block(
            &mut document,
            &BlockRewrite::text_only(page + 1, bbox, text.clone()),
        )?;

        // Извлекаем текст так же, как это делает выделение блока в редакторе.
        let mut bytes = Vec::new();
        document.save_to(&mut bytes)?;
        let reloaded = pdfium.load_pdf_from_byte_vec(bytes, None)?;
        let pdf_page = reloaded.pages().get(page as i32)?;
        let extracted = pdfcore::extract::extract_page_text(&pdf_page)?;
        let reblocked = detect_blocks(extracted.runs, &BlockOptions::default());
        let read_back = reblocked
            .iter()
            .filter(|b| b.bbox.intersects(&bbox.inflate(3.0)))
            .max_by_key(|b| b.text().len())
            .map(|b| b.text())
            .unwrap_or_default();

        let same = read_back == original;
        println!(
            "круг {round}: {} знаков, {}",
            read_back.len(),
            if same {
                "совпадает с исходным"
            } else {
                "РАЗОШЁЛСЯ"
            }
        );
        if !same {
            let diff_at = read_back
                .chars()
                .zip(original.chars())
                .position(|(a, b)| a != b)
                .unwrap_or(original.len().min(read_back.len()));
            let from = diff_at.saturating_sub(20);
            let got: String = read_back.chars().skip(from).take(60).collect();
            let want: String = original.chars().skip(from).take(60).collect();
            println!("  стало: …{got}…");
            println!("  было:  …{want}…");
            bail!("текст разъехался на круге {round}");
        }
        text = read_back;
    }

    println!("\nпять кругов подряд — текст байт в байт тот же");
    Ok(())
}
