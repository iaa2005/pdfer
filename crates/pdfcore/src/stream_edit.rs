//! Хирургия потока содержимого страницы.
//!
//! Прошлый механизм правки строился на объектной модели pdfium и
//! `FPDFPage_GenerateContent`. Он оказался разрушительным: пересборка потока
//! из объектной модели теряет у **соседних** объектов межбуквенный интервал,
//! цвета заливки и нестандартные кодировки глифов. Проверено сравнением
//! отрисовки до и после (`examples/probe_full_edit.rs`).
//!
//! Здесь другой подход. Поток страницы разбирается на операторы, у операторов
//! показа текста внутри правимого абзаца обнуляется строковый операнд, а новый
//! текст дописывается в конец потока. Всё остальное — позиционирование, цвета,
//! разрядка, чужие объекты — проходит через разбор и сборку без изменений; это
//! отдельно проверено пробой `probe_stream_roundtrip` (0 пикселей различий на
//! настоящей книге).
//!
//! Шрифт берётся тот же, что был у абзаца: в поток пишется то же имя ресурса,
//! что стояло в `Tf`. Поэтому встраивать ничего не нужно и сломать шрифтовые
//! ресурсы страницы невозможно.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, dictionary};

use crate::fonts::FontRequest;
use crate::geom::Rect;
use crate::model::Align;

/// Положение знака относительно базовой линии.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Script {
    #[default]
    Baseline,
    Superscript,
    Subscript,
}

impl Script {
    /// Во сколько раз мельче основного кегля набирается знак.
    fn scale(self) -> f32 {
        match self {
            Script::Baseline => 1.0,
            _ => 0.6,
        }
    }

    /// Подъём над базовой линией в долях основного кегля.
    fn rise(self) -> f32 {
        match self {
            Script::Baseline => 0.0,
            Script::Superscript => 0.33,
            Script::Subscript => -0.20,
        }
    }
}

/// Кусок текста с собственным оформлением.
///
/// Абзац задаётся последовательностью таких кусков: именно так внутри одного
/// блока уживаются разные гарнитуры, цвета, индексы и подчёркивание.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct StyledSpan {
    pub text: String,
    /// Гарнитура. `None` — шрифт абзаца, тогда ничего не встраивается.
    pub font: Option<FontRequest>,
    /// Гарнитура из самого документа: кусок был набран ею и остаётся при ней.
    ///
    /// Разрешается в уже существующий шрифт ресурсов страницы — ничего не
    /// встраивается и не подменяется. Именно это позволяет перенабирать
    /// пёстрые абзацы: жирное начало остаётся жирным, курсив курсивом, каждый
    /// своим встроенным шрифтом. Учитывается, только когда `font` пуст:
    /// явная смена гарнитуры пользователем главнее исходной.
    pub page_family: Option<String>,
    /// Кегль в пунктах. `None` — кегль абзаца.
    pub size: Option<f32>,
    /// Цвет как доли единицы. `None` — цвет абзаца.
    pub color: Option<[f32; 3]>,
    pub script: Script,
    pub underline: bool,
    /// Начертание куска, каким его видит пользователь. Само по себе ничего не
    /// встраивает: пока `font` пуст, кусок остаётся при шрифте страницы. Но
    /// когда гарнитуру приходится подменять — вручную или автозаменой, — эти
    /// флаги говорят, какое начертание просить у замены. Раньше подмена
    /// строила запрос с «не жирный, не курсив», и полужирные куски абзаца
    /// разом худели.
    pub bold: bool,
    pub italic: bool,
}

impl StyledSpan {
    pub fn plain(text: impl Into<String>) -> StyledSpan {
        StyledSpan {
            text: text.into(),
            ..Default::default()
        }
    }
}

/// Запрос на замену текста абзаца.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockRewrite {
    /// Номер страницы, начиная с единицы, — как их нумерует lopdf.
    pub page_number: u32,
    /// Где искать текст, который заменяется. Это рамка найденного абзаца, и
    /// меняться она не должна: по ней опознаются операторы показа на странице.
    pub bbox: Rect,
    /// Куда класть новый текст. `None` — туда же, где он был.
    ///
    /// Отдельно от `bbox` затем, что рамку можно подвинуть и растянуть
    /// маркерами. Тогда стирать надо по-прежнему на старом месте, а печатать —
    /// на новом, и одним прямоугольником эти два места уже не описать.
    pub target: Option<Rect>,
    /// Содержимое абзаца по кускам оформления.
    pub spans: Vec<StyledSpan>,
    /// Интерлиньяж в пунктах. `None` — 1.2 кегля.
    pub line_height: Option<f32>,
    /// Поворот абзаца в градусах против часовой стрелки, вокруг центра рамки.
    pub rotation: f32,
    /// Выключка строк внутри рамки.
    pub align: Align,
    /// Межбуквенный интервал `Tc` в пунктах. `None` — как было в блоке.
    pub char_spacing: Option<f32>,
    /// Отбивка между абзацами в пунктах — добавка к интерлиньяжу после
    /// каждого жёсткого переноса (Enter). `None` — без отбивки.
    pub para_spacing: Option<f32>,
    /// Фон за буквами: прямоугольник рамки, закрашенный этим цветом до
    /// вывода текста. `None` — без фона.
    pub fill: Option<[f32; 3]>,
    /// Номер стиля документа, за которым следует блок. Пишется в метку.
    pub style_id: Option<i64>,
    /// Горизонтальный масштаб `Tz` в процентах. `None` — как было в блоке.
    pub h_scale: Option<f32>,
    /// Создание нового блока: в рамке нет и не должно быть старого текста.
    ///
    /// Обычный перенабор без единого оператора показа в рамке — ошибка
    /// пользователя, и она честно отклоняется. Но новый блок начинается именно
    /// с пустого места, поэтому создание объявляется явно, а не выводится из
    /// пустоты.
    pub create: bool,
    /// Владелец правимого текста: номер метки блока либо `None` для текста,
    /// не помеченного редактором.
    ///
    /// Рамки блоков пересекаются, когда один надвинут на другой, и одного
    /// прямоугольника мало: правка, ищущая «свой» текст по геометрии, стёрла
    /// бы и чужой, оказавшийся внутри. Владелец разрезает страницу точно:
    /// правится только текст своей метки, чужой — прозрачен.
    pub owner: Option<i64>,
}

impl BlockRewrite {
    /// Замена одного текста, с сохранением всего оформления абзаца.
    pub fn text_only(page_number: u32, bbox: Rect, text: impl Into<String>) -> BlockRewrite {
        BlockRewrite {
            page_number,
            bbox,
            target: None,
            spans: vec![StyledSpan::plain(text)],
            line_height: None,
            rotation: 0.0,
            align: Align::Left,
            char_spacing: None,
            h_scale: None,
            para_spacing: None,
            fill: None,
            style_id: None,
            create: false,
            owner: None,
        }
    }

    /// Куда ложится новый текст.
    pub fn target(&self) -> Rect {
        self.target.unwrap_or(self.bbox)
    }

    /// Тот же текст, но целиком другой гарнитурой и кеглем.
    pub fn with_font(mut self, font: Option<FontRequest>, size: Option<f32>) -> BlockRewrite {
        for span in &mut self.spans {
            span.font = font.clone();
            span.size = size;
        }
        self
    }

    /// Весь текст абзаца одной строкой — для проверок и сообщений.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RewriteOutcome {
    /// Сколько операторов показа текста опустошено.
    pub cleared_ops: usize,
    pub created_lines: usize,
    /// Итоговая высота набранного текста в пунктах.
    pub height: f32,
}

/// Имя, которым редактор помечает свои блоки в потоке содержимого.
///
/// Метка — это ответ на вопрос «как не склеить наложенные абзацы». Блоки
/// опознаются по геометрии, и когда один абзац надвинут на другой, их строки
/// при следующем разборе неотличимы от одного блока. Метка даёт блоку
/// собственное имя прямо в файле: пары `BDC /PdfEd <</ID N>> … EMC` переживают
/// сохранение и открытие в любом другом просмотрщике — стандартный marked
/// content для отрисовки невидим.
const MARK_TAG: &[u8] = b"PdfEd";

/// Блок, помеченный редактором: номер и начала его строк на странице.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockMark {
    pub id: i64,
    /// Начала базовых линий, в координатах страницы.
    pub origins: Vec<(f32, f32)>,
}

/// Читает метки блоков со страницы.
///
/// Начала строк считаются той же машинерией матриц, что и поиск текста,
/// поэтому совпадают с тем, что при извлечении отдаёт pdfium, — по ним строки
/// потом и раскладываются по меткам.
pub fn page_marks(document: &Document, page_number: u32) -> Result<Vec<BlockMark>> {
    let page_id = page_id(document, page_number)?;
    // Текст может лежать не в потоке страницы, а в форме, вызванной с неё.
    let (_canvas, content) = text_canvas(document, page_id)?;
    Ok(collect_marks(&content))
}

fn collect_marks(content: &Content) -> Vec<BlockMark> {
    let mut marks: Vec<BlockMark> = Vec::new();
    // Стек открытых секций marked content: наши несут номер, чужие — нет.
    let mut open: Vec<Option<i64>> = Vec::new();

    let mut ctm = IDENTITY;
    let mut ctm_stack: Vec<Matrix> = Vec::new();
    let mut text_matrix = IDENTITY;
    let mut line_matrix = IDENTITY;
    let mut leading = 0.0f32;

    for operation in &content.operations {
        match operation.operator.as_str() {
            "q" => ctm_stack.push(ctm),
            "Q" => ctm = ctm_stack.pop().unwrap_or(IDENTITY),
            "cm" => {
                let v = numbers(operation);
                if v.len() == 6 {
                    ctm = multiply([v[0], v[1], v[2], v[3], v[4], v[5]], ctm);
                }
            }
            "BDC" | "BMC" => open.push(mark_id_of(operation)),
            "EMC" => {
                open.pop();
            }
            "BT" => {
                text_matrix = IDENTITY;
                line_matrix = IDENTITY;
            }
            "TL" => leading = numbers(operation).first().copied().unwrap_or(leading),
            "Tm" => {
                let v = numbers(operation);
                if v.len() == 6 {
                    line_matrix = [v[0], v[1], v[2], v[3], v[4], v[5]];
                    text_matrix = line_matrix;
                }
            }
            "Td" | "TD" => {
                let v = numbers(operation);
                if v.len() == 2 {
                    if operation.operator == "TD" {
                        leading = -v[1];
                    }
                    line_matrix = multiply([1.0, 0.0, 0.0, 1.0, v[0], v[1]], line_matrix);
                    text_matrix = line_matrix;
                }
            }
            "T*" => {
                line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -leading], line_matrix);
                text_matrix = line_matrix;
            }
            "Tj" | "TJ" | "'" | "\"" => {
                if matches!(operation.operator.as_str(), "'" | "\"") {
                    line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -leading], line_matrix);
                    text_matrix = line_matrix;
                }
                // Показ принадлежит ближайшей из открытых наших меток.
                let Some(id) = open.iter().rev().find_map(|entry| *entry) else {
                    continue;
                };
                let origin = apply_origin(multiply(text_matrix, ctm));
                match marks.iter_mut().find(|mark| mark.id == id) {
                    Some(mark) => mark.origins.push(origin),
                    None => marks.push(BlockMark {
                        id,
                        origins: vec![origin],
                    }),
                }
            }
            _ => {}
        }
    }
    marks
}

/// Раскладывает страницу на варианты по владельцам текста.
///
/// Для каждого владельца — нашей метки либо «ничейного» текста — собирается
/// отдельный одностраничный документ, в котором чужие показы опустошены.
/// Это ответ на вопрос «как не склеить наложенные абзацы» без единого
/// геометрического допуска: разбор каждого варианта физически не видит чужого
/// текста, и блоки разных владельцев не могут слиться, даже когда их строки
/// легли точь-в-точь друг на друга.
///
/// Пустой список означает «меток нет» — страница разбирается обычным путём,
/// не тратя ни одной лишней загрузки.
/// Вариант страницы одного владельца текста.
pub struct MarkVariant {
    /// Номер метки; `None` — текст, не помеченный редактором.
    pub owner: Option<i64>,
    /// Угол поворота блока, записанный в метке.
    pub rotation: f32,
    /// Номер стиля из метки, если блок привязан к стилю.
    pub style: Option<i64>,
    /// Выключка из метки: у однострочного блока по геометрии её не понять.
    pub align: Option<Align>,
    /// Одностраничный документ, где чужие показы опустошены.
    pub document: Document,
}

pub fn mark_variants(document: &Document, page_number: u32) -> Result<Vec<MarkVariant>> {
    let page_id = page_id(document, page_number)?;
    // Текст может лежать не в потоке страницы, а в форме, вызванной с неё.
    let (_canvas, content) = text_canvas(document, page_id)?;

    let shows = show_owners(&content);
    let mut owners: Vec<Option<i64>> = Vec::new();
    for (_, owner) in &shows {
        if !owners.contains(owner) {
            owners.push(*owner);
        }
    }
    if owners.iter().all(Option::is_none) {
        return Ok(Vec::new());
    }

    let base = extract_page(document, page_number)?;
    let base_page = base
        .page_iter()
        .next()
        .ok_or_else(|| anyhow!("в выжимке страницы нет страниц"))?;

    // Углы и стили, записанные в метках. Метке без угла остаётся ноль.
    let mut rotations: HashMap<i64, f32> = HashMap::new();
    let mut styles: HashMap<i64, Option<i64>> = HashMap::new();
    let mut aligns: HashMap<i64, Option<Align>> = HashMap::new();
    for operation in &content.operations {
        if operation.operator == "BDC"
            && let Some(id) = mark_id_of(operation)
        {
            rotations
                .entry(id)
                .or_insert_with(|| mark_rotation_of(operation));
            styles.entry(id).or_insert_with(|| mark_style_of(operation));
            aligns.entry(id).or_insert_with(|| mark_align_of(operation));
        }
    }

    let mut variants = Vec::with_capacity(owners.len());
    for owner in owners {
        let mut ops = content.operations.clone();
        for (index, show_owner) in &shows {
            if *show_owner != owner {
                clear_text_operand(&mut ops[*index]);
            }
        }
        let encoded = Content { operations: ops }
            .encode()
            .map_err(|e| anyhow!("не удалось собрать вариант страницы: {e}"))?;
        let mut variant = base.clone();
        variant
            .change_page_content(base_page, encoded)
            .map_err(|e| anyhow!("не удалось записать вариант страницы: {e}"))?;
        variants.push(MarkVariant {
            owner,
            rotation: owner
                .and_then(|id| rotations.get(&id).copied())
                .unwrap_or(0.0),
            style: owner.and_then(|id| styles.get(&id).copied().flatten()),
            align: owner.and_then(|id| aligns.get(&id).copied().flatten()),
            document: variant,
        });
    }
    Ok(variants)
}

/// Владелец каждого оператора показа текста: номер нашей метки либо `None`.
fn show_owners(content: &Content) -> Vec<(usize, Option<i64>)> {
    let mut owners = Vec::new();
    let mut open: Vec<Option<i64>> = Vec::new();
    for (index, operation) in content.operations.iter().enumerate() {
        match operation.operator.as_str() {
            "BDC" | "BMC" => open.push(mark_id_of(operation)),
            "EMC" => {
                open.pop();
            }
            "Tj" | "TJ" | "'" | "\"" => {
                owners.push((index, open.iter().rev().find_map(|entry| *entry)));
            }
            _ => {}
        }
    }
    owners
}

/// Номер нашей метки в операндах BDC; `None` для чужих секций.
fn mark_id_of(operation: &Operation) -> Option<i64> {
    match operation.operands.first() {
        Some(Object::Name(tag)) if tag == MARK_TAG => {}
        _ => return None,
    }
    let Some(Object::Dictionary(properties)) = operation.operands.get(1) else {
        return None;
    };
    properties
        .get(b"ID")
        .ok()
        .and_then(|value| value.as_i64().ok())
}

/// Номер для следующей метки: больше всех уже занятых на странице.
fn next_mark_id(content: &Content) -> i64 {
    content
        .operations
        .iter()
        .filter_map(mark_id_of)
        .max()
        .unwrap_or(0)
        + 1
}

