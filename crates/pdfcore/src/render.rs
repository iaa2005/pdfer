//! Выделенный поток рендера.
//!
//! Один поток, а не пул: pdfium допускает единственный экземпляр библиотеки на
//! процесс (см. [`crate::engine`]) и не даёт гарантий при параллельном доступе
//! к документу. Потери производительности здесь нет — растеризация страницы
//! текста занимает единицы миллисекунд, и одного потока с запасом хватает,
//! чтобы держать вьюпорт заполненным при любой скорости прокрутки.
//!
//! Отмены как отдельного механизма нет: список нужных тайлов заменяется
//! целиком при каждом изменении вьюпорта, поэтому запросы, потерявшие
//! актуальность, просто не доживают до исполнения. Уже отрисованный тайл
//! отдаётся всегда, даже если вьюпорт успел уехать, — он пригодится в кэше.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use flume::Sender;
use lopdf::Document as LoDocument;
use parking_lot::{Condvar, Mutex};
use pdfium_render::prelude::*;

use crate::blocks::{BlockOptions, detect_blocks};
use crate::cache::{Bitmap, TileKey};
use crate::engine::pdfium;
use crate::extract::extract_page_text;
use crate::geom::{PageSize, Rect};
use crate::model::Block;
use crate::pages::{PageOp, PagesSnapshot, apply_page_op};
use crate::stream_edit::{
    BlockErase, BlockFont, BlockRewrite, BlockTransform, RewriteOutcome, block_font, erase_block,
    extract_page, rewrite_block, transform_block,
};

/// Что именно делают с абзацем.
///
/// Различие не косметическое. Перенабор нужен, только когда изменился сам
/// текст, и он требует, чтобы всё оформление абзаца было одним. Перестановка не
/// трогает содержимое вовсе — поправляет координаты уже написанного, — и потому
/// работает на любом абзаце. Выбор между ними делает тот, кто знает, что
/// пользователь изменил.
#[derive(Clone, Debug, PartialEq)]
pub enum BlockEdit {
    Rewrite(BlockRewrite),
    Transform(BlockTransform),
    Erase(BlockErase),
}

impl BlockEdit {
    pub fn page_number(&self) -> u32 {
        match self {
            BlockEdit::Rewrite(request) => request.page_number,
            BlockEdit::Transform(request) => request.page_number,
            BlockEdit::Erase(request) => request.page_number,
        }
    }

    /// Тот же запрос, но к одностраничной выжимке, где страница всегда первая.
    fn on_first_page(&self) -> BlockEdit {
        match self.clone() {
            BlockEdit::Rewrite(request) => BlockEdit::Rewrite(BlockRewrite {
                page_number: 1,
                ..request
            }),
            BlockEdit::Transform(request) => BlockEdit::Transform(BlockTransform {
                page_number: 1,
                ..request
            }),
            BlockEdit::Erase(request) => BlockEdit::Erase(BlockErase {
                page_number: 1,
                ..request
            }),
        }
    }

    fn apply(&self, document: &mut LoDocument) -> Result<()> {
        match self {
            BlockEdit::Rewrite(request) => rewrite_block(document, request).map(|_| ()),
            BlockEdit::Transform(request) => transform_block(document, request).map(|_| ()),
            BlockEdit::Erase(request) => erase_block(document, request).map(|_| ()),
        }
    }
}

/// Геометрия документа, снятая при открытии.
///
/// Размеры всех страниц читаются одним вызовом `FPDF_GetPageSizeByIndex`, без
/// загрузки самих страниц. Для 700-страничной книги это единицы миллисекунд, а
/// взамен вьюпорт сразу знает полную высоту документа и может показать
/// корректную полосу прокрутки, ещё не отрисовав ни одной страницы.
#[derive(Clone, Debug)]
pub struct DocumentInfo {
    pub page_count: u32,
    pub sizes: Vec<PageSize>,
}

impl DocumentInfo {
    pub fn size(&self, page: u32) -> Option<PageSize> {
        self.sizes.get(page as usize).copied()
    }
}

/// Готовый результат работы потока рендера.
#[derive(Debug)]
pub enum RenderEvent {
    Tile {
        key: TileKey,
        bitmap: Arc<Bitmap>,
    },
    Blocks {
        page: u32,
        blocks: Vec<Block>,
    },
    /// Оформление выделенного абзаца вместе со встроенной программой шрифта.
    BlockFont {
        page: u32,
        font: BlockFont,
        /// Метрики всех шрифтов абзаца: пёстрый текст меряется каждым своим
        /// шрифтом, а не одним доминантным.
        metrics: Vec<(String, crate::stream_edit::Encoder)>,
    },
    /// Опись шрифтов документа: чем он набран и чего не хватает системе.
    Fonts {
        fonts: Vec<crate::fonts::DocumentFont>,
    },
    /// Каталог стилей документа. `changed` — каталог только что переписали,
    /// а не просто перечитали.
    Styles {
        styles: Vec<crate::StyleDef>,
        changed: bool,
    },
    /// Страница с непринятой ещё правкой, отрисованная по-настоящему.
    /// Показывается вместо тайла, пока идёт набор.
    Preview {
        page: u32,
        bitmap: Arc<Bitmap>,
    },
    /// Правка применена к документу в памяти. Растры этой страницы устарели.
    Edited {
        page: u32,
        outcome: Option<RewriteOutcome>,
    },
    /// Состав страниц изменился: вставка, удаление или поворот. Все кэши по
    /// номерам страниц обесценились — номера съехали.
    PagesChanged {
        info: DocumentInfo,
    },
    Saved {
        path: PathBuf,
    },
    /// `page` пуст для операций, не привязанных к странице, — например
    /// для сохранения файла.
    Failed {
        page: Option<u32>,
        message: String,
    },
}

#[derive(Debug)]
enum Job {
    Tile(TileKey),
    Blocks(u32),
    BlockFont {
        page_number: u32,
        bbox: Rect,
        owner: Option<i64>,
    },
    Preview {
        edit: BlockEdit,
        scale: f32,
    },
    Edit(BlockEdit),
    EditBatch(Vec<BlockEdit>),
    Page(PageOp),
    Undo,
    Redo,
    /// Опись шрифтов документа для окна «Шрифты».
    Fonts,
    /// Каталог стилей документа.
    Styles,
    /// Перезапись каталога стилей без перенабора блоков: переименование,
    /// создание, удаление.
    SaveStyles {
        styles: Vec<crate::StyleDef>,
    },
    /// Правка стиля с каскадом: все блоки этого стиля перенабираются.
    ApplyStyle {
        def: crate::StyleDef,
    },
    Save(PathBuf),
    ExportPage {
        page_number: u32,
        target: PathBuf,
    },
}

