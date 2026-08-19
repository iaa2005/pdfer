//! Пошаговая проба операций правки — ищем, на каком вызове падает pdfium.
//!
//!     cargo run -p pdfcore --example probe_edit -- file.pdf
//!
//! Каждый шаг печатается со сбросом буфера, поэтому при аварийном завершении
//! последняя строка указывает на виновника.

use std::io::Write;

use anyhow::{Result, bail};
use pdfium_render::prelude::*;

macro_rules! step {
    ($($arg:tt)*) => {{
        println!($($arg)*);
        let _ = std::io::stdout().flush();
    }};
}

fn main() -> Result<()> {
    let Some(path) = std::env::args().nth(1) else {
        bail!("использование: probe_edit <файл.pdf>")
    };

    step!("1. инициализация pdfium");
    let pdfium = pdfcore::engine::pdfium()?;

    step!("2. открытие документа");
    let document = pdfium.load_pdf_from_file(&path, None)?;

    step!("3. загрузка страницы 0");
    let mut page = document.pages().get(0)?;

    step!(
        "4. перебор объектов страницы, всего {}",
        page.objects().len()
    );
    let mut first_text_index = None;
    for (index, object) in page.objects().iter().enumerate() {
        if matches!(object, PdfPageObject::Text(_)) {
            first_text_index = Some(index as PdfPageObjectIndex);
            break;
        }
    }
    let Some(index) = first_text_index else {
        bail!("на странице нет текста")
    };
    step!("   первый текстовый объект: {index}");

    step!("5. чтение шрифта и кегля");
    let (token, size) = {
        let object = page.objects().get(index)?;
        let PdfPageObject::Text(text) = &object else {
            bail!("не текст")
        };
        let font = text.font();
        let size = text.unscaled_font_size().value;
        step!("   шрифт: {}, кегль: {size}", font.family());
        (font.token(), size)
    };

    step!("6. создание НЕПРИВЯЗАННОГО текстового объекта");
    let detached = PdfPageTextObject::new(&document, "Hello", token, PdfPoints::new(size))?;
    step!("   создан");

    step!("7. bounds() у непривязанного объекта  <-- подозреваемый");
    match detached.bounds() {
        Ok(b) => step!("   ширина: {}", b.right().value - b.left().value),
        Err(e) => step!("   ошибка: {e}"),
    }
    drop(detached);
    step!("   объект освобождён");

    step!("8. создание объекта, ПРИВЯЗАННОГО к странице");
    let attached = page.objects_mut().create_text_object(
        PdfPoints::new(0.0),
        PdfPoints::new(0.0),
        "Hello",
        token,
        PdfPoints::new(size),
    )?;
    step!("   создан");

    step!("9. bounds() у привязанного объекта");
    match attached.bounds() {
        Ok(b) => step!("   ширина: {}", b.right().value - b.left().value),
        Err(e) => step!("   ошибка: {e}"),
    }

    step!("10. освобождение обёртки перед удалением (иначе двойной release)");
    drop(attached);
    step!("   обёртка отпущена");

    step!("11. удаление временного объекта по индексу");
    let last = page.objects().len() - 1;
    let removed = page.objects_mut().remove_object_at_index(last)?;
    step!("   удалён, объектов осталось {}", page.objects().len());

    step!("12. снятый объект НЕ освобождаем (Drop в pdfium-render валит pdfium)");
    std::mem::forget(removed);
    step!("   обёртка забыта");

    step!("13. удаление ИСХОДНОГО объекта страницы (индекс {index})");
    let removed = page.objects_mut().remove_object_at_index(index)?;
    step!("   удалён, объектов осталось {}", page.objects().len());
    std::mem::forget(removed);
    step!("   обёртка забыта");

    step!("14. regenerate_content");
    page.regenerate_content()?;
    step!("   страница пересобрана");

    step!("15. сохранение результата");
    let out = std::env::temp_dir().join("probe_edit_out.pdf");
    document.save_to_file(&out)?;
    step!("   сохранено: {}", out.display());

    step!("16. переоткрытие сохранённого файла");
    let check = pdfium.load_pdf_from_file(&out, None)?;
    let page0 = check.pages().get(0)?;
    step!("   объектов на странице: {}", page0.objects().len());

    step!("\nвсе шаги пройдены");
    Ok(())
}