/// Открывает метку блока. Кроме номера в ней хранится угол поворота:
/// повёрнутый текст при разборе страницы пропускается — модель абзацев
/// горизонтальна, — и без записанного угла повёрнутый блок стал бы невидим
/// и невыделяем сразу после правки.
fn mark_open(id: i64, rotation: f32, style: Option<i64>, align: Option<Align>) -> Operation {
    let mut properties = dictionary! { "ID" => id, "R" => rotation };
    // Номер стиля — связь блока с каталогом стилей документа. Стиль без
    // блока и блок без стиля равно возможны, поэтому ключ необязателен.
    if let Some(style) = style {
        properties.set("St", style);
    }
    // Выключка хранится в метке затем, что по геометрии её не всегда видно:
    // у блока в одну строку «по центру» и «влево» неотличимы, и без записи
    // выбор пользователя пропадал при следующем разборе страницы.
    if let Some(align) = align {
        properties.set("A", align as i64);
    }
    Operation::new(
        "BDC",
        vec![
            Object::Name(MARK_TAG.to_vec()),
            Object::Dictionary(properties),
        ],
    )
}

/// Выключка из метки, если записана.
fn mark_align_of(operation: &Operation) -> Option<Align> {
    let Some(Object::Dictionary(properties)) = operation.operands.get(1) else {
        return None;
    };
    match properties.get(b"A") {
        Ok(Object::Integer(0)) => Some(Align::Left),
        Ok(Object::Integer(1)) => Some(Align::Center),
        Ok(Object::Integer(2)) => Some(Align::Right),
        Ok(Object::Integer(3)) => Some(Align::Justify),
        _ => None,
    }
}

/// Номер стиля из метки, если записан.
fn mark_style_of(operation: &Operation) -> Option<i64> {
    let Some(Object::Dictionary(properties)) = operation.operands.get(1) else {
        return None;
    };
    match properties.get(b"St") {
        Ok(Object::Integer(value)) => Some(*value),
        _ => None,
    }
}

/// Угол поворота из метки; ноль, если не записан.
fn mark_rotation_of(operation: &Operation) -> f32 {
    let Some(Object::Dictionary(properties)) = operation.operands.get(1) else {
        return 0.0;
    };
    match properties.get(b"R") {
        Ok(Object::Real(value)) => *value,
        Ok(Object::Integer(value)) => *value as f32,
        _ => 0.0,
    }
}

fn mark_close() -> Operation {
    Operation::new("EMC", vec![])
}

/// Запрос на удаление блока: текст пропадает, страница остаётся прежней.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockErase {
    /// Номер страницы, начиная с единицы.
    pub page_number: u32,
    pub bbox: Rect,
    pub owner: Option<i64>,
}

/// Стирает блок со страницы.
///
/// Операторы показа опустошаются, а не вырезаются: так не сдвигается ни один
/// сосед по потоку, и вся остальная страница остаётся байт в байт прежней —
/// та же дисциплина, что у перенабора.
pub fn erase_block(document: &mut Document, request: &BlockErase) -> Result<usize> {
    let page_id = page_id(document, request.page_number)?;
    // Текст может лежать не в потоке страницы, а в форме, вызванной с неё.
    let (canvas, content) = text_canvas(document, page_id)?;

    let hits = find_hits(&content, request.bbox, request.owner);
    if hits.is_empty() {
        bail!("в границах блока нет текста, который можно удалить");
    }

    let mut operations = content.operations;
    for hit in &hits {
        clear_text_operand(&mut operations[hit.index]);
    }

    let cleared = hits.len();
    let encoded = Content { operations }
        .encode()
        .map_err(|e| anyhow!("не удалось собрать поток страницы: {e}"))?;
    write_canvas(document, &canvas, encoded)?;
    Ok(cleared)
}

/// Перестановка абзаца без перенабора: перенос, поворот, цвет.

/// Перестановка абзаца без перенабора: перенос, поворот, цвет.
///
/// Это принципиально другой путь, чем [`BlockRewrite`], и нужен он вот почему.
/// Перенабор берёт текст блока и выкладывает его заново одним оформлением —
/// а значит, применим только там, где всё оформление и было одним. Стоит
/// абзацу содержать хоть одно слово другим шрифтом или кеглем, как перенабор
/// пришлось бы отклонить: он растерял бы различия.
///
/// Но чтобы **сдвинуть** абзац, перенабирать его незачем. Достаточно поправить
/// координаты у тех операторов, которые уже стоят в потоке: шрифты, кегли,
/// подъёмы, межбуквенные интервалы — всё остаётся ровно тем же, потому что его
/// никто не трогает. Поэтому перенос, поворот и смена цвета работают на любом
/// абзаце, даже на пёстром.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockTransform {
    /// Номер страницы, начиная с единицы.
    pub page_number: u32,
    /// Где текст стоит сейчас.
    pub bbox: Rect,
    /// Куда его переставить. Ширина не важна: перестановка не переносит строки.
    pub target: Rect,
    /// Поворот в градусах против часовой стрелки, вокруг центра `target`.
    pub rotation: f32,
    /// Новый цвет текста, если его меняли.
    pub color: Option<[f32; 3]>,
    /// Владелец переставляемого текста — та же дисциплина, что у перенабора:
    /// чужой текст в той же рамке остаётся на месте.
    pub owner: Option<i64>,
}

/// Что сделала перестановка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransformOutcome {
    /// Сколько независимых кусков текста переставлено.
    pub moved_runs: usize,
}

/// Переставляет абзац, ничего в нём не перенабирая.
///
/// Правится только положение: у операторов `Tm`, задающих начало кусков текста
/// внутри рамки, пересчитываются координаты. Всё остальное содержимое потока
/// остаётся байт в байт прежним.
///
/// Отказ возможен ровно в одном случае: если кусок текста начинается внутри
/// рамки, но продолжается за её пределами. Сдвинуть такой кусок значило бы
/// утащить с собой чужой текст, и лучше ничего не делать.
pub fn transform_block(
    document: &mut Document,
    request: &BlockTransform,
) -> Result<TransformOutcome> {
    let page_id = page_id(document, request.page_number)?;
    // Текст может лежать не в потоке страницы, а в форме, вызванной с неё.
    let (canvas, content) = text_canvas(document, page_id)?;

    let runs = find_runs(&content, request.bbox, request.owner);
    if runs.inside.is_empty() {
        bail!("в рамке блока нет текста, который можно переставить");
    }
    if !runs.straddling.is_empty() {
        bail!(
            "кусок текста начинается в рамке блока и продолжается снаружи              ({} шт.). Перестановка отклонена: иначе уехал бы и соседний текст",
            runs.straddling.len()
        );
    }

    let dx = request.target.left - request.bbox.left;
    let dy = request.target.top - request.bbox.top;
    let centre = (
        request.target.center_x(),
        (request.target.top + request.target.bottom) * 0.5,
    );

    let mark_id = next_mark_id(&content);
    let wanted_rotation = normalise_degrees(request.rotation);
    let mut operations = content.operations;
    let mut moved = 0;
    // Вставки собираются одним списком и вносятся одним проходом: каждая
    // вставка сдвигает номера операторов, и порознь их пришлось бы
    // пересчитывать после каждой.
    let mut insertions: Vec<(usize, Operation)> = Vec::new();

    // Показ с переходом (`'`/`"`) двигать нечем: он рисует сам, и заменить
    // его позиционирующим оператором нельзя.
    if runs.inside.iter().any(|run| run.opener == RunOpener::Show) {
        bail!(
            "строка позиционирована оператором показа с переходом — такой текст пока не переставляется"
        );
    }

    for run in &runs.inside {
        let Some(inverse) = invert(run.ctm) else {
            continue;
        };
        let rendered = multiply(run.matrix, run.ctm);

        // Угол задан абсолютно, но кусок может быть уже повёрнут — тогда
        // докручивается только разница. Иначе повторное «применить» с тем же
        // углом крутило бы блок дальше и дальше.
        let current = rendered[1].atan2(rendered[0]).to_degrees();
        let delta = normalise_degrees(wanted_rotation - current);
        let linear = rotate_linear(rendered, delta);
        let (x, y) = rotate_about((rendered[4] + dx, rendered[5] + dy), centre, delta);
        let placed = multiply([linear[0], linear[1], linear[2], linear[3], x, y], inverse);

        operations[run.index] = Operation::new(
            "Tm",
            placed.iter().map(|v| Object::Real(*v)).collect::<Vec<_>>(),
        );
        // `TD` заодно ставил интерлиньяж — его обязаны увидеть последующие
        // `T*`, иначе поедут строки ниже.
        if let RunOpener::TdWithLeading(leading) = run.opener {
            insertions.push((run.index, Operation::new("TL", vec![Object::Real(leading)])));
        }
        moved += 1;

        // Переставленный кусок получает метку блока: без неё наложенные
        // абзацы при следующем разборе слились бы в один. Кускам, уже
        // лежащим в метке, вторая не нужна.
        let after_last = run
            .shows
            .last()
            .map(|index| index + 1)
            .unwrap_or(run.index + 1);
        if run.mark_bdc.is_none() {
            insertions.push((run.index, mark_open(mark_id, wanted_rotation, None, None)));
        }
        if let Some(color) = request.color {
            if let Some(first) = run.shows.first() {
                insertions.push((*first, color_operation(color)));
            }
            if let (Some(previous), Some(_)) = (&run.previous_color, run.shows.last()) {
                insertions.push((after_last, previous.clone()));
            }
        }
        if run.mark_bdc.is_none() {
            insertions.push((after_last, mark_close()));
        }
    }

    // Якорение хвоста: относительный `Td` сразу после сдвинутого куска
    // продолжил бы от новой матрицы и уехал бы следом. Первому же чужому
    // куску в той же секции BT его абсолютная матрица ставится явно —
    // дальше всё считается уже от него.
    let last_moved_by_bt: HashMap<usize, usize> = runs
        .inside
        .iter()
        .map(|run| (run.bt_serial, run.index))
        .fold(HashMap::new(), |mut acc, (bt, index)| {
            let slot = acc.entry(bt).or_insert(index);
            *slot = (*slot).max(index);
            acc
        });
    let mut followers: Vec<&TextRun> = runs
        .outside
        .iter()
        .chain(runs.straddling.iter())
        .filter(|run| {
            last_moved_by_bt
                .get(&run.bt_serial)
                .is_some_and(|last| run.index > *last)
        })
        .collect();
    followers.sort_by_key(|run| run.index);
    let mut anchored_bts: Vec<usize> = Vec::new();
    for follower in followers {
        if anchored_bts.contains(&follower.bt_serial) {
            continue;
        }
        anchored_bts.push(follower.bt_serial);
        match follower.opener {
            RunOpener::Tm => {}
            RunOpener::Show => bail!(
                "за переставляемым текстом идёт показ с переходом — перенос отклонён, иначе уехали бы соседние строки"
            ),
            RunOpener::Td | RunOpener::NextLine | RunOpener::TdWithLeading(_) => {
                operations[follower.index] = Operation::new(
                    "Tm",
                    follower
                        .matrix
                        .iter()
                        .map(|v| Object::Real(*v))
                        .collect::<Vec<_>>(),
                );
                if let RunOpener::TdWithLeading(leading) = follower.opener {
                    insertions.push((
                        follower.index,
                        Operation::new("TL", vec![Object::Real(leading)]),
                    ));
                }
            }
        }
    }

    apply_insertions(&mut operations, insertions);

    // У кусков, уже лежавших в метках, обёртка не добавлялась — но угол в их
    // метках устарел. Перезаписываем словари этих меток: по записанному углу
    // блок восстанавливается при следующем разборе страницы.
    let owned_ids: Vec<i64> = runs.inside.iter().filter_map(|run| run.mark_id).collect();
    for operation in operations.iter_mut() {
        if operation.operator == "BDC"
            && let Some(id) = mark_id_of(operation)
            && owned_ids.contains(&id)
        {
            operation.operands[1] =
                Object::Dictionary(dictionary! { "ID" => id, "R" => wanted_rotation });
        }
    }

    let encoded = Content { operations }
        .encode()
        .map_err(|e| anyhow!("не удалось собрать поток страницы: {e}"))?;
    write_canvas(document, &canvas, encoded)?;

    Ok(TransformOutcome { moved_runs: moved })
}

/// Чем открыт кусок текста: каким оператором задано его начало.
#[derive(Debug, Clone, Copy, PartialEq)]
enum RunOpener {
    /// Абсолютная матрица `Tm`.
    Tm,
    /// Относительный сдвиг `Td`; в `matrix` уже вычисленная абсолютная.
    Td,
    /// `TD` — то же, но с побочной установкой интерлиньяжа.
    TdWithLeading(f32),
    /// Переход на новую строку `T*`.
    NextLine,
    /// Показ с переходом `'` или `"`: он рисует сам и заменить его на `Tm`
    /// нельзя.
    Show,
}

/// Кусок текста, начатый одним позиционирующим оператором.
#[derive(Debug, Clone)]
struct TextRun {
    /// Место открывшего оператора в потоке.
    index: usize,
    /// Чем кусок открыт. Для переноса `Tm`, `Td`, `TD` и `T*` равноправны:
    /// каждый заменяется абсолютной матрицей.
    opener: RunOpener,
    /// Порядковый номер секции BT: якорить хвост нужно только внутри своей.
    bt_serial: usize,
    matrix: Matrix,
    ctm: Matrix,
    /// Места операторов показа, относящихся к этому куску.
    shows: Vec<usize>,
    /// Цвет заливки, действовавший до куска, — его надо вернуть после.
    previous_color: Option<Operation>,
    /// Место BDC нашей метки, внутри которой лежит кусок, если она есть.
    mark_bdc: Option<usize>,
    /// Номер той же метки.
    mark_id: Option<i64>,
}

#[derive(Default)]
struct Runs {
    /// Куски, целиком лежащие в рамке.
    inside: Vec<TextRun>,
    /// Куски, что начинаются в рамке, а кончаются вне её.
    straddling: Vec<TextRun>,
    /// Куски вне рамки — по ним якорится хвост после сдвинутого текста:
    /// относительный `Td` следом за переставленным куском уехал бы вместе
    /// с ним.
    outside: Vec<TextRun>,
}

