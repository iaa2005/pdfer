//! Сквозные тесты поверх настоящего pdfium.
//!
//! PDF для них генерируется здесь же через lopdf — так тесты не зависят от
//! внешних файлов, а ожидаемая геометрия текста известна точно.
//!
//! Все тесты сериализованы общим замком: pdfium не потокобезопасен, а
//! `cargo test` по умолчанию гоняет тесты параллельно. Без замка два потока
//! рендера обращались бы к библиотеке одновременно. Отравление замка при
//! падении одного теста игнорируется: иначе одна неудача рушила бы весь
//! набор и прятала настоящую причину.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use pdfcore::geom::{Rect, Rotation};
use pdfcore::{BlockEdit, BlockRewrite, RenderEvent, Renderer, TileKey, ZoomBucket};

static PDFIUM_LOCK: Mutex<()> = Mutex::new(());

const PAGE_WIDTH: f32 = 595.0;
const PAGE_HEIGHT: f32 = 842.0;

struct Line {
    text: &'static str,
    x: f32,
    y: f32,
    size: f32,
}

/// Заголовок кеглем 20 и два абзаца кеглем 11 с разной отбивкой.
/// Разрыв между абзацами (41 pt) заметно больше интерлиньяжа (13 pt).
fn sample_lines() -> Vec<Line> {
    let mut lines = vec![Line {
        text: "Illustrierende Pruefungsaufgaben",
        x: 72.0,
        y: 780.0,
        size: 20.0,
    }];
    for (i, text) in [
        "Bei der Bearbeitung der Aufgaben duerfen",
        "folgende Hilfsmittel verwendet werden,",
        "darunter ein Taschenrechner sowie eine",
        "Merkhilfe fuer das Gymnasium.",
    ]
    .iter()
    .enumerate()
    {
        lines.push(Line {
            text,
            x: 72.0,
            y: 740.0 - 13.0 * i as f32,
            size: 11.0,
        });
    }
    for (i, text) in [
        "Die Hilfsmittel duerfen keine Kommentare",
        "enthalten; Hervorhebungen und Verweisungen",
        "sind gestattet.",
    ]
    .iter()
    .enumerate()
    {
        lines.push(Line {
            text,
            x: 72.0,
            y: 660.0 - 13.0 * i as f32,
            size: 11.0,
        });
    }
    lines
}

fn build_pdf(path: &Path, page_count: usize, lines: &[Line]) -> Result<()> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let mut operations = vec![Operation::new("BT", vec![])];
    for line in lines {
        operations.push(Operation::new(
            "Tf",
            vec![Object::Name(b"F1".to_vec()), Object::Real(line.size)],
        ));
        // Tm задаёт положение абсолютно, в отличие от относительного Td.
        operations.push(Operation::new(
            "Tm",
            vec![
                Object::Real(1.0),
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(1.0),
                Object::Real(line.x),
                Object::Real(line.y),
            ],
        ));
        operations.push(Operation::new(
            "Tj",
            vec![Object::string_literal(line.text)],
        ));
    }
    operations.push(Operation::new("ET", vec![]));

    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        Content { operations }.encode()?,
    ));

    let page_ids: Vec<Object> = (0..page_count)
        .map(|_| {
            doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "MediaBox" => vec![
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(PAGE_WIDTH),
                    Object::Real(PAGE_HEIGHT),
                ],
            })
            .into()
        })
        .collect();

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids,
            "Count" => page_count as i64,
            "Resources" => resources_id,
        }),
    );

    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    doc.save(path)?;
    Ok(())
}

/// PDF с одной заливкой чистого красного в левом нижнем углу страницы.
/// Красный выбран намеренно: он различает RGBA и BGRA однозначно, тогда как
/// серый или белый выглядят одинаково при любом порядке каналов.
fn build_red_square_pdf(path: &Path) -> Result<()> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let operations = vec![
        Operation::new(
            "rg",
            vec![Object::Real(1.0), Object::Real(0.0), Object::Real(0.0)],
        ),
        Operation::new(
            "re",
            vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(200.0),
                Object::Real(200.0),
            ],
        ),
        Operation::new("f", vec![]),
    ];
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        Content { operations }.encode()?,
    ));

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "MediaBox" => vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(PAGE_WIDTH),
            Object::Real(PAGE_HEIGHT),
        ],
    });

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1_i64,
            "Resources" => dictionary! {},
        }),
    );

    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    doc.save(path)?;
    Ok(())
}

fn fixture(name: &str, page_count: usize) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(name);
    build_pdf(&path, page_count, &sample_lines())?;
    Ok(path)
}

#[test]
fn document_geometry_is_read_without_rendering() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = fixture("pdfcore_geometry.pdf", 700)?;

    let (tx, _rx) = flume::unbounded();
    let started = std::time::Instant::now();
    let (_renderer, info) = Renderer::open(&path, tx)?;
    let elapsed = started.elapsed();

    assert_eq!(info.page_count, 700);
    let first = info.size(0).expect("размер первой страницы");
    assert!(
        (first.width - PAGE_WIDTH).abs() < 0.5,
        "ширина {}",
        first.width
    );
    assert!(
        (first.height - PAGE_HEIGHT).abs() < 0.5,
        "высота {}",
        first.height
    );

    // Смысл всей затеи: полная геометрия 700 страниц доступна сразу, до
    // растеризации хоть чего-нибудь. Порог щедрый — это защита от регрессии
    // вида «размеры вдруг стали читаться через загрузку страниц», а не бенчмарк.
    assert!(
        elapsed.as_millis() < 2000,
        "чтение геометрии заняло {elapsed:?}"
    );
    Ok(())
}

#[test]
fn tile_is_rendered_at_the_requested_zoom_bucket() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = fixture("pdfcore_render.pdf", 3)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&path, tx)?;

    let key = TileKey {
        page: 1,
        zoom: ZoomBucket::from_scale(2.0),
        rotation: Rotation::None,
    };
    renderer.set_wanted_tiles(vec![key]);

    match rx.recv_timeout(std::time::Duration::from_secs(30))? {
        RenderEvent::Tile { key: got, bitmap } => {
            assert_eq!(got, key);
            let expected_w = (PAGE_WIDTH * 2.0).round() as u32;
            assert!(
                bitmap.width.abs_diff(expected_w) <= 2,
                "ширина растра {}",
                bitmap.width
            );
            assert_eq!(
                bitmap.pixels.len(),
                (bitmap.width * bitmap.height * 4) as usize
            );
            // Страница с текстом не может быть полностью однотонной.
            let first = &bitmap.pixels[..4];
            assert!(
                bitmap.pixels.chunks_exact(4).any(|p| p != first),
                "растр пустой"
            );
        }
        other => panic!("ожидался тайл, получено {other:?}"),
    }
    Ok(())
}

/// Ждёт первое событие нужного вида, пропуская попутные тайлы.
fn next_event(rx: &flume::Receiver<RenderEvent>) -> Result<RenderEvent> {
    Ok(rx.recv_timeout(std::time::Duration::from_secs(30))?)
}

