//! Операции над страницами: вставка пустой, удаление, поворот.
//!
//! Всё сделано с оглядкой на историю правок. Удаление не уничтожает объект
//! страницы — она лишь вынимается из дерева `/Pages`, и откат возвращает её
//! одной вставкой ссылки обратно. Вставка и поворот так же обратимы: каждый
//! шаг снимает точный снимок затронутых узлов дерева до и после.

use anyhow::{Result, anyhow, bail};
use lopdf::{Dictionary, Document, Object, ObjectId, dictionary};

/// Что сделать со страницей.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageOp {
    /// Вставить пустую страницу после указанной (нумерация с единицы).
    InsertAfter { page_number: u32 },
    /// Убрать страницу из документа.
    Delete { page_number: u32 },
    /// Повернуть страницу на четверть оборота.
    Rotate { page_number: u32, clockwise: bool },
    /// Вставить копию страницы `source` после страницы `after`.
    Duplicate { source: u32, after: u32 },
    /// Вставить все страницы другого файла после указанной страницы.
    InsertDocument {
        page_number: u32,
        path: std::path::PathBuf,
    },
    /// Переставить страницу на новое место: `to` — номер, который она займёт
    /// в получившемся документе (нумерация с единицы).
    Move { from: u32, to: u32 },
}

impl PageOp {
    pub fn page_number(&self) -> u32 {
        match self {
            PageOp::InsertAfter { page_number }
            | PageOp::Delete { page_number }
            | PageOp::Rotate { page_number, .. }
            | PageOp::InsertDocument { page_number, .. } => *page_number,
            PageOp::Duplicate { after, .. } => *after,
            PageOp::Move { to, .. } => *to,
        }
    }
}

/// Снимок дерева страниц до и после операции — шаг истории.
///
/// Хранится только затронутое: массив `Kids` родителя, счётчики предков и
/// `/Rotate` самой страницы. Объекты страниц не копируются вовсе — удалённая
/// страница продолжает жить в документе и ждёт отката.
#[derive(Clone, Debug)]
pub struct PagesSnapshot {
    /// Родитель, чей список детей менялся, и список до/после.
    kids: Option<(ObjectId, Vec<Object>, Vec<Object>)>,
    /// Счётчики страниц по цепочке предков: узел, до, после.
    counts: Vec<(ObjectId, i64, i64)>,
    /// Поворот страницы: объект, до, после.
    rotate: Option<(ObjectId, Option<Object>, Option<Object>)>,
}

impl PagesSnapshot {
    /// Возвращает дерево в состояние «до».
    pub fn undo(&self, document: &mut Document) -> Result<()> {
        self.apply(document, true)
    }

    /// Возвращает дерево в состояние «после».
    pub fn redo(&self, document: &mut Document) -> Result<()> {
        self.apply(document, false)
    }

    fn apply(&self, document: &mut Document, backwards: bool) -> Result<()> {
        if let Some((parent, before, after)) = &self.kids {
            let node = document
                .get_object_mut(*parent)
                .and_then(|o| o.as_dict_mut())
                .map_err(|e| anyhow!("узел дерева страниц недоступен: {e}"))?;
            let value = if backwards { before } else { after };
            node.set("Kids", Object::Array(value.clone()));
        }
        for (node_id, before, after) in &self.counts {
            let node = document
                .get_object_mut(*node_id)
                .and_then(|o| o.as_dict_mut())
                .map_err(|e| anyhow!("узел дерева страниц недоступен: {e}"))?;
            node.set("Count", if backwards { *before } else { *after });
        }
        if let Some((page_id, before, after)) = &self.rotate {
            let page = document
                .get_object_mut(*page_id)
                .and_then(|o| o.as_dict_mut())
                .map_err(|e| anyhow!("страница недоступна: {e}"))?;
            let value = if backwards { before } else { after };
            match value {
                Some(object) => page.set("Rotate", object.clone()),
                None => {
                    page.remove(b"Rotate");
                }
            }
        }
        Ok(())
    }
}

/// Выполняет операцию и возвращает снимок для истории.
pub fn apply_page_op(document: &mut Document, op: &PageOp) -> Result<PagesSnapshot> {
    match op {
        PageOp::InsertAfter { page_number } => insert_after(document, *page_number),
        PageOp::Delete { page_number } => delete(document, *page_number),
        PageOp::Rotate {
            page_number,
            clockwise,
        } => rotate(document, *page_number, *clockwise),
        PageOp::Duplicate { source, after } => duplicate(document, *source, *after),
        PageOp::Move { from, to } => move_page(document, *from, *to),
        PageOp::InsertDocument { page_number, path } => {
            insert_document(document, *page_number, path)
        }
    }
}