/// Разбирает поток на куски, начатые оператором `Tm`, и раскладывает их по
/// признаку «целиком внутри рамки».
///
/// Куски, целиком лежащие снаружи, не интересны и не собираются.
fn find_runs(content: &Content, bbox: Rect, owner: Option<i64>) -> Runs {
    let outer = bbox.inflate(HIT_SLACK);

    let mut runs = Runs::default();
    let mut ctm = IDENTITY;
    let mut ctm_stack: Vec<Matrix> = Vec::new();
    let mut color_stack: Vec<Option<Operation>> = Vec::new();
    let mut fill_color: Option<Operation> = None;
    // Открытые секции marked content: чтобы не оборачивать в метку то, что в
    // ней уже лежит. Для наших запоминается и место BDC: понадобится, когда
    // у метки придётся переписать угол.
    let mut open_marks: Vec<Option<(usize, i64)>> = Vec::new();

    let mut text_matrix = IDENTITY;
    let mut line_matrix = IDENTITY;
    let mut leading = 0.0f32;
    let mut current: Option<TextRun> = None;
    let mut inside = 0usize;
    let mut outside = 0usize;

    let close = |current: &mut Option<TextRun>,
                 inside: &mut usize,
                 outside: &mut usize,
                 runs: &mut Runs| {
        if let Some(run) = current.take() {
            if *inside > 0 {
                if *outside > 0 {
                    runs.straddling.push(run)
                } else {
                    runs.inside.push(run)
                }
            } else if !run.shows.is_empty() {
                runs.outside.push(run);
            }
        }
        *inside = 0;
        *outside = 0;
    };
    let mut bt_serial = 0usize;

    for (index, operation) in content.operations.iter().enumerate() {
        match operation.operator.as_str() {
            "q" => {
                ctm_stack.push(ctm);
                color_stack.push(fill_color.clone());
            }
            "Q" => {
                ctm = ctm_stack.pop().unwrap_or(IDENTITY);
                fill_color = color_stack.pop().flatten();
            }
            "cm" => {
                let v = numbers(operation);
                if v.len() == 6 {
                    ctm = multiply([v[0], v[1], v[2], v[3], v[4], v[5]], ctm);
                }
            }
            "rg" | "g" | "k" | "sc" | "scn" | "cs" => fill_color = Some(operation.clone()),
            "BDC" | "BMC" => {
                open_marks.push(mark_id_of(operation).map(|id| (index, id)));
            }
            "EMC" => {
                open_marks.pop();
            }
            "BT" => {
                close(&mut current, &mut inside, &mut outside, &mut runs);
                text_matrix = IDENTITY;
                line_matrix = IDENTITY;
                bt_serial += 1;
            }
            "ET" => close(&mut current, &mut inside, &mut outside, &mut runs),
            "TL" => leading = numbers(operation).first().copied().unwrap_or(leading),
            "Tm" => {
                let v = numbers(operation);
                if v.len() == 6 {
                    close(&mut current, &mut inside, &mut outside, &mut runs);
                    line_matrix = [v[0], v[1], v[2], v[3], v[4], v[5]];
                    text_matrix = line_matrix;
                    current = Some(TextRun {
                        index,
                        opener: RunOpener::Tm,
                        bt_serial,
                        matrix: line_matrix,
                        ctm,
                        shows: Vec::new(),
                        previous_color: fill_color.clone(),
                        mark_bdc: open_marks
                            .iter()
                            .rev()
                            .find_map(|entry| entry.map(|(at, _)| at)),
                        mark_id: open_marks
                            .iter()
                            .rev()
                            .find_map(|entry| entry.map(|(_, id)| id)),
                    });
                }
            }
            "Td" | "TD" => {
                let v = numbers(operation);
                if v.len() == 2 {
                    if operation.operator == "TD" {
                        leading = -v[1];
                    }
                    line_matrix = multiply([1.0, 0.0, 0.0, 1.0, v[0], v[1]], line_matrix);
                    text_matrix = line_matrix;
                    // Каждый сдвиг строки — начало собственного куска: у
                    // страниц без `Tm` иначе вовсе не было бы чего двигать.
                    // В `matrix` кладётся уже вычисленная абсолютная
                    // матрица — при переносе она встанет на место `Td` как
                    // равнозначный `Tm`.
                    close(&mut current, &mut inside, &mut outside, &mut runs);
                    current = Some(TextRun {
                        index,
                        opener: if operation.operator == "TD" {
                            RunOpener::TdWithLeading(leading)
                        } else {
                            RunOpener::Td
                        },
                        bt_serial,
                        matrix: line_matrix,
                        ctm,
                        shows: Vec::new(),
                        previous_color: fill_color.clone(),
                        mark_bdc: open_marks
                            .iter()
                            .rev()
                            .find_map(|entry| entry.map(|(at, _)| at)),
                        mark_id: open_marks
                            .iter()
                            .rev()
                            .find_map(|entry| entry.map(|(_, id)| id)),
                    });
                }
            }
            "T*" => {
                line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -leading], line_matrix);
                text_matrix = line_matrix;
                close(&mut current, &mut inside, &mut outside, &mut runs);
                current = Some(TextRun {
                    index,
                    opener: RunOpener::NextLine,
                    bt_serial,
                    matrix: line_matrix,
                    ctm,
                    shows: Vec::new(),
                    previous_color: fill_color.clone(),
                    mark_bdc: open_marks
                        .iter()
                        .rev()
                        .find_map(|entry| entry.map(|(at, _)| at)),
                    mark_id: open_marks
                        .iter()
                        .rev()
                        .find_map(|entry| entry.map(|(_, id)| id)),
                });
            }
            "Tj" | "TJ" | "'" | "\"" => {
                if matches!(operation.operator.as_str(), "'" | "\"") {
                    line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -leading], line_matrix);
                    text_matrix = line_matrix;
                    // Показ с переходом сам открывает строку. Двигать его
                    // нельзя — он рисует; такой кусок только якорит хвост.
                    close(&mut current, &mut inside, &mut outside, &mut runs);
                    current = Some(TextRun {
                        index,
                        opener: RunOpener::Show,
                        bt_serial,
                        matrix: line_matrix,
                        ctm,
                        shows: Vec::new(),
                        previous_color: fill_color.clone(),
                        mark_bdc: open_marks
                            .iter()
                            .rev()
                            .find_map(|entry| entry.map(|(at, _)| at)),
                        mark_id: open_marks
                            .iter()
                            .rev()
                            .find_map(|entry| entry.map(|(_, id)| id)),
                    });
                }
                // Чужие показы прозрачны: они не считаются ни своими, ни
                // соседскими — правка их просто не видит.
                let show_owner = open_marks
                    .iter()
                    .rev()
                    .find_map(|entry| entry.map(|(_, id)| id));
                if show_owner != owner {
                    continue;
                }
                let (x, y) = apply_origin(multiply(text_matrix, ctm));
                if outer.contains(x, y) {
                    inside += 1;
                } else {
                    outside += 1;
                }
                // Показ запоминается в своём куске в обеих ветках: куски вне
                // рамки нужны якорению хвоста, а кусок без показов — пустышка.
                if let Some(run) = current.as_mut() {
                    run.shows.push(index);
                }
            }
            _ => {}
        }
    }
    close(&mut current, &mut inside, &mut outside, &mut runs);

    runs
}

/// Вносит вставки в поток одним проходом.
///
/// Позиции считаются по нумерации **до** вставок; при равных позициях порядок
/// добавления сохраняется. Цвет в PDF — состояние, а не свойство буквы,
/// поэтому пары «новый цвет перед куском, прежний после» тоже идут через этот
/// механизм: вернуть старый цвет обязательно, иначе перекрасился бы весь
/// текст ниже по странице.
fn apply_insertions(operations: &mut Vec<Operation>, mut insertions: Vec<(usize, Operation)>) {
    if insertions.is_empty() {
        return;
    }
    insertions.sort_by_key(|(at, _)| *at);

    let mut rebuilt = Vec::with_capacity(operations.len() + insertions.len());
    let mut pending = insertions.into_iter().peekable();
    for (index, operation) in operations.drain(..).enumerate() {
        while pending.peek().is_some_and(|(at, _)| *at == index) {
            rebuilt.push(pending.next().expect("проверено выше").1);
        }
        rebuilt.push(operation);
    }
    for (_, operation) in pending {
        rebuilt.push(operation);
    }
    *operations = rebuilt;
}

/// Обратное преобразование. `None`, если матрица вырождена.
fn invert(m: Matrix) -> Option<Matrix> {
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-9 {
        return None;
    }
    let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
    Some([a, b, c, d, -(m[4] * a + m[5] * c), -(m[4] * b + m[5] * d)])
}

/// Допуск при проверке «начало текста внутри рамки блока». Рамка построена по
/// габаритам глифов, а начало отсчёта строки лежит на базовой линии, ниже
/// верха и левее первого штриха у символов с отрицательным левым выносом.
const HIT_SLACK: f32 = 3.0;

pub fn rewrite_block(document: &mut Document, request: &BlockRewrite) -> Result<RewriteOutcome> {
    let page_id = page_id(document, request.page_number)?;
    // Текст может лежать не в потоке страницы, а в форме, вызванной с неё.
    let (canvas, content) = text_canvas(document, page_id)?;

    let hits = find_hits(&content, request.bbox, request.owner);
    if hits.is_empty() && !request.create {
        bail!("в границах блока нет операторов показа текста");
    }

    let first = hits.first();

    // Разное оформление внутри рамки — не повод для отказа: спаны запроса
    // сами несут стили кусков, вплоть до гарнитур из документа, и перенабор
    // воспроизводит пёстрый абзац как есть. Отказ остался только у текста,
    // выходящего за рамку, — его проверила предыдущая ступень.

    let first = first.cloned();
    let mut resolved = resolve_styles(document, page_id, first.as_ref(), request)?;
    // Панель свойств задаёт разрядку и масштаб явно; без явного значения
    // блок сохраняет свои.
    if let Some(spacing) = request.char_spacing {
        resolved.char_spacing = spacing;
    }
    if let Some(scale_percent) = request.h_scale {
        resolved.h_scale = scale_percent;
    }

    // Недостающие глифы ищем у каждого куска своим шрифтом.
    for (index, span) in request.spans.iter().enumerate() {
        let style = &resolved.styles[index];
        let missing = resolved.encoders[style.slot].missing(&span.text);
        if missing.is_empty() {
            continue;
        }
        let listed: String = missing.iter().collect();
        match &span.font {
            Some(wanted) => bail!(
                "в шрифте «{}» нет символов: {listed}. Выберите гарнитуру, в которой они есть",
                wanted.family
            ),
            None => bail!(
                "во встроенном шрифте нет символов: {listed}. Это подмножество — в файл попали \
                 только использованные глифы. Выберите гарнитуру в панели формата, и шрифт \
                 будет встроен целиком"
            ),
        }
    }

    // У нового блока опоры в потоке нет: кегль берётся из первого куска,
    // а без него — двенадцать, обычный книжный по умолчанию.
    let base_size = first
        .as_ref()
        .map(|hit| hit.visible_size())
        .or_else(|| request.spans.iter().find_map(|span| span.size))
        .unwrap_or(12.0);
    let lines = wrap_spans(request, &resolved);
    let line_height = request.line_height.unwrap_or(base_size * 1.2);
    let cleared = hits.len();

    let mut operations = content.operations;
    for hit in &hits {
        clear_text_operand(&mut operations[hit.index]);
    }

    // Новый текст дописывается отдельным блоком в конец потока. Оборачиваем в
    // q/Q: состояние графики к концу потока может быть любым. Метка вокруг —
    // имя блока в самом файле: по ней наложенные абзацы не склеиваются при
    // следующем разборе.
    operations.push(mark_open(
        next_mark_id(&Content {
            operations: operations.clone(),
        }),
        normalise_degrees(request.rotation),
        request.style_id,
        Some(request.align),
    ));
    operations.push(Operation::new("q", vec![]));

    // Кегль в `Tf` задаётся с оглядкой на масштаб матрицы: если исходный текст
    // написан приёмом «Tf 1 + масштаб в Tm», то и новый должен следовать ему,
    // иначе получится текст размером в один пункт.
    //
    // Угол — абсолютный, а не прибавка к матрице: матрица уже повёрнутого
    // абзаца несёт старый угол, и композиция удваивала бы его при каждой
    // правке. От матрицы берутся только масштабы, поворот строится заново.
    let linear = absolute_linear(
        first.as_ref().map(|hit| hit.matrix).unwrap_or(IDENTITY),
        request.rotation,
    );
    let scale = matrix_scale(linear).max(0.0001);

    // Текст ложится в рамку назначения: она совпадает с исходной, пока блок
    // не двигали маркерами.
    let target = request.target();

    // Фон за буквами кладётся первым, до BT: четырёхугольник рамки, при
    // повороте блока повёрнутый вместе с ним. Он живёт внутри метки блока и
    // при следующем перенаборе честно заменится вместе с текстом.
    if let Some([r, g, b]) = request.fill {
        let centre_for_fill = (target.center_x(), (target.top + target.bottom) * 0.5);
        let corners = [
            (target.left, target.bottom),
            (target.right, target.bottom),
            (target.right, target.top),
            (target.left, target.top),
        ]
        .map(|corner| rotate_about(corner, centre_for_fill, request.rotation));
        operations.push(Operation::new(
            "rg",
            vec![Object::Real(r), Object::Real(g), Object::Real(b)],
        ));
        operations.push(Operation::new(
            "m",
            vec![Object::Real(corners[0].0), Object::Real(corners[0].1)],
        ));
        for corner in &corners[1..] {
            operations.push(Operation::new(
                "l",
                vec![Object::Real(corner.0), Object::Real(corner.1)],
            ));
        }
        operations.push(Operation::new("h", vec![]));
        operations.push(Operation::new("f", vec![]));
    }
    operations.push(Operation::new("BT", vec![]));
    let ascent = resolved.encoders[resolved.styles[0].slot].ascent(base_size);
    let mut baseline = target.top - ascent;
    let created = lines.len();

    // Поворот отсчитывается от центра рамки, а не от начала строки: иначе
    // повёрнутый абзац уезжал бы прочь от того места, где его повернули.
    // Строки раскладываются в неповёрнутых координатах, а затем начало каждой
    // поворачивается вокруг центра — это и есть поворот блока целиком.
    let centre = (target.center_x(), (target.top + target.bottom) * 0.5);
    let max_width = target.width().max(1.0);

    // Подчёркивания рисуются линиями после текста: в PDF это не свойство
    // символов. Копим их вместе с положением строки.
    let mut underlines: Vec<Underline> = Vec::new();

    // Разрядка и масштаб пишутся один раз на блок: внутри q/Q они не утекут
    // наружу.
    if resolved.char_spacing != 0.0 {
        operations.push(Operation::new(
            "Tc",
            vec![Object::Real(resolved.char_spacing / scale)],
        ));
    }
    if resolved.h_scale != 100.0 {
        operations.push(Operation::new("Tz", vec![Object::Real(resolved.h_scale)]));
    }

    let para_spacing = request.para_spacing.unwrap_or(0.0);
    for line in &lines {
        // Выключка сдвигает начало строки внутри рамки. Ширину строки надо
        // знать до вывода, поэтому она считается отдельным проходом.
        let line_width: f32 = line
            .fragments
            .iter()
            .map(|f| resolved.width(f.style, &f.text))
            .sum();
        let indent = match request.align {
            // Выключку по формату честнее показать как левую: растягивать
            // пробелы нечем — межсловный интервал в поток пока не пишется.
            Align::Left | Align::Justify => 0.0,
            Align::Center => (max_width - line_width) * 0.5,
            Align::Right => max_width - line_width,
        }
        .max(0.0);

        let origin = rotate_about((target.left + indent, baseline), centre, request.rotation);
        operations.push(Operation::new(
            "Tm",
            vec![
                Object::Real(linear[0]),
                Object::Real(linear[1]),
                Object::Real(linear[2]),
                Object::Real(linear[3]),
                Object::Real(origin.0),
                Object::Real(origin.1),
            ],
        ));

        // Внутри одного BT текстовая матрица сдвигается сама после каждого
        // показа, поэтому куски одной строки идут подряд без нового Tm.
        let mut applied: Option<AppliedStyle> = None;
        let mut offset = 0.0f32;

        for fragment in &line.fragments {
            let style = &resolved.styles[fragment.style];
            let wanted = AppliedStyle {
                slot: style.slot,
                size: style.effective_size(),
                color: style.color.clone(),
                rise: style.size * style.script.rise(),
            };

            if applied.as_ref().map(|a| (a.slot, a.size)) != Some((wanted.slot, wanted.size)) {
                operations.push(Operation::new(
                    "Tf",
                    vec![
                        Object::Name(resolved.names[wanted.slot].clone().into_bytes()),
                        Object::Real(wanted.size / scale),
                    ],
                ));
            }
            let color_changed = !same_operation(
                applied.as_ref().and_then(|style| style.color.as_ref()),
                wanted.color.as_ref(),
            );
            if color_changed && let Some(color) = &wanted.color {
                operations.push(color.clone());
            }
            if applied.as_ref().map(|a| a.rise) != Some(wanted.rise) {
                operations.push(Operation::new(
                    "Ts",
                    vec![Object::Real(wanted.rise / scale)],
                ));
            }

            operations.push(Operation::new(
                "Tj",
                vec![Object::String(
                    resolved.encoders[style.slot].encode(&fragment.text),
                    lopdf::StringFormat::Hexadecimal,
                )],
            ));

            let width = resolved.width(fragment.style, &fragment.text);
            if style.underline {
                underlines.push(Underline {
                    origin,
                    from: offset,
                    to: offset + width,
                    size: style.size,
                    color: style.color.clone(),
                });
            }
            offset += width;
            applied = Some(wanted);
        }

        // Подъём обязан вернуться к нулю: иначе он утечёт на следующую строку.
        if applied.map(|a| a.rise).unwrap_or(0.0) != 0.0 {
            operations.push(Operation::new("Ts", vec![Object::Real(0.0)]));
        }
        // Жёсткий перенос добавляет отбивку — по одной на каждый Enter,
        // так что пустая строка между абзацами превращается в двойную.
        baseline -= line_height + para_spacing * line.hard_breaks as f32;
    }

    operations.push(Operation::new("ET", vec![]));
    emit_underlines(&mut operations, &underlines, linear, scale);
    operations.push(Operation::new("Q", vec![]));
    operations.push(mark_close());

    let encoded = Content { operations }
        .encode()
        .map_err(|e| anyhow!("не удалось собрать поток страницы: {e}"))?;
    write_canvas(document, &canvas, encoded)?;

    Ok(RewriteOutcome {
        cleared_ops: cleared,
        created_lines: created,
        height: created as f32 * line_height,
    })
}

/// Чем набран абзац — чтобы показать его правку тем же шрифтом, а не
/// подстановкой.
#[derive(Clone)]
pub struct BlockFont {
    /// Имя из PDF как есть: `Inter-Medium`, `ABCDEF+Gotham-Bold`.
    pub base_font: String,
    /// Семейство по данным самой программы шрифта. `None`, если программа не
    /// встроена — тогда гарнитуру придётся искать в системе.
    pub family: Option<String>,
    /// Видимый кегль в пунктах.
    pub size: f32,
    /// Цвет текста, если его удалось прочитать.
    pub color: Option<[f32; 3]>,
    /// Встроенная программа шрифта.
    pub program: Option<Vec<u8>>,
    /// Метрики шрифта: по ним редактор считает, куда встанет каретка.
    pub metrics: Option<Encoder>,
    /// Межбуквенный интервал блока в пунктах.
    pub char_spacing: f32,
    /// Горизонтальный масштаб блока в процентах.
    pub h_scale: f32,
}

