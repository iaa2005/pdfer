//! Какой именно шаг правки портит страницу.
//!
//!     cargo run -p pdfcore --example probe_damage -- book.pdf [страница]
//!
//! Каждый вариант начинается с заново открытого документа, выполняет ровно
//! один шаг из `edit::apply_block_edit`, сохраняет и сравнивает отрисовку с
//! исходной. Виноват тот шаг, после которого пиксели разошлись.

use anyhow::{Result, bail};
use pdfium_render::prelude::*;

fn render(document: &PdfDocument<'_>, page: PdfPageIndex) -> Result<(u32, u32, Vec<u8>)> {
    let page = document.pages().get(page)?;
    let config = PdfRenderConfig::new()
        .scale_page_by_factor(1.0)
        .set_reverse_byte_order(false);
    let bitmap = page.render_with_config(&config)?;
    Ok((
        bitmap.width() as u32,
        bitmap.height() as u32,
        bitmap.as_raw_bytes(),
    ))
}

fn diff(a: &(u32, u32, Vec<u8>), b: &(u32, u32, Vec<u8>)) -> String {
    if a.0 != b.0 || a.1 != b.1 {
        return "размеры разошлись".to_owned();
    }
    let differing =
        a.2.chunks_exact(4)
            .zip(b.2.chunks_exact(4))
            .filter(|(x, y)| x != y)
            .count();
    let total = (a.0 * a.1) as usize;
    let percent = differing as f64 * 100.0 / total as f64;
    if percent < 0.01 {
        format!("без изменений ({differing} px)")
    } else {
        format!("ИЗМЕНИЛАСЬ: {differing} px, {percent:.2}%")
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        bail!("использование: probe_damage <файл.pdf> [страница]")
    };
    let page: PdfPageIndex = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    let pdfium = pdfcore::engine::pdfium()?;
    let baseline = {
        let document = pdfium.load_pdf_from_file(&path, None)?;
        render(&document, page)?
    };
    println!("эталон: {}x{}\n", baseline.0, baseline.1);

    let temp = std::env::temp_dir();

    // A. Только пересборка потока.
    {
        let document = pdfium.load_pdf_from_file(&path, None)?;
        let mut target = document.pages().get(page)?;
        target.regenerate_content()?;
        drop(target);
        let out = temp.join("damage_a.pdf");
        document.save_to_file(&out)?;
        let after = render(&pdfium.load_pdf_from_file(&out, None)?, page)?;
        println!(
            "A. пересборка потока              → {}",
            diff(&baseline, &after)
        );
    }

    // B. Встраивание системного шрифта.
    {
        let mut document = pdfium.load_pdf_from_file(&path, None)?;
        let request = pdfcore::FontRequest::new("Arial", true, false);
        let fonts = pdfcore::system_fonts();
        let face = fonts
            .resolve(&request)
            .expect("Arial должен быть в системе");
        let face_path = face.path.clone();
        document
            .fonts_mut()
            .load_true_type_from_file(&face_path, true)?;
        let mut target = document.pages().get(page)?;
        target.regenerate_content()?;
        drop(target);
        let out = temp.join("damage_b.pdf");
        document.save_to_file(&out)?;
        let after = render(&pdfium.load_pdf_from_file(&out, None)?, page)?;
        println!(
            "B. встраивание шрифта             → {}",
            diff(&baseline, &after)
        );
    }

    // C. Создание и уничтожение временных объектов для измерения ширины.
    {
        let document = pdfium.load_pdf_from_file(&path, None)?;
        let mut target = document.pages().get(page)?;
        let token = {
            let mut found = None;
            for object in target.objects().iter() {
                if let PdfPageObject::Text(text) = &object {
                    found = Some(text.font().token());
                    break;
                }
            }
            found.expect("на странице нет текста")
        };
        for word in ["Погружение", "в", "современный", "ИИ", "проверка", "ширины"]
        {
            let temp_object = PdfPageTextObject::new(&document, word, token, PdfPoints::new(12.0))?;
            let _ = temp_object.bounds();
            drop(temp_object); // здесь зовётся FPDFPageObj_Destroy
        }
        target.regenerate_content()?;
        drop(target);
        let out = temp.join("damage_c.pdf");
        document.save_to_file(&out)?;
        let after = render(&pdfium.load_pdf_from_file(&out, None)?, page)?;
        println!(
            "C. временные объекты для замера   → {}",
            diff(&baseline, &after)
        );
    }

    // D. Удаление одного текстового объекта.
    {
        let document = pdfium.load_pdf_from_file(&path, None)?;
        let mut target = document.pages().get(page)?;
        let index = {
            let mut found = None;
            for (i, object) in target.objects().iter().enumerate() {
                if matches!(object, PdfPageObject::Text(_)) {
                    found = Some(i as PdfPageObjectIndex);
                    break;
                }
            }
            found.expect("на странице нет текста")
        };
        let removed = target.objects_mut().remove_object_at_index(index)?;
        std::mem::forget(removed);
        target.regenerate_content()?;
        drop(target);
        let out = temp.join("damage_d.pdf");
        document.save_to_file(&out)?;
        let after = render(&pdfium.load_pdf_from_file(&out, None)?, page)?;
        println!(
            "D. удаление объекта (+forget)     → {}",
            diff(&baseline, &after)
        );
        println!("   (один блок текста обязан пропасть — это ожидаемо)");
    }

    Ok(())
}