#[test]
fn replacing_block_text_survives_save_and_reopen() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_edit_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_edit_out.pdf");
    let _ = std::fs::remove_file(&saved);

    // Открываем, находим второй блок — первый абзац основного текста.
    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);

    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let target = blocks[1].clone();
    let original_bbox = target.bbox;
    assert!(target.text().starts_with("Bei der Bearbeitung"));

    // Заменяем текст на заметно более длинный: перевёрстка обязана добавить
    // строк, а не вылезти за правое поле.
    let new_text = "Die Aufgaben wurden vollstaendig neu formuliert und deutlich \
                    ausfuehrlicher beschrieben, damit die Umbrueche im Text sichtbar \
                    nachgerechnet werden koennen.";
    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        line_height: Some(13.0),
        ..BlockRewrite::text_only(1, original_bbox, new_text)
    }));

    let outcome = loop {
        match next_event(&rx)? {
            RenderEvent::Edited { outcome, .. } => break outcome,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    };
    assert!(
        outcome.as_ref().is_some_and(|o| o.cleared_ops > 0),
        "старые операторы показа должны быть опустошены"
    );
    assert!(
        outcome.as_ref().is_some_and(|o| o.created_lines >= 3),
        "длинный текст обязан занять несколько строк"
    );

    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    // Переоткрываем сохранённый файл и сверяем, что в нём.
    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };

    let joined: String = reopened
        .iter()
        .map(|b| b.text())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("vollstaendig neu formuliert"),
        "новый текст не найден после переоткрытия; получено: {joined}"
    );
    assert!(
        !joined.contains("Taschenrechner"),
        "старый текст блока обязан исчезнуть; получено: {joined}"
    );
    // Соседние блоки трогать было нельзя.
    assert!(
        joined.contains("Illustrierende Pruefungsaufgaben"),
        "заголовок пропал"
    );
    assert!(
        joined.contains("Hervorhebungen und Verweisungen"),
        "второй абзац пострадал"
    );

    // Перевёрстка обязана уложиться в исходную ширину блока.
    let edited = reopened
        .iter()
        .find(|b| b.text().contains("vollstaendig neu formuliert"))
        .expect("заменённый блок");
    assert!(
        edited.bbox.width() <= original_bbox.width() + 2.0,
        "текст вылез за правое поле: {} против {}",
        edited.bbox.width(),
        original_bbox.width()
    );
    Ok(())
}

/// Показ по ходу набора обязан показывать правку и при этом ничего не менять.
///
/// Это две разные вещи, и обе существенны. Видно должно быть набранное —
/// иначе правка «прямо в тексте» превращается в обещание. А документ обязан
/// остаться прежним до нажатия «Применить»: пользователь ещё может передумать,
/// и отказ не должен ничего восстанавливать.
#[test]
fn preview_shows_the_typed_text_without_changing_the_document() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_preview_src.pdf", 1)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let bbox = blocks[1].bbox;

    // Растр до показа — эталон нетронутой страницы.
    let before = match next_tile(&renderer, &rx)? {
        Some(tile) => tile,
        None => panic!("страница не отрисовалась"),
    };

    renderer.request_preview(
        BlockEdit::Rewrite(BlockRewrite::text_only(1, bbox, "Vorschau statt Overlay")),
        1.0,
    );
    let preview = loop {
        match next_event(&rx)? {
            RenderEvent::Preview { bitmap, .. } => break bitmap,
            RenderEvent::Failed { message, .. } => panic!("показ не удался: {message}"),
            _ => continue,
        }
    };
    assert_eq!((preview.width, preview.height), (before.0, before.1));
    assert_ne!(
        preview.pixels, before.2,
        "показ обязан отличаться от исходной страницы"
    );

    // А сам документ — нет. Блоки перечитываются из него же.
    renderer.request_blocks(0);
    let after = loop {
        match next_event(&rx)? {
            RenderEvent::Blocks { blocks, .. } => break blocks,
            _ => continue,
        }
    };
    let joined: String = after.iter().map(|b| b.text()).collect::<Vec<_>>().join(" ");
    assert!(
        !joined.contains("Vorschau statt Overlay"),
        "показ просочился в документ; получено: {joined}"
    );
    assert!(
        joined.contains("Bei der Bearbeitung"),
        "исходный текст блока обязан остаться"
    );
    Ok(())
}

/// Передвинутая маркерами рамка уносит текст с собой.
///
/// Стирать и печатать приходится в разных местах: старый текст опознаётся по
/// рамке найденного абзаца, а новый ложится туда, куда её перетащили. Если бы
/// эти два прямоугольника не разделялись, блок было бы невозможно подвинуть —
/// он либо не стёрся бы, либо напечатался на старом месте.
#[test]
fn a_moved_frame_takes_the_text_with_it() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_move_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_move_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let bbox = blocks[1].bbox;

    // Сдвиг вправо и вниз, ширина прежняя.
    let moved = Rect::new(
        bbox.left + 40.0,
        bbox.bottom - 60.0,
        bbox.right + 40.0,
        bbox.top - 60.0,
    );
    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        target: Some(moved),
        ..BlockRewrite::text_only(1, bbox, "Verschobener Absatz")
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let placed = reopened
        .iter()
        .find(|b| b.text().contains("Verschobener Absatz"))
        .expect("перенесённый блок не найден");

    assert!(
        (placed.bbox.left - moved.left).abs() < 2.0,
        "левый край не там: {} вместо {}",
        placed.bbox.left,
        moved.left
    );
    assert!(
        (placed.bbox.top - moved.top).abs() < 4.0,
        "верхний край не там: {} вместо {}",
        placed.bbox.top,
        moved.top
    );
    Ok(())
}

/// Выключка по центру ставит короткую строку посередине рамки.
#[test]
fn centred_text_sits_in_the_middle_of_the_frame() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_align_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_align_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let bbox = blocks[1].bbox;

    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        align: pdfcore::model::Align::Center,
        ..BlockRewrite::text_only(1, bbox, "Mitte")
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let placed = reopened
        .iter()
        .find(|b| b.text().contains("Mitte"))
        .expect("блок не найден");

    // Слово короткое, поэтому при левой выключке оно прижалось бы к левому
    // краю; проверяем именно то, что оно оказалось посередине.
    assert!(
        placed.bbox.left > bbox.left + 20.0,
        "строка осталась у левого края: {} против {}",
        placed.bbox.left,
        bbox.left
    );
    assert!(
        (placed.bbox.center_x() - bbox.center_x()).abs() < 3.0,
        "центр строки {} против центра рамки {}",
        placed.bbox.center_x(),
        bbox.center_x()
    );
    Ok(())
}