impl std::fmt::Debug for BlockFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Программу шрифта и метрики печатать нечего — это сотни килобайт.
        f.debug_struct("BlockFont")
            .field("base_font", &self.base_font)
            .field("family", &self.family)
            .field("size", &self.size)
            .field("color", &self.color)
            .field("embedded", &self.program.is_some())
            .finish()
    }
}

/// Перечисляет шрифты всего документа — по одному на гарнитуру.
///
/// Ресурсы страниц перебираются подряд: один и тот же шрифт встречается на
/// десятках страниц, и в описи он появляется один раз со счётчиком страниц.
/// Проверка системы идёт через тот же поиск семейства, каким пользуется
/// подстановка при наборе, — чтобы окно «Шрифты» показывало ровно то, на что
/// редактор способен на деле.
pub fn document_fonts(document: &Document) -> Vec<crate::fonts::DocumentFont> {
    let system = crate::fonts::system_fonts();
    let mut found: Vec<crate::fonts::DocumentFont> = Vec::new();

    for page_id in document.page_iter() {
        let Ok(fonts) = page_fonts(document, page_id) else {
            continue;
        };
        // Один шрифт может стоять на странице под несколькими именами —
        // страницу он всё равно занимает одну.
        let mut counted: Vec<String> = Vec::new();

        for font_id in fonts.values() {
            let Ok(font) = document.get_object(*font_id).and_then(|o| o.as_dict()) else {
                continue;
            };
            let raw = font
                .get(b"BaseFont")
                .ok()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_else(|| "(без имени)".to_owned());
            let subset = raw.contains('+');
            let base_font = raw.split('+').next_back().unwrap_or(&raw).to_owned();
            if counted.contains(&base_font) {
                continue;
            }
            counted.push(base_font.clone());

            let subtype = font
                .get(b"Subtype")
                .ok()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();

            if let Some(existing) = found.iter_mut().find(|f| f.base_font == base_font) {
                existing.pages += 1;
                continue;
            }

            found.push(crate::fonts::DocumentFont {
                embedded: face_of(document, font).is_some(),
                system_family: system.find_family(&base_font).map(|f| f.to_owned()),
                base_font,
                subtype,
                subset,
                pages: 1,
            });
        }
    }

    // Сперва проблемные — с них и начинают разбираться.
    found.sort_by(|a, b| {
        b.is_missing()
            .cmp(&a.is_missing())
            .then(b.pages.cmp(&a.pages))
            .then(a.base_font.cmp(&b.base_font))
    });
    found
}

/// Метрики всех шрифтов абзаца, по одному на гарнитуру.
///
/// Пёстрый абзац — обычный, полужирный, курсив — редактор обязан мерить
/// метриками каждого куска, а не одного доминантного: полужирные буквы шире,
/// и раскладка с одним общим шрифтом расходилась с настоящей страницей —
/// слова «прыгали», подсветка выделения ехала мимо строк.
pub fn block_metrics(
    document: &Document,
    page_number: u32,
    bbox: Rect,
    owner: Option<i64>,
) -> Result<Vec<(String, Encoder)>> {
    let page_id = page_id(document, page_number)?;
    // Текст может лежать не в потоке страницы, а в форме, вызванной с неё.
    let (_canvas, content) = text_canvas(document, page_id)?;

    let hits = find_hits(&content, bbox, owner);
    let fonts = page_fonts(document, page_id)?;

    let mut seen: Vec<String> = Vec::new();
    let mut result = Vec::new();
    for hit in &hits {
        if seen.contains(&hit.font_name) {
            continue;
        }
        seen.push(hit.font_name.clone());
        let Some(font_ref) = fonts.get(&hit.font_name).copied() else {
            continue;
        };
        let Ok(font) = document.get_object(font_ref).and_then(|o| o.as_dict()) else {
            continue;
        };
        let base_font = font
            .get(b"BaseFont")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_else(|| hit.font_name.clone());
        let clean = base_font
            .split('+')
            .next_back()
            .unwrap_or(&base_font)
            .to_owned();
        if let Ok(metrics) = Encoder::load(document, font_ref) {
            result.push((clean, metrics));
        }
    }
    Ok(result)
}

/// Поток, в котором лежит текст страницы.
///
/// Обычно это поток самой страницы. Но Word, 1С и генераторы отчётов кладут
/// всё содержимое в форму (`Form XObject`) и вызывают её со страницы одним
/// оператором `Do` — тогда в потоке страницы текста нет вовсе, и править
/// нужно поток формы. Снаружи разницы быть не должно: холст прячет её.
pub(crate) enum Canvas {
    /// Текст прямо в потоке страницы.
    Page(ObjectId),
    /// Текст в форме, вызванной со страницы.
    Form(ObjectId),
}

/// Есть ли в потоке хоть один показ текста.
fn shows_text(content: &Content) -> bool {
    content
        .operations
        .iter()
        .any(|op| matches!(op.operator.as_str(), "Tj" | "TJ" | "'" | "\""))
}

/// Выбирает холст страницы и отдаёт его содержимое.
///
/// Сперва смотрит поток самой страницы: если текст там, ничего не меняется.
/// Иначе обходит формы, вызванные со страницы, и берёт первую, где текст
/// есть. Вложенность в один уровень покрывает всё, что встречается на
/// практике: генераторы заворачивают страницу в одну форму.
pub(crate) fn text_canvas(document: &Document, page_id: ObjectId) -> Result<(Canvas, Content)> {
    let page_content = Content::decode(&document.get_page_content(page_id))
        .map_err(|e| anyhow!("не удалось разобрать поток страницы: {e}"))?;
    if shows_text(&page_content) {
        return Ok((Canvas::Page(page_id), page_content));
    }

    for form_id in page_forms(document, page_id) {
        let Ok(stream) = document.get_object(form_id).and_then(|o| o.as_stream()) else {
            continue;
        };
        let Ok(bytes) = stream.decompressed_content() else {
            continue;
        };
        let Ok(form_content) = Content::decode(&bytes) else {
            continue;
        };
        if shows_text(&form_content) {
            return Ok((Canvas::Form(form_id), form_content));
        }
    }

    // Текста нет нигде — отдаём страницу, как было: сообщение об ошибке
    // сформирует вызывающий, ему виднее, что именно не вышло.
    Ok((Canvas::Page(page_id), page_content))
}

/// Формы, вызванные со страницы, в порядке появления в ресурсах.
fn page_forms(document: &Document, page_id: ObjectId) -> Vec<ObjectId> {
    let mut forms = Vec::new();
    let Ok(resources) = document.get_page_resources(page_id) else {
        return forms;
    };

    let mut collect = |dict: &Dictionary| {
        if let Ok(Object::Dictionary(xobjects)) = dict.get(b"XObject") {
            for (_, value) in xobjects.iter() {
                if let Object::Reference(id) = value
                    && document
                        .get_object(*id)
                        .and_then(|o| o.as_stream())
                        .map(|stream| {
                            stream
                                .dict
                                .get(b"Subtype")
                                .and_then(|o| o.as_name())
                                .map(|name| name == b"Form")
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
                {
                    forms.push(*id);
                }
            }
        }
    };

    if let Some(dict) = resources.0 {
        collect(dict);
    }
    for id in resources.1 {
        if let Ok(dict) = document.get_object(id).and_then(|o| o.as_dict()) {
            collect(dict);
        }
    }
    forms
}

/// Записывает переписанный поток обратно в холст.
pub(crate) fn write_canvas(
    document: &mut Document,
    canvas: &Canvas,
    encoded: Vec<u8>,
) -> Result<()> {
    match canvas {
        Canvas::Page(page_id) => document
            .change_page_content(*page_id, encoded)
            .map_err(|e| anyhow!("не удалось записать поток страницы: {e}")),
        Canvas::Form(form_id) => {
            let stream = document
                .get_object_mut(*form_id)
                .and_then(|o| o.as_stream_mut())
                .map_err(|e| anyhow!("форма недоступна для записи: {e}"))?;
            stream.set_plain_content(encoded);
            // Сжатие снимается вместе с содержимым: иначе читатель развернёт
            // старые байты по фильтру, которого больше нет.
            stream.dict.remove(b"Filter");
            Ok(())
        }
    }
}

/// Читает оформление абзаца, ничего не меняя.
pub fn block_font(
    document: &Document,
    page_number: u32,
    bbox: Rect,
    owner: Option<i64>,
) -> Result<BlockFont> {
    let page_id = page_id(document, page_number)?;
    // Текст может лежать не в потоке страницы, а в форме, вызванной с неё.
    let (_canvas, content) = text_canvas(document, page_id)?;

    let hits = find_hits(&content, bbox, owner);
    let first = hits
        .first()
        .ok_or_else(|| anyhow!("в границах блока нет текста"))?;

    let fonts = page_fonts(document, page_id)?;
    let font_ref = fonts
        .get(&first.font_name)
        .copied()
        .ok_or_else(|| anyhow!("шрифт /{} не найден в ресурсах страницы", first.font_name))?;
    let font = document
        .get_object(font_ref)
        .and_then(|o| o.as_dict())
        .map_err(|e| anyhow!("шрифт недоступен: {e}"))?;

    let base_font = font
        .get(b"BaseFont")
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .unwrap_or_else(|| first.font_name.clone());

    let face = face_of(document, font);
    let family = face.as_ref().and_then(|face| face.family());
    let program = face.map(|face| face.data);

    let metrics = Encoder::load(document, font_ref).ok();

    Ok(BlockFont {
        base_font,
        family,
        size: first.visible_size(),
        color: first.fill_color.as_ref().and_then(color_components),
        program,
        metrics,
        char_spacing: first.char_spacing,
        h_scale: first.h_scale,
    })
}

/// Три компоненты цвета из оператора заливки, если он их несёт.
///
/// CMYK разбирается наравне с RGB: в книгах, свёрстанных под печать, цвет
/// почти всегда задан именно им, и без этого цвет абзаца выглядел бы
/// неизвестным на большинстве настоящих документов.
fn color_components(operation: &Operation) -> Option<[f32; 3]> {
    let values = numbers(operation);
    match (operation.operator.as_str(), values.len()) {
        ("rg", 3) | ("sc", 3) | ("scn", 3) => Some([values[0], values[1], values[2]]),
        ("g", 1) | ("sc", 1) | ("scn", 1) => Some([values[0], values[0], values[0]]),
        ("k", 4) | ("sc", 4) | ("scn", 4) => {
            Some(cmyk_to_rgb(values[0], values[1], values[2], values[3]))
        }
        _ => None,
    }
}

/// Простое преобразование CMYK в RGB, без учёта цветового профиля.
/// Для показа квадратика с цветом абзаца этого достаточно.
fn cmyk_to_rgb(c: f32, m: f32, y: f32, k: f32) -> [f32; 3] {
    let ink = 1.0 - k.clamp(0.0, 1.0);
    [
        (1.0 - c.clamp(0.0, 1.0)) * ink,
        (1.0 - m.clamp(0.0, 1.0)) * ink,
        (1.0 - y.clamp(0.0, 1.0)) * ink,
    ]
}

/// Программа шрифта с учётом составных шрифтов: у них она лежит у потомка.
fn face_of(document: &Document, font: &Dictionary) -> Option<FaceData> {
    if let Some(face) = font_program(document, font) {
        return Some(face);
    }
    let descendants = font
        .get(b"DescendantFonts")
        .map(|o| resolve(document, o))
        .ok()?;
    let first = descendants.as_array().ok()?.first()?;
    let descendant = resolve(document, first).as_dict().ok()?;
    font_program(document, descendant)
}

fn page_id(document: &Document, page_number: u32) -> Result<ObjectId> {
    document
        .get_pages()
        .get(&page_number)
        .copied()
        .ok_or_else(|| anyhow!("в документе нет страницы {page_number}"))
}

/// Разрешённое оформление всех кусков абзаца.
struct Resolved {
    /// Межбуквенный интервал абзаца в пунктах страницы. Разрядку заголовков
    /// делают именно им, и перенабор без него сжимал «T H E» в «THE».
    char_spacing: f32,
    /// Горизонтальный масштаб `Tz` в процентах: сжатый на 90% текст обязан
    /// остаться сжатым, а ширины строк — считаться с тем же множителем.
    h_scale: f32,
    /// Кодировщики по слотам; слот — это уникальный шрифт.
    encoders: Vec<Encoder>,
    /// Имена ресурсов, параллельно `encoders`.
    names: Vec<String>,
    /// Оформление, параллельно кускам запроса.
    styles: Vec<ResolvedStyle>,
}

impl Resolved {
    fn width(&self, span: usize, text: &str) -> f32 {
        let style = &self.styles[span];
        // Разрядка прибавляется после каждого знака, а горизонтальный масштаб
        // умножает всё разом — так же считает и сам растеризатор, поэтому
        // перенос строк сходится с отрисовкой.
        let spacing = self.char_spacing * text.chars().count() as f32;
        (self.encoders[style.slot].width(text, style.effective_size()) + spacing)
            * (self.h_scale / 100.0)
    }
}

struct ResolvedStyle {
    slot: usize,
    size: f32,
    color: Option<Operation>,
    script: Script,
    underline: bool,
}

impl ResolvedStyle {
    /// Кегль с поправкой на индекс: он набирается мельче основного.
    fn effective_size(&self) -> f32 {
        self.size * self.script.scale()
    }
}

/// Оформление, уже выставленное операторами. Нужно, чтобы не повторять `Tf`,
/// цвет и подъём на каждом куске.
struct AppliedStyle {
    slot: usize,
    size: f32,
    color: Option<Operation>,
    rise: f32,
}

/// Кусок строки с одним оформлением.
struct Fragment {
    style: usize,
    text: String,
}

/// Линия подчёркивания в координатах строки.
struct Underline {
    /// Начало строки на странице, уже с учётом выключки и поворота.
    origin: (f32, f32),
    from: f32,
    to: f32,
    size: f32,
    color: Option<Operation>,
}

/// Оператор показа текста, попавший в рамку блока.
#[derive(Debug, Clone)]
struct Hit {
    index: usize,
    font_name: String,
    /// Кегль ровно так, как он стоял в `Tf`.
    ///
    /// Это **не** видимый размер текста: распространённый приём — написать
    /// `Tf /F1 1`, а настоящий кегль задать масштабом текстовой матрицы.
    /// Видимый размер даёт [`Hit::visible_size`].
    tf_size: f32,
    /// Матрица отрисовки текста, `Tm × CTM`. Несёт в себе и масштаб, и
    /// поворот, и положение.
    matrix: Matrix,
    fill_color: Option<Operation>,
    /// Межбуквенный интервал `Tc`, действовавший на этом показе.
    char_spacing: f32,
    /// Горизонтальный масштаб `Tz` в процентах.
    h_scale: f32,
}

impl Hit {
    /// Вертикальный масштаб матрицы отрисовки.
    fn scale(&self) -> f32 {
        (self.matrix[2] * self.matrix[2] + self.matrix[3] * self.matrix[3]).sqrt()
    }

    /// Кегль, каким его видит читатель, в пунктах страницы.
    fn visible_size(&self) -> f32 {
        self.tf_size * self.scale()
    }
}

/// Матрица PDF: a b c d e f.
type Matrix = [f32; 6];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn multiply(m: Matrix, n: Matrix) -> Matrix {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

fn apply_origin(m: Matrix) -> (f32, f32) {
    (m[4], m[5])
}

fn numbers(operation: &Operation) -> Vec<f32> {
    operation
        .operands
        .iter()
        .filter_map(|o| match o {
            Object::Integer(v) => Some(*v as f32),
            Object::Real(v) => Some(*v),
            _ => None,
        })
        .collect()
}

/// Проходит поток, отслеживая состояние текста и графики, и собирает
/// операторы показа, чьё начало лежит внутри рамки блока.
fn find_hits(content: &Content, bbox: Rect, owner: Option<i64>) -> Vec<Hit> {
    let outer = bbox.inflate(HIT_SLACK);

    let mut hits = Vec::new();
    let mut ctm = IDENTITY;
    let mut ctm_stack: Vec<Matrix> = Vec::new();
    let mut color_stack: Vec<Option<Operation>> = Vec::new();
    let mut fill_color: Option<Operation> = None;
    // Владельцы открытых секций marked content: показы чужих владельцев для
    // этой правки прозрачны, в какой бы прямоугольник они ни попали.
    let mut open_marks: Vec<Option<i64>> = Vec::new();

    let mut text_matrix = IDENTITY;
    let mut line_matrix = IDENTITY;
    let mut font_name = String::new();
    let mut size = 0.0f32;
    let mut leading = 0.0f32;
    let mut char_spacing = 0.0f32;
    let mut h_scale = 100.0f32;

    for (index, operation) in content.operations.iter().enumerate() {
        match operation.operator.as_str() {
            "q" => {
                ctm_stack.push(ctm);
                color_stack.push(fill_color.clone());
            }
            "Q" => {
                ctm = ctm_stack.pop().unwrap_or(IDENTITY);
                fill_color = color_stack.pop().flatten();
            }
            "cm" => {
                let v = numbers(operation);
                if v.len() == 6 {
                    ctm = multiply([v[0], v[1], v[2], v[3], v[4], v[5]], ctm);
                }
            }
            // Цвет заливки запоминаем целиком, чтобы повторить его дословно.
            "rg" | "g" | "k" | "sc" | "scn" | "cs" => fill_color = Some(operation.clone()),
            "BDC" | "BMC" => open_marks.push(mark_id_of(operation)),
            "EMC" => {
                open_marks.pop();
            }
            "BT" => {
                text_matrix = IDENTITY;
                line_matrix = IDENTITY;
            }
            "Tf" => {
                if let Some(Object::Name(name)) = operation.operands.first() {
                    font_name = String::from_utf8_lossy(name).into_owned();
                }
                size = numbers(operation).first().copied().unwrap_or(size);
            }
            "TL" => leading = numbers(operation).first().copied().unwrap_or(leading),
            "Tc" => char_spacing = numbers(operation).first().copied().unwrap_or(char_spacing),
            "Tz" => h_scale = numbers(operation).first().copied().unwrap_or(h_scale),
            "Tm" => {
                let v = numbers(operation);
                if v.len() == 6 {
                    line_matrix = [v[0], v[1], v[2], v[3], v[4], v[5]];
                    text_matrix = line_matrix;
                }
            }
            "Td" => {
                let v = numbers(operation);
                if v.len() == 2 {
                    line_matrix = multiply([1.0, 0.0, 0.0, 1.0, v[0], v[1]], line_matrix);
                    text_matrix = line_matrix;
                }
            }
            "TD" => {
                let v = numbers(operation);
                if v.len() == 2 {
                    leading = -v[1];
                    line_matrix = multiply([1.0, 0.0, 0.0, 1.0, v[0], v[1]], line_matrix);
                    text_matrix = line_matrix;
                }
            }
            "T*" => {
                line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -leading], line_matrix);
                text_matrix = line_matrix;
            }
            "Tj" | "TJ" | "'" | "\"" => {
                if matches!(operation.operator.as_str(), "'" | "\"") {
                    line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -leading], line_matrix);
                    text_matrix = line_matrix;
                }

                let show_owner = open_marks.iter().rev().find_map(|entry| *entry);
                let render_matrix = multiply(text_matrix, ctm);
                let (x, y) = apply_origin(render_matrix);
                if show_owner == owner && outer.contains(x, y) {
                    hits.push(Hit {
                        index,
                        font_name: font_name.clone(),
                        tf_size: size,
                        matrix: render_matrix,
                        fill_color: fill_color.clone(),
                        char_spacing,
                        h_scale,
                    });
                }
            }
            _ => {}
        }
    }

    hits
}