#[derive(Default)]
struct Queue {
    /// Тайлы в порядке убывания приоритета: индекс 0 нужен раньше всех.
    tiles: Vec<TileKey>,
    /// Задания по прямому запросу пользователя. Идут вперёд спекулятивных
    /// тайлов: клик по странице должен отзываться немедленно, даже если в
    /// очереди стоит десяток упреждающе запрошенных растров.
    jobs: VecDeque<Job>,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
}

pub struct Renderer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Renderer {
    /// Открывает документ в фоновом потоке и возвращает его геометрию.
    ///
    /// Блокирует вызывающий поток только на время открытия — дальше всё
    /// асинхронно, результаты приходят в `events`.
    pub fn open(
        path: impl AsRef<Path>,
        events: Sender<RenderEvent>,
    ) -> Result<(Renderer, DocumentInfo)> {
        let path = path.as_ref().to_path_buf();
        let shared = Arc::new(Shared::default());
        let (info_tx, info_rx) = flume::bounded(1);

        let thread = std::thread::Builder::new()
            .name("pdf-render".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || worker(path, shared, events, info_tx)
            })
            .context("не удалось запустить поток рендера")?;

        let info = info_rx
            .recv()
            .context("поток рендера завершился, не открыв документ")??;

        Ok((
            Renderer {
                shared,
                thread: Some(thread),
            },
            info,
        ))
    }

    /// Задаёт полный список нужных тайлов в порядке приоритета, заменяя
    /// предыдущий. Вызывается на каждое изменение вьюпорта.
    pub fn set_wanted_tiles(&self, tiles: Vec<TileKey>) {
        let mut queue = self.shared.queue.lock();
        queue.tiles = tiles;
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Ставит в очередь разбор текста страницы. Повторные запросы одной и той
    /// же страницы схлопываются.
    pub fn request_blocks(&self, page: u32) {
        let mut queue = self.shared.queue.lock();
        let already = queue
            .jobs
            .iter()
            .any(|j| matches!(j, Job::Blocks(p) if *p == page));
        if !already {
            queue.jobs.push_back(Job::Blocks(page));
        }
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Запрашивает оформление абзаца: гарнитуру, кегль, цвет и встроенную
    /// программу шрифта.
    pub fn request_block_font(&self, page_number: u32, bbox: Rect, owner: Option<i64>) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::BlockFont {
            page_number,
            bbox,
            owner,
        });
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Просит отрисовать страницу так, как она будет выглядеть с этой правкой,
    /// ничего не меняя в самом документе.
    ///
    /// Запросы схлопываются: пока пользователь печатает, их приходит по одному
    /// на нажатие, а показать нужно только последний. Устаревшие просто
    /// выбрасываются из очереди — рисовать их незачем.
    pub fn request_preview(&self, edit: BlockEdit, scale: f32) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.retain(|job| !matches!(job, Job::Preview { .. }));
        queue.jobs.push_back(Job::Preview { edit, scale });
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Ставит в очередь замену текста абзаца.
    ///
    /// Правка идёт в том же потоке, что владеет документом: pdfium не
    /// потокобезопасен, и менять документ из UI-потока было бы гонкой с
    /// рендером соседних страниц.
    pub fn apply_edit(&self, edit: BlockEdit) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::Edit(edit));
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Ставит в очередь операцию над страницей: вставку, удаление, поворот.
    pub fn apply_page_op(&self, op: PageOp) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::Page(op));
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Ставит в очередь пачку правок одной страницы.
    ///
    /// Пачка — одно действие пользователя: «сделать все выделенные блоки
    /// крупнее». Поэтому она применяется одним заходом, попадает в историю
    /// одним шагом и откатывается одним нажатием.
    pub fn apply_edits(&self, edits: Vec<BlockEdit>) {
        if edits.is_empty() {
            return;
        }
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::EditBatch(edits));
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Откатывает последнюю правку.
    pub fn undo(&self) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::Undo);
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Возвращает откаченную правку.
    pub fn redo(&self) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::Redo);
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Ставит в очередь чтение каталога стилей.
    pub fn request_styles(&self) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.retain(|job| !matches!(job, Job::Styles));
        queue.jobs.push_back(Job::Styles);
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Переписывает каталог стилей: имена, создание, удаление. Блоки не
    /// перенабираются — за это отвечает [`Renderer::apply_style`].
    pub fn save_styles(&self, styles: Vec<crate::StyleDef>) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::SaveStyles { styles });
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Правит стиль и перенабирает все его блоки по документу — одним шагом
    /// истории.
    pub fn apply_style(&self, def: crate::StyleDef) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::ApplyStyle { def });
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Ставит в очередь опись шрифтов документа.
    pub fn request_fonts(&self) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.retain(|job| !matches!(job, Job::Fonts));
        queue.jobs.push_back(Job::Fonts);
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Ставит в очередь экспорт одной страницы в отдельный файл.
    pub fn export_page(&self, page_number: u32, target: impl Into<PathBuf>) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::ExportPage {
            page_number,
            target: target.into(),
        });
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Ставит в очередь сохранение документа.
    pub fn save_as(&self, path: impl Into<PathBuf>) {
        let mut queue = self.shared.queue.lock();
        queue.jobs.push_back(Job::Save(path.into()));
        drop(queue);
        self.shared.wake.notify_all();
    }

    /// Сколько тайлов ещё не отрисовано — для индикатора занятости.
    pub fn pending_tiles(&self) -> usize {
        self.shared.queue.lock().tiles.len()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        {
            let mut queue = self.shared.queue.lock();
            queue.stop = true;
            queue.tiles.clear();
            queue.jobs.clear();
        }
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn worker(
    path: PathBuf,
    shared: Arc<Shared>,
    events: Sender<RenderEvent>,
    info_tx: Sender<Result<DocumentInfo>>,
) {
    let mut document = match open_document(&path) {
        Ok(doc) => doc,
        Err(e) => {
            let _ = info_tx.send(Err(e));
            return;
        }
    };
    // Индекс системных шрифтов понадобится при первой же смене начертания;
    // прогреваем его заранее, чтобы клик по «Ж» не ждал сканирования.
    crate::fonts::warm_up();

    // Разобранная структура документа для правки. Грузится лениво: пока
    // пользователь только читает, она не нужна и памяти не занимает.
    let mut source: Option<LoDocument> = None;
    // Помечен ли файл нашими правками. Проверяется один раз, по хвосту файла,
    // без разбора структуры: для нетронутой книги ответ «нет» стоит одного
    // чтения четырёх килобайт.
    let mut marked: Option<bool> = None;
    // Одностраничная выжимка для показа правки по ходу набора.
    let mut preview: Option<Preview> = None;
    // Накладки: правленые страницы, перечитанные растеризатором поодиночке.
    //
    // Полная перечитка после каждой правки не по карману: сериализация
    // двухсотмегабайтной книги занимает секунды, и на это время встаёт вся
    // очередь — от растров до показа набора. Правка меняет ровно одну
    // страницу, поэтому растеризатору отдаётся её одностраничная выжимка, а
    // основной документ остаётся от исходного файла. Целиком документ
    // пересобирается только там, где меняется состав страниц.
    let mut overlays: HashMap<u32, PdfDocument<'static>> = HashMap::new();
    // История правок для отката. Правка меняет ровно одну страницу, поэтому
    // шаг истории — это пара «поток содержимого до и после». Дёшево: страница
    // книги — доли мегабайта, сто шагов помещаются без раздумий.
    let mut history = History::default();

    match describe(&document) {
        Ok(info) => {
            if info_tx.send(Ok(info)).is_err() {
                return; // вызывающая сторона уже не ждёт
            }
        }
        Err(e) => {
            let _ = info_tx.send(Err(e));
            return;
        }
    }

    loop {
        let job = {
            let mut queue = shared.queue.lock();
            loop {
                if queue.stop {
                    return;
                }
                if let Some(job) = queue.jobs.pop_front() {
                    break job;
                }
                if !queue.tiles.is_empty() {
                    break Job::Tile(queue.tiles.remove(0));
                }
                shared.wake.wait(&mut queue);
            }
        };

        let event = match job {
            Job::Tile(key) => match render_tile(page_in(&document, &overlays, key.page), key) {
                Ok(bitmap) => RenderEvent::Tile {
                    key,
                    bitmap: Arc::new(bitmap),
                },
                Err(e) => RenderEvent::Failed {
                    page: Some(key.page),
                    message: e.to_string(),
                },
            },
            Job::Blocks(page) => match read_blocks(page_in(&document, &overlays, page)) {
                Ok(blocks) => {
                    let blocks = split_by_marks(blocks, page, &path, &mut source, &mut marked);
                    RenderEvent::Blocks { page, blocks }
                }
                Err(e) => RenderEvent::Failed {
                    page: Some(page),
                    message: e.to_string(),
                },
            },
            Job::BlockFont {
                page_number,
                bbox,
                owner,
            } => {
                let page = page_number.saturating_sub(1);
                match structure(&path, &mut source).and_then(|document| {
                    let font = block_font(document, page_number, bbox, owner)?;
                    let metrics =
                        crate::stream_edit::block_metrics(document, page_number, bbox, owner)
                            .unwrap_or_default();
                    Ok((font, metrics))
                }) {
                    Ok((font, metrics)) => RenderEvent::BlockFont {
                        page,
                        font,
                        metrics,
                    },
                    Err(e) => RenderEvent::Failed {
                        page: Some(page),
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::Preview { edit, scale } => {
                let page = edit.page_number().saturating_sub(1);
                match render_preview(&path, &mut source, &mut preview, &edit, scale) {
                    Ok(bitmap) => RenderEvent::Preview {
                        page,
                        bitmap: Arc::new(bitmap),
                    },
                    Err(e) => RenderEvent::Failed {
                        page: Some(page),
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::Edit(request) => {
                let page = request.page_number().saturating_sub(1);
                // Страница изменилась — заготовка для показа устарела.
                preview = None;
                let result =
                    apply_edit(&path, &mut source, &request, &mut history).and_then(|outcome| {
                        refresh_overlay(&path, &mut source, &mut overlays, request.page_number())?;
                        Ok(outcome)
                    });
                match result {
                    Ok(outcome) => RenderEvent::Edited { page, outcome },
                    Err(e) => RenderEvent::Failed {
                        page: Some(page),
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::EditBatch(edits) => {
                let page_number = edits.first().map(|edit| edit.page_number()).unwrap_or(1);
                let page = page_number.saturating_sub(1);
                preview = None;
                let result = apply_batch(&path, &mut source, &edits, &mut history)
                    .and_then(|()| refresh_overlay(&path, &mut source, &mut overlays, page_number));
                match result {
                    Ok(()) => RenderEvent::Edited {
                        page,
                        outcome: None,
                    },
                    Err(e) => RenderEvent::Failed {
                        page: Some(page),
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::Page(op) => {
                preview = None;
                match run_page_op(&path, &mut source, &mut document, &op, &mut history) {
                    Ok(info) => {
                        // Номера страниц съехали — накладки прибиты к старым.
                        overlays.clear();
                        RenderEvent::PagesChanged { info }
                    }
                    Err(e) => RenderEvent::Failed {
                        page: Some(op.page_number().saturating_sub(1)),
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::Undo => {
                preview = None;
                let result = step_history(&path, &mut source, &mut document, &mut history, true)
                    .and_then(|outcome| {
                        match outcome {
                            // Шаг тронул содержимое страницы — её накладка
                            // пересобирается, остальные верны как были.
                            Some(StepOutcome::Content(page)) => {
                                refresh_overlay(&path, &mut source, &mut overlays, page + 1)?;
                            }
                            // Состав страниц другой — накладки прибиты к
                            // старым номерам.
                            Some(StepOutcome::Pages(_)) => overlays.clear(),
                            None => {}
                        }
                        Ok(outcome)
                    });
                match result {
                    Ok(Some(StepOutcome::Content(page))) => RenderEvent::Edited {
                        page,
                        outcome: None,
                    },
                    Ok(Some(StepOutcome::Pages(info))) => RenderEvent::PagesChanged { info },
                    Ok(None) => RenderEvent::Failed {
                        page: None,
                        message: "Отменять больше нечего".into(),
                    },
                    Err(e) => RenderEvent::Failed {
                        page: None,
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::Redo => {
                preview = None;
                let result = step_history(&path, &mut source, &mut document, &mut history, false)
                    .and_then(|outcome| {
                        match outcome {
                            // Шаг тронул содержимое страницы — её накладка
                            // пересобирается, остальные верны как были.
                            Some(StepOutcome::Content(page)) => {
                                refresh_overlay(&path, &mut source, &mut overlays, page + 1)?;
                            }
                            // Состав страниц другой — накладки прибиты к
                            // старым номерам.
                            Some(StepOutcome::Pages(_)) => overlays.clear(),
                            None => {}
                        }
                        Ok(outcome)
                    });
                match result {
                    Ok(Some(StepOutcome::Content(page))) => RenderEvent::Edited {
                        page,
                        outcome: None,
                    },
                    Ok(Some(StepOutcome::Pages(info))) => RenderEvent::PagesChanged { info },
                    Ok(None) => RenderEvent::Failed {
                        page: None,
                        message: "Возвращать больше нечего".into(),
                    },
                    Err(e) => RenderEvent::Failed {
                        page: None,
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::Fonts => match structure(&path, &mut source) {
                Ok(document) => RenderEvent::Fonts {
                    fonts: crate::stream_edit::document_fonts(document),
                },
                Err(e) => RenderEvent::Failed {
                    page: None,
                    message: format!("{e:#}"),
                },
            },
            Job::Styles => match structure(&path, &mut source) {
                Ok(document) => RenderEvent::Styles {
                    styles: crate::styles::read_styles(document),
                    changed: false,
                },
                Err(e) => RenderEvent::Failed {
                    page: None,
                    message: format!("{e:#}"),
                },
            },
            Job::SaveStyles { styles } => match structure(&path, &mut source) {
                Ok(document) => {
                    let catalog_before = crate::styles::read_styles(document);
                    crate::styles::write_styles(document, &styles);
                    history.record(HistoryStep::Restyle {
                        catalog_before,
                        catalog_after: styles.clone(),
                        pages: Vec::new(),
                    });
                    RenderEvent::Styles {
                        styles,
                        changed: true,
                    }
                }
                Err(e) => RenderEvent::Failed {
                    page: None,
                    message: format!("{e:#}"),
                },
            },
            Job::ApplyStyle { def } => {
                preview = None;
                match restyle_document(
                    &path,
                    &mut source,
                    &mut document,
                    &mut marked,
                    &def,
                    &mut history,
                ) {
                    Ok((restyled, info)) => {
                        tracing::info!(style = def.id, blocks = restyled, "стиль применён");
                        // Каскад мог тронуть много страниц — накладки не в счёт.
                        overlays.clear();
                        RenderEvent::PagesChanged { info }
                    }
                    Err(e) => RenderEvent::Failed {
                        page: None,
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::ExportPage {
                page_number,
                target,
            } => {
                // Выжимка страницы со всем её поддеревом — тем же извлечением,
                // каким пользуется предпросмотр. Непочатый документ читается
                // с диска: лениво загруженной структуры могло ещё не быть.
                let result = match source.as_ref() {
                    Some(document) => extract_page(document, page_number),
                    None => LoDocument::load(&path)
                        .map_err(|e| anyhow::anyhow!("не удалось открыть исходник: {e}"))
                        .and_then(|document| extract_page(&document, page_number)),
                }
                .and_then(|mut single| single.save(&target).map(|_| ()).map_err(Into::into));
                match result {
                    Ok(_) => RenderEvent::Saved {
                        path: target.clone(),
                    },
                    Err(e) => RenderEvent::Failed {
                        page: None,
                        message: format!("{e:#}"),
                    },
                }
            }
            Job::Save(target) => match save_document(&mut source, &mut document, &path, &target)
                .map(|()| {
                    // Растеризатор пересел на сохранённые байты — они уже
                    // содержат всё, что показывали накладки.
                    overlays.clear();
                }) {
                Ok(()) => RenderEvent::Saved { path: target },
                Err(e) => RenderEvent::Failed {
                    page: None,
                    message: format!("{e:#}"),
                },
            },
        };

        // Приёмник закрыт — окно с документом уже уничтожено.
        if events.send(event).is_err() {
            return;
        }
    }
}

/// Применяет правку и переоткрывает pdfium на изменённых байтах.
///
/// Правка идёт по структуре документа через lopdf, а рисует по-прежнему
/// pdfium. Чтобы вид отражал правку, после каждого изменения pdfium получает
/// свежие байты. Это дороже, чем менять объекты на месте, — зато pdfium
/// никогда ничего не переписывает, а значит и испортить не может.
/// Разобранная структура документа, загружаемая при первой надобности.
fn structure<'a>(path: &Path, source: &'a mut Option<LoDocument>) -> Result<&'a mut LoDocument> {
    match source {
        Some(document) => Ok(document),
        None => Ok(source.insert(
            LoDocument::load(path)
                .with_context(|| format!("не удалось разобрать структуру {}", path.display()))?,
        )),
    }
}

/// Нетронутая копия одной страницы, из которой раз за разом собирается показ.
struct Preview {
    /// Номер страницы в исходном документе, считая с единицы.
    page_number: u32,
    /// Исходное состояние. Каждый показ начинается с его копии: переписывать
    /// одну и ту же страницу поверх уже переписанной нельзя — второй проход
    /// нашёл бы в рамке текст, дописанный первым.
    pristine: LoDocument,
}

/// Рисует страницу такой, какой она станет с этой правкой, не трогая документ.
///
/// Правка в стиле Acrobat означает, что буквы появляются в самом документе, а
/// не в накладке поверх него. Значит на каждое нажатие нужен настоящий растр
/// настоящей страницы. Целый документ для этого сериализовать нельзя: на книге
/// в 205 МБ это 250–330 мс на каждое нажатие. Поэтому страница один раз
/// вынимается в отдельный документ (около миллисекунды, 0.3 МБ), и дальше
/// пересобирается только она — примерно 54 мс на обновление.
fn render_preview(
    path: &Path,
    source: &mut Option<LoDocument>,
    preview: &mut Option<Preview>,
    edit: &BlockEdit,
    scale: f32,
) -> Result<Bitmap> {
    let page_number = edit.page_number();
    let stale = preview
        .as_ref()
        .is_none_or(|state| state.page_number != page_number);
    if stale {
        let document = structure(path, source)?;
        let pristine = extract_page(document, page_number)
            .with_context(|| format!("не удалось выделить страницу {page_number} для показа"))?;
        *preview = Some(Preview {
            page_number,
            pristine,
        });
    }
    let state = preview.as_ref().expect("заготовка только что создана");

    // В выжимке страница ровно одна, поэтому её номер всегда первый.
    let mut page = state.pristine.clone();
    edit.on_first_page().apply(&mut page)?;

    let mut bytes = Vec::new();
    page.save_to(&mut bytes)
        .context("не удалось сериализовать страницу для показа")?;

    // Отдельный экземпляр документа, живущий только до конца отрисовки:
    // основной документ обязан остаться нетронутым, пока правку не приняли.
    let rendered = pdfium()?
        .load_pdf_from_byte_vec(bytes, None)
        .context("не удалось перечитать страницу для показа")?;
    let page = rendered.pages().get(0).context("в выжимке нет страницы")?;

    let config = PdfRenderConfig::new()
        .scale_page_by_factor(scale)
        .render_annotations(true)
        .set_reverse_byte_order(false);
    let bitmap = page
        .render_with_config(&config)
        .context("сбой растеризации показа")?;

    Ok(Bitmap {
        width: bitmap.width() as u32,
        height: bitmap.height() as u32,
        pixels: bitmap.as_raw_bytes(),
    })
}

/// Один шаг истории.
enum HistoryStep {
    /// Правка содержимого одной страницы: поток до и после.
    Content {
        page_number: u32,
        before: Vec<u8>,
        after: Vec<u8>,
    },
    /// Операция над деревом страниц: точный снимок затронутых узлов.
    Pages(PagesSnapshot),
    /// Правка каталога стилей, возможно с перенабором блоков на многих
    /// страницах. Каталог и все затронутые потоки откатываются одним шагом.
    Restyle {
        catalog_before: Vec<crate::StyleDef>,
        catalog_after: Vec<crate::StyleDef>,
        /// Затронутые страницы: номер с единицы, поток до и после.
        pages: Vec<(u32, Vec<u8>, Vec<u8>)>,
    },
}

/// История правок: стопка сделанного и стопка отменённого.
///
/// Новая правка сжигает ветку возврата — как в любом редакторе: после отката
/// и новой правки «вперёд» вести некуда.
#[derive(Default)]
struct History {
    done: Vec<HistoryStep>,
    undone: Vec<HistoryStep>,
}

/// Глубина истории. Сто шагов — как заказывали.
const HISTORY_LIMIT: usize = 100;

impl History {
    fn record(&mut self, step: HistoryStep) {
        self.undone.clear();
        self.done.push(step);
        if self.done.len() > HISTORY_LIMIT {
            self.done.remove(0);
        }
    }
}

fn apply_edit(
    path: &Path,
    source: &mut Option<LoDocument>,
    request: &BlockEdit,
    history: &mut History,
) -> Result<Option<RewriteOutcome>> {
    let document = structure(path, source)?;

    let page_number = request.page_number();
    let page_id = document
        .page_iter()
        .nth(page_number.saturating_sub(1) as usize)
        .ok_or_else(|| anyhow::anyhow!("нет страницы {page_number}"))?;
    let before = document.get_page_content(page_id);

    let outcome = match request {
        BlockEdit::Rewrite(request) => Some(rewrite_block(document, request)?),
        BlockEdit::Transform(request) => {
            transform_block(document, request)?;
            None
        }
        BlockEdit::Erase(request) => {
            erase_block(document, request)?;
            None
        }
    };

    // Шаг истории пишется только после удачной правки: отклонённая ничего
    // не изменила, и отменять её нечего.
    history.record(HistoryStep::Content {
        page_number,
        before,
        after: document.get_page_content(page_id),
    });
    Ok(outcome)
}

/// Операция над страницами: применяется, ложится в историю, возвращает свежую
/// геометрию документа.
fn run_page_op(
    path: &Path,
    source: &mut Option<LoDocument>,
    rendered: &mut PdfDocument<'static>,
    op: &PageOp,
    history: &mut History,
) -> Result<DocumentInfo> {
    let document = structure(path, source)?;
    let snapshot = apply_page_op(document, op)?;
    history.record(HistoryStep::Pages(snapshot));
    reload(document, rendered)?;
    describe(rendered)
}

/// Пачка правок одной страницы: применяется целиком, в историю ложится одним
/// шагом. Ошибка на любой правке отменяет всю пачку — половинчатое «три блока
/// из пяти перекрасились» хуже честного отказа.
fn apply_batch(
    path: &Path,
    source: &mut Option<LoDocument>,
    edits: &[BlockEdit],
    history: &mut History,
) -> Result<()> {
    let document = structure(path, source)?;
    let Some(first) = edits.first() else {
        return Ok(());
    };
    let page_number = first.page_number();
    let page_id = document
        .page_iter()
        .nth(page_number.saturating_sub(1) as usize)
        .ok_or_else(|| anyhow::anyhow!("нет страницы {page_number}"))?;
    let before = document.get_page_content(page_id);

    for edit in edits {
        if let Err(e) = edit.apply(document) {
            // Всё или ничего: уже применённая часть пачки откатывается.
            document
                .change_page_content(page_id, before.clone())
                .map_err(|inner| anyhow::anyhow!("не удалось откатить пачку: {inner}"))?;
            return Err(e);
        }
    }

    history.record(HistoryStep::Content {
        page_number,
        before,
        after: document.get_page_content(page_id),
    });
    Ok(())
}

/// Что изменил шаг истории — вызывающему решать, какое событие слать.
enum StepOutcome {
    /// Правка содержимого: обесценились растры одной страницы.
    Content(u32),
    /// Операция над деревом страниц: обесценилось всё постраничное.
    Pages(DocumentInfo),
}

/// Откат или возврат одного шага. `None` — шагать некуда.
fn step_history(
    path: &Path,
    source: &mut Option<LoDocument>,
    rendered: &mut PdfDocument<'static>,
    history: &mut History,
    backwards: bool,
) -> Result<Option<StepOutcome>> {
    let Some(step) = (if backwards {
        history.done.pop()
    } else {
        history.undone.pop()
    }) else {
        return Ok(None);
    };

    let document = structure(path, source)?;
    let outcome = match &step {
        HistoryStep::Content {
            page_number,
            before,
            after,
        } => {
            let page_id = document
                .page_iter()
                .nth(page_number.saturating_sub(1) as usize)
                .ok_or_else(|| anyhow::anyhow!("нет страницы {page_number}"))?;
            let content = if backwards { before } else { after };
            document
                .change_page_content(page_id, content.clone())
                .map_err(|e| anyhow::anyhow!("не удалось вернуть поток страницы: {e}"))?;
            StepOutcome::Content(page_number.saturating_sub(1))
        }
        HistoryStep::Pages(snapshot) => {
            if backwards {
                snapshot.undo(document)?;
            } else {
                snapshot.redo(document)?;
            }
            // Геометрия соберётся после перезагрузки растеризатора ниже.
            StepOutcome::Pages(DocumentInfo {
                page_count: 0,
                sizes: Vec::new(),
            })
        }
        HistoryStep::Restyle {
            catalog_before,
            catalog_after,
            pages,
        } => {
            let catalog = if backwards {
                catalog_before
            } else {
                catalog_after
            };
            crate::styles::write_styles(document, catalog);
            for (page_number, before, after) in pages {
                let page_id = document
                    .page_iter()
                    .nth(page_number.saturating_sub(1) as usize)
                    .ok_or_else(|| anyhow::anyhow!("нет страницы {page_number}"))?;
                let content = if backwards { before } else { after };
                document
                    .change_page_content(page_id, content.clone())
                    .map_err(|e| anyhow::anyhow!("не удалось вернуть поток страницы: {e}"))?;
            }
            // Затронуто много страниц разом — дешевле обесценить всё.
            StepOutcome::Pages(DocumentInfo {
                page_count: 0,
                sizes: Vec::new(),
            })
        }
    };

    if backwards {
        history.undone.push(step);
    } else {
        history.done.push(step);
    }

    // Полная перечитка — только когда менялся состав страниц: правка
    // содержимого обслуживается накладкой у вызывающего.
    let outcome = match outcome {
        StepOutcome::Pages(_) => {
            reload(document, rendered)?;
            StepOutcome::Pages(describe(rendered)?)
        }
        content => content,
    };
    Ok(Some(outcome))
}

/// Перенабирает все блоки стиля по документу и переписывает каталог.
///
/// Один шаг истории на всё: каталог и каждая затронутая страница
/// откатываются разом. Блоки стиля ищутся по меткам — значит, страницы без
/// меток пропускаются одним дешёвым разбором потока.
fn restyle_document(
    path: &Path,
    source: &mut Option<LoDocument>,
    rendered: &mut PdfDocument<'static>,
    marked: &mut Option<bool>,
    def: &crate::StyleDef,
    history: &mut History,
) -> Result<(u32, DocumentInfo)> {
    // Каскад читает блоки из растеризатора, а тот после точечных правок
    // отстаёт от структуры: правленые страницы жили в накладках. Один раз
    // перечитываем документ целиком — для редкой и без того тяжёлой операции
    // это честная цена.
    reload(structure(path, source)?, rendered)?;
    let page_count = rendered.pages().len() as u32;

    // Сначала каталог: даже если блоков у стиля пока нет, правка спека
    // должна сохраниться и попасть в историю.
    let (catalog_before, catalog_after) = {
        let document = structure(path, source)?;
        let before = crate::styles::read_styles(document);
        let mut after = before.clone();
        match after.iter_mut().find(|d| d.id == def.id) {
            Some(slot) => *slot = def.clone(),
            None => after.push(def.clone()),
        }
        crate::styles::write_styles(document, &after);
        (before, after)
    };

    let mut pages: Vec<(u32, Vec<u8>, Vec<u8>)> = Vec::new();
    let mut restyled = 0u32;
    let mut failure: Option<anyhow::Error> = None;

    'pages: for page in 0..page_count {
        let page_number = page + 1;
        // Блоки со стилем есть только среди помеченных страниц.
        let has_style = {
            let document = structure(path, source)?;
            crate::stream_edit::mark_variants(document, page_number)
                .unwrap_or_default()
                .iter()
                .any(|variant| variant.style == Some(def.id))
        };
        if !has_style {
            continue;
        }

        let blocks = read_blocks((rendered, page))?;
        let blocks = split_by_marks(blocks, page, path, source, marked);
        let targets: Vec<&Block> = blocks
            .iter()
            .filter(|block| block.style == Some(def.id))
            .collect();
        if targets.is_empty() {
            continue;
        }

        let document = structure(path, source)?;
        let page_id = document
            .page_iter()
            .nth(page as usize)
            .ok_or_else(|| anyhow::anyhow!("нет страницы {page_number}"))?;
        let before = document.get_page_content(page_id);

        for block in &targets {
            let request = BlockRewrite {
                page_number,
                bbox: block.bbox,
                target: None,
                spans: spans_from_block(block, def),
                line_height: block.leading(),
                rotation: block.rotation,
                align: block.align,
                char_spacing: None,
                h_scale: None,
                para_spacing: None,
                fill: None,
                style_id: Some(def.id),
                create: false,
                owner: block.mark,
            };
            if let Err(e) = rewrite_block(document, &request) {
                document
                    .change_page_content(page_id, before)
                    .map_err(|inner| anyhow::anyhow!("не удалось откатить страницу: {inner}"))?;
                failure = Some(e);
                break 'pages;
            }
            restyled += 1;
        }
        pages.push((page_number, before, document.get_page_content(page_id)));
    }

    let document = structure(path, source)?;
    if let Some(e) = failure {
        // Всё или ничего: откатываем и страницы, и каталог.
        for (undo_page, undo_before, _) in &pages {
            let undo_id = document
                .page_iter()
                .nth(undo_page.saturating_sub(1) as usize)
                .ok_or_else(|| anyhow::anyhow!("нет страницы {undo_page}"))?;
            document
                .change_page_content(undo_id, undo_before.clone())
                .map_err(|inner| anyhow::anyhow!("не удалось откатить страницу: {inner}"))?;
        }
        crate::styles::write_styles(document, &catalog_before);
        return Err(e);
    }

    history.record(HistoryStep::Restyle {
        catalog_before,
        catalog_after,
        pages,
    });
    reload(document, rendered)?;
    Ok((restyled, describe(rendered)?))
}

/// Куски блока для перенабора его же текстом, но по спеку стиля.
///
/// Всё, что стиль не задаёт, остаётся как в документе: кусок без гарнитуры
/// стиля держится своего шрифта страницы, без кегля — своего кегля.
fn spans_from_block(block: &Block, def: &crate::StyleDef) -> Vec<crate::stream_edit::StyledSpan> {
    let mut spans: Vec<crate::stream_edit::StyledSpan> = Vec::new();
    for (index, line) in block.lines.iter().enumerate() {
        if index > 0
            && let Some(last) = spans.last_mut()
        {
            // Перенос по дефису сшивается, остальные строки склеиваются
            // пробелом — как при обычной правке блока.
            if last.text.ends_with('-') {
                last.text.pop();
            } else {
                last.text.push(' ');
            }
        }
        for run in &line.runs {
            let font = def
                .family
                .as_ref()
                .map(|family| crate::fonts::FontRequest::new(family, def.bold, def.italic));
            let colour = def.color.or_else(|| {
                let c = run.style.color;
                Some([
                    f32::from(c.r) / 255.0,
                    f32::from(c.g) / 255.0,
                    f32::from(c.b) / 255.0,
                ])
            });
            spans.push(crate::stream_edit::StyledSpan {
                page_family: font.is_none().then(|| run.style.clean_family().to_owned()),
                font,
                text: run.text.clone(),
                size: def.size.or(Some(run.style.size)),
                color: colour,
                script: crate::stream_edit::Script::Baseline,
                underline: def.underline,
            });
        }
    }
    spans
}

/// Документ, из которого рисуется страница: накладка, если страница правлена.
///
/// Возвращается пара «документ, номер страницы в нём»: в накладке страница
/// всегда одна и всегда первая.
fn page_in<'a>(
    document: &'a PdfDocument<'static>,
    overlays: &'a HashMap<u32, PdfDocument<'static>>,
    page: u32,
) -> (&'a PdfDocument<'static>, u32) {
    match overlays.get(&page) {
        Some(single) => (single, 0),
        None => (document, page),
    }
}

/// Пересобирает накладку правленой страницы: одностраничная выжимка из
/// структуры сериализуется и перечитывается растеризатором. Стоит долей
/// секунды против секунд полной перечитки книги.
fn refresh_overlay(
    path: &Path,
    source: &mut Option<LoDocument>,
    overlays: &mut HashMap<u32, PdfDocument<'static>>,
    page_number: u32,
) -> Result<()> {
    let document = structure(path, source)?;
    let mut single = extract_page(document, page_number)
        .with_context(|| format!("не удалось выделить страницу {page_number} для накладки"))?;
    let mut bytes = Vec::new();
    single
        .save_to(&mut bytes)
        .context("не удалось сериализовать накладку")?;
    let rendered = pdfium()?
        .load_pdf_from_byte_vec(bytes, None)
        .context("не удалось перечитать накладку")?;
    overlays.insert(page_number.saturating_sub(1), rendered);
    Ok(())
}

/// Перечитывает документ растеризатором после изменения структуры.
fn reload(document: &mut LoDocument, rendered: &mut PdfDocument<'static>) -> Result<()> {
    let mut bytes = Vec::new();
    document
        .save_to(&mut bytes)
        .context("не удалось сериализовать изменённый документ")?;
    *rendered = pdfium()?
        .load_pdf_from_byte_vec(bytes, None)
        .context("не удалось перечитать изменённый документ")?;
    Ok(())
}

/// Сохраняет документ — в указанный файл либо в сам открытый.
///
/// В открытый файл прямо писать нельзя: pdfium держит его, а сбой посреди
/// записи угробил бы исходник. Поэтому запись идёт во временного соседа в той
/// же папке, растеризатор пересаживается на свежие байты в памяти — и только
/// потом файлы меняются местами; переименование внутри одного диска атомарно,
/// и на каждом шаге на диске есть целый документ.
fn save_document(
    source: &mut Option<LoDocument>,
    document: &mut PdfDocument<'static>,
    original: &Path,
    target: &Path,
) -> Result<()> {
    if target != original {
        return save_copy(source, original, target);
    }

    let temp = original.with_extension("pdf.saving");
    save_copy(source, original, &temp)?;

    // Пересадка растеризатора на байты из памяти отпускает ручку исходного
    // файла — иначе Windows не даст его подменить. Память на это уже
    // заложена: точно так же документ перечитывается после каждой правки.
    let bytes = std::fs::read(&temp)
        .with_context(|| format!("не удалось перечитать {}", temp.display()))?;
    *document = pdfium()?
        .load_pdf_from_byte_vec(bytes, None)
        .context("не удалось перечитать сохранённый документ")?;

    let backup = original.with_extension("pdf.bak");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(original, &backup)
        .with_context(|| format!("не удалось отложить {}", original.display()))?;
    if let Err(e) = std::fs::rename(&temp, original) {
        // Подмена сорвалась — возвращаем исходник на место, ничего не потеряно.
        let _ = std::fs::rename(&backup, original);
        return Err(e).with_context(|| format!("не удалось заменить {}", original.display()));
    }
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

/// Пишет документ в указанный файл. Если правок не было, файл просто
/// копируется — так исходные байты доходят до цели вообще нетронутыми.
fn save_copy(source: &mut Option<LoDocument>, original: &Path, target: &Path) -> Result<()> {
    match source {
        Some(document) => {
            // Флаг в трейлере — дешёвый ответ на вопрос «есть ли в файле метки
            // блоков». Трейлер лежит в хвосте файла несжатым, и при следующем
            // открытии хватает прочитать последние килобайты, чтобы решить,
            // загружать ли структуру ради меток.
            document.trailer.set("PdfEdMarked", true);
            document
                .save(target)
                .map(|_| ())
                .with_context(|| format!("не удалось сохранить {}", target.display()))
        }
        None => std::fs::copy(original, target)
            .map(|_| ())
            .with_context(|| format!("не удалось скопировать в {}", target.display())),
    }
}

/// Перечитывает блоки с учётом меток, если файл ими помечен.
///
/// Для нетронутого документа это одна проверка хвоста файла и всё. Для
/// помеченного страница раскладывается на варианты по владельцам текста
/// ([`crate::stream_edit::mark_variants`]), и каждый разбирается отдельно —
/// блоки разных владельцев не могут слиться, даже лёжа строка в строку.
fn split_by_marks(
    blocks: Vec<Block>,
    page: u32,
    path: &Path,
    source: &mut Option<LoDocument>,
    marked: &mut Option<bool>,
) -> Vec<Block> {
    if source.is_none() {
        let flagged = *marked.get_or_insert_with(|| file_tail_has_mark_flag(path));
        if !flagged {
            return blocks;
        }
    }
    let Ok(document) = structure(path, source) else {
        return blocks;
    };
    match blocks_by_owner(document, page) {
        Ok(Some(separated)) => separated,
        Ok(None) => blocks,
        Err(e) => {
            tracing::debug!(page, "разбор по меткам не удался: {e:#}");
            blocks
        }
    }
}

/// Блоки страницы, разобранные по владельцам. `None` — меток на странице нет.
fn blocks_by_owner(document: &LoDocument, page: u32) -> Result<Option<Vec<Block>>> {
    let variants = crate::stream_edit::mark_variants(document, page + 1)?;
    if variants.is_empty() {
        return Ok(None);
    }

    let mut all = Vec::new();
    for variant in variants {
        let mut bytes = Vec::new();
        let mut document = variant.document;
        document
            .save_to(&mut bytes)
            .context("не удалось сериализовать вариант страницы")?;
        let loaded = pdfium()?
            .load_pdf_from_byte_vec(bytes, None)
            .context("не удалось перечитать вариант страницы")?;
        let variant_page = loaded.pages().get(0).context("в варианте нет страницы")?;

        let owner = variant.owner;
        if variant.rotation.abs() > 0.5 {
            // Повёрнутый блок. Обычный разбор его бы выбросил: модель абзацев
            // горизонтальна. Поэтому символы берутся вместе с поворотом,
            // распрямляются вокруг центра на записанный в метке угол,
            // разбираются как обычный текст — и полученным блокам возвращается
            // и угол, и честная повёрнутая рамка.
            let text = crate::extract::extract_page_text_with(&variant_page, true)?;
            all.extend(
                rotated_blocks(text.runs, variant.rotation)
                    .into_iter()
                    .map(|mut block| {
                        block.mark = owner;
                        block.style = variant.style;
                        if let Some(align) = variant.align {
                            block.align = align;
                        }
                        block
                    }),
            );
        } else {
            let text = extract_page_text(&variant_page)?;
            all.extend(
                detect_blocks(text.runs, &BlockOptions::default())
                    .into_iter()
                    .map(|mut block| {
                        block.mark = owner;
                        block.style = variant.style;
                        if let Some(align) = variant.align {
                            block.align = align;
                        }
                        block
                    }),
            );
        }
    }
    // Сверху вниз, как выглядят на странице: иначе блоки разных владельцев
    // шли бы группами и порядок казался бы случайным.
    all.sort_by(|a, b| b.bbox.top.total_cmp(&a.bbox.top));
    Ok(Some(all))
}

/// Собирает блок из повёрнутого на известный угол текста.
///
/// Эвристики детектора здесь не нужны и только мешают: раны повёрнутого
/// текста приходят по одному символу с приблизительными рамками, и обычная
/// модель абзацев на них рассыпается. Но для помеченного варианта гадать не о
/// чем — весь его текст и есть один блок. Раны распрямляются вокруг общего
/// центра, группируются в строки по базовым линиям — после распрямления они
/// сходятся до сотых пункта, — и блок собирается напрямую.
fn rotated_blocks(runs: Vec<crate::model::TextRun>, rotation: f32) -> Vec<Block> {
    use crate::geom::union_all;
    use crate::model::Line;
    use crate::stream_edit::rotate_about;

    let Some(whole) = union_all(runs.iter().map(|run| &run.bbox)) else {
        return Vec::new();
    };
    let centre = (whole.center_x(), whole.center_y());

    // Распрямление. Рамка рана строится от базовой линии и кегля: пересчёт
    // повёрнутой рамки дал бы раздутый прямоугольник, а базовая линия точна.
    let upright: Vec<crate::model::TextRun> = runs
        .into_iter()
        .map(|mut run| {
            run.origin = rotate_about(run.origin, centre, -rotation);
            let width = run.bbox.width().min(run.bbox.height());
            let size = run.style.size.max(1.0);
            run.bbox = crate::geom::Rect::new(
                run.origin.0,
                run.origin.1 - size * 0.25,
                run.origin.0 + width,
                run.origin.1 + size * 0.85,
            );
            run
        })
        .collect();

    // Строки: кластеры по базовой линии сверху вниз. Сортировать сразу и по
    // иксу нельзя: после распрямления игреки одной строки различаются на
    // тысячные, и «равных» значений для второго ключа не бывает — порядок
    // внутри строки получался бы случайным. Поэтому сначала кластеры по
    // игреку, а слева направо каждая строка сортируется отдельно.
    let mut lines: Vec<Line> = Vec::new();
    let mut sorted = upright;
    sorted.sort_by(|a, b| b.origin.1.total_cmp(&a.origin.1));
    for run in sorted {
        match lines.last_mut() {
            Some(line) if (line.baseline - run.origin.1).abs() <= 0.5 => {
                line.bbox = line.bbox.union(&run.bbox);
                line.runs.push(run);
            }
            _ => lines.push(Line {
                baseline: run.origin.1,
                bbox: run.bbox,
                runs: vec![run],
            }),
        }
    }
    for line in &mut lines {
        line.runs.sort_by(|a, b| a.origin.0.total_cmp(&b.origin.0));
    }
    if lines.is_empty() {
        return Vec::new();
    }

    // Рамка блока — прямоугольник вокруг текста, как он лежит на странице:
    // углы распрямлённой рамки поворачиваются обратно.
    let flat = union_all(lines.iter().map(|line| &line.bbox)).unwrap_or(whole);
    let corners = [
        (flat.left, flat.bottom),
        (flat.left, flat.top),
        (flat.right, flat.bottom),
        (flat.right, flat.top),
    ];
    let turned: Vec<(f32, f32)> = corners
        .iter()
        .map(|corner| rotate_about(*corner, centre, rotation))
        .collect();
    let xs = turned.iter().map(|p| p.0);
    let ys = turned.iter().map(|p| p.1);
    let bbox = crate::geom::Rect::new(
        xs.clone().fold(f32::INFINITY, f32::min),
        ys.clone().fold(f32::INFINITY, f32::min),
        xs.fold(f32::NEG_INFINITY, f32::max),
        ys.fold(f32::NEG_INFINITY, f32::max),
    );

    vec![Block {
        lines,
        bbox,
        align: crate::model::Align::Left,
        rotation,
        mark: None,
        style: None,
    }]
}

/// Есть ли в хвосте файла флаг «документ помечен нашими правками».
///
/// Трейлер PDF пишется в конце файла в открытом виде, поэтому четырёх
/// килобайт с конца достаточно. Ложное «нет» невозможно для наших файлов —
/// флаг ставится при каждом сохранении правленого документа.
fn file_tail_has_mark_flag(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(length) = file.seek(SeekFrom::End(0)) else {
        return false;
    };
    let take = length.min(4096);
    if file.seek(SeekFrom::End(-(take as i64))).is_err() {
        return false;
    }
    let mut tail = Vec::with_capacity(take as usize);
    if file.read_to_end(&mut tail).is_err() {
        return false;
    }
    tail.windows(b"PdfEdMarked".len())
        .any(|window| window == b"PdfEdMarked")
}

fn open_document(path: &Path) -> Result<PdfDocument<'static>> {
    pdfium()?
        .load_pdf_from_file(path, None)
        .with_context(|| format!("не удалось открыть {}", path.display()))
}

fn describe(document: &PdfDocument<'static>) -> Result<DocumentInfo> {
    let rects = document
        .pages()
        .page_sizes()
        .context("не удалось прочитать размеры страниц")?;
    let sizes = rects
        .iter()
        .map(|r| PageSize {
            width: r.right().value - r.left().value,
            height: r.top().value - r.bottom().value,
        })
        .collect::<Vec<_>>();

    Ok(DocumentInfo {
        page_count: sizes.len() as u32,
        sizes,
    })
}

fn render_tile(source: (&PdfDocument<'static>, u32), key: TileKey) -> Result<Bitmap> {
    let (document, index) = source;
    let page = document
        .pages()
        .get(index as PdfPageIndex)
        .with_context(|| format!("нет страницы {}", key.page))?;

    // Поворот здесь не задаётся намеренно: он хранится в самой странице
    // (`/Rotate`) и применяется pdfium автоматически. В ключе тайла поворот
    // присутствует лишь затем, чтобы поворот страницы обесценивал кэш.
    let config = PdfRenderConfig::new()
        .scale_page_by_factor(key.zoom.scale())
        .render_annotations(true)
        // pdfium-render по умолчанию включает FPDF_REVERSE_BYTE_ORDER, и тогда
        // буфер приходит в RGBA. Нам нужен родной для pdfium BGRA — ровно в
        // таком порядке хранит текстуры gpui, так что растр уходит на GPU без
        // единого прохода по пикселям. Порядок каналов закреплён тестом
        // `rendered_pixels_are_in_bgra_order`.
        .set_reverse_byte_order(false);

    let bitmap = page
        .render_with_config(&config)
        .context("сбой растеризации страницы")?;

    Ok(Bitmap {
        width: bitmap.width() as u32,
        height: bitmap.height() as u32,
        pixels: bitmap.as_raw_bytes(),
    })
}

fn read_blocks(source: (&PdfDocument<'static>, u32)) -> Result<Vec<Block>> {
    let (document, page_index) = source;
    let page = document
        .pages()
        .get(page_index as PdfPageIndex)
        .with_context(|| format!("нет страницы {page_index}"))?;

    let text = extract_page_text(&page)?;
    if text.rotated_chars > 0 {
        tracing::debug!(
            page = page_index,
            skipped = text.rotated_chars,
            "повёрнутый текст пропущен"
        );
    }
    Ok(detect_blocks(text.runs, &BlockOptions::default()))
}