/// Блок, надвинутый на соседний, не склеивается с ним.
///
/// В содержимом страницы каждый абзац остаётся своей группой операторов
/// показа, и правка одного не трогает другой, даже когда их рамки пересекаются
/// на бумаге. Проверяется именно это: после наложения оба текста на месте, и ни
/// один не пропал.
#[test]
fn a_block_moved_over_another_does_not_swallow_it() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_overlap_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_overlap_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let first = blocks[1].bbox;
    let second = blocks[2].bbox;
    assert!(
        !first.intersects(&second),
        "в образце абзацы не должны пересекаться"
    );

    // Двигаем первый абзац ровно на второй.
    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        target: Some(second),
        ..BlockRewrite::text_only(1, first, "Aufgeschobener Absatz")
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let joined: String = reopened
        .iter()
        .map(|b| b.text())
        .collect::<Vec<_>>()
        .join(" ");

    assert!(
        joined.contains("Aufgeschobener Absatz"),
        "перенесённый текст пропал: {joined}"
    );
    assert!(
        joined.contains("Hervorhebungen und Verweisungen"),
        "текст, на который наехали, обязан уцелеть; получено: {joined}"
    );
    assert!(
        !joined.contains("Taschenrechner"),
        "старое место первого абзаца обязано опустеть; получено: {joined}"
    );

    // Главное: наложенные абзацы остались РАЗДЕЛЬНЫМИ блоками. Метка в файле
    // даёт перенесённому блоку собственное имя, и разбор страницы разнимает
    // его от текста, на который он надвинут, — вместо одного слипшегося кома.
    let moved_block = reopened
        .iter()
        .find(|b| b.text().contains("Aufgeschobener Absatz"))
        .expect("перенесённый блок не найден");
    assert!(
        !moved_block.text().contains("Hervorhebungen"),
        "блоки слиплись: перенесённый абзац впитал текст под собой — {:?}",
        moved_block.text()
    );
    let underlying = reopened
        .iter()
        .find(|b| b.text().contains("Hervorhebungen und Verweisungen"))
        .expect("нижний абзац не найден");
    assert!(
        !underlying.text().contains("Aufgeschobener"),
        "блоки слиплись в обратную сторону: {:?}",
        underlying.text()
    );
    Ok(())
}

/// Пёстрый абзац перенабирается с сохранением стилей кусков.
///
/// Рамка накрывает заголовок кеглем 20 и абзац кеглем 11. Раньше перенабор
/// такое отклонял — выложить два кегля одним оформлением нельзя. Теперь спаны
/// сами несут стили, вплоть до гарнитуры из документа, и оба куска после
/// правки остаются при своих кеглях.
#[test]
fn a_mixed_block_is_retypeset_with_its_styles_kept() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_mixed_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_mixed_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };

    // Рамка на заголовок и первый абзац разом — внутри два разных кегля.
    let mixed = blocks[0].bbox.union(&blocks[1].bbox);
    let heading_size = blocks[0].dominant_style().size;
    let body_size = blocks[1].dominant_style().size;
    assert!(
        heading_size > body_size + 5.0,
        "в образце обязаны быть два кегля"
    );

    // Спаны несут кегли кусков — как их собирает модель редактора.
    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        spans: vec![
            pdfcore::stream_edit::StyledSpan {
                text: "Neuer Titel".to_owned(),
                size: Some(heading_size),
                ..Default::default()
            },
            pdfcore::stream_edit::StyledSpan {
                text: " und ein ersetzter Absatz danach".to_owned(),
                size: Some(body_size),
                ..Default::default()
            },
        ],
        ..BlockRewrite::text_only(1, mixed, "")
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => {
                panic!("перенабор пёстрого блока отклонён: {message}")
            }
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let joined: String = reopened
        .iter()
        .map(|b| b.text())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("Neuer Titel"),
        "новый заголовок пропал: {joined}"
    );
    assert!(
        joined.contains("ersetzter Absatz"),
        "новый абзац пропал: {joined}"
    );

    // Кегли кусков сохранились: заголовок остался крупным, абзац обычным.
    let title = reopened
        .iter()
        .flat_map(|b| b.lines.iter())
        .flat_map(|l| l.runs.iter())
        .find(|r| r.text.contains("Neuer Titel"))
        .expect("ран заголовка не найден");
    assert!(
        (title.style.size - heading_size).abs() < 1.0,
        "кегль заголовка потерялся: {} против {}",
        title.style.size,
        heading_size
    );
    let body = reopened
        .iter()
        .flat_map(|b| b.lines.iter())
        .flat_map(|l| l.runs.iter())
        .find(|r| r.text.contains("ersetzter"))
        .expect("ран абзаца не найден");
    assert!(
        (body.style.size - body_size).abs() < 1.0,
        "кегль абзаца потерялся: {} против {}",
        body.style.size,
        body_size
    );
    Ok(())
}