fn page_id(document: &Document, page_number: u32) -> Result<ObjectId> {
    document
        .page_iter()
        .nth(page_number.saturating_sub(1) as usize)
        .ok_or_else(|| anyhow!("нет страницы {page_number}"))
}

fn parent_of(document: &Document, page: ObjectId) -> Result<ObjectId> {
    document
        .get_object(page)
        .and_then(|o| o.as_dict())
        .and_then(|dict| dict.get(b"Parent"))
        .and_then(Object::as_reference)
        .map_err(|e| anyhow!("у страницы нет родителя: {e}"))
}

/// Все предки узла, начиная с него самого.
fn ancestors(document: &Document, from: ObjectId) -> Vec<ObjectId> {
    let mut chain = vec![from];
    let mut current = from;
    // Глубина дерева страниц в живых файлах — единицы; тридцать двух хватит
    // и на испорченный файл с кольцом ссылок.
    for _ in 0..32 {
        let Some(parent) = document
            .get_object(current)
            .ok()
            .and_then(|o| o.as_dict().ok())
            .and_then(|dict| dict.get(b"Parent").ok())
            .and_then(|o| o.as_reference().ok())
        else {
            break;
        };
        chain.push(parent);
        current = parent;
    }
    chain
}

/// Меняет счётчики страниц по цепочке предков и запоминает их до/после.
fn bump_counts(
    document: &mut Document,
    chain: &[ObjectId],
    delta: i64,
) -> Result<Vec<(ObjectId, i64, i64)>> {
    let mut log = Vec::with_capacity(chain.len());
    for node_id in chain {
        let node = document
            .get_object_mut(*node_id)
            .and_then(|o| o.as_dict_mut())
            .map_err(|e| anyhow!("узел дерева страниц недоступен: {e}"))?;
        let before = node.get(b"Count").and_then(Object::as_i64).unwrap_or(0);
        let after = before + delta;
        node.set("Count", after);
        log.push((*node_id, before, after));
    }
    Ok(log)
}

fn kids_of(document: &Document, parent: ObjectId) -> Result<Vec<Object>> {
    document
        .get_object(parent)
        .and_then(|o| o.as_dict())
        .and_then(|dict| dict.get(b"Kids"))
        .and_then(Object::as_array)
        .cloned()
        .map_err(|e| anyhow!("у узла дерева страниц нет детей: {e}"))
}

fn set_kids(document: &mut Document, parent: ObjectId, kids: Vec<Object>) -> Result<()> {
    let node = document
        .get_object_mut(parent)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("узел дерева страниц недоступен: {e}"))?;
    node.set("Kids", Object::Array(kids));
    Ok(())
}

fn insert_after(document: &mut Document, page_number: u32) -> Result<PagesSnapshot> {
    // Ноль — «перед первой»: якорем тогда служит она сама.
    let anchor = page_id(document, page_number.max(1))?;
    let parent = parent_of(document, anchor)?;

    // Размер новой страницы — как у соседки: пустой лист чужого формата
    // выглядел бы заплаткой.
    let media_box = document
        .get_object(anchor)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|dict| dict.get(b"MediaBox").ok())
        .cloned()
        .unwrap_or_else(|| {
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(595.0),
                Object::Real(842.0),
            ])
        });

    let contents = document.add_object(lopdf::Stream::new(dictionary! {}, Vec::new()));
    let mut page = Dictionary::new();
    page.set("Type", "Page");
    page.set("Parent", Object::Reference(parent));
    page.set("MediaBox", media_box);
    page.set("Resources", Object::Dictionary(Dictionary::new()));
    page.set("Contents", Object::Reference(contents));
    let new_page = document.add_object(Object::Dictionary(page));

    // Место в списке детей выбирает общий помощник: он же умеет «в начало».
    insert_pages_after(document, page_number, vec![new_page])
}

fn delete(document: &mut Document, page_number: u32) -> Result<PagesSnapshot> {
    if document.page_iter().count() <= 1 {
        bail!("последнюю страницу удалить нельзя: документ без страниц не откроется");
    }
    let target = page_id(document, page_number)?;
    let parent = parent_of(document, target)?;

    let kids_before = kids_of(document, parent)?;
    let kids_after: Vec<Object> = kids_before
        .iter()
        .filter(|kid| !matches!(kid, Object::Reference(id) if *id == target))
        .cloned()
        .collect();
    if kids_after.len() == kids_before.len() {
        bail!("страница не найдена среди детей своего родителя");
    }
    set_kids(document, parent, kids_after.clone())?;

    // Сам объект страницы не трогается: он остаётся в документе и ждёт
    // отката. Лишний объект в файле безвреден, потерянная страница — нет.
    let counts = bump_counts(document, &ancestors(document, parent), -1)?;
    Ok(PagesSnapshot {
        kids: Some((parent, kids_before, kids_after)),
        counts,
        rotate: None,
    })
}