/// Опустошает строковый операнд оператора показа, не трогая сам оператор.
///
/// Именно «опустошает», а не удаляет: операторы показа сдвигают текстовую
/// матрицу, и выкидывание оператора целиком поехало бы по позициям у всего,
/// что идёт следом в том же блоке.
fn clear_text_operand(operation: &mut Operation) {
    let empty = Object::String(Vec::new(), lopdf::StringFormat::Literal);
    match operation.operator.as_str() {
        "Tj" => operation.operands = vec![empty],
        "'" => operation.operands = vec![empty],
        "\"" => {
            let v = numbers(operation);
            operation.operands = vec![
                Object::Real(v.first().copied().unwrap_or(0.0)),
                Object::Real(v.get(1).copied().unwrap_or(0.0)),
                empty,
            ];
        }
        "TJ" => operation.operands = vec![Object::Array(Vec::new())],
        _ => {}
    }
}

/// Карта «имя ресурса → идентификатор объекта шрифта» для страницы.
/// Слот существующего шрифта страницы для гарнитуры из документа.
///
/// Сопоставление идёт по **полному** имени, вместе с начертанием: сравнение
/// по семейству без стилевых хвостов сваливало «Inter-Bold», «Inter-SemiBold»
/// и «Inter-Medium» в один ключ, и блок перенабирался чужим подмножеством —
/// у того нет половины глифов, и «GUIDE TO AI» превращался в «I AI».
/// Не найденный шрифт — честный отказ, а не молчаливая подмена.
fn page_font_slot(
    document: &Document,
    fonts: &HashMap<String, ObjectId>,
    family: &str,
    encoders: &mut Vec<Encoder>,
    names: &mut Vec<String>,
    page_slots: &mut HashMap<String, usize>,
) -> Result<usize> {
    let wanted = full_font_key(family);
    if let Some(slot) = page_slots.get(&wanted) {
        return Ok(*slot);
    }

    // Сначала точное совпадение, затем — единственный кандидат того же
    // семейства: pdfium иногда называет шрифт короче, чем BaseFont, и жёсткое
    // сравнение оставило бы такой блок без шрифта вовсе. Но при нескольких
    // кандидатах семейства выбор наугад запрещён — именно он терял глифы.
    let mut exact: Option<(String, ObjectId)> = None;
    let mut same_family: Vec<(String, ObjectId)> = Vec::new();
    for (name, font_id) in fonts {
        let Ok(font) = document.get_object(*font_id).and_then(|o| o.as_dict()) else {
            continue;
        };
        let base = font
            .get(b"BaseFont")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        // Префикс подмножества вида «FNPBYN+» отрезается перед сравнением.
        let clean = base.split('+').next_back().unwrap_or(&base);
        if full_font_key(clean) == wanted {
            exact = Some((name.clone(), *font_id));
            break;
        }
        if crate::fonts::family_key(clean) == crate::fonts::family_key(family) {
            same_family.push((name.clone(), *font_id));
        }
    }
    let chosen = exact.or_else(|| {
        if same_family.len() == 1 {
            same_family.pop()
        } else {
            None
        }
    });
    let Some((name, font_id)) = chosen else {
        bail!(
            "шрифт «{family}» не найден среди шрифтов страницы однозначно —              выберите гарнитуру в панели формата"
        );
    };

    let encoder = Encoder::load(document, font_id)?;
    encoders.push(encoder);
    names.push(name);
    let slot = encoders.len() - 1;
    page_slots.insert(wanted, slot);
    Ok(slot)
}

/// Полный ключ шрифта: буквы и цифры в нижнем регистре, начертание в составе.
///
/// В отличие от [`crate::fonts::family_key`], стилевые хвосты не отрезаются:
/// здесь ищется не «похожая гарнитура», а ровно тот шрифт, которым набран
/// кусок.
fn full_font_key(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn page_fonts(document: &Document, page_id: ObjectId) -> Result<HashMap<String, ObjectId>> {
    let mut fonts = HashMap::new();

    let resources = document
        .get_page_resources(page_id)
        .map_err(|e| anyhow!("не удалось прочитать ресурсы страницы: {e}"))?;

    let mut collect = |dict: &Dictionary| {
        if let Ok(Object::Dictionary(font_dict)) = dict.get(b"Font") {
            for (name, value) in font_dict.iter() {
                if let Object::Reference(id) = value {
                    fonts.insert(String::from_utf8_lossy(name).into_owned(), *id);
                }
            }
        }
    };

    if let Some(dict) = resources.0 {
        collect(dict);
    }
    for id in resources.1 {
        if let Ok(Object::Dictionary(dict)) = document.get_object(id) {
            collect(dict);
        }
    }

    // Шрифты формы: когда текст живёт в ней, `/F1` объявлен в её собственных
    // ресурсах, а не в ресурсах страницы.
    for form_id in page_forms(document, page_id) {
        let Ok(stream) = document.get_object(form_id).and_then(|o| o.as_stream()) else {
            continue;
        };
        match stream.dict.get(b"Resources") {
            Ok(Object::Dictionary(dict)) => collect(dict),
            Ok(Object::Reference(id)) => {
                if let Ok(dict) = document.get_object(*id).and_then(|o| o.as_dict()) {
                    collect(dict);
                }
            }
            _ => {}
        }
    }

    Ok(fonts)
}

/// Метрики шрифта абзаца.
///
/// Тем же самым меряет ширину строк и запись в поток содержимого, поэтому
/// каретка в редакторе встаёт ровно туда, где потом окажется буква. Системный
/// шрифт для этого не годится: его ширины другие, и текст на экране разошёлся
/// бы с напечатанным.
impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Внутри — таблицы ширин и программа шрифта на сотни килобайт;
        // печатать это некуда и незачем.
        f.write_str("Encoder")
    }
}

#[derive(Clone)]
pub enum Encoder {
    /// Однобайтная кодировка: в строку пишется код символа по кодировке шрифта.
    ///
    /// `codes` — соответствие «символ → код», развёрнутое из таблицы
    /// `ToUnicode` документа. Без него код приходилось бы угадывать по кодовой
    /// точке Unicode, а это верно только для латиницы: в самой ходовой
    /// кодировке WinAnsi апостроф «’» записывается байтом 0x92, а не 0x2019.
    Simple {
        widths: HashMap<u8, f32>,
        default_width: f32,
        face: Option<FaceData>,
        codes: HashMap<char, u8>,
    },
    /// Составной шрифт с Identity-H: в строке лежат двухбайтные номера глифов.
    ///
    /// `to_unicode` — соответствие «символ → номер глифа», развёрнутое из
    /// таблицы `ToUnicode` документа. Оно главнее таблицы `cmap` самой
    /// программы шрифта: в подмножествах с `Identity-H` эту таблицу почти
    /// всегда вырезают за ненадобностью, и по ней не находится вообще ничего —
    /// даже буквы того текста, который на странице уже напечатан.
    Identity {
        face: FaceData,
        to_unicode: HashMap<char, u16>,
        /// Последовательности, у которых в шрифте один глиф: «fi», «ffi»…
        /// При записи они сворачиваются обратно в лигатуру — текст на странице
        /// остаётся байт в байт устойчивым к повторным правкам.
        ligatures: HashMap<String, u16>,
    },
}

/// Разобранная программа шрифта.
#[derive(Clone)]
pub struct FaceData {
    data: Vec<u8>,
    units_per_em: f32,
}

impl FaceData {
    fn parse(&self) -> Option<ttf_parser::Face<'_>> {
        ttf_parser::Face::parse(&self.data, 0).ok()
    }

    fn glyph(&self, ch: char) -> Option<u16> {
        self.parse()?.glyph_index(ch).map(|g| g.0)
    }

    /// Ширина глифа в долях кегля.
    fn advance(&self, gid: u16) -> f32 {
        self.parse()
            .and_then(|face| face.glyph_hor_advance(ttf_parser::GlyphId(gid)))
            .map(|advance| advance as f32 / self.units_per_em)
            .unwrap_or(0.5)
    }

    /// Типографское семейство по таблице `name` самой программы шрифта.
    fn family(&self) -> Option<String> {
        let face = self.parse()?;
        for id in [16u16, 1] {
            let name = face
                .names()
                .into_iter()
                .filter(|n| n.name_id == id)
                .find_map(|n| n.to_string())
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty());
            if name.is_some() {
                return name;
            }
        }
        None
    }

    fn ascent_ratio(&self) -> f32 {
        self.parse()
            .map(|face| face.ascender() as f32 / self.units_per_em)
            .unwrap_or(0.8)
    }
}

impl Encoder {
    fn load(document: &Document, font_id: ObjectId) -> Result<Encoder> {
        let font = document
            .get_object(font_id)
            .and_then(|o| o.as_dict())
            .map_err(|e| anyhow!("шрифт недоступен: {e}"))?;

        let subtype = font
            .get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();

        if subtype == "Type0" {
            let descendants = font
                .get(b"DescendantFonts")
                .map(|o| resolve(document, o))
                .and_then(|o| o.as_array())
                .map_err(|e| anyhow!("у составного шрифта нет потомков: {e}"))?;
            let first = descendants
                .first()
                .ok_or_else(|| anyhow!("список потомков составного шрифта пуст"))?;
            let descendant = resolve(document, first)
                .as_dict()
                .map_err(|e| anyhow!("потомок составного шрифта не словарь: {e}"))?;

            let face = font_program(document, descendant)
                .ok_or_else(|| anyhow!("в составной шрифт не встроена программа шрифта"))?;
            let maps = read_to_unicode(document, font);
            return Ok(Encoder::Identity {
                face,
                to_unicode: maps.singles,
                ligatures: maps.sequences,
            });
        }

        // Простой шрифт: ширины лежат в /Widths, индексируясь от /FirstChar.
        let first_char = font.get(b"FirstChar").and_then(|o| o.as_i64()).unwrap_or(0);
        let mut widths = HashMap::new();
        if let Ok(array) = font.get(b"Widths").and_then(|o| o.as_array()) {
            for (offset, value) in array.iter().enumerate() {
                let width = match value {
                    Object::Integer(v) => *v as f32,
                    Object::Real(v) => *v,
                    _ => continue,
                };
                let code = first_char + offset as i64;
                if (0..=255).contains(&code) {
                    widths.insert(code as u8, width / 1000.0);
                }
            }
        }

        let codes = simple_codes(document, font);
        let face = font_program(document, font);

        // Ширины не записаны — так бывает у базовых четырнадцати шрифтов
        // вроде Helvetica: их метрики принято знать наизусть. Наизусть мы их
        // не знаем, зато можем взять из начертания, которым текст и рисуется:
        // из встроенной программы, а без неё — из системного собрата (для
        // Helvetica это метрически совместимый Arial, его же подставляет и
        // pdfium). Без этого каждый знак мерился как пол-кегля, и каретка с
        // подсветкой выделения ехали мимо букв.
        if widths.is_empty() {
            let fallback = face.clone().or_else(|| {
                let name = font
                    .get(b"BaseFont")
                    .ok()
                    .and_then(|o| o.as_name().ok())
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .unwrap_or_default();
                let clean = name.split('+').next_back().unwrap_or(&name);
                let lower = clean.to_lowercase();
                // Сначала точное семейство, затем ближайшее по духу: самой
                // Helvetica в Windows нет, а метрически совместимый Arial
                // есть всегда — его же подставляет и растеризатор.
                let bold = lower.contains("bold");
                let italic = lower.contains("italic") || lower.contains("oblique");
                let fonts = crate::fonts::system_fonts();
                let info = fonts
                    .resolve(&crate::fonts::FontRequest::new(clean, bold, italic))
                    .cloned()
                    .or_else(|| {
                        let similar = crate::fonts::suggest_substitute(clean)?;
                        fonts
                            .resolve(&crate::fonts::FontRequest::new(&similar, bold, italic))
                            .cloned()
                    })?;
                let data = std::fs::read(&info.path).ok()?;
                let units_per_em = ttf_parser::Face::parse(&data, 0)
                    .map(|f| f.units_per_em() as f32)
                    .unwrap_or(1000.0);
                Some(FaceData { data, units_per_em })
            });
            if let Some(source) = fallback.as_ref()
                && let Ok(parsed) = ttf_parser::Face::parse(&source.data, 0)
            {
                for code in 32u16..=255 {
                    let ch = simple_char(&codes, code as u8);
                    if let Some(glyph) = parsed.glyph_index(ch)
                        && let Some(advance) = parsed.glyph_hor_advance(glyph)
                    {
                        widths.insert(code as u8, advance as f32 / source.units_per_em);
                    }
                }
            }
        }

        Ok(Encoder::Simple {
            widths,
            default_width: 0.5,
            face,
            codes,
        })
    }

    fn encode(&self, text: &str) -> Vec<u8> {
        match self {
            Encoder::Simple { codes, .. } => text
                .chars()
                .filter_map(|ch| simple_code(codes, ch))
                .collect(),
            Encoder::Identity {
                face,
                to_unicode,
                ligatures,
            } => {
                let mut bytes = Vec::new();
                for piece in split_ligatures(text, ligatures) {
                    let gid = match piece {
                        Piece::Ligature(gid) => gid,
                        Piece::Char(ch) => self_glyph(face, to_unicode, ch).unwrap_or(0),
                    };
                    bytes.extend_from_slice(&gid.to_be_bytes());
                }
                bytes
            }
        }
    }