/// И перенос без перенабора оставляет наложенные блоки раздельными.
///
/// Перестановка — обычный путь переноса мышью, и метку блока она обязана
/// ставить так же, как перенабор: иначе слипание возвращалось бы ровно в том
/// движении, которым пользуются чаще всего.
#[test]
fn a_transformed_block_keeps_its_identity_over_another() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_transform_overlap_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_transform_overlap_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let first = blocks[1].bbox;
    let second = blocks[2].bbox;

    renderer.apply_edit(BlockEdit::Transform(pdfcore::BlockTransform {
        page_number: 1,
        bbox: first,
        target: second,
        rotation: 0.0,
        color: None,
        owner: None,
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("перестановка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };

    let moved = reopened
        .iter()
        .find(|b| b.text().contains("Bei der Bearbeitung"))
        .expect("перенесённый абзац пропал");
    assert!(
        !moved.text().contains("Hervorhebungen"),
        "блоки слиплись после переноса: {:?}",
        moved.text()
    );
    let underlying = reopened
        .iter()
        .find(|b| b.text().contains("Hervorhebungen und Verweisungen"))
        .expect("нижний абзац пропал");
    assert!(
        !underlying.text().contains("Bei der Bearbeitung"),
        "блоки слиплись в обратную сторону: {:?}",
        underlying.text()
    );
    Ok(())
}

/// Повёрнутый блок остаётся выделяемым и помнит свой угол.
///
/// Повёрнутый текст обычный разбор пропускает — модель абзацев горизонтальна.
/// Раньше это значило, что после поворота блок исчезал из выделения навсегда.
/// Теперь угол хранится в метке блока, и разбор восстанавливает и текст, и
/// угол, и рамку — после сохранения и переоткрытия в том числе.
#[test]
fn a_rotated_block_survives_reopening_with_its_angle() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_rotate_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_rotate_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let bbox = blocks[1].bbox;

    renderer.apply_edit(BlockEdit::Transform(pdfcore::BlockTransform {
        page_number: 1,
        bbox,
        target: bbox,
        rotation: 25.0,
        color: None,
        owner: None,
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("поворот отклонён: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };

    let rotated = reopened
        .iter()
        .find(|b| b.text().contains("Bei der Bearbeitung"))
        .expect("повёрнутый блок пропал из разбора — его нельзя выделить");
    assert!(
        (rotated.rotation - 25.0).abs() < 0.5,
        "угол не восстановился: {}",
        rotated.rotation
    );
    // Рамка повёрнутого текста шире и выше горизонтальной: прямоугольник
    // обнимает наклонённые строки.
    assert!(
        rotated.bbox.height() > bbox.height() + 10.0,
        "рамка не похожа на повёрнутую: высота {} против {}",
        rotated.bbox.height(),
        bbox.height()
    );

    // Повторный поворот на тот же угол ничего не должен менять: угол
    // абсолютный, а не прибавка.
    let (tx3, rx3) = flume::unbounded();
    let (renderer3, _) = Renderer::open(&saved, tx3)?;
    renderer3.apply_edit(BlockEdit::Transform(pdfcore::BlockTransform {
        page_number: 1,
        bbox: rotated.bbox,
        target: rotated.bbox,
        rotation: 25.0,
        color: None,
        owner: Some(1),
    }));
    loop {
        match next_event(&rx3)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("повторный поворот отклонён: {message}"),
            _ => continue,
        }
    }
    let again = std::env::temp_dir().join("pdfcore_rotate_out2.pdf");
    let _ = std::fs::remove_file(&again);
    renderer3.save_as(&again);
    loop {
        match next_event(&rx3)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer3);

    let (tx4, rx4) = flume::unbounded();
    let (renderer4, _) = Renderer::open(&again, tx4)?;
    renderer4.request_blocks(0);
    let twice = match next_event(&rx4)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let still = twice
        .iter()
        .find(|b| b.text().contains("Bei der Bearbeitung"))
        .expect("блок пропал после повторного поворота");
    assert!(
        (still.rotation - 25.0).abs() < 0.5,
        "угол уехал при повторном повороте: {}",
        still.rotation
    );
    Ok(())
}

/// Правка блока, лежащего поверх чужого текста, не трогает чужой.
///
/// Рамки наложенных блоков пересекаются, и поиск «своего» текста по одному
/// прямоугольнику стёр бы обоих. Владелец из метки решает это точно: правка
/// нашего блока видит только его текст, а правка нетронутого абзаца — только
/// свой, в каком бы прямоугольнике они ни лежали.
#[test]
fn editing_an_overlapping_block_leaves_the_other_owner_alone() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_owner_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_owner_out.pdf");
    let saved2 = std::env::temp_dir().join("pdfcore_owner_out2.pdf");
    let _ = std::fs::remove_file(&saved);
    let _ = std::fs::remove_file(&saved2);

    // Шаг 1: надвигаем первый абзац на второй и сохраняем.
    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let first = blocks[1].bbox;
    let second = blocks[2].bbox;
    renderer.apply_edit(BlockEdit::Transform(pdfcore::BlockTransform {
        page_number: 1,
        bbox: first,
        target: second,
        rotation: 0.0,
        color: None,
        owner: None,
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("перенос отклонён: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    // Шаг 2: открываем заново, правим ТЕКСТ нашего (помеченного) блока —
    // теперь он лежит ровно на нетронутом абзаце.
    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let ours = reopened
        .iter()
        .find(|b| b.text().contains("Bei der Bearbeitung"))
        .expect("наш блок не найден");
    assert!(ours.mark.is_some(), "наш блок обязан быть помечен");

    renderer2.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        owner: ours.mark,
        ..BlockRewrite::text_only(1, ours.bbox, "Nur unser Text wurde ersetzt")
    }));
    loop {
        match next_event(&rx2)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer2.save_as(&saved2);
    loop {
        match next_event(&rx2)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer2);

    // Шаг 3: чужой абзац обязан уцелеть буква в букву, наш — замениться.
    let (tx3, rx3) = flume::unbounded();
    let (renderer3, _) = Renderer::open(&saved2, tx3)?;
    renderer3.request_blocks(0);
    let final_blocks = match next_event(&rx3)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let joined: String = final_blocks
        .iter()
        .map(|b| b.text())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("Nur unser Text wurde ersetzt"),
        "новый текст пропал: {joined}"
    );
    assert!(
        joined.contains("Die Hilfsmittel duerfen keine Kommentare"),
        "чужой абзац пострадал от правки соседа: {joined}"
    );
    assert!(
        !joined.contains("Bei der Bearbeitung"),
        "старый текст нашего блока обязан исчезнуть: {joined}"
    );
    Ok(())
}

/// История правок ходит назад и вперёд.
///
/// Две правки, два отката — текст исходный; один возврат — первая правка
/// снова на месте. Новая правка после отката сжигает ветку возврата.
#[test]
fn history_walks_backwards_and_forwards() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_history_src.pdf", 1)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let bbox = blocks[1].bbox;

    let mut edit = |text: &str| -> Result<()> {
        renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite::text_only(1, bbox, text)));
        loop {
            match next_event(&rx)? {
                RenderEvent::Edited { .. } => return Ok(()),
                RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
                _ => continue,
            }
        }
    };
    edit("Erste Fassung")?;
    edit("Zweite Fassung")?;

    // Текст блока после каждого шага истории.
    let read = |renderer: &Renderer, rx: &flume::Receiver<RenderEvent>| -> Result<String> {
        renderer.request_blocks(0);
        loop {
            match next_event(rx)? {
                RenderEvent::Blocks { blocks, .. } => {
                    return Ok(blocks
                        .iter()
                        .map(|b| b.text())
                        .collect::<Vec<_>>()
                        .join(" "));
                }
                _ => continue,
            }
        }
    };
    let wait_edited = |rx: &flume::Receiver<RenderEvent>| -> Result<()> {
        loop {
            match next_event(rx)? {
                RenderEvent::Edited { .. } => return Ok(()),
                RenderEvent::Failed { message, .. } => panic!("шаг истории не удался: {message}"),
                _ => continue,
            }
        }
    };

    assert!(read(&renderer, &rx)?.contains("Zweite Fassung"));

    renderer.undo();
    wait_edited(&rx)?;
    assert!(
        read(&renderer, &rx)?.contains("Erste Fassung"),
        "первый откат вернул не то"
    );

    renderer.undo();
    wait_edited(&rx)?;
    let original = read(&renderer, &rx)?;
    assert!(
        original.contains("Bei der Bearbeitung"),
        "второй откат не дошёл до исходного"
    );
    assert!(
        !original.contains("Fassung"),
        "правки обязаны исчезнуть: {original}"
    );

    renderer.redo();
    wait_edited(&rx)?;
    assert!(
        read(&renderer, &rx)?.contains("Erste Fassung"),
        "возврат вернул не то"
    );

    // Новая правка после отката: ветка возврата сгорает.
    edit("Dritte Fassung")?;
    renderer.redo();
    let refusal = loop {
        match next_event(&rx)? {
            RenderEvent::Failed { message, .. } => break message,
            RenderEvent::Edited { .. } => panic!("возврат после новой правки обязан быть пуст"),
            _ => continue,
        }
    };
    assert!(refusal.contains("Возвращать"), "не та причина: {refusal}");
    Ok(())
}

/// Пачка правок применяется целиком и откатывается одним шагом.
///
/// Группа выделенных блоков правится одним действием пользователя — и одним
/// же нажатием отменяется, а не по блоку за нажатие.
#[test]
fn a_batch_of_edits_lands_and_undoes_as_one_step() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_batch_src.pdf", 1)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };

    renderer.apply_edits(vec![
        BlockEdit::Rewrite(BlockRewrite::text_only(
            1,
            blocks[1].bbox,
            "Erster Teil neu",
        )),
        BlockEdit::Rewrite(BlockRewrite::text_only(
            1,
            blocks[2].bbox,
            "Zweiter Teil neu",
        )),
    ]);
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("пачка отклонена: {message}"),
            _ => continue,
        }
    }

    let read = |rx: &flume::Receiver<RenderEvent>| -> Result<String> {
        renderer.request_blocks(0);
        loop {
            match next_event(rx)? {
                RenderEvent::Blocks { blocks, .. } => {
                    return Ok(blocks
                        .iter()
                        .map(|b| b.text())
                        .collect::<Vec<_>>()
                        .join(" "));
                }
                _ => continue,
            }
        }
    };
    let after = read(&rx)?;
    assert!(
        after.contains("Erster Teil neu"),
        "первая правка пачки пропала: {after}"
    );
    assert!(
        after.contains("Zweiter Teil neu"),
        "вторая правка пачки пропала: {after}"
    );

    // Один откат снимает всю пачку.
    renderer.undo();
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("откат не удался: {message}"),
            _ => continue,
        }
    }
    let reverted = read(&rx)?;
    assert!(
        !reverted.contains("Teil neu"),
        "откат обязан снять обе правки: {reverted}"
    );
    assert!(
        reverted.contains("Bei der Bearbeitung"),
        "исходный текст не вернулся"
    );
    Ok(())
}

