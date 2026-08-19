//! Проба порядка каналов на настоящей странице.
//!
//!     cargo run -p pdfcore --example probe_color -- book.pdf 11
//!
//! Сохраняет два PNG из одних и тех же байтов: в одном они трактуются как
//! RGBA, в другом — как BGRA. Какой из файлов выглядит правильно, тот порядок
//! pdfium и отдаёт.

use anyhow::{Result, bail};
use image::RgbaImage;
use pdfcore::geom::Rotation;
use pdfcore::{RenderEvent, Renderer, TileKey, ZoomBucket};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_color <файл.pdf> [страница]")
    };
    let page: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    let (tx, rx) = flume::unbounded();
    let (renderer, info) = Renderer::open(&path, tx)?;
    if page >= info.page_count {
        bail!("в документе нет страницы {page}");
    }

    renderer.set_wanted_tiles(vec![TileKey {
        page,
        zoom: ZoomBucket::from_scale(0.5),
        rotation: Rotation::None,
    }]);

    let bitmap = loop {
        match rx.recv_timeout(std::time::Duration::from_secs(60))? {
            RenderEvent::Tile { bitmap, .. } => break bitmap,
            RenderEvent::Failed { message, .. } => bail!("рендер не удался: {message}"),
            _ => continue,
        }
    };

    // Точка в середине страницы — там, где на фотографии дерево.
    let (w, h) = (bitmap.width, bitmap.height);
    let offset = (((h / 2) * w + w / 2) * 4) as usize;
    let px = &bitmap.pixels[offset..offset + 4];
    println!("страница {page}, растр {w}x{h}");
    println!(
        "байты центрального пикселя: [{}, {}, {}, {}]",
        px[0], px[1], px[2], px[3]
    );
    println!("  если это RGBA → цвет r={} g={} b={}", px[0], px[1], px[2]);
    println!("  если это BGRA → цвет r={} g={} b={}", px[2], px[1], px[0]);

    let dir = std::env::temp_dir();

    // Вариант 1: байты как есть, трактуются как RGBA.
    let as_is = RgbaImage::from_raw(w, h, bitmap.pixels.clone())
        .ok_or_else(|| anyhow::anyhow!("размер буфера не сошёлся"))?;
    let path_as_is = dir.join("probe_as_rgba.png");
    as_is.save(&path_as_is)?;

    // Вариант 2: с перестановкой R и B, то есть исходник считаем BGRA.
    let mut swapped_bytes = bitmap.pixels.clone();
    for pixel in swapped_bytes.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let swapped = RgbaImage::from_raw(w, h, swapped_bytes)
        .ok_or_else(|| anyhow::anyhow!("размер буфера не сошёлся"))?;
    let path_swapped = dir.join("probe_swapped.png");
    swapped.save(&path_swapped)?;

    println!("\n{}", path_as_is.display());
    println!("{}", path_swapped.display());
    Ok(())
}