fn rotate(document: &mut Document, page_number: u32, clockwise: bool) -> Result<PagesSnapshot> {
    let target = page_id(document, page_number)?;
    let page = document
        .get_object_mut(target)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("страница недоступна: {e}"))?;

    let before = page.get(b"Rotate").ok().cloned();
    let current = before.as_ref().and_then(|o| o.as_i64().ok()).unwrap_or(0);
    let step = if clockwise { 90 } else { -90 };
    let next = (current + step).rem_euclid(360);
    let after = Object::Integer(next);
    page.set("Rotate", after.clone());

    Ok(PagesSnapshot {
        kids: None,
        counts: Vec::new(),
        rotate: Some((target, before, Some(after))),
    })
}

/// Переставляет страницу на новое место в дереве.
///
/// Правится только список детей: сама страница, её содержимое и ресурсы
/// остаются теми же объектами. Поэтому перестановка ничего не портит и
/// откатывается одним снимком списка.
fn move_page(document: &mut Document, from: u32, to: u32) -> Result<PagesSnapshot> {
    let page = page_id(document, from)?;
    let parent = parent_of(document, page)?;
    let kids_before = kids_of(document, parent)?;

    let at = kids_before
        .iter()
        .position(|kid| matches!(kid, Object::Reference(id) if *id == page))
        .ok_or_else(|| anyhow!("страница не найдена среди детей своего родителя"))?;

    // Целевой номер считается в документе БЕЗ переставляемой страницы:
    // «поставить пятой» значит «после четырёх оставшихся».
    let mut kids_after = kids_before.clone();
    let moved = kids_after.remove(at);
    let target = (to.max(1) as usize - 1).min(kids_after.len());
    kids_after.insert(target, moved);

    if kids_after == kids_before {
        // Ставить страницу туда, где она и стоит, — не правка: пустой снимок
        // не засоряет историю.
        return Ok(PagesSnapshot {
            kids: None,
            counts: Vec::new(),
            rotate: None,
        });
    }

    set_kids(document, parent, kids_after.clone())?;
    Ok(PagesSnapshot {
        kids: Some((parent, kids_before, kids_after)),
        counts: Vec::new(),
        rotate: None,
    })
}

/// Вставляет список готовых страниц после указанной, с одним снимком на всё.
fn insert_pages_after(
    document: &mut Document,
    page_number: u32,
    new_pages: Vec<ObjectId>,
) -> Result<PagesSnapshot> {
    // Ноль означает «перед первой страницей»: щель слева от неё в сетке —
    // такое же законное место вставки, как и все прочие.
    let before_first = page_number == 0;
    let anchor = page_id(document, page_number.max(1))?;
    let parent = parent_of(document, anchor)?;

    // Родительская ссылка у новых страниц — обязательна: без неё дерево
    // страниц оказывается со сломанной спинкой.
    for new_page in &new_pages {
        if let Ok(page) = document
            .get_object_mut(*new_page)
            .and_then(|o| o.as_dict_mut())
        {
            page.set("Parent", Object::Reference(parent));
        }
    }

    let kids_before = kids_of(document, parent)?;
    let mut kids_after = kids_before.clone();
    let at = kids_after
        .iter()
        .position(|kid| matches!(kid, Object::Reference(id) if *id == anchor))
        .ok_or_else(|| anyhow!("страница не найдена среди детей своего родителя"))?;
    let at = if before_first { at } else { at + 1 };
    for (offset, new_page) in new_pages.iter().enumerate() {
        kids_after.insert(at + offset, Object::Reference(*new_page));
    }
    set_kids(document, parent, kids_after.clone())?;

    let counts = bump_counts(
        document,
        &ancestors(document, parent),
        new_pages.len() as i64,
    )?;
    Ok(PagesSnapshot {
        kids: Some((parent, kids_before, kids_after)),
        counts,
        rotate: None,
    })
}