/// Слияние двух блоков в один абзац — «Сгруппировать».
///
/// Детектор геометрический и иногда разрезает единый абзац. Группировка
/// перенабирает выбранные блоки одним куском в общую рамку; проверяем, что
/// после переоткрытия они опознаются одним блоком с целым текстом.
#[test]
fn grouping_merges_two_blocks_into_one_paragraph() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_group_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_group_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let first = &blocks[1];
    let second = &blocks[2];
    let union = first.bbox.union(&second.bbox);
    let merged_text = format!("{} {}", first.text(), second.text());

    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        target: Some(union),
        line_height: first.leading(),
        ..BlockRewrite::text_only(1, union, merged_text.clone())
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("группировка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let merged = reopened
        .iter()
        .find(|b| b.text().contains("Bei der Bearbeitung"))
        .expect("слитый блок не найден");
    assert!(
        merged.text().contains("Hervorhebungen und Verweisungen"),
        "второй абзац не влился в первый: {:?}",
        merged.text()
    );
    Ok(())
}

/// Страничные операции проходят сквозь весь конвейер и откатываются.
#[test]
fn page_operations_flow_through_and_undo() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_pageops_src.pdf", 3)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, info) = Renderer::open(&source, tx)?;
    assert_eq!(info.page_count, 3);

    let wait_pages = |rx: &flume::Receiver<RenderEvent>| -> Result<u32> {
        loop {
            match next_event(rx)? {
                RenderEvent::PagesChanged { info } => return Ok(info.page_count),
                RenderEvent::Failed { message, .. } => panic!("операция не удалась: {message}"),
                _ => continue,
            }
        }
    };

    renderer.apply_page_op(pdfcore::PageOp::InsertAfter { page_number: 1 });
    assert_eq!(wait_pages(&rx)?, 4, "после вставки страниц четыре");

    renderer.apply_page_op(pdfcore::PageOp::Delete { page_number: 4 });
    assert_eq!(wait_pages(&rx)?, 3, "после удаления снова три");

    renderer.apply_page_op(pdfcore::PageOp::Rotate {
        page_number: 1,
        clockwise: true,
    });
    let count = wait_pages(&rx)?;
    assert_eq!(count, 3, "поворот не меняет число страниц");

    // Откат снимает поворот, затем возвращает удалённую, затем убирает
    // вставленную — ровно в обратном порядке.
    renderer.undo();
    assert_eq!(wait_pages(&rx)?, 3);
    renderer.undo();
    assert_eq!(wait_pages(&rx)?, 4, "откат удаления вернул страницу");
    renderer.undo();
    assert_eq!(wait_pages(&rx)?, 3, "откат вставки убрал пустую");
    Ok(())
}

/// Просит тайл первой страницы в масштабе 1:1 и ждёт его.
fn next_tile(
    renderer: &Renderer,
    rx: &flume::Receiver<RenderEvent>,
) -> Result<Option<(u32, u32, Vec<u8>)>> {
    renderer.set_wanted_tiles(vec![TileKey {
        page: 0,
        zoom: ZoomBucket::from_scale(1.0),
        rotation: Rotation::None,
    }]);
    loop {
        match next_event(rx)? {
            RenderEvent::Tile { bitmap, .. } => {
                return Ok(Some((bitmap.width, bitmap.height, bitmap.pixels.clone())));
            }
            RenderEvent::Failed { message, .. } => panic!("растеризация не удалась: {message}"),
            _ => continue,
        }
    }
}

/// Рендер страницы в масштабе 1:1 через поток рендера.
fn render_page(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(path, tx)?;
    renderer.set_wanted_tiles(vec![TileKey {
        page: 0,
        zoom: ZoomBucket::from_scale(1.0),
        rotation: Rotation::None,
    }]);
    match next_event(&rx)? {
        RenderEvent::Tile { bitmap, .. } => {
            Ok((bitmap.width, bitmap.height, bitmap.pixels.clone()))
        }
        other => panic!("ожидался тайл, получено {other:?}"),
    }
}

#[test]
fn editing_a_block_leaves_the_rest_of_the_page_untouched() -> Result<()> {
    // Регрессия на дефект, который дошёл до пользователя: прежний механизм на
    // объектной модели pdfium при правке одного абзаца терял у соседних
    // объектов цвета, разрядку и глифы. Проверять извлечение текста было
    // недостаточно — нужно сравнивать пиксели.
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_fidelity_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_fidelity_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let before = render_page(&source)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let target = blocks[1].bbox;

    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        line_height: Some(13.0),
        ..BlockRewrite::text_only(
            1,
            target,
            "Diese Zeile wurde ersetzt und ist deutlich kuerzer.",
        )
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let after = render_page(&saved)?;
    assert_eq!(
        (before.0, before.1),
        (after.0, after.1),
        "размеры растров разошлись"
    );

    // Рамка правленого блока в координатах растра: масштаб 1:1, ось Y
    // перевёрнута. Запас снизу — на случай, если текст займёт больше строк.
    let margin = 6.0;
    let x0 = (target.left - margin).max(0.0) as u32;
    let x1 = ((target.right + margin) as u32).min(before.0);
    let y0 = (PAGE_HEIGHT - target.top - margin).max(0.0) as u32;
    let y1 = ((PAGE_HEIGHT - target.bottom + margin + 40.0) as u32).min(before.1);

    let mut outside = 0usize;
    let mut inside = 0usize;
    for y in 0..before.1 {
        for x in 0..before.0 {
            let offset = ((y * before.0 + x) * 4) as usize;
            if before.2[offset..offset + 4] == after.2[offset..offset + 4] {
                continue;
            }
            if x >= x0 && x < x1 && y >= y0 && y < y1 {
                inside += 1;
            } else {
                outside += 1;
            }
        }
    }

    assert!(inside > 0, "правленый блок обязан был измениться");
    assert_eq!(
        outside, 0,
        "правка задела {outside} пикселей за пределами блока"
    );
    Ok(())
}