    /// Символы, которых в шрифте нет.
    pub fn missing(&self, text: &str) -> Vec<char> {
        let mut missing = Vec::new();
        for ch in text.chars().filter(|c| !c.is_control()) {
            let present = match self {
                // У простого шрифта в строку пишется код символа, а не номер
                // глифа, и соответствие задаёт словарь шрифта. Судить о
                // наличии глифа по таблице `cmap` программы нельзя: в
                // подмножествах её вырезают, и тогда «отсутствующими»
                // оказываются даже те буквы, что уже напечатаны на странице.
                // Надёжный признак — массив ширин.
                Encoder::Simple { widths, codes, .. } => match simple_code(codes, ch) {
                    Some(code) => widths.is_empty() || widths.contains_key(&code),
                    None => false,
                },
                Encoder::Identity {
                    face, to_unicode, ..
                } => ch.is_whitespace() || self_glyph(face, to_unicode, ch).is_some(),
            };
            if !present && !missing.contains(&ch) {
                missing.push(ch);
            }
        }
        missing
    }

    /// Ширина строки в пунктах.
    pub fn width(&self, text: &str, size: f32) -> f32 {
        let em: f32 = match self {
            Encoder::Simple {
                widths,
                default_width,
                codes,
                ..
            } => text
                .chars()
                .map(|ch| {
                    simple_code(codes, ch)
                        .and_then(|code| widths.get(&code).copied())
                        .unwrap_or(*default_width)
                })
                .sum(),
            Encoder::Identity {
                face,
                to_unicode,
                ligatures,
            } => split_ligatures(text, ligatures)
                .into_iter()
                .map(|piece| match piece {
                    Piece::Ligature(gid) => face.advance(gid),
                    Piece::Char(ch) => self_glyph(face, to_unicode, ch)
                        .map(|gid| face.advance(gid))
                        .unwrap_or(0.5),
                })
                .sum(),
        };
        em * size
    }

    pub fn ascent(&self, size: f32) -> f32 {
        match self {
            Encoder::Simple {
                face: Some(face), ..
            }
            | Encoder::Identity { face, .. } => face.ascent_ratio() * size,
            Encoder::Simple { face: None, .. } => size * 0.8,
        }
    }
}

/// Собирает документ из одной страницы — для показа правки по ходу набора.
///
/// Перерисовать страницу после каждого нажатия иначе не выходит: сериализация
/// книги на две сотни мегабайт занимает четверть секунды, а одной страницы —
/// меньше миллисекунды. Обрезать копию целого документа тоже не годится:
/// обход всех объектов ради этого стоит несколько секунд. Поэтому копируется
/// только то, на что страница ссылается, — её собственное поддерево.
pub fn extract_page(document: &Document, page_number: u32) -> Result<Document> {
    let source_id = page_id(document, page_number)?;

    let mut out = Document::with_version("1.7");
    let mut copied: HashMap<ObjectId, ObjectId> = HashMap::new();
    let pages_id = out.new_object_id();

    let Ok(source) = document.get_object(source_id).and_then(|o| o.as_dict()) else {
        bail!("страница {page_number} недоступна");
    };

    // Ресурсы могут быть унаследованы от узла дерева — тогда в самой странице
    // их нет, и надо взять то, что она видит.
    let mut page = source.clone();
    if page.get(b"Resources").is_err()
        && let Some(inherited) = inherited_entry(document, source_id, b"Resources")
    {
        page.set("Resources", inherited);
    }
    if page.get(b"MediaBox").is_err()
        && let Some(media_box) = inherited_media_box(document, source_id)
    {
        page.set("MediaBox", media_box);
    }
    page.remove(b"Parent");
    page.remove(b"Annots");

    let mut copy = Object::Dictionary(page);
    copy_references(document, &mut out, &mut copy, &mut copied);
    let Object::Dictionary(mut page) = copy else {
        unreachable!("страница — словарь")
    };
    page.set("Parent", Object::Reference(pages_id));

    let new_page_id = out.add_object(Object::Dictionary(page));
    out.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(new_page_id)],
            "Count" => 1_i64,
        }),
    );
    let catalog = out.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    out.trailer.set("Root", Object::Reference(catalog));

    Ok(out)
}

/// Переносит все объекты, на которые ссылается значение, и подменяет ссылки.
pub(crate) fn copy_references(
    source: &Document,
    target: &mut Document,
    value: &mut Object,
    copied: &mut HashMap<ObjectId, ObjectId>,
) {
    match value {
        Object::Reference(id) => {
            if let Some(existing) = copied.get(id) {
                *value = Object::Reference(*existing);
                return;
            }
            let Ok(referenced) = source.get_object(*id) else {
                // Битая ссылка: заменяем пустотой, иначе читатель споткнётся.
                *value = Object::Null;
                return;
            };

            // Место занимается заранее: объекты в PDF ссылаются друг на друга
            // по кругу, и без этого обход не закончился бы.
            let new_id = target.new_object_id();
            copied.insert(*id, new_id);

            let mut clone = referenced.clone();
            copy_references(source, target, &mut clone, copied);
            target.objects.insert(new_id, clone);
            *value = Object::Reference(new_id);
        }
        Object::Array(items) => {
            for item in items {
                copy_references(source, target, item, copied);
            }
        }
        Object::Dictionary(dict) => {
            for (_, item) in dict.iter_mut() {
                copy_references(source, target, item, copied);
            }
        }
        Object::Stream(stream) => {
            for (_, item) in stream.dict.iter_mut() {
                copy_references(source, target, item, copied);
            }
        }
        _ => {}
    }
}

/// Размер страницы, унаследованный от узла дерева страниц.
fn inherited_media_box(document: &Document, page_id: ObjectId) -> Option<Object> {
    inherited_entry(document, page_id, b"MediaBox")
}

/// Значение, унаследованное страницей от дерева страниц.
///
/// `/Resources` и `/MediaBox` разрешено объявить один раз на узле дерева, и
/// тогда в самой странице их нет. Объявить их можно и ссылкой, а не значением,
/// — поэтому найденное здесь же и разыменовывается. Без этого выжимка страницы
/// оставалась без шрифтов, и показ падал на первом же `Tf`.
///
/// Поиск идёт снизу вверх и останавливается на первом найденном: ближайший к
/// странице узел главнее дальних, так устроено наследование в PDF. Ограничение
/// на глубину — от испорченных документов, где `/Parent` замкнут в кольцо.
fn inherited_entry(document: &Document, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut current = document.get_object(page_id).ok()?.as_dict().ok()?.clone();
    for _ in 0..32 {
        if let Ok(value) = current.get(key) {
            return Some(resolve(document, value).clone());
        }
        let parent = current.get(b"Parent").ok()?;
        current = resolve(document, parent).as_dict().ok()?.clone();
    }
    None
}

/// Код символа в однобайтной кодировке шрифта.
///
/// Сначала спрашиваем сам документ, потом — кодировку WinAnsi, и лишь затем
/// падаем на кодовую точку. Порядок именно такой: документ знает свою
/// кодировку точно, а догадки годятся только для латиницы.
fn simple_code(codes: &HashMap<char, u8>, ch: char) -> Option<u8> {
    if let Some(code) = codes.get(&ch) {
        return Some(*code);
    }
    if let Some(code) = win_ansi_code(ch) {
        return Some(code);
    }
    let point = ch as u32;
    (point < 256).then_some(point as u8)
}

/// Знак по однобайтному коду — обратная сторона [`simple_code`].
///
/// Сначала таблица документа, затем особые места WinAnsi, затем Latin-1, где
/// код равен кодовой точке.
fn simple_char(codes: &HashMap<char, u8>, code: u8) -> char {
    if let Some((ch, _)) = codes.iter().find(|(_, c)| **c == code) {
        return *ch;
    }
    if let Some((_, ch)) = WIN_ANSI_SPECIALS.iter().find(|(c, _)| *c == code) {
        return *ch;
    }
    code as char
}

/// Соответствие «символ → код» из таблицы `ToUnicode` шрифта.
///
/// У простого шрифта коды однобайтные, поэтому всё, что не влезает в байт,
/// отбрасывается: это либо чужая запись, либо разбор пошёл не туда.
fn simple_codes(document: &Document, font: &Dictionary) -> HashMap<char, u8> {
    read_to_unicode(document, font)
        .singles
        .into_iter()
        .filter_map(|(ch, code)| (code < 256).then_some((ch, code as u8)))
        .collect()
}

/// Знаки, которыми WinAnsi заполняет промежуток 0x80–0x9F.
///
/// Именно они и ломали правку: кавычки, тире и многоточие лежат здесь, а их
/// кодовые точки Unicode далеко за пределами байта. Остальная часть WinAnsi
/// совпадает с Latin-1, где код равен кодовой точке, и отдельной таблицы не
/// требует.
const WIN_ANSI_SPECIALS: [(u8, char); 27] = [
    (0x80, '\u{20ac}'),
    (0x82, '\u{201a}'),
    (0x83, '\u{0192}'),
    (0x84, '\u{201e}'),
    (0x85, '\u{2026}'),
    (0x86, '\u{2020}'),
    (0x87, '\u{2021}'),
    (0x88, '\u{02c6}'),
    (0x89, '\u{2030}'),
    (0x8a, '\u{0160}'),
    (0x8b, '\u{2039}'),
    (0x8c, '\u{0152}'),
    (0x8e, '\u{017d}'),
    (0x91, '\u{2018}'),
    (0x92, '\u{2019}'),
    (0x93, '\u{201c}'),
    (0x94, '\u{201d}'),
    (0x95, '\u{2022}'),
    (0x96, '\u{2013}'),
    (0x97, '\u{2014}'),
    (0x98, '\u{02dc}'),
    (0x99, '\u{2122}'),
    (0x9a, '\u{0161}'),
    (0x9b, '\u{203a}'),
    (0x9c, '\u{0153}'),
    (0x9e, '\u{017e}'),
    (0x9f, '\u{0178}'),
];

fn win_ansi_code(ch: char) -> Option<u8> {
    WIN_ANSI_SPECIALS
        .iter()
        .find_map(|(code, mapped)| (*mapped == ch).then_some(*code))
}

/// Кусок строки при записи: лигатурный глиф либо одиночный символ.
enum Piece {
    Ligature(u16),
    Char(char),
}

/// Режет строку на лигатуры и одиночные символы, жадно и с начала.
///
/// Жадность обязана быть по самой длинной лигатуре: в «affix» тройная «ffi»
/// главнее двойной «ff», иначе хвост «x» останется при чужом глифе. Порядок
/// проверки — от длинных ключей к коротким.
fn split_ligatures(text: &str, ligatures: &HashMap<String, u16>) -> Vec<Piece> {
    if ligatures.is_empty() {
        return text.chars().map(Piece::Char).collect();
    }
    let longest = ligatures
        .keys()
        .map(|key| key.chars().count())
        .max()
        .unwrap_or(0);

    let chars: Vec<char> = text.chars().collect();
    let mut pieces = Vec::with_capacity(chars.len());
    let mut at = 0;
    while at < chars.len() {
        let mut matched = None;
        for take in (2..=longest.min(chars.len() - at)).rev() {
            let candidate: String = chars[at..at + take].iter().collect();
            if let Some(gid) = ligatures.get(&candidate) {
                matched = Some((take, *gid));
                break;
            }
        }
        match matched {
            Some((take, gid)) => {
                pieces.push(Piece::Ligature(gid));
                at += take;
            }
            None => {
                pieces.push(Piece::Char(chars[at]));
                at += 1;
            }
        }
    }
    pieces
}

/// Номер глифа для символа: сначала по `ToUnicode` документа, затем по
/// таблице `cmap` самой программы шрифта.
fn self_glyph(face: &FaceData, to_unicode: &HashMap<char, u16>, ch: char) -> Option<u16> {
    to_unicode.get(&ch).copied().or_else(|| face.glyph(ch))
}

/// Читает таблицу `ToUnicode` и разворачивает её в «символ → номер глифа».
///
/// Таблица описывает обратное соответствие — по ней читатели извлекают текст.
/// Нам же нужно записывать, поэтому её приходится выворачивать. Формат — CMap
/// в PostScript-подобном синтаксисе с блоками `bfchar` и `bfrange`.
/// Развёрнутая таблица `ToUnicode`: прямые соответствия и лигатуры порознь.
#[derive(Default)]
struct ToUnicodeMaps {
    /// Глифы, отвечающие ровно одному символу.
    singles: HashMap<char, u16>,
    /// Глифы, разворачивающиеся в несколько символов: лигатуры вроде «ﬃ».
    ///
    /// Держать их отдельно жизненно важно. Если, как раньше, брать от
    /// значения первый символ, в карту попадает «f → глиф ﬃ» — и каждая
    /// правка превращает «f» в «ffi», а следующая — в «ffiffii», лавинообразно
    /// разъедая текст. Именно так выглядела порча абзацев при смене кегля
    /// туда-сюда.
    sequences: HashMap<String, u16>,
}

fn read_to_unicode(document: &Document, font: &Dictionary) -> ToUnicodeMaps {
    let Ok(object) = font.get(b"ToUnicode") else {
        return ToUnicodeMaps::default();
    };
    let Ok(stream) = resolve(document, object).as_stream() else {
        return ToUnicodeMaps::default();
    };
    let Ok(data) = stream.decompressed_content() else {
        return ToUnicodeMaps::default();
    };
    parse_to_unicode(&String::from_utf8_lossy(&data))
}

/// Разбирает текст CMap. Вынесено из [`read_to_unicode`] ради тестов: сюда
/// можно подать строку, не собирая документ.
fn parse_to_unicode(text: &str) -> ToUnicodeMaps {
    let mut maps = ToUnicodeMaps::default();

    for section in text.split("beginbfchar").skip(1) {
        let Some(body) = section.split("endbfchar").next() else {
            continue;
        };
        let codes = hex_tokens(body);
        // Записи идут парами: номер глифа, затем символы.
        for pair in codes.chunks_exact(2) {
            let (Some(gid), Some(value)) = (first_u16(&pair[0]), utf16_value(&pair[1])) else {
                continue;
            };
            let mut chars = value.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => {
                    maps.singles.entry(ch).or_insert(gid);
                }
                (Some(_), Some(_)) => {
                    maps.sequences.entry(value).or_insert(gid);
                }
                _ => {}
            }
        }
    }

    for section in text.split("beginbfrange").skip(1) {
        let Some(body) = section.split("endbfrange").next() else {
            continue;
        };
        let codes = hex_tokens(body);
        // Записи идут тройками: начало и конец диапазона глифов, затем символ,
        // с которого начинается соответствующий диапазон символов.
        for triple in codes.chunks_exact(3) {
            let (Some(from), Some(to), Some(ch)) = (
                first_u16(&triple[0]),
                first_u16(&triple[1]),
                first_char(&triple[2]),
            ) else {
                continue;
            };
            for step in 0..=to.saturating_sub(from) {
                let Some(mapped) = char::from_u32(ch as u32 + step as u32) else {
                    continue;
                };
                maps.singles.entry(mapped).or_insert(from + step);
            }
        }
    }

    maps
}

/// Все шестнадцатеричные строки `<...>` подряд.
fn hex_tokens(text: &str) -> Vec<Vec<u8>> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start + 1..].find('>') else {
            break;
        };
        let body = &rest[start + 1..start + 1 + end];
        let bytes: Vec<u8> = body
            .as_bytes()
            .chunks(2)
            .filter_map(|pair| {
                let text = std::str::from_utf8(pair).ok()?;
                u8::from_str_radix(text, 16).ok()
            })
            .collect();
        tokens.push(bytes);
        rest = &rest[start + 1 + end + 1..];
    }
    tokens
}

fn first_u16(bytes: &[u8]) -> Option<u16> {
    match bytes {
        [high, low, ..] => Some(u16::from_be_bytes([*high, *low])),
        [single] => Some(*single as u16),
        _ => None,
    }
}

/// Первый символ значения — для диапазонов `bfrange`, где значение всегда
/// начало последовательного ряда одиночных символов.
fn first_char(bytes: &[u8]) -> Option<char> {
    utf16_value(bytes)?.chars().next()
}