/// Копия страницы этого же документа, вставленная следом.
///
/// Словарь страницы и её поток содержимого копируются по-настоящему — иначе
/// правка одной из близняшек меняла бы обе. Ресурсы остаются общей ссылкой:
/// их редактор не переписывает, а только дополняет новыми именами, и у каждой
/// страницы это происходит в её собственном словаре.
fn duplicate(document: &mut Document, source: u32, after: u32) -> Result<PagesSnapshot> {
    let source_id = page_id(document, source)?;
    let source_dict = document
        .get_object(source_id)
        .and_then(|o| o.as_dict())
        .map_err(|e| anyhow!("страница недоступна: {e}"))?
        .clone();

    let content = document.get_page_content(source_id);
    let contents = document.add_object(lopdf::Stream::new(dictionary! {}, content));

    let mut copy = source_dict;
    copy.set("Contents", Object::Reference(contents));
    // Ресурсы могли быть заданы прямым словарём — тогда они и так копия.
    // А ссылка остаётся ссылкой: см. заметку выше.
    let new_page = document.add_object(Object::Dictionary(copy));

    insert_pages_after(document, after, vec![new_page])
}

/// Вставляет все страницы другого файла после указанной страницы.
///
/// Каждая страница чужого документа переезжает со всем своим поддеревом —
/// шрифтами, картинками, потоками — через тот же перенос ссылок, что собирает
/// одностраничные выжимки. Один снимок на всю пачку: откат убирает весь
/// вставленный документ одним нажатием.
fn insert_document(
    document: &mut Document,
    page_number: u32,
    path: &std::path::Path,
) -> Result<PagesSnapshot> {
    let foreign =
        Document::load(path).map_err(|e| anyhow!("не удалось открыть {}: {e}", path.display()))?;

    let mut new_pages = Vec::new();
    for foreign_page in foreign.page_iter() {
        let source = foreign
            .get_object(foreign_page)
            .and_then(|o| o.as_dict())
            .map_err(|e| anyhow!("страница вставляемого файла недоступна: {e}"))?;

        let mut page = source.clone();
        // Наследуемое надо забрать с собой: в нашем дереве родителей-доноров
        // не будет.
        if page.get(b"Resources").is_err()
            && let Ok((Some(inherited), _)) = foreign.get_page_resources(foreign_page)
        {
            page.set("Resources", Object::Dictionary(inherited.clone()));
        }
        page.remove(b"Parent");
        page.remove(b"Annots");

        let mut copied = std::collections::HashMap::new();
        let mut object = Object::Dictionary(page);
        crate::stream_edit::copy_references(&foreign, document, &mut object, &mut copied);
        new_pages.push(document.add_object(object));
    }
    if new_pages.is_empty() {
        bail!("в {} нет страниц", path.display());
    }

    insert_pages_after(document, page_number, new_pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Stream;

    /// Документ с тремя страницами разного «содержимого».
    fn sample() -> Document {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut kids = Vec::new();
        for _ in 0..3 {
            let contents = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
            let page = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Contents" => contents,
            });
            kids.push(Object::Reference(page));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 3_i64,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        doc
    }

    fn count(document: &Document) -> usize {
        document.page_iter().count()
    }

    #[test]
    fn a_blank_page_is_inserted_after_the_anchor_and_undone() {
        let mut doc = sample();
        let ids_before: Vec<ObjectId> = doc.page_iter().collect();

        let snapshot = apply_page_op(&mut doc, &PageOp::InsertAfter { page_number: 1 }).unwrap();
        assert_eq!(count(&doc), 4);
        let ids_after: Vec<ObjectId> = doc.page_iter().collect();
        assert_eq!(ids_after[0], ids_before[0], "первая страница на месте");
        assert_eq!(ids_after[2], ids_before[1], "бывшая вторая сдвинулась");

        snapshot.undo(&mut doc).unwrap();
        assert_eq!(count(&doc), 3);
        assert_eq!(doc.page_iter().collect::<Vec<_>>(), ids_before);

        snapshot.redo(&mut doc).unwrap();
        assert_eq!(count(&doc), 4);
    }

    #[test]
    fn a_deleted_page_comes_back_whole_on_undo() {
        let mut doc = sample();
        let ids_before: Vec<ObjectId> = doc.page_iter().collect();

        let snapshot = apply_page_op(&mut doc, &PageOp::Delete { page_number: 2 }).unwrap();
        assert_eq!(count(&doc), 2);
        assert!(
            doc.get_object(ids_before[1]).is_ok(),
            "объект удалённой страницы обязан выжить — иначе откату нечего возвращать"
        );

        snapshot.undo(&mut doc).unwrap();
        assert_eq!(doc.page_iter().collect::<Vec<_>>(), ids_before);
    }

    #[test]
    fn the_last_page_refuses_to_die() {
        let mut doc = sample();
        apply_page_op(&mut doc, &PageOp::Delete { page_number: 1 }).unwrap();
        apply_page_op(&mut doc, &PageOp::Delete { page_number: 1 }).unwrap();
        let refusal = apply_page_op(&mut doc, &PageOp::Delete { page_number: 1 });
        assert!(refusal.is_err(), "последняя страница обязана остаться");
    }

    #[test]
    fn a_duplicated_page_lives_its_own_life() {
        let mut doc = sample();
        let snapshot = apply_page_op(
            &mut doc,
            &PageOp::Duplicate {
                source: 1,
                after: 1,
            },
        )
        .unwrap();
        assert_eq!(count(&doc), 4);

        // У копии свой поток содержимого: правка одной не заденет другую.
        let ids: Vec<ObjectId> = doc.page_iter().collect();
        let original_contents = doc
            .get_object(ids[0])
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        let copy_contents = doc
            .get_object(ids[1])
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        assert_ne!(
            original_contents, copy_contents,
            "поток обязан быть скопирован"
        );

        snapshot.undo(&mut doc).unwrap();
        assert_eq!(count(&doc), 3);
    }

    #[test]
    fn a_whole_document_is_inserted_and_undone_in_one_step() {
        let mut host = sample();
        // Донор — такой же трёхстраничный документ на диске.
        let donor_path = std::env::temp_dir().join("pdfcore_insert_donor.pdf");
        sample().save(&donor_path).unwrap();

        let snapshot = apply_page_op(
            &mut host,
            &PageOp::InsertDocument {
                page_number: 2,
                path: donor_path.clone(),
            },
        )
        .unwrap();
        assert_eq!(count(&host), 6, "три родные плюс три вставленные");

        snapshot.undo(&mut host).unwrap();
        assert_eq!(
            count(&host),
            3,
            "весь вставленный документ ушёл одним откатом"
        );

        snapshot.redo(&mut host).unwrap();
        assert_eq!(count(&host), 6);
    }

    #[test]
    fn a_blank_page_can_go_before_the_first_one() {
        let mut doc = sample();
        let before: Vec<ObjectId> = doc.page_iter().collect();
        let snapshot = apply_page_op(&mut doc, &PageOp::InsertAfter { page_number: 0 }).unwrap();
        let after: Vec<ObjectId> = doc.page_iter().collect();
        assert_eq!(count(&doc), 4);
        assert_eq!(&after[1..], &before[..], "прежние страницы съехали на одну");

        snapshot.undo(&mut doc).unwrap();
        assert_eq!(doc.page_iter().collect::<Vec<_>>(), before);
    }

    #[test]
    fn a_page_moves_to_a_new_place_and_back() {
        let mut doc = sample();
        let order = |doc: &Document| -> Vec<ObjectId> { doc.page_iter().collect() };
        let before = order(&doc);

        // Третью страницу ставим первой.
        let snapshot = apply_page_op(&mut doc, &PageOp::Move { from: 3, to: 1 }).unwrap();
        let after = order(&doc);
        assert_eq!(after[0], before[2], "переставленная обязана стать первой");
        assert_eq!(after[1], before[0]);
        assert_eq!(after[2], before[1]);
        assert_eq!(count(&doc), 3, "страниц не прибавилось и не убыло");

        snapshot.undo(&mut doc).unwrap();
        assert_eq!(order(&doc), before, "откат возвращает прежний порядок");
        snapshot.redo(&mut doc).unwrap();
        assert_eq!(order(&doc), after);
    }

    #[test]
    fn moving_a_page_onto_itself_changes_nothing() {
        let mut doc = sample();
        let before: Vec<ObjectId> = doc.page_iter().collect();
        apply_page_op(&mut doc, &PageOp::Move { from: 2, to: 2 }).unwrap();
        assert_eq!(doc.page_iter().collect::<Vec<_>>(), before);
    }

    #[test]
    fn rotation_walks_the_quarters_and_undoes() {
        let mut doc = sample();
        let first = doc.page_iter().next().unwrap();

        let step = apply_page_op(
            &mut doc,
            &PageOp::Rotate {
                page_number: 1,
                clockwise: true,
            },
        )
        .unwrap();
        let rotate_of = |doc: &Document| {
            doc.get_object(first)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Rotate")
                .ok()
                .and_then(|o| o.as_i64().ok())
        };
        assert_eq!(rotate_of(&doc), Some(90));

        step.undo(&mut doc).unwrap();
        assert_eq!(
            rotate_of(&doc),
            None,
            "откат возвращает исходное отсутствие поворота"
        );

        // Против часовой из нуля — 270, а не минус девяносто.
        apply_page_op(
            &mut doc,
            &PageOp::Rotate {
                page_number: 1,
                clockwise: false,
            },
        )
        .unwrap();
        assert_eq!(rotate_of(&doc), Some(270));
    }
}