/// Первое из перечисленных семейств, которое есть в системе.
fn cyrillic_family() -> Option<String> {
    let fonts = pdfcore::system_fonts();
    ["Arial", "Times New Roman", "Segoe UI", "Tahoma", "Verdana"]
        .into_iter()
        .find(|family| fonts.has_family(family))
        .map(str::to_owned)
}

#[test]
fn embedded_font_brings_cyrillic_into_a_latin_document() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(family) = cyrillic_family() else {
        eprintln!("подходящего системного шрифта нет, тест пропущен");
        return Ok(());
    };

    let source = fixture("pdfcore_embed_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_embed_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let before = render_page(&source)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let target = blocks[1].bbox;

    // Исходный шрифт — Helvetica без кириллицы. Без встраивания правка обязана
    // быть отклонена, а со встраиванием — пройти.
    let cyrillic = "Задачи переписаны по-русски.";
    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite::text_only(
        1, target, cyrillic,
    )));
    match next_event(&rx)? {
        RenderEvent::Failed { message, .. } => {
            assert!(
                message.contains("нет символов"),
                "неожиданная причина: {message}"
            );
        }
        other => panic!("правка без встраивания обязана быть отклонена, получено {other:?}"),
    }

    renderer.apply_edit(BlockEdit::Rewrite(
        BlockRewrite {
            line_height: Some(13.0),
            ..BlockRewrite::text_only(1, target, cyrillic)
        }
        .with_font(Some(pdfcore::FontRequest::new(&family, false, false)), None),
    ));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    // Текст обязан читаться обратно — это работа таблицы ToUnicode. Без неё
    // редактор не смог бы заново разобрать собственную правку.
    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    drop(renderer2);

    let joined: String = reopened
        .iter()
        .map(|b| b.text())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("по-русски"),
        "кириллица не прочиталась обратно: {joined}"
    );

    // И соседние блоки по-прежнему нетронуты.
    let after = render_page(&saved)?;
    let margin = 6.0;
    let x0 = (target.left - margin).max(0.0) as u32;
    let x1 = ((target.right + margin) as u32).min(before.0);
    let y0 = (PAGE_HEIGHT - target.top - margin).max(0.0) as u32;
    let y1 = ((PAGE_HEIGHT - target.bottom + margin + 40.0) as u32).min(before.1);

    let mut outside = 0usize;
    for y in 0..before.1 {
        for x in 0..before.0 {
            let offset = ((y * before.0 + x) * 4) as usize;
            if before.2[offset..offset + 4] == after.2[offset..offset + 4] {
                continue;
            }
            if !(x >= x0 && x < x1 && y >= y0 && y < y1) {
                outside += 1;
            }
        }
    }
    assert_eq!(
        outside, 0,
        "встраивание шрифта задело {outside} пикселей за пределами блока"
    );
    Ok(())
}

#[test]
fn one_block_carries_several_styles_at_once() -> Result<()> {
    use pdfcore::stream_edit::{Script, StyledSpan};

    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_spans_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_spans_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let before = render_page(&source)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let target = blocks[1].bbox;

    // Абзац из кусков: обычный текст, нижний индекс, надстрочная сноска,
    // красный подчёркнутый хвост.
    let spans = vec![
        StyledSpan::plain("Water H"),
        StyledSpan {
            script: Script::Subscript,
            ..StyledSpan::plain("2")
        },
        StyledSpan::plain("O boils"),
        StyledSpan {
            script: Script::Superscript,
            ..StyledSpan::plain("7")
        },
        StyledSpan {
            color: Some([1.0, 0.0, 0.0]),
            underline: true,
            ..StyledSpan::plain(" and stays red")
        },
    ];

    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        spans,
        line_height: Some(13.0),
        ..BlockRewrite::text_only(1, target, "")
    }));
    let outcome = loop {
        match next_event(&rx)? {
            RenderEvent::Edited { outcome, .. } => break outcome,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    };
    assert!(outcome.is_some_and(|outcome| outcome.created_lines >= 1));

    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    // Текст обязан вычитываться целиком, вместе с индексами.
    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    drop(renderer2);

    let joined: String = reopened
        .iter()
        .map(|b| b.text())
        .collect::<Vec<_>>()
        .join(" ");
    for expected in ["Water H", "2", "O boils", "7", "and stays red"] {
        assert!(
            joined.contains(expected),
            "пропало «{expected}» в: {joined}"
        );
    }

    // Индексы обязаны быть мельче основного текста — иначе `Ts` и уменьшение
    // кегля не сработали.
    let sizes: Vec<f32> = reopened
        .iter()
        .flat_map(|block| block.lines.iter())
        .flat_map(|line| line.runs.iter())
        .map(|run| run.style.size)
        .collect();
    let smallest = sizes.iter().copied().fold(f32::INFINITY, f32::min);
    let largest = sizes.iter().copied().fold(0.0f32, f32::max);
    assert!(
        smallest < largest * 0.8,
        "индексы не уменьшились: от {smallest} до {largest}"
    );

    // И соседние блоки по-прежнему нетронуты.
    let after = render_page(&saved)?;
    let margin = 6.0;
    let x0 = (target.left - margin).max(0.0) as u32;
    let x1 = ((target.right + margin) as u32).min(before.0);
    let y0 = (PAGE_HEIGHT - target.top - margin).max(0.0) as u32;
    let y1 = ((PAGE_HEIGHT - target.bottom + margin + 40.0) as u32).min(before.1);

    let mut outside = 0usize;
    for y in 0..before.1 {
        for x in 0..before.0 {
            let offset = ((y * before.0 + x) * 4) as usize;
            if before.2[offset..offset + 4] != after.2[offset..offset + 4]
                && !(x >= x0 && x < x1 && y >= y0 && y < y1)
            {
                outside += 1;
            }
        }
    }
    assert_eq!(
        outside, 0,
        "смешанное оформление задело {outside} пикселей вне блока"
    );
    Ok(())
}