/// Полное значение записи, как оно есть: UTF-16BE, возможно несколько
/// символов разом.
fn utf16_value(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

/// Разыменовывает ссылку, если объект ею является.
fn resolve<'a>(document: &'a Document, object: &'a Object) -> &'a Object {
    match object {
        Object::Reference(id) => document.get_object(*id).unwrap_or(object),
        other => other,
    }
}

/// Достаёт встроенную программу шрифта из дескриптора.
fn font_program(document: &Document, font: &Dictionary) -> Option<FaceData> {
    let descriptor = font
        .get(b"FontDescriptor")
        .map(|o| resolve(document, o))
        .ok()
        .and_then(|o| o.as_dict().ok())?;

    for key in [
        b"FontFile2".as_slice(),
        b"FontFile3".as_slice(),
        b"FontFile".as_slice(),
    ] {
        let Ok(object) = descriptor.get(key) else {
            continue;
        };
        let Ok(stream) = resolve(document, object).as_stream() else {
            continue;
        };
        let Ok(data) = stream.decompressed_content() else {
            continue;
        };

        let units_per_em = ttf_parser::Face::parse(&data, 0)
            .map(|face| face.units_per_em() as f32)
            .unwrap_or(1000.0);
        return Some(FaceData { data, units_per_em });
    }
    None
}

/// Разрешает оформление каждого куска: подбирает шрифт, кегль и цвет.
fn resolve_styles(
    document: &mut Document,
    page_id: ObjectId,
    first: Option<&Hit>,
    request: &BlockRewrite,
) -> Result<Resolved> {
    let mut encoders = Vec::new();
    let mut names = Vec::new();

    // Слот 0 — шрифт самого абзаца. Куски без явной гарнитуры берут его, и
    // тогда в файл ничего не добавляется. У нового блока абзаца нет — там
    // каждый кусок обязан назвать гарнитуру сам.
    let fonts = page_fonts(document, page_id)?;
    let base_slot = match first {
        Some(first) => {
            let base_ref = fonts.get(&first.font_name).copied().ok_or_else(|| {
                anyhow!("шрифт /{} не найден в ресурсах страницы", first.font_name)
            })?;
            encoders.push(Encoder::load(document, base_ref)?);
            names.push(first.font_name.clone());
            Some(0)
        }
        None => None,
    };

    let mut slots: HashMap<FontRequest, usize> = HashMap::new();
    let mut page_slots: HashMap<String, usize> = HashMap::new();
    let base_size = first
        .map(|hit| hit.visible_size())
        .or_else(|| request.spans.iter().find_map(|span| span.size))
        .unwrap_or(12.0);
    let mut styles = Vec::with_capacity(request.spans.len());

    for span in &request.spans {
        let slot = match &span.font {
            None => match &span.page_family {
                Some(family) => page_font_slot(
                    document,
                    &fonts,
                    family,
                    &mut encoders,
                    &mut names,
                    &mut page_slots,
                )?,
                None => base_slot.ok_or_else(|| {
                    anyhow!("для нового блока выберите гарнитуру в панели формата")
                })?,
            },
            Some(wanted) => match slots.get(wanted) {
                Some(slot) => *slot,
                None => {
                    let system = crate::fonts::system_fonts();
                    let face = system
                        .resolve(wanted)
                        .ok_or_else(|| anyhow!("шрифт «{}» не найден в системе", wanted.family))?;
                    let path = face.path.clone();
                    let font_id = crate::embed::embed_truetype_file(document, &path)?;
                    let name = crate::embed::register_on_page(document, page_id, font_id)?;
                    encoders.push(Encoder::load(document, font_id)?);
                    names.push(name);
                    let slot = encoders.len() - 1;
                    slots.insert(wanted.clone(), slot);
                    slot
                }
            },
        };

        styles.push(ResolvedStyle {
            slot,
            size: span.size.unwrap_or(base_size),
            color: span
                .color
                .map(color_operation)
                .or_else(|| first.and_then(|hit| hit.fill_color.clone())),
            script: span.script,
            underline: span.underline,
        });
    }

    Ok(Resolved {
        char_spacing: first.map(|hit| hit.char_spacing).unwrap_or(0.0),
        h_scale: first.map(|hit| hit.h_scale).unwrap_or(100.0),
        encoders,
        names,
        styles,
    })
}

/// Совпадают ли операторы. `Operation` из lopdf не сравнивается сам по себе,
/// а нам нужно лишь понять, не изменился ли цвет.
fn same_operation(a: Option<&Operation>, b: Option<&Operation>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.operator == b.operator && numbers(a) == numbers(b),
        _ => false,
    }
}

fn color_operation(rgb: [f32; 3]) -> Operation {
    Operation::new(
        "rg",
        vec![
            Object::Real(rgb[0]),
            Object::Real(rgb[1]),
            Object::Real(rgb[2]),
        ],
    )
}

/// Разбивка на строки с учётом того, что у каждого куска своя метрика.
/// Строка после переноса — с пометкой, был ли перенос жёстким (Enter).
/// По жёстким переносам считается отбивка между абзацами.
struct WrappedLine {
    fragments: Vec<Fragment>,
    /// Сколько жёстких переносов стояло сразу после этой строки: каждый
    /// прибавляет одну отбивку, и пустая строка из двойного Enter не
    /// теряется, а превращается в двойной интервал.
    hard_breaks: u32,
}

fn wrap_spans(request: &BlockRewrite, resolved: &Resolved) -> Vec<WrappedLine> {
    let max_width = request.target().width().max(1.0);
    let mut lines: Vec<WrappedLine> = Vec::new();
    let mut current: Vec<Fragment> = Vec::new();
    let mut width = 0.0f32;

    let push_line = |line: Vec<Fragment>, lines: &mut Vec<WrappedLine>, hard: bool| {
        if line.is_empty() {
            // Пустая строка от двойного Enter: её жёсткий перенос копится на
            // предыдущей строке.
            if hard && let Some(last) = lines.last_mut() {
                last.hard_breaks += 1;
            }
            return;
        }
        lines.push(WrappedLine {
            fragments: line,
            hard_breaks: u32::from(hard),
        });
    };

    for (index, span) in request.spans.iter().enumerate() {
        for (paragraph, text) in span.text.split('\n').enumerate() {
            if paragraph > 0 {
                push_line(std::mem::take(&mut current), &mut lines, true);
                width = 0.0;
            }
            for word in text.split_whitespace() {
                // Пробел приклеивается к слову и меряется его же шрифтом:
                // иначе на стыке кусков разного кегля он был бы не тот.
                let spaced = !current.is_empty();
                let piece = if spaced {
                    format!(" {word}")
                } else {
                    word.to_owned()
                };
                let piece_width = resolved.width(index, &piece);

                if spaced && width + piece_width > max_width {
                    push_line(std::mem::take(&mut current), &mut lines, false);
                    width = resolved.width(index, word);
                    current.push(Fragment {
                        style: index,
                        text: word.to_owned(),
                    });
                } else {
                    width += piece_width;
                    append_fragment(&mut current, index, &piece);
                }
            }
        }
    }
    push_line(current, &mut lines, false);

    if lines.is_empty() {
        lines.push(WrappedLine {
            fragments: vec![Fragment {
                style: 0,
                text: String::new(),
            }],
            hard_breaks: 0,
        });
    }
    lines
}

/// Дописывает текст к последнему куску строки, если оформление то же.
fn append_fragment(line: &mut Vec<Fragment>, style: usize, text: &str) {
    match line.last_mut() {
        Some(last) if last.style == style => last.text.push_str(text),
        _ => line.push(Fragment {
            style,
            text: text.to_owned(),
        }),
    }
}

/// Линейная часть с масштабами исходной матрицы и заново заданным углом.
///
/// Композиция с уже повёрнутой матрицей удваивала бы угол при каждой правке,
/// поэтому старый поворот выбрасывается, остаются только масштабы осей.
fn absolute_linear(matrix: Matrix, degrees: f32) -> Matrix {
    let sx = (matrix[0] * matrix[0] + matrix[1] * matrix[1]).sqrt();
    let sy = (matrix[2] * matrix[2] + matrix[3] * matrix[3]).sqrt();
    let (sin, cos) = degrees.to_radians().sin_cos();
    [sx * cos, sx * sin, -sy * sin, sy * cos, 0.0, 0.0]
}

/// Приводит угол к промежутку от минус до плюс ста восьмидесяти градусов.
fn normalise_degrees(degrees: f32) -> f32 {
    let wrapped = degrees % 360.0;
    if wrapped > 180.0 {
        wrapped - 360.0
    } else if wrapped <= -180.0 {
        wrapped + 360.0
    } else {
        wrapped
    }
}

/// Линейная часть матрицы, повёрнутая на заданный угол.
fn rotate_linear(matrix: Matrix, degrees: f32) -> Matrix {
    let linear = [matrix[0], matrix[1], matrix[2], matrix[3], 0.0, 0.0];
    if degrees == 0.0 {
        return linear;
    }
    let (sin, cos) = degrees.to_radians().sin_cos();
    multiply(linear, [cos, sin, -sin, cos, 0.0, 0.0])
}

/// Поворачивает точку вокруг заданного центра.
pub fn rotate_about(point: (f32, f32), centre: (f32, f32), degrees: f32) -> (f32, f32) {
    if degrees == 0.0 {
        return point;
    }
    let (sin, cos) = degrees.to_radians().sin_cos();
    let (dx, dy) = (point.0 - centre.0, point.1 - centre.1);
    (
        centre.0 + dx * cos - dy * sin,
        centre.1 + dx * sin + dy * cos,
    )
}

/// Вертикальный масштаб матрицы.
fn matrix_scale(matrix: Matrix) -> f32 {
    (matrix[2] * matrix[2] + matrix[3] * matrix[3]).sqrt()
}

/// Рисует подчёркивания линиями после текста.
///
/// В PDF подчёркивание не свойство символов, а отдельная фигура. Рисуется она
/// в системе координат строки — так линия следует за поворотом абзаца.
fn emit_underlines(
    operations: &mut Vec<Operation>,
    underlines: &[Underline],
    linear: Matrix,
    scale: f32,
) {
    // Матрица без масштаба: нужен только поворот, размеры линии заданы в
    // пунктах страницы.
    let unit = [
        linear[0] / scale,
        linear[1] / scale,
        linear[2] / scale,
        linear[3] / scale,
    ];

    for line in underlines {
        let thickness = (line.size * 0.055).max(0.4);
        let offset = line.size * 0.12;

        operations.push(Operation::new("q", vec![]));
        if let Some(color) = &line.color {
            operations.push(color.clone());
        }
        operations.push(Operation::new(
            "cm",
            vec![
                Object::Real(unit[0]),
                Object::Real(unit[1]),
                Object::Real(unit[2]),
                Object::Real(unit[3]),
                Object::Real(line.origin.0),
                Object::Real(line.origin.1),
            ],
        ));
        operations.push(Operation::new(
            "re",
            vec![
                Object::Real(line.from),
                Object::Real(-offset - thickness),
                Object::Real(line.to - line.from),
                Object::Real(thickness),
            ],
        ));
        operations.push(Operation::new("f", vec![]));
        operations.push(Operation::new("Q", vec![]));
    }
}

