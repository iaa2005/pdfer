//! Текст внутри Form XObject: видит ли его правка.
//!
//!     cargo run -p pdfcore --example probe_xobject
//!
//! Собирает страницу, у которой весь текст лежит не в потоке страницы, а в
//! форме, вызванной оператором `Do`, — так делают Word и генераторы справок.
//! Печатает, что об этой странице думает pdfium и что находит наш разбор
//! потока содержимого.

use anyhow::Result;
use lopdf::content::{Content, Operation};
use lopdf::{Object, Stream, dictionary};
use pdfcore::geom::Rect;
use pdfcore::stream_edit::block_font;

fn main() -> Result<()> {
    let mut doc = lopdf::Document::with_version("1.7");

    let font = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });

    // Текст живёт в форме, а не на странице.
    let inner = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![72.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("Справка о доходах")]),
            Operation::new("ET", vec![]),
        ],
    };
    let form = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        },
        inner.encode()?,
    ));

    // Поток самой страницы только вызывает форму.
    let page_content = Content {
        operations: vec![Operation::new("Do", vec!["Fm0".into()])],
    };
    let contents = doc.add_object(Stream::new(dictionary! {}, page_content.encode()?));

    let pages_id = doc.new_object_id();
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        "Contents" => contents,
        "Resources" => dictionary! {
            "XObject" => dictionary! { "Fm0" => form },
            "Font" => dictionary! { "F1" => font },
        },
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);

    let path = std::env::temp_dir().join("pdfer_xobject.pdf");
    doc.save(&path)?;
    println!("страница собрана: {}", path.display());

    // Что видит pdfium — тот же разбор, каким пользуется редактор.
    let bindings = pdfium_render::prelude::Pdfium::bind_to_library("vendor/pdfium/bin/pdfium.dll")
        .or_else(|_| pdfium_render::prelude::Pdfium::bind_to_system_library())?;
    let pdfium = pdfium_render::prelude::Pdfium::new(bindings);
    let loaded = pdfium.load_pdf_from_file(&path, None)?;
    let page = loaded.pages().get(0)?;
    let text = pdfcore::extract::extract_page_text(&page)?;
    let blocks = pdfcore::detect_blocks(text.runs, &pdfcore::BlockOptions::default());
    println!("pdfium нашёл блоков: {}", blocks.len());
    for block in &blocks {
        println!("  «{}»  bbox {:?}", block.text(), block.bbox);
    }

    // А что находит разбор потока содержимого — тот, что делает правку.
    let bbox = blocks
        .first()
        .map(|b| b.bbox)
        .unwrap_or(Rect::new(70.0, 690.0, 300.0, 715.0));
    let reopened = lopdf::Document::load(&path)?;
    match block_font(&reopened, 1, bbox, None) {
        Ok(font) => println!("правка видит текст: шрифт {}", font.base_font),
        Err(e) => println!("правка НЕ видит текст: {e:#}"),
    }
    Ok(())
}