#[test]
fn rendered_pixels_are_in_bgra_order() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = std::env::temp_dir().join("pdfcore_red_square.pdf");
    build_red_square_pdf(&path)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&path, tx)?;
    renderer.set_wanted_tiles(vec![TileKey {
        page: 0,
        zoom: ZoomBucket::from_scale(1.0),
        rotation: Rotation::None,
    }]);

    let bitmap = match rx.recv_timeout(std::time::Duration::from_secs(30))? {
        RenderEvent::Tile { bitmap, .. } => bitmap,
        other => panic!("ожидался тайл, получено {other:?}"),
    };

    // Квадрат лежит в левом нижнем углу страницы PDF. В растре начало
    // координат сверху, поэтому берём точку у нижнего края.
    let x = 100;
    let y = bitmap.height - 100;
    let offset = ((y * bitmap.width + x) * 4) as usize;
    let pixel = &bitmap.pixels[offset..offset + 4];

    assert_eq!(
        pixel,
        [0, 0, 255, 255],
        "чистый красный обязан лежать в порядке BGRA, получено {pixel:?}. \
         [255, 0, 0, 255] означает включённый FPDF_REVERSE_BYTE_ORDER — тогда \
         gpui покажет тёплые цвета синими"
    );
    Ok(())
}

#[test]
fn heading_and_two_paragraphs_are_detected_as_three_blocks() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = fixture("pdfcore_blocks.pdf", 1)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&path, tx)?;
    renderer.request_blocks(0);

    match rx.recv_timeout(std::time::Duration::from_secs(30))? {
        RenderEvent::Blocks { page, blocks } => {
            assert_eq!(page, 0);
            assert_eq!(
                blocks.len(),
                3,
                "блоки: {:#?}",
                blocks.iter().map(|b| b.text()).collect::<Vec<_>>()
            );

            assert_eq!(blocks[0].text(), "Illustrierende Pruefungsaufgaben");
            assert_eq!(blocks[0].lines.len(), 1);
            assert!((blocks[0].dominant_style().size - 20.0).abs() < 0.5);

            assert_eq!(blocks[1].lines.len(), 4);
            assert!(blocks[1].text().starts_with("Bei der Bearbeitung"));
            assert!(blocks[1].text().ends_with("Gymnasium."));

            assert_eq!(blocks[2].lines.len(), 3);
            assert!(blocks[2].text().starts_with("Die Hilfsmittel"));
            assert!((blocks[2].dominant_style().size - 11.0).abs() < 0.5);
        }
        other => panic!("ожидались блоки, получено {other:?}"),
    }
    Ok(())
}

/// Группировка пакетом «стереть каждый + создать один» сшивает блоки даже с
/// разными владельцами: первый абзац уже переписан (у него метка), второй —
/// нетронутый текст страницы без метки. Именно так работает кнопка
/// «Сгруппировать» в интерфейсе.
#[test]
fn grouping_via_erase_and_create_merges_mixed_owners() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_group_mixed_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_group_mixed_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };

    // Первый абзац переписывается — теперь у него есть метка-владелец.
    let first_bbox = blocks[1].bbox;
    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite::text_only(
        1,
        first_bbox,
        "Erster Absatz wurde bereits bearbeitet",
    )));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("первая правка отклонена: {message}"),
            _ => continue,
        }
    }

    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let marked = blocks
        .iter()
        .find(|b| b.text().contains("bereits bearbeitet"))
        .expect("переписанный блок не найден");
    let plain = blocks
        .iter()
        .find(|b| b.text().contains("sind gestattet"))
        .expect("нетронутый блок не найден");
    assert!(
        marked.mark.is_some(),
        "у переписанного блока должна быть метка"
    );
    assert!(
        plain.mark.is_none(),
        "нетронутый блок обязан быть без метки"
    );

    // Пакет группировки: стереть оба у их владельцев, создать один в общей
    // рамке. Всё — одним шагом истории.
    let union = marked.bbox.union(&plain.bbox);
    let merged_text = format!("{} {}", marked.text(), plain.text());
    renderer.apply_edits(vec![
        BlockEdit::Erase(pdfcore::BlockErase {
            page_number: 1,
            bbox: marked.bbox,
            owner: marked.mark,
        }),
        BlockEdit::Erase(pdfcore::BlockErase {
            page_number: 1,
            bbox: plain.bbox,
            owner: plain.mark,
        }),
        BlockEdit::Rewrite(BlockRewrite {
            target: Some(union),
            create: true,
            line_height: marked.leading(),
            ..BlockRewrite::text_only(1, union, merged_text)
        }),
    ]);
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("пакет группировки отклонён: {message}"),
            _ => continue,
        }
    }

    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let merged = reopened
        .iter()
        .find(|b| b.text().contains("bereits bearbeitet"))
        .expect("слитый блок не найден");
    assert!(
        merged.text().contains("sind gestattet"),
        "куски разных владельцев не слились: {:?}",
        merged.text()
    );
    Ok(())
}

/// Разрядка `Tc` переживает перенабор: заголовок, набранный вразрядку,
/// не должен сжиматься в сплошное слово после правки.
#[test]
fn letter_spacing_survives_a_rewrite() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Страница с одной строкой вразрядку: Tc 2.5 при кегле 14.
    let source = std::env::temp_dir().join("pdfcore_tc_src.pdf");
    {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let operations = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), Object::Real(14.0)]),
            Operation::new("Tc", vec![Object::Real(2.5)]),
            Operation::new(
                "Tm",
                vec![
                    Object::Real(1.0),
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(1.0),
                    Object::Real(72.0),
                    Object::Real(700.0),
                ],
            ),
            Operation::new("Tj", vec![Object::string_literal("GESPERRTE ZEILE")]),
            Operation::new("ET", vec![]),
        ];
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations }.encode()?,
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![
                0.into(),
                0.into(),
                Object::Real(PAGE_WIDTH),
                Object::Real(PAGE_HEIGHT),
            ],
            "Contents" => content_id,
            "Resources" => resources_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&source)?;
    }
    let saved = std::env::temp_dir().join("pdfcore_tc_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let original = &blocks[0];
    let original_width = original.bbox.right - original.bbox.left;
    let text = original.text();

    // Перенабор тем же текстом. Рамка пошире, чтобы перенос строк не мешал
    // сравнению ширин.
    let mut roomy = original.bbox;
    roomy.right = roomy.left + original_width + 40.0;
    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite::text_only(1, roomy, text)));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("перенабор отклонён: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let rewritten = &reopened[0];
    let new_width = rewritten.bbox.right - rewritten.bbox.left;
    assert!(
        (new_width - original_width).abs() < 3.0,
        "разрядка потерялась: ширина была {original_width:.1}, стала {new_width:.1}"
    );
    Ok(())
}