/// Жадная разбивка на строки по ширине блока.
#[cfg(test)]
fn wrap(text: &str, encoder: &Encoder, size: f32, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        let words: Vec<&str> = paragraph.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }

        let mut current = String::new();
        for word in words {
            let candidate = if current.is_empty() {
                word.to_owned()
            } else {
                format!("{current} {word}")
            };
            if current.is_empty() || encoder.width(&candidate, size) <= max_width {
                current = candidate;
            } else {
                lines.push(std::mem::take(&mut current));
                current = word.to_owned();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собирает страницу, весь текст которой лежит в форме, вызванной со
    /// страницы одним `Do`, — так делают Word, 1С и генераторы справок.
    fn page_with_text_in_form() -> (Document, Rect) {
        let mut doc = Document::with_version("1.7");
        let font = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let inner = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("Spravka")]),
                Operation::new("ET", vec![]),
            ],
        };
        let form = doc.add_object(lopdf::Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            },
            inner.encode().expect("поток формы"),
        ));
        let page_content = Content {
            operations: vec![Operation::new("Do", vec!["Fm0".into()])],
        };
        let contents = doc.add_object(lopdf::Stream::new(
            dictionary! {},
            page_content.encode().expect("поток страницы"),
        ));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Contents" => contents,
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Fm0" => form },
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
        (doc, Rect::new(70.0, 695.0, 300.0, 715.0))
    }

    /// Текст в форме читается и правится наравне с текстом страницы.
    ///
    /// Раньше разбор смотрел только в поток страницы, а там — один `Do`:
    /// редактор честно отвечал «в границах блока нет текста» и отказывался
    /// править целый класс документов.
    #[test]
    fn text_inside_a_form_is_found_and_rewritten() {
        let (mut doc, bbox) = page_with_text_in_form();

        let font = block_font(&doc, 1, bbox, None).expect("оформление читается");
        assert_eq!(font.base_font, "Helvetica");

        let outcome = rewrite_block(
            &mut doc,
            &BlockRewrite {
                page_number: 1,
                bbox,
                target: Some(bbox),
                spans: vec![StyledSpan::plain("Perepisano")],
                align: crate::model::Align::Left,
                line_height: None,
                rotation: 0.0,
                char_spacing: None,
                h_scale: None,
                para_spacing: None,
                fill: None,
                style_id: None,
                create: false,
                owner: None,
            },
        )
        .expect("перенабор проходит");
        assert_eq!(outcome.cleared_ops, 1, "старый показ текста снят");

        // Правка легла в поток формы, а не в поток страницы: страница
        // по-прежнему только вызывает форму.
        let page_id = page_id(&doc, 1).expect("страница");
        let page_bytes = doc.get_page_content(page_id);
        let page_text = String::from_utf8_lossy(&page_bytes);
        assert!(
            page_text.contains("Do") && !page_text.contains("Perepisano"),
            "страница осталась вызовом формы: {page_text}"
        );

        let form_id = page_forms(&doc, page_id)
            .first()
            .copied()
            .expect("форма на месте");
        let form_bytes = doc
            .get_object(form_id)
            .and_then(|o| o.as_stream())
            .expect("форма")
            .decompressed_content()
            .expect("поток формы");
        // Текст пишется шестнадцатеричной строкой — в кодировке шрифта.
        let form_text = String::from_utf8_lossy(&form_bytes);
        let hex: String = "Perepisano"
            .bytes()
            .map(|byte| format!("{byte:02X}"))
            .collect();
        assert!(
            form_text.contains(&hex),
            "новый текст записан в форму: {form_text}"
        );
    }

    /// Helvetica без /Widths мерилась «пол-кегля на знак» — и каретка с
    /// подсветкой выделения ехали мимо букв. Теперь ширины берутся из
    /// системного собрата, и узкая «i» обязана быть уже широкой «M».
    #[test]
    fn a_base14_font_without_widths_gets_real_metrics() {
        let mut doc = Document::with_version("1.5");
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let encoder = Encoder::load(&doc, font_id).expect("шрифт читается");
        let narrow = encoder.width("iiii", 10.0);
        let wide = encoder.width("MMMM", 10.0);
        assert!(
            narrow < wide * 0.6,
            "узкие буквы обязаны быть уже широких: i={narrow}, M={wide}"
        );
    }

    fn op(operator: &str, operands: Vec<Object>) -> Operation {
        Operation::new(operator, operands)
    }

    fn real(v: f32) -> Object {
        Object::Real(v)
    }

    #[test]
    fn rotation_keeps_the_centre_of_the_block_in_place() {
        // Поворот блока не должен его уносить: точка, вокруг которой крутят,
        // остаётся ровно на месте при любом угле.
        let centre = (100.0, 200.0);
        for degrees in [0.0, 15.0, 90.0, 180.0, -37.5] {
            let stays = rotate_about(centre, centre, degrees);
            assert!(
                (stays.0 - centre.0).abs() < 1e-4,
                "центр уехал по x при {degrees}°"
            );
            assert!(
                (stays.1 - centre.1).abs() < 1e-4,
                "центр уехал по y при {degrees}°"
            );
        }
    }

    #[test]
    fn rotation_turns_counterclockwise_and_keeps_the_distance() {
        // Против часовой, как принято в системе координат PDF: точка справа от
        // центра при 90° оказывается над ним.
        let turned = rotate_about((110.0, 200.0), (100.0, 200.0), 90.0);
        assert!((turned.0 - 100.0).abs() < 1e-3, "получено {turned:?}");
        assert!((turned.1 - 210.0).abs() < 1e-3, "получено {turned:?}");

        // И расстояние до центра сохраняется — это именно поворот, а не сдвиг.
        let far = rotate_about((130.0, 240.0), (100.0, 200.0), 33.0);
        let before = (30.0f32).hypot(40.0);
        let after = (far.0 - 100.0).hypot(far.1 - 200.0);
        assert!((after - before).abs() < 1e-3);
    }

    #[test]
    fn matrix_multiplication_matches_pdf_convention() {
        let translate = [1.0, 0.0, 0.0, 1.0, 10.0, 20.0];
        let scale = [2.0, 0.0, 0.0, 2.0, 0.0, 0.0];
        // Сначала перенос, затем масштаб: перенос тоже удваивается.
        let combined = multiply(translate, scale);
        assert_eq!(apply_origin(combined), (20.0, 40.0));
    }

    #[test]
    fn hits_are_found_by_text_origin_inside_the_block() {
        let content = Content {
            operations: vec![
                op("BT", vec![]),
                op("Tf", vec![Object::Name(b"F1".to_vec()), real(11.0)]),
                // Внутри рамки.
                op(
                    "Tm",
                    vec![
                        real(1.0),
                        real(0.0),
                        real(0.0),
                        real(1.0),
                        real(70.0),
                        real(700.0),
                    ],
                ),
                op("Tj", vec![Object::string_literal("внутри")]),
                // Снаружи: сильно ниже.
                op(
                    "Tm",
                    vec![
                        real(1.0),
                        real(0.0),
                        real(0.0),
                        real(1.0),
                        real(70.0),
                        real(300.0),
                    ],
                ),
                op("Tj", vec![Object::string_literal("снаружи")]),
                op("ET", vec![]),
            ],
        };

        let hits = find_hits(&content, Rect::new(60.0, 690.0, 300.0, 715.0), None);
        assert_eq!(hits.len(), 1, "должен найтись ровно один оператор");
        assert_eq!(hits[0].index, 3);
        assert_eq!(hits[0].font_name, "F1");
        assert_eq!(hits[0].tf_size, 11.0);
        assert_eq!(
            hits[0].visible_size(),
            11.0,
            "при единичной матрице размеры совпадают"
        );
    }

    #[test]
    fn visible_size_comes_from_the_matrix_not_from_tf() {
        // Приём, на котором сломалась первая версия: в `Tf` стоит единица, а
        // настоящий кегль задан масштабом текстовой матрицы.
        let content = Content {
            operations: vec![
                op("BT", vec![]),
                op("Tf", vec![Object::Name(b"F1".to_vec()), real(1.0)]),
                op(
                    "Tm",
                    vec![
                        real(24.0),
                        real(0.0),
                        real(0.0),
                        real(24.0),
                        real(70.0),
                        real(700.0),
                    ],
                ),
                op("Tj", vec![Object::string_literal("крупно")]),
                op("ET", vec![]),
            ],
        };

        let hits = find_hits(&content, Rect::new(60.0, 690.0, 300.0, 715.0), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tf_size, 1.0);
        assert_eq!(
            hits[0].visible_size(),
            24.0,
            "кегль обязан читаться из матрицы"
        );
    }

    #[test]
    fn transform_matrix_is_taken_into_account() {
        // Текст рисуется в начале координат, но `cm` сдвигает всю систему.
        let content = Content {
            operations: vec![
                op(
                    "cm",
                    vec![
                        real(1.0),
                        real(0.0),
                        real(0.0),
                        real(1.0),
                        real(100.0),
                        real(500.0),
                    ],
                ),
                op("BT", vec![]),
                op("Tf", vec![Object::Name(b"F1".to_vec()), real(12.0)]),
                op("Td", vec![real(0.0), real(0.0)]),
                op("Tj", vec![Object::string_literal("сдвинуто")]),
                op("ET", vec![]),
            ],
        };

        assert_eq!(
            find_hits(&content, Rect::new(90.0, 490.0, 200.0, 510.0), None).len(),
            1
        );
        // Без учёта `cm` оператор считался бы лежащим в начале координат.
        assert!(find_hits(&content, Rect::new(-10.0, -10.0, 20.0, 20.0), None).is_empty());
    }

    #[test]
    fn graphics_state_stack_restores_the_matrix() {
        let content = Content {
            operations: vec![
                op("q", vec![]),
                op(
                    "cm",
                    vec![
                        real(1.0),
                        real(0.0),
                        real(0.0),
                        real(1.0),
                        real(400.0),
                        real(400.0),
                    ],
                ),
                op("Q", vec![]),
                op("BT", vec![]),
                op("Tf", vec![Object::Name(b"F1".to_vec()), real(12.0)]),
                op(
                    "Tm",
                    vec![
                        real(1.0),
                        real(0.0),
                        real(0.0),
                        real(1.0),
                        real(50.0),
                        real(50.0),
                    ],
                ),
                op("Tj", vec![Object::string_literal("после Q")]),
                op("ET", vec![]),
            ],
        };
        // `Q` вернул матрицу, поэтому текст остался у начала координат.
        assert_eq!(
            find_hits(&content, Rect::new(40.0, 40.0, 100.0, 60.0), None).len(),
            1
        );
        assert!(find_hits(&content, Rect::new(440.0, 440.0, 500.0, 460.0), None).is_empty());
    }

    #[test]
    fn clearing_keeps_the_operator_but_drops_the_text() {
        let mut operation = op("Tj", vec![Object::string_literal("старый текст")]);
        clear_text_operand(&mut operation);
        assert_eq!(operation.operator, "Tj");
        assert_eq!(
            operation.operands,
            vec![Object::String(Vec::new(), lopdf::StringFormat::Literal)]
        );

        let mut array = op(
            "TJ",
            vec![Object::Array(vec![
                Object::string_literal("a"),
                real(-20.0),
            ])],
        );
        clear_text_operand(&mut array);
        assert_eq!(array.operands, vec![Object::Array(Vec::new())]);
    }

    #[test]
    fn leading_moves_the_next_line_down() {
        let content = Content {
            operations: vec![
                op("BT", vec![]),
                op("Tf", vec![Object::Name(b"F1".to_vec()), real(10.0)]),
                op("TL", vec![real(14.0)]),
                op(
                    "Tm",
                    vec![
                        real(1.0),
                        real(0.0),
                        real(0.0),
                        real(1.0),
                        real(50.0),
                        real(700.0),
                    ],
                ),
                op("Tj", vec![Object::string_literal("первая")]),
                op("T*", vec![]),
                op("Tj", vec![Object::string_literal("вторая")]),
                op("ET", vec![]),
            ],
        };
        // Вторая строка обязана оказаться на 14 пунктов ниже.
        let hits = find_hits(&content, Rect::new(40.0, 680.0, 200.0, 690.0), None);
        assert_eq!(
            hits.len(),
            1,
            "в узкую полосу попадает только вторая строка"
        );
        assert_eq!(hits[0].index, 6);
    }

    #[test]
    fn wrapping_respects_the_block_width() {
        let encoder = Encoder::Simple {
            widths: HashMap::new(),
            default_width: 0.5, // каждый символ шириной в половину кегля
            face: None,
            codes: HashMap::new(),
        };
        // Кегль 10 → символ 5 пунктов. Ширина 50 пунктов → 10 символов в строке.
        let lines = wrap("аааа бббб вввв", &encoder, 10.0, 50.0);
        assert!(lines.len() >= 2, "текст обязан разбиться: {lines:?}");
        for line in &lines {
            assert!(
                encoder.width(line, 10.0) <= 50.0,
                "строка шире блока: {line:?}"
            );
        }
    }

    #[test]
    fn fill_colour_is_read_from_rgb_grey_and_cmyk() {
        let rgb = op("rg", vec![real(1.0), real(0.0), real(0.0)]);
        assert_eq!(color_components(&rgb), Some([1.0, 0.0, 0.0]));

        let grey = op("g", vec![real(0.5)]);
        assert_eq!(color_components(&grey), Some([0.5, 0.5, 0.5]));

        // Чистый голубой в CMYK.
        let cyan = op("k", vec![real(1.0), real(0.0), real(0.0), real(0.0)]);
        assert_eq!(color_components(&cyan), Some([0.0, 1.0, 1.0]));

        // Чёрный через ключевую краску.
        let black = op("k", vec![real(0.0), real(0.0), real(0.0), real(1.0)]);
        assert_eq!(color_components(&black), Some([0.0, 0.0, 0.0]));

        // Оператор без числовых операндов цвета не несёт.
        assert_eq!(
            color_components(&op("cs", vec![Object::Name(b"DeviceRGB".to_vec())])),
            None
        );
    }

    #[test]
    fn a_ligature_never_swallows_a_plain_letter() {
        // Порча из книги: глиф «ﬃ» разворачивается в «ffi», и если от значения
        // брать первый символ, «f» начинает записываться лигатурой. Каждая
        // правка тогда превращает «f» в «ffi», следующая — в «ffiffii», и
        // абзац лавинообразно разъедается. Разбор обязан класть лигатуру в
        // отдельную карту, а «f» оставлять «f».
        let maps = parse_to_unicode(
            "beginbfchar\n\
             <0001> <0066>\n\
             <0002> <0069>\n\
             <0003> <00660066начинается0069>\n\
             endbfchar",
        );
        // Третья запись повреждена нарочно — недесятичные знаки внутри
        // шестнадцатеричного тела просто выпадают при разборе токена.
        let maps_clean = parse_to_unicode(
            "beginbfchar <0001> <0066> <0002> <0069> <0003> <006600660069> endbfchar",
        );
        assert_eq!(maps.singles.get(&'f'), Some(&1));
        assert_eq!(
            maps_clean.singles.get(&'f'),
            Some(&1),
            "буква f обязана остаться собой"
        );
        assert_eq!(maps_clean.singles.get(&'i'), Some(&2));
        assert_eq!(
            maps_clean.sequences.get("ffi"),
            Some(&3),
            "лигатура живёт в своей карте, а не подменяет первую букву"
        );
    }

    #[test]
    fn text_with_ligatures_survives_a_round_trip() {
        // Запись и обратное чтение обязаны сходиться: «ffi» кодируется одним
        // глифом лигатуры, а не тремя подменёнными буквами, и потому после
        // извлечения текст остаётся тем же. Это и есть защита от «поменял
        // размер туда-сюда — абзац рассыпался».
        let to_unicode: HashMap<char, u16> = [('a', 10), ('f', 11), ('i', 12), ('x', 13)]
            .into_iter()
            .collect();
        let ligatures: HashMap<String, u16> = [("ffi".to_owned(), 30), ("fi".to_owned(), 31)]
            .into_iter()
            .collect();
        let encoder = Encoder::Identity {
            face: FaceData {
                data: Vec::new(),
                units_per_em: 1000.0,
            },
            to_unicode,
            ligatures,
        };

        // В «affix» тройная лигатура главнее двойной: f-f-i сворачивается в
        // один глиф 30, а не в «f» плюс «fi».
        assert_eq!(
            encoder.encode("affix"),
            vec![0, 10, 0, 30, 0, 13],
            "жадный разбор обязан взять самую длинную лигатуру"
        );
        assert_eq!(encoder.encode("fi"), vec![0, 31]);
        // Буква сама по себе остаётся буквой.
        assert_eq!(encoder.encode("f"), vec![0, 11]);
    }

    #[test]
    fn the_full_font_key_keeps_the_style_in_the_name() {
        // Именно потеря начертания в ключе роняла обложку: «Inter-Bold»,
        // «Inter-SemiBold» и «Inter-Medium» становились одним «inter», и блок
        // перенабирался чужим подмножеством без половины заглавных букв.
        assert_ne!(full_font_key("Inter-Bold"), full_font_key("Inter-SemiBold"));
        assert_ne!(full_font_key("Inter-Bold"), full_font_key("Inter-Medium"));
        // А варианты записи одного и того же имени сходятся.
        assert_eq!(
            full_font_key("Inter-SemiBold"),
            full_font_key("Inter SemiBold")
        );
        assert_eq!(full_font_key("MinionPro-It"), full_font_key("minionpro it"));
    }

    #[test]
    fn typographic_punctuation_is_encoded_by_win_ansi_not_by_codepoint() {
        // Апостроф, тире и многоточие живут в WinAnsi в промежутке 0x80–0x9F, а
        // их кодовые точки Unicode далеко за пределами байта. Пока код брали
        // прямо из кодовой точки, эти знаки считались отсутствующими — и
        // редактор отказывался править обычные абзацы книги, в которых они
        // преспокойно напечатаны.
        let widths: HashMap<u8, f32> = [(0x92u8, 0.3), (0x97, 1.0), (0x85, 1.0), (b'a', 0.5)]
            .into_iter()
            .collect();
        let encoder = Encoder::Simple {
            widths,
            default_width: 0.5,
            face: None,
            codes: HashMap::new(),
        };

        for ch in ['\u{2019}', '\u{2014}', '\u{2026}'] {
            let missing = encoder.missing(&ch.to_string());
            assert!(missing.is_empty(), "знак {ch:?} сочли отсутствующим");
        }
        assert_eq!(encoder.encode("\u{2019}"), vec![0x92]);
        assert_eq!(encoder.encode("\u{2014}"), vec![0x97]);

        // А то, чего в шрифте правда нет, по-прежнему находится.
        assert_eq!(encoder.missing("b"), vec!['b']);
    }

    #[test]
    fn the_documents_own_table_outranks_the_guess() {
        // Если документ объявил своё соответствие, оно главнее любых догадок:
        // кодировка у шрифта бывает какой угодно, вплоть до самодельной.
        let codes: HashMap<char, u8> = [('\u{2019}', 0x27u8)].into_iter().collect();
        let encoder = Encoder::Simple {
            widths: HashMap::new(),
            default_width: 0.5,
            face: None,
            codes,
        };
        assert_eq!(encoder.encode("\u{2019}"), vec![0x27]);
    }

    #[test]
    fn simple_encoder_reports_characters_beyond_one_byte() {
        let encoder = Encoder::Simple {
            widths: HashMap::new(),
            default_width: 0.5,
            face: None,
            codes: HashMap::new(),
        };
        assert!(encoder.missing("hello").is_empty());
        let missing = encoder.missing("привет");
        assert!(
            !missing.is_empty(),
            "кириллица не влезает в однобайтную кодировку"
        );
    }
}

#[cfg(test)]
mod td_transform_tests {
    use super::*;

    fn td_page() -> (Document, ObjectId) {
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
        // Страница в духе латеховских: один BT, строки через Td без Tm.
        let operations = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), Object::Real(11.0)]),
            Operation::new("Td", vec![Object::Real(72.0), Object::Real(700.0)]),
            Operation::new("Tj", vec![Object::string_literal("Block one line")]),
            Operation::new("Td", vec![Object::Real(0.0), Object::Real(-40.0)]),
            Operation::new("Tj", vec![Object::string_literal("Block two line")]),
            Operation::new("Td", vec![Object::Real(0.0), Object::Real(-40.0)]),
            Operation::new("Tj", vec![Object::string_literal("Block three line")]),
            Operation::new("ET", vec![]),
        ];
        let content_id = doc.add_object(lopdf::Stream::new(
            dictionary! {},
            Content { operations }.encode().unwrap(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
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
        (doc, page_id)
    }

    fn line_positions(doc: &Document, page_id: ObjectId) -> Vec<(f32, f32)> {
        let content = Content::decode(&doc.get_page_content(page_id)).unwrap();
        let everywhere = Rect::new(-1e6, -1e6, 1e6, 1e6);
        // Перенесённые куски живут под меткой-владельцем — собираем всех.
        let mut all: Vec<TextRun> = Vec::new();
        for owner in [None, Some(1), Some(2)] {
            all.extend(find_runs(&content, everywhere, owner).inside);
        }
        all.sort_by_key(|r| r.index);
        all.iter().map(|r| (r.matrix[4], r.matrix[5])).collect()
    }

    /// Пакет из двух переносов на странице, свёрстанной через Td: обе строки
    /// уезжают, а третья — не сдвигается ни на волос.
    #[test]
    fn moving_two_td_lines_keeps_the_third_anchored() {
        let (mut doc, page_id) = td_page();
        let strip = |y: f32| Rect::new(60.0, y - 4.0, 300.0, y + 12.0);

        for (bbox_y, dy) in [(700.0, -100.0f32), (660.0, -100.0)] {
            let request = BlockTransform {
                page_number: 1,
                bbox: strip(bbox_y),
                target: {
                    let t = strip(bbox_y + dy);
                    Rect::new(t.left + 30.0, t.bottom, t.right + 30.0, t.top)
                },
                rotation: 0.0,
                color: None,
                owner: None,
            };
            transform_block(&mut doc, &request).expect("перенос обязан пройти");
        }

        let positions = line_positions(&doc, page_id);
        assert_eq!(positions.len(), 3, "все три строки на месте: {positions:?}");
        assert!(
            positions.contains(&(102.0, 600.0)),
            "первая строка уехала не туда: {positions:?}"
        );
        assert!(
            positions.contains(&(102.0, 560.0)),
            "вторая строка уехала не туда: {positions:?}"
        );
        assert!(
            positions.contains(&(72.0, 620.0)),
            "третья строка обязана остаться: {positions:?}"
        );
    }
}