/// Фон за буквами и отбивка абзацев переживают сохранение: прямоугольник
/// заливки рисуется до текста внутри метки блока, а каждый жёсткий перенос
/// добавляет свою отбивку к интерлиньяжу.
#[test]
fn fill_and_paragraph_spacing_survive_saving() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_fill_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_fill_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let bbox = blocks[1].bbox;

    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        fill: Some([1.0, 0.9, 0.2]),
        para_spacing: Some(6.0),
        line_height: Some(12.0),
        ..BlockRewrite::text_only(1, bbox, "Erster Absatz\nZweiter Absatz\nDritter")
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    // В сохранённом потоке страницы обязан быть закрашенный контур до текста.
    let doc = lopdf::Document::load(&saved)?;
    let page_id = *doc.get_pages().get(&1).expect("страница на месте");
    let content = lopdf::content::Content::decode(&doc.get_page_content(page_id))?;
    let ops: Vec<&str> = content
        .operations
        .iter()
        .map(|op| op.operator.as_str())
        .collect();
    let fill_at = ops
        .iter()
        .position(|op| *op == "f")
        .expect("заливка не записана");
    assert!(
        ops[..fill_at].contains(&"m") && ops[..fill_at].contains(&"rg"),
        "перед заливкой обязан идти контур с цветом: {ops:?}"
    );

    // Отбивка раздвигает абзацы: расстояние между базовыми линиями абзацев
    // больше интерлиньяжа.
    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let all_lines: Vec<f32> = reopened
        .iter()
        .flat_map(|block| block.lines.iter().map(|line| line.baseline))
        .collect();
    let mut paragraphs: Vec<f32> = reopened
        .iter()
        .filter(|block| block.text().contains("Absatz") || block.text().contains("Dritter"))
        .flat_map(|block| block.lines.iter().map(|line| line.baseline))
        .collect();
    paragraphs.sort_by(|a, b| b.total_cmp(a));
    assert!(
        paragraphs.len() >= 3,
        "должно быть три строки-абзаца: {all_lines:?}"
    );
    let step = paragraphs[0] - paragraphs[1];
    assert!(
        (step - 18.0).abs() < 1.5,
        "шаг абзацев должен быть 12 + 6 отбивки, а вышло {step}"
    );
    Ok(())
}

/// Стиль каскадом: блок привязывается к стилю, правка стиля перенабирает
/// его, а Ctrl+Z откатывает и каталог, и текст одним шагом.
#[test]
fn a_style_cascades_to_its_blocks_and_undoes() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_styles_src.pdf", 1)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let bbox = blocks[1].bbox;

    // Каталог со стилем и блок, привязанный к нему.
    let def = pdfcore::StyleDef {
        id: 7,
        name: "Основной".into(),
        family: None,
        size: None,
        color: None,
        bold: false,
        italic: false,
        underline: false,
    };
    renderer.save_styles(vec![def.clone()]);
    loop {
        match next_event(&rx)? {
            RenderEvent::Styles { styles, changed } => {
                assert!(changed);
                assert_eq!(styles.len(), 1);
                break;
            }
            RenderEvent::Failed { message, .. } => panic!("каталог не записался: {message}"),
            _ => continue,
        }
    }
    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        style_id: Some(7),
        ..BlockRewrite::text_only(1, bbox, "Absatz im Stil")
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("привязка отклонена: {message}"),
            _ => continue,
        }
    }

    // Правка стиля: крупнее и красным. Блок обязан перенабраться сам.
    let mut louder = def.clone();
    louder.size = Some(19.0);
    louder.color = Some([0.9, 0.1, 0.1]);
    renderer.apply_style(louder);
    loop {
        match next_event(&rx)? {
            RenderEvent::PagesChanged { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("каскад не прошёл: {message}"),
            _ => continue,
        }
    }
    renderer.request_blocks(0);
    let after = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let styled = after
        .iter()
        .find(|block| block.text().contains("Absatz im Stil"))
        .expect("блок стиля не найден");
    assert_eq!(styled.style, Some(7), "связь со стилем обязана сохраниться");
    let size = styled.dominant_style().size;
    assert!(
        (size - 19.0).abs() < 0.5,
        "кегль стиля не применился: {size}"
    );

    // Откат: кегль возвращается, каталог тоже.
    renderer.undo();
    loop {
        match next_event(&rx)? {
            RenderEvent::PagesChanged { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("откат не прошёл: {message}"),
            _ => continue,
        }
    }
    renderer.request_blocks(0);
    let reverted = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let back = reverted
        .iter()
        .find(|block| block.text().contains("Absatz im Stil"))
        .expect("блок пропал после отката");
    let size = back.dominant_style().size;
    assert!(
        (size - 11.0).abs() < 0.5,
        "после отката кегль обязан вернуться к 11: {size}"
    );
    Ok(())
}

/// Выключка однострочного блока переживает сохранение: по геометрии одной
/// строки «по центру» не определить, поэтому она хранится в метке блока.
#[test]
fn a_single_line_block_remembers_its_alignment() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_align_mark_src.pdf", 1)?;
    let saved = std::env::temp_dir().join("pdfcore_align_mark_out.pdf");
    let _ = std::fs::remove_file(&saved);

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    // Заголовок — одна строка.
    let heading = &blocks[0];
    assert_eq!(heading.lines.len(), 1);

    renderer.apply_edit(BlockEdit::Rewrite(BlockRewrite {
        align: pdfcore::model::Align::Center,
        ..BlockRewrite::text_only(1, heading.bbox, heading.text())
    }));
    loop {
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("правка отклонена: {message}"),
            _ => continue,
        }
    }
    renderer.save_as(&saved);
    loop {
        match next_event(&rx)? {
            RenderEvent::Saved { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("сохранение не удалось: {message}"),
            _ => continue,
        }
    }
    drop(renderer);

    let (tx2, rx2) = flume::unbounded();
    let (renderer2, _) = Renderer::open(&saved, tx2)?;
    renderer2.request_blocks(0);
    let reopened = match next_event(&rx2)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let heading = reopened
        .iter()
        .find(|block| block.lines.len() == 1 && block.text().contains("Pruefungsaufgaben"))
        .expect("однострочный заголовок не найден");
    assert_eq!(
        heading.align,
        pdfcore::model::Align::Center,
        "выключка обязана прочитаться из метки"
    );
    Ok(())
}

/// Перетаскивание группы: пакет из двух перестановок непомеченных блоков.
#[test]
fn a_group_of_plain_blocks_moves_as_a_batch() -> Result<()> {
    let _guard = PDFIUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = fixture("pdfcore_groupmove_src.pdf", 1)?;

    let (tx, rx) = flume::unbounded();
    let (renderer, _info) = Renderer::open(&source, tx)?;
    renderer.request_blocks(0);
    let blocks = match next_event(&rx)? {
        RenderEvent::Blocks { blocks, .. } => blocks,
        other => panic!("ожидались блоки, получено {other:?}"),
    };
    let (dx, dy) = (30.0, -40.0);
    let edits: Vec<BlockEdit> = blocks[1..3]
        .iter()
        .map(|block| {
            let b = block.bbox;
            BlockEdit::Transform(pdfcore::BlockTransform {
                page_number: 1,
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
        match next_event(&rx)? {
            RenderEvent::Edited { .. } => break,
            RenderEvent::Failed { message, .. } => panic!("перенос группы отклонён: {message}"),
            _ => continue,
        }
    }
    Ok(())
}
