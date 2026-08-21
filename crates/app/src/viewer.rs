//! Просмотр документа: миниатюры, виртуализированная лента страниц, зум и
//! подсветка найденных текстовых блоков.
//!
//! Ни одна страница не растеризуется «на всякий случай». На каждый кадр
//! вычисляется список нужных тайлов вокруг вьюпорта и целиком передаётся в
//! [`Renderer`], заменяя предыдущий, — устаревшие запросы отмирают сами.
//! Поэтому документ на 700 страниц открывается так же быстро, как на пяти:
//! известны только размеры страниц, а пиксели появляются по мере надобности.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Div, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ListAlignment, ListSizingBehavior, ListState, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Point, Render, RenderImage, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Window, div, img, list, px, rgba, white,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, Sizable};
use image::{Frame, RgbaImage};
use pdfcore::cache::{DEFAULT_TILE_BUDGET, THUMBNAIL_HEIGHT_PX};
use pdfcore::geom::Rect;
use pdfcore::model::{Align, Style};
use pdfcore::{
    Bitmap, Block, BlockEdit, BlockRewrite, DocumentInfo, RenderEvent, Renderer, Rotation,
    TileCache, TileKey, ZoomBucket,
};

use crate::rich_text::{RichText, RunStyle};
use crate::templates::Templates;
use crate::text_element::{EditorEvent, RichTextEditor};
use crate::text_layout::BaseStyle;

/// Высота плавающей панели формата вместе с отступом до блока.
pub(crate) const FORMAT_BAR_HEIGHT: f32 = 42.0;

/// Выделенный для правки абзац вместе с текущим состоянием панели формата.
pub(crate) struct Selection {
    pub page: u32,
    /// Рамка найденного абзаца. По ней на странице опознаётся текст, который
    /// заменяется, поэтому она не меняется никогда.
    pub bbox: Rect,
    /// Рамка на экране: куда ляжет новый текст и по какой ширине он
    /// переносится. Поначалу совпадает с найденной, дальше её двигают и
    /// растягивают маркерами.
    pub frame: Rect,
    pub style: Style,
    /// Текст абзаца в тот миг, когда его выделили. По нему видно, правили
    /// содержимое или только двигали рамку.
    pub original_text: String,
    pub lines: usize,
    /// Метка стиля, с которой блок пришёл со страницы. Сравнивается с
    /// текущей: привязка и отвязка стиля — это правка документа, даже когда
    /// текст и оформление остались прежними.
    pub seeded_style_id: Option<i64>,
    /// Строки блока как они стоят на странице: текст, настоящая ширина и
    /// кегль. Эталон для выбора между шрифтами-тёзками при приёме метрик.
    pub sample_lines: Vec<(String, f32, f32)>,
    pub align: Align,
    /// Выключку или интерлиньяж меняли вручную, значит абзац придётся
    /// перенабрать, даже если сам текст остался прежним.
    pub needs_retypeset: bool,
    /// Интерлиньяж исходного блока; сохраняется при перевёрстке.
    pub line_height: Option<f32>,
    /// Оформление, вычитанное из документа. Приходит асинхронно, поэтому
    /// поначалу пусто.
    pub page_font: Option<PageFont>,
    /// Владелец блока: номер метки редактора. По нему правка берёт только
    /// свой текст, когда рамки блоков пересекаются.
    pub owner: Option<i64>,
    /// Правку трогали: текст, рамку, оформление — неважно. По этому признаку
    /// щелчок мимо блока решает, применять или просто снять выделение.
    pub touched: bool,
    /// Блок создаётся с нуля инструментом «Текст»: в рамке ещё нет ни одного
    /// оператора показа, и перенабору это надо знать заранее.
    /// Межбуквенный интервал, заданный в панели. `None` — как в документе.
    pub char_spacing_override: Option<f32>,
    /// Горизонтальный масштаб в процентах, заданный в панели.
    pub h_scale_override: Option<f32>,
    /// Значения блока из документа — чтобы поля панели было чем заполнить.
    pub seeded_char_spacing: f32,
    pub seeded_h_scale: f32,
    /// Отбивка между абзацами, заданная в панели.
    pub para_spacing_override: Option<f32>,
    /// Фон за буквами. Приходит из метки блока и правится панелью.
    pub fill: Option<[f32; 3]>,
    /// Стиль документа, за которым следует блок. Пишется в метку при
    /// перенаборе; выставляется кнопкой «Применить» в окне стилей.
    pub style_id: Option<i64>,
    pub creating: bool,
    /// Поворот абзаца в градусах.
    pub rotation: f32,
}

/// Активный инструмент нижней панели.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Tool {
    /// Обычный курсор: выделение и правка существующего.
    #[default]
    Cursor,
    /// Текст: растяжка по пустому месту создаёт новый текстовый блок.
    Text,
}

/// Пункт списка гарнитур: имя, написанное этой же гарнитурой.
///
/// Какому свойству показать список готовых значений.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresetKind {
    /// Межбуквенное расстояние, пункты.
    CharSpacing,
    /// Горизонтальный масштаб, проценты.
    HScale,
    /// Интерлиньяж — множители кегля.
    LineHeight,
}

/// Кому применяется готовое значение: правимому блоку или группе.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresetTarget {
    Editor,
    Multi,
}

/// Раскрытый список готовых значений: у какого поля и где на экране.
pub(crate) struct PresetMenu {
    pub kind: PresetKind,
    pub target: PresetTarget,
    pub at: Point<Pixels>,
}

/// Кому пикер выбирает шрифт.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum FontTarget {
    /// Правимому блоку — панель свойств одиночного выделения.
    Editor,
    /// Группе блоков.
    Multi,
    /// Шрифту документа, которого нет в системе: выбранная гарнитура станет
    /// его заменой. Имя — как оно записано в PDF.
    Substitute(String),
}

/// Раскрывающийся список шрифтов — как в издательских пакетах: семейства с
/// начертаниями внутри. Обычный выпадающий список умеет показать только
/// плоский перечень имён, а здесь у каждого семейства раскрывается свой
/// список: Light, Regular, Semibold, Italic…
pub(crate) struct FontPickerUi {
    pub target: FontTarget,
    /// Строка поиска по именам семейств.
    pub query: Entity<InputState>,
    /// Раскрытые семейства.
    pub expanded: HashSet<String>,
    /// Прокрутка списка.
    pub scroll: gpui::ScrollHandle,
}

/// Состояние режима «Систематизировать страницы».
pub(crate) struct Organise {
    /// Выбранные страницы, нумерация с нуля.
    pub picked: Vec<u32>,
    /// Разрешено ли выделять несколько страниц разом.
    pub multi: bool,
    /// Ширина карточки в пикселях — ползунок внизу.
    pub card: f32,
    /// Сколько карточек помещается в ряд. Считается по ширине сетки при
    /// пересчёте спроса на растры: по нему страницы разбиваются на ряды, и
    /// колонки стоят строго друг под другом.
    pub columns: u32,
    /// Прокрутка сетки: по ней и только по ней решается, какие страницы
    /// сейчас нужны растеризатору. Панель миниатюр в этом режиме спрятана и
    /// на подгрузку больше не влияет.
    pub scroll: gpui::ScrollHandle,
    /// Страница, которую тащат, и место, куда её положат.
    pub dragging: Option<u32>,
    /// Номер щели, над которой сейчас курсор: страница встанет перед ней.
    pub drop_at: Option<u32>,
    /// Нажатая, но ещё не понесённая страница: перенос начинается только
    /// после того, как курсор ушёл с места. Иначе простой щелчок считался
    /// переносом — сетка перестраивалась вся, растры выгружались из атласа и
    /// страницы «перезагружались» на каждый клик.
    pub press: Option<(u32, Point<Pixels>)>,
    /// Точка отсчёта для выделения диапазона по Shift.
    pub anchor: Option<u32>,
    /// Скопированные страницы: номера с нуля на момент копирования.
    pub clipboard: Vec<u32>,
}

impl Default for Organise {
    fn default() -> Self {
        Organise {
            picked: Vec::new(),
            multi: false,
            card: 150.0,
            columns: 1,
            scroll: gpui::ScrollHandle::new(),
            dragging: None,
            drop_at: None,
            press: None,
            anchor: None,
            clipboard: Vec::new(),
        }
    }
}

/// Окно «Стили»: именованные стили документа.
pub(crate) struct StylesWindow {
    pub defs: Vec<pdfcore::StyleDef>,
    pub loading: bool,
    /// Поля рядов. Пересобираются в кадре, когда каталог сменился:
    /// собирать их в обработчике события нельзя — там нет окна.
    pub rows: Vec<StyleRowInputs>,
}

/// Редактируемые поля одного стиля.
pub(crate) struct StyleRowInputs {
    pub id: i64,
    pub name: Entity<InputState>,
    pub family: Entity<InputState>,
    pub size: Entity<InputState>,
}

/// Окно «Шрифты»: чем набран документ и чего не хватает системе.
pub(crate) struct FontsWindow {
    /// Опись из документа. Пусто, пока идёт первый обход страниц.
    pub fonts: Vec<pdfcore::DocumentFont>,
    /// Обход ещё идёт.
    pub loading: bool,
}

/// Меню панели миниатюр: по странице либо по разделителю-вставке.
pub(crate) enum PanelMenu {
    /// Контекстное меню страницы, позиция — в координатах окна.
    Page { page: u32, at: Point<Pixels> },
    /// Правый клик по тексту в чтении: копирование, вставка, удаление.
    /// Пункты гаснут по обстановке — пустой буфер, нет выделения.
    TextOps { at: Point<Pixels> },
    /// Меню вставки после страницы: пустой лист либо целый документ.
    /// `before_first` — щель перед самой первой страницей.
    Insert {
        after: u32,
        at: Point<Pixels>,
        before_first: bool,
    },
}

/// Порог, после которого нажатие считается протяжкой, а не щелчком.
///
/// Мышь под пальцем всегда чуть дрожит; без порога каждый щелчок оборачивался
/// бы рамкой шириной в пиксель и снимал выделение вместо того, чтобы его
/// поставить.
const DRAG_THRESHOLD: f32 = 8.0;

/// Отошло ли нажатие от места достаточно далеко, чтобы считаться протяжкой.
fn press_became_drag(from: Point<Pixels>, to: Point<Pixels>) -> bool {
    f32::from((to.x - from.x).abs()) > DRAG_THRESHOLD
        || f32::from((to.y - from.y).abs()) > DRAG_THRESHOLD
}

/// Снимок блока в буфере: всё, что нужно, чтобы выложить копию.
/// Буфер скопированных блоков — один на всю программу.
///
/// Раньше он был полем просмотрщика и умирал при открытии другого документа:
/// Ctrl+C в одной книге, Ctrl+V в другой — и вставлять уже нечего. Рядом с
/// блоками хранится путь исходника: вставка в чужой документ не может
/// полагаться на его шрифты и переводит документные стили в явные запросы.
static BLOCK_CLIPBOARD: std::sync::Mutex<(Option<PathBuf>, Vec<CopiedBlock>)> =
    std::sync::Mutex::new((None, Vec::new()));

/// Кладёт блоки в буфер программы.
fn clipboard_store(source: &Path, blocks: Vec<CopiedBlock>) {
    if let Ok(mut guard) = BLOCK_CLIPBOARD.lock() {
        *guard = (Some(source.to_path_buf()), blocks);
    }
}

/// Забирает копию буфера и признак «скопировано в этом же документе».
fn clipboard_take(current: &Path) -> (bool, Vec<CopiedBlock>) {
    match BLOCK_CLIPBOARD.lock() {
        Ok(guard) => (guard.0.as_deref() == Some(current), guard.1.clone()),
        Err(_) => (false, Vec::new()),
    }
}

/// Пуст ли буфер блоков.
pub(crate) fn clipboard_is_empty() -> bool {
    BLOCK_CLIPBOARD
        .lock()
        .map(|guard| guard.1.is_empty())
        .unwrap_or(true)
}

#[derive(Clone)]
pub(crate) struct CopiedBlock {
    pub runs: Vec<crate::rich_text::Run>,
    pub bbox: Rect,
    pub line_height: Option<f32>,
    pub align: Align,
    pub rotation: f32,
    pub base_family: String,
    pub page: u32,
}

/// Перетаскивание всей группы за любой её блок.
pub(crate) struct GroupDrag {
    pub page: u32,
    pub start: Point<Pixels>,
    pub current: Point<Pixels>,
}

/// Нажатие мыши, судьба которого решится при отпускании.
pub(crate) struct PendingPress {
    pub page: u32,
    /// Блок под курсором, если нажали по блоку.
    pub block: Option<usize>,
    pub at: Point<Pixels>,
}

/// Резиновая рамка выделения нескольких блоков.
pub(crate) struct Rubber {
    /// Страница, по которой тянут рамку.
    pub page: u32,
    /// Блок под точкой нажатия, если рамку начали прямо с текста. Если рамка
    /// в итоге никого не накрыла, выделяется он: значит, человек всё-таки
    /// щёлкал по блоку, а рамка вышла случайно, от дрогнувшей руки.
    pub block: Option<usize>,
    /// Начало и текущая точка, в координатах окна.
    pub start: Point<Pixels>,
    pub current: Point<Pixels>,
}

impl Rubber {
    /// Рамка уже похожа на рамку, а не на дрогнувший щелчок.
    pub fn meaningful(&self) -> bool {
        press_became_drag(self.start, self.current)
    }
}

/// Блок в групповом выделении: всё, что нужно для правки без повторного
/// поиска по странице.
#[derive(Clone)]
pub(crate) struct MultiTarget {
    pub page: u32,
    pub bbox: Rect,
    pub owner: Option<i64>,
    pub size: f32,
    pub line_height: Option<f32>,
    pub align: Align,
}

/// Оформление абзаца, вычитанное из самого документа.
pub(crate) struct PageFont {
    /// Имя шрифта так, как оно записано в PDF, — для сообщений пользователю.
    pub base_font: String,
    pub size: f32,
    pub color: Option<gpui::Hsla>,
    /// Вшита ли программа шрифта в документ. Без неё и без системного
    /// собрата блок можно набрать только заменой.
    pub embedded: bool,
}

/// Поля ввода, живущие вместе с выделением.
/// Числовые поля панели свойств: геометрия и текстовые метрики.
///
/// Живут вместе с выделением и пересеваются свежими значениями на каждом
/// кадре, пока их не редактируют: значение, изменённое мышью на странице,
/// обязано тут же появиться в поле.
pub(crate) struct GeometryInputs {
    pub x: Entity<InputState>,
    pub y: Entity<InputState>,
    pub w: Entity<InputState>,
    pub h: Entity<InputState>,
    pub angle: Entity<InputState>,
    pub char_spacing: Entity<InputState>,
    pub h_scale: Entity<InputState>,
    pub para_spacing: Entity<InputState>,
}

pub(crate) struct EditWidgets {
    /// Собственный текстовый элемент: он рисует текст оформлением документа и
    /// умеет разные шрифты внутри одного абзаца.
    pub editor: Entity<RichTextEditor>,
    pub size: Entity<InputState>,
    pub geometry: GeometryInputs,
    /// Палитра своего цвета: свотчей мало, а книга красит текст как хочет.
    pub color: Entity<gpui_component::color_picker::ColorPickerState>,
    /// Палитра заливки — фона за буквами.
    pub fill: Entity<gpui_component::color_picker::ColorPickerState>,
}

const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;
const ZOOM_STEP: f32 = 1.25;

/// Сколько страниц растеризуется наперёд по ходу прокрутки и сколько — назад.
/// Вперёд больше: читают почти всегда вниз.
/// Поля вокруг страницы в ленте: отступы карточки, рамка и строка с номером.
/// Вычитаются при подгонке, иначе подогнанная страница на пиксель да не
/// влезает и лента получает лишнюю полосу прокрутки.
const PAGE_CHROME_X: f32 = 24.0;
const PAGE_CHROME_Y: f32 = 52.0;
/// Зазор между страницами разворота.
const SPREAD_GAP: f32 = 16.0;

/// Потолок растра одной страницы в точках. Двенадцать мегапикселей — это
/// 48 МБ на тайл: пять-шесть страниц ещё умещаются в бюджет кэша разом.
const MAX_TILE_PIXELS: f32 = 12_000_000.0;

/// Насколько близко к заказанному месту должна лечь вставленная копия,
/// чтобы считаться ею. Перенабор ставит строки по метрикам шрифта, и рамка
/// сходится не буква в букву; но чужой блок рядом отстоит куда дальше.
const PASTE_MATCH: f32 = 8.0;

const PREFETCH_AHEAD: u32 = 4;
const PREFETCH_BEHIND: u32 = 1;
/// Сколько миниатюр готовится за пределами видимой части панели.
const THUMB_PREFETCH: u32 = 14;

const THUMB_PANEL_WIDTH: f32 = 208.0;

/// Синий акцент панели страниц — тот же, что на макете. Тема в тёмном
/// режиме делает `primary` белым, а лист и разделитель должны выделяться
/// именно синим, иначе панель выглядит серой полосой без опор для глаза.
fn accent_blue() -> gpui::Hsla {
    gpui_component::blue_500()
}

/// Ширина выпадающего меню панели страниц.
const MENU_WIDTH: f32 = 190.0;

fn v_flex() -> Div {
    div().flex().flex_col()
}

fn h_flex() -> Div {
    div().flex().flex_row()
}

/// Открытый документ со всем состоянием просмотра.
pub(crate) struct Document {
    renderer: Renderer,
    pub(crate) info: DocumentInfo,
    /// Лента страниц и панель миниатюр — два независимых виртуальных списка
    /// над одним и тем же документом.
    pages: ListState,
    thumbs: ListState,
    /// Один кэш на оба списка: миниатюра — это тот же тайл, только в другом
    /// бакете масштаба, и разделять их незачем.
    /// Каталог именованных стилей документа. Обновляется каждым событием
    /// `Styles`; по нему панель свойств правит стиль выделенного блока.
    pub(crate) styles: Vec<pdfcore::StyleDef>,
    tiles: TileCache<Arc<RenderImage>>,
    pub(crate) blocks: HashMap<u32, Vec<Block>>,
    blocks_requested: HashSet<u32>,
    pub(crate) zoom: f32,
    rotation: Rotation,
    show_blocks: bool,
    current_page: u32,
    /// Последний отправленный список — чтобы не дёргать поток рендера
    /// одинаковыми запросами на каждый кадр.
    requested: Vec<TileKey>,
    /// Страницы, чьи растры устарели после правки, но ещё показываются:
    /// выбрасывать старый растр до прихода нового — это белая вспышка на
    /// месте страницы при каждом «Применить».
    stale_pages: HashSet<u32>,
    /// Ключ миниатюры первой страницы: по нему узнаётся тайл, который стоит
    /// сохранить на диск для стартовой страницы.
    thumb_key_page0: Option<TileKey>,
    thumbnail_saved: bool,
    /// Страница с непринятой правкой, отрисованная по-настоящему. Показывается
    /// вместо тайла, пока идёт набор, и тем самым правка видна прямо в тексте
    /// документа. Хранится отдельно от кэша: это не состояние документа, а
    /// временная картинка, которая исчезает вместе с выделением.
    preview: Option<(u32, Arc<RenderImage>)>,
    /// Есть несохранённые правки.
    dirty: bool,
    /// Последнее сообщение для строки состояния: результат сохранения либо
    /// причина отказа в правке.
    status: Option<String>,
}

impl Document {
    fn thumbnail_key(&self, page: u32, scale_factor: f32) -> Option<TileKey> {
        self.preview_key(page, THUMBNAIL_HEIGHT_PX, scale_factor)
    }

    /// Ключ растра, подобранный под нужную высоту показа.
    ///
    /// Сетка «Систематизировать» показывает страницы крупнее ленты миниатюр,
    /// и растр обязан расти вместе с карточкой: иначе увеличенная страница
    /// выглядит размытой, а уменьшенная зря занимает память.
    fn preview_key(&self, page: u32, height_px: f32, scale_factor: f32) -> Option<TileKey> {
        let size = self.info.size(page)?;
        if size.height <= 0.0 {
            return None;
        }
        let scale = height_px / size.height * scale_factor;
        Some(TileKey {
            page,
            zoom: ZoomBucket::from_scale(scale),
            rotation: self.rotation,
        })
    }

    fn page_key(&self, page: u32, scale_factor: f32) -> TileKey {
        TileKey {
            page,
            zoom: ZoomBucket::from_scale(self.raster_scale(page, scale_factor)),
            rotation: self.rotation,
        }
    }

    /// Масштаб растеризации страницы — тот же зум, но с потолком по числу
    /// точек.
    ///
    /// Растр целой страницы растёт как квадрат зума: на 500 % разворот в
    /// 5300×6900 точек весит полтораста мегабайт, в бюджет кэша влезает пара
    /// таких, и соседи вытесняются на каждой прокрутке — счётчики внизу
    /// пляшут, страницы моргают. Выше потолка картинка просто растягивается:
    /// мягче, чем была бы, но зато без вечной перерисовки.
    fn raster_scale(&self, page: u32, scale_factor: f32) -> f32 {
        let want = self.zoom * scale_factor;
        let Some(size) = self.info.size(page) else {
            return want;
        };
        let area = (size.width * size.height).max(1.0);
        let cap = (MAX_TILE_PIXELS / area).sqrt();
        want.min(cap)
    }
}

/// О чём просмотрщик сообщает наружу. Сам он не знает ни про список недавних,
/// ни про файловые диалоги — этим ведает [`crate::workspace::Workspace`].
pub enum ViewerEvent {
    GoHome,
}

impl gpui::EventEmitter<ViewerEvent> for Viewer {}

pub struct Viewer {
    path: PathBuf,
    pub(crate) doc: Option<Document>,
    error: Option<String>,
    focus: FocusHandle,
    pub(crate) selected: Option<Selection>,
    pub(crate) widgets: Option<EditWidgets>,
    pub(crate) templates: Templates,
    /// Начатое перетаскивание рамки выделенного абзаца.
    pub(crate) drag: Option<crate::frame::FrameDrag>,
    /// Выравнивать ли рамку по соседним блокам. Переключается магнитом в
    /// заголовке и хранится на весь сеанс: это привычка, а не свойство
    /// документа.
    pub(crate) snapping: bool,
    /// Линии, показывающие, чему рамка стала вровень. Живут только пока её
    /// тянут.
    pub(crate) guides: Vec<crate::frame::Guide>,
    /// Левый верхний угол страницы с выделением, в координатах окна. Мышь
    /// приходит именно в них, а рамка живёт в координатах страницы.
    pub(crate) page_origin: Point<Pixels>,
    /// Показывать ли правую панель свойств.
    pub(crate) show_properties: bool,
    /// Режим «Систематизировать»: сетка страниц вместо ленты. `None` —
    /// обычное чтение.
    pub(crate) organise: Option<Organise>,
    /// Окно «Стили»: каталог стилей документа. `None` — окно закрыто.
    pub(crate) styles_window: Option<StylesWindow>,
    /// Окно «Шрифты»: опись шрифтов документа. `None` — окно закрыто.
    pub(crate) fonts_window: Option<FontsWindow>,
    /// Подмены отсутствующих шрифтов: ключ семейства из документа → выбранная
    /// системная гарнитура. Ими набираются блоки, чей шрифт не встроен и не
    /// установлен: без подмены такой абзац вообще нечем перенабрать.
    pub(crate) font_substitutes: HashMap<String, String>,
    /// Открытое меню панели миниатюр.
    pub(crate) panel_menu: Option<PanelMenu>,
    /// Скопированная страница — для «Вставить».
    pub(crate) copied_page: Option<u32>,
    /// Буфер блоков: Ctrl+C кладёт сюда выделенный блок или группу,
    /// Ctrl+V выкладывает копии на видимую страницу.
    /// Перетаскивание группы целиком: нажали на блок группы и повели.
    group_drag: Option<GroupDrag>,
    /// Нажатие, ещё не ставшее ни щелчком, ни рамкой. Разрешается на
    /// отпускании: без движения это щелчок, с движением — рамка выделения.
    pub(crate) pending_press: Option<PendingPress>,
    /// Резиновая рамка выделения, пока её тянут по странице.
    pub(crate) rubber: Option<Rubber>,
    /// Несколько выделенных блоков — для групповых свойств и «Сгруппировать».
    pub(crate) multi: Vec<MultiTarget>,
    /// Кегль в панели групповых свойств. Пустой, когда у блоков он разный, —
    /// как в Acrobat.
    pub(crate) multi_size: Option<Entity<InputState>>,
    /// Поля типографики группы: разрядка и горизонтальный масштаб. Пустые,
    /// пока не введено значение, — у блоков группы они могут различаться.
    pub(crate) multi_char_spacing: Option<Entity<InputState>>,
    pub(crate) multi_h_scale: Option<Entity<InputState>>,
    pub(crate) multi_para_spacing: Option<Entity<InputState>>,
    /// Гарнитура для всей группы: выбор в списке перенабирает каждый блок.
    /// Гарнитура, выбранная для группы. Пустая, пока не выбирали, — как в
    /// Acrobat: без общего значения показывать нечего.
    pub(crate) multi_family: Option<String>,
    /// Свой цвет для всей группы — палитра, как у одиночного блока.
    pub(crate) multi_color: Option<Entity<gpui_component::color_picker::ColorPickerState>>,
    /// Углы всех видимых страниц в координатах окна: мышь приходит в оконных,
    /// а рамка выделения живёт в страничных.
    ///
    /// На страницу приходится несколько кандидатов: лента раскладывает её и
    /// на своём месте, и ещё раз ниже — примеркой для следующего кадра. Со
    /// стороны обе покраски неотличимы, поэтому нужный угол выбирается по
    /// самой точке нажатия: настоящая страница — та, внутри которой она
    /// лежит. Раньше побеждала последняя запись, и рамка выделения уезжала на
    /// страницу вниз.
    pub(crate) page_origins: HashMap<u32, Vec<Point<Pixels>>>,
    /// Углы, которые копятся прямо сейчас, в идущем кадре. Читать их рано:
    /// отрисовка идёт после сборки дерева, и в самой сборке они ещё пусты.
    /// Поэтому в начале каждого кадра собранное переезжает в `page_origins`, а
    /// это поле начинает копить заново.
    page_origins_next: HashMap<u32, Vec<Point<Pixels>>>,
    /// Активный инструмент нижней панели.
    pub(crate) tool: Tool,
    /// Куда легла вставленная копия: страница и рамки, которых ждём в свежем
    /// разборе. Как только блоки придут, они станут выделенными — иначе
    /// вставленное поверх чужого текста уже не выбрать мышью.
    pending_paste: Option<(u32, Vec<Rect>)>,
    /// Открытый список выбора шрифта.
    pub(crate) font_picker: Option<FontPickerUi>,
    /// Открытый список готовых значений у числового поля панели.
    pub(crate) preset_menu: Option<PresetMenu>,
    /// Гарнитура, выбранная в списке для правимого блока. `None` — гарнитура
    /// блока не менялась.
    pub(crate) chosen_family: Option<String>,
    /// Последние выбранные гарнитуры, свежие впереди. Показываются шапкой
    /// пикера: за правку одной книги шрифты гоняют по кругу.
    recent_fonts: Vec<String>,
    /// Точное начертание, если его выбрали в раскрытом семействе.
    pub(crate) chosen_face: Option<String>,
    /// Последний отказ движка. Показывается заметной полосой поверх
    /// страницы: строку состояния внизу окна легко не заметить, и человек
    /// решал, что правка «просто не работает».
    pub(crate) failure: Option<String>,
    /// Правимому абзацу нужно отдать клавиатуру на ближайшей отрисовке.
    focus_editor: bool,
    /// Просьба вернуть клавиатуру самому виду: в сетке страниц горячие
    /// клавиши работают только когда фокус не увели поля ввода.
    focus_root: bool,
    /// Плотность пикселей окна, снятая на последней отрисовке. Нужна там, где
    /// растр запрашивается не из `render` и окна под рукой нет.
    scale_factor: f32,
    /// Сдвиг страниц поперёк ленты: положительный — содержимое уехало влево,
    /// то есть видно правый край. Нужен, когда страница шире окна: ленте
    /// самой вбок ехать некуда.
    pan_x: f32,
    /// Протяжка колёсиком: с чего начали и каким был сдвиг на тот момент.
    pan_drag: Option<PanDrag>,
    /// По какому размеру подбирается масштаб чтения.
    fit: Fit,
    /// Разворот: две страницы в ряд, как в раскрытой книге.
    spread: bool,
    /// Место, отведённое ленте в окне, снятое на прошлой отрисовке. По нему
    /// считается подгонка: размер окна известен только после раскладки.
    canvas_size: gpui::Size<Pixels>,
}

/// Протяжка страницы колёсиком: точка захвата и сдвиг на её момент.
#[derive(Clone, Copy)]
pub(crate) struct PanDrag {
    at: Point<Pixels>,
    pan: f32,
}

/// Как подбирается масштаб чтения.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Fit {
    /// Масштаб задан руками — колесом, кнопками, «100 %».
    #[default]
    Free,
    /// Страница по ширине окна.
    Width,
    /// Страница целиком по высоте окна.
    Height,
}

impl Viewer {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut viewer = Viewer {
            path: path.clone(),
            doc: None,
            error: None,
            focus: cx.focus_handle(),
            selected: None,
            widgets: None,
            templates: Templates::load(),
            drag: None,
            organise: None,
            styles_window: None,
            fonts_window: None,
            font_substitutes: HashMap::new(),
            panel_menu: None,
            copied_page: None,
            group_drag: None,
            pending_press: None,
            rubber: None,
            pending_paste: None,
            font_picker: None,
            preset_menu: None,
            failure: None,
            chosen_family: None,
            recent_fonts: Vec::new(),
            chosen_face: None,
            tool: Tool::default(),
            multi: Vec::new(),
            multi_size: None,
            multi_char_spacing: None,
            multi_h_scale: None,
            multi_para_spacing: None,
            multi_family: None,
            multi_color: None,
            page_origins: HashMap::new(),
            page_origins_next: HashMap::new(),
            snapping: true,
            guides: Vec::new(),
            page_origin: gpui::point(px(0.0), px(0.0)),
            show_properties: true,
            focus_editor: false,
            focus_root: false,
            scale_factor: 1.0,
            pan_x: 0.0,
            pan_drag: None,
            fit: Fit::default(),
            spread: false,
            canvas_size: gpui::size(px(0.0), px(0.0)),
        };
        viewer.open(path, cx);
        viewer
    }

    /// Число страниц открытого документа; `None`, если открыть не удалось.
    pub fn page_count(&self) -> Option<u32> {
        self.doc.as_ref().map(|doc| doc.info.page_count)
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let (tx, rx) = flume::unbounded();

        let (renderer, info) = match Renderer::open(&path, tx) {
            Ok(opened) => opened,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                tracing::error!(path = %path.display(), "не удалось открыть документ: {e:#}");
                return;
            }
        };
        tracing::info!(pages = info.page_count, path = %path.display(), "документ открыт");

        let count = info.page_count as usize;
        // Запас отрисовки за пределами экрана: список успевает подготовить
        // соседние страницы до того, как они въедут в кадр.
        let pages = ListState::new(count, ListAlignment::Top, px(900.0));
        let thumbs = ListState::new(count, ListAlignment::Top, px(400.0));

        // Прокрутка обязана перерисовать вид: именно в render() пересчитывается
        // список нужных тайлов.
        for state in [&pages, &thumbs] {
            let this = cx.entity().downgrade();
            state.set_scroll_handler(move |_, _, cx| {
                this.update(cx, |_, cx| cx.notify()).ok();
            });
        }

        // Результаты рендера приходят из фонового потока. Ждём их асинхронно,
        // без опроса по таймеру.
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv_async().await {
                if this
                    .update(cx, |viewer, cx| viewer.on_render_event(event, cx))
                    .is_err()
                {
                    break; // окно закрыто
                }
            }
        })
        .detach();

        // Каталог стилей нужен панели свойств с первого выделения.
        renderer.request_styles();

        self.path = path;
        self.error = None;
        self.doc = Some(Document {
            renderer,
            info,
            pages,
            thumbs,
            styles: Vec::new(),
            tiles: TileCache::new(DEFAULT_TILE_BUDGET),
            blocks: HashMap::new(),
            blocks_requested: HashSet::new(),
            stale_pages: HashSet::new(),
            zoom: 1.0,
            rotation: Rotation::None,
            show_blocks: false,
            current_page: 0,
            requested: Vec::new(),
            thumb_key_page0: None,
            thumbnail_saved: false,
            preview: None,
            dirty: false,
            status: None,
        });
    }

    fn on_render_event(&mut self, event: RenderEvent, cx: &mut Context<Self>) {
        let document_path = self.path.clone();
        // Страница, для которой показ ещё имеет смысл. Пока растр готовился,
        // правку могли отменить — и тогда пришедшая картинка показывала бы
        // текст, которого в документе нет.
        let previewing = self.selected.as_ref().map(|selection| selection.page);
        let evicted = {
            let Some(doc) = self.doc.as_mut() else { return };
            match event {
                RenderEvent::Tile { key, bitmap } => {
                    // Миниатюра первой страницы уходит на диск: из неё
                    // стартовая страница строит превью, не открывая документ.
                    if !doc.thumbnail_saved && doc.thumb_key_page0 == Some(key) {
                        doc.thumbnail_saved = true;
                        let target = crate::recents::thumbnail_path(&document_path);
                        if let Err(e) = save_thumbnail(&target, &bitmap) {
                            tracing::warn!("не удалось сохранить миниатюру: {e:#}");
                        }
                    }
                    let bytes = bitmap.pixels.len();
                    // Первый свежий растр правленой страницы вытесняет все её
                    // старые: они рисовали ещё непоправленный текст.
                    let mut evicted = if doc.stale_pages.remove(&key.page) {
                        doc.tiles.invalidate_page(key.page)
                    } else {
                        Vec::new()
                    };
                    let bytes_len = bytes;
                    match to_texture(bitmap) {
                        Some(texture) => {
                            evicted.extend(doc.tiles.insert(key, texture, bytes_len));
                            evicted
                        }
                        None => Vec::new(),
                    }
                }
                RenderEvent::Blocks { page, blocks } => {
                    doc.blocks.insert(page, blocks);
                    Vec::new()
                }
                RenderEvent::Preview { page, bitmap } => {
                    // Растр приходит уже в BGRA, как и обычный тайл, поэтому и
                    // путь на GPU у него тот же. А вот опоздавший показ на GPU
                    // не попадает вовсе: правку успели отменить, и он рисовал
                    // бы текст, которого в документе нет.
                    let previous = match previewing == Some(page) {
                        true => match to_texture(bitmap) {
                            Some(texture) => doc.preview.replace((page, texture)),
                            None => doc.preview.take(),
                        },
                        false => doc.preview.take(),
                    };
                    previous.map(|(_, texture)| texture).into_iter().collect()
                }
                RenderEvent::Edited { page, outcome } => {
                    match outcome {
                        Some(outcome) => tracing::info!(
                            page,
                            cleared = outcome.cleared_ops,
                            lines = outcome.created_lines,
                            "блок перенабран"
                        ),
                        None => tracing::info!(page, "блок переставлен без перенабора"),
                    }
                    doc.dirty = true;
                    // Страница изменилась: её растры и разбор устарели. Но
                    // старый растр продолжает показываться, пока не придёт
                    // новый, — иначе страница на мгновение белеет при каждом
                    // применении правки.
                    doc.blocks.remove(&page);
                    doc.blocks_requested.remove(&page);
                    doc.requested.clear();
                    doc.stale_pages.insert(page);
                    doc.renderer.request_blocks(page);
                    Vec::new()
                }
                RenderEvent::Styles { styles, changed } => {
                    if changed {
                        doc.dirty = true;
                    }
                    // Каталог держится при документе: он нужен не только окну
                    // стилей, но и панели свойств — там правится стиль блока.
                    doc.styles = styles.clone();
                    if let Some(window) = self.styles_window.as_mut() {
                        window.defs = styles;
                        window.loading = false;
                        // Ряды пересоберёт кадр: их поля требуют окна.
                        window.rows.clear();
                    }
                    cx.notify();
                    return;
                }
                RenderEvent::Fonts { fonts } => {
                    if let Some(window) = self.fonts_window.as_mut() {
                        window.fonts = fonts;
                        window.loading = false;
                    }
                    cx.notify();
                    return;
                }
                RenderEvent::BlockFont { font, metrics, .. } => {
                    self.adopt_page_font(font, metrics, cx);
                    return;
                }
                RenderEvent::PagesChanged { info } => {
                    // Каскад стиля и его откат приходят этим событием; если
                    // окно стилей открыто, каталог перечитывается.
                    if self.styles_window.is_some() {
                        doc.renderer.request_styles();
                    }
                    // Номера страниц съехали: все постраничные кэши мертвы.
                    let count = info.page_count as usize;
                    // Куда смотрели обе ленты. `reset` обязателен — высоты
                    // страниц изменились, — но он же отматывает ленту в самое
                    // начало: повернул страницу и оказался на обложке.
                    // Ленты запоминаются порознь: панель миниатюр листают
                    // сами по себе, и возвращать её к странице документа
                    // значило бы отнимать у неё место, где человек искал.
                    let pages_at = doc.pages.logical_scroll_top().item_ix;
                    let thumbs_at = doc.thumbs.logical_scroll_top().item_ix;
                    doc.info = info;
                    // Лента считает рядами: в развороте их вдвое меньше, чем
                    // страниц, и сбрасывать её по числу страниц значило бы
                    // показать половину документа пустыми рядами.
                    let rows = if self.spread {
                        count.div_ceil(2)
                    } else {
                        count
                    };
                    let doc = self.doc.as_mut().expect("документ открыт");
                    doc.pages.reset(rows);
                    doc.thumbs.reset(count);
                    if count > 0 {
                        // Именно `scroll_to`, а не «показать элемент»: после
                        // сброса высоты ещё не измерены, и расчёт «сколько
                        // прокрутить» на нулях всегда выдаёт начало ленты.
                        // Смещение внутри страницы по той же причине не
                        // восстанавливается — только её номер.
                        let at = |item_ix: usize, last: usize| gpui::ListOffset {
                            item_ix: item_ix.min(last),
                            offset_in_item: px(0.0),
                        };
                        let row = at(pages_at, rows - 1);
                        doc.pages.scroll_to(row);
                        doc.thumbs.scroll_to(at(thumbs_at, count - 1));
                        // Иначе следующий же кадр решит, что читаемая
                        // страница сменилась, и потащит миниатюры за собой.
                        doc.current_page = (row.item_ix as u32 * if rows == count { 1 } else { 2 })
                            .min(count as u32 - 1);
                    }
                    doc.blocks.clear();
                    doc.blocks_requested.clear();
                    doc.requested.clear();
                    doc.dirty = true;
                    doc.status = Some("Состав страниц изменён".into());
                    let evicted = doc.tiles.clear();
                    if let Some((_, texture)) = doc.preview.take() {
                        let mut all = evicted;
                        all.push(texture);
                        all
                    } else {
                        evicted
                    }
                }
                RenderEvent::Saved { path } => {
                    tracing::info!(path = %path.display(), "документ сохранён");
                    doc.dirty = false;
                    doc.status = Some(format!("Сохранено: {}", path.display()));
                    Vec::new()
                }
                RenderEvent::Failed { page, message } => {
                    tracing::warn!(?page, "операция не удалась: {message}");
                    doc.status = Some(message.clone());
                    self.failure = Some(message);
                    Vec::new()
                }
            }
        };

        // Вытеснение из нашего LRU освобождает только буфер в оперативной
        // памяти. Копия текстуры живёт ещё и в атласе спрайтов gpui, и без
        // явного снятия расход памяти растёт до конца сеанса.
        for texture in evicted {
            cx.drop_image(texture, None);
        }

        cx.notify();
    }

    /// Пересчитывает список нужных тайлов по текущему положению вьюпорта.
    fn sync_requests(&mut self, scale_factor: f32) {
        let spread = self.spread;
        let Some(doc) = self.doc.as_mut() else { return };
        let count = doc.info.page_count;
        if count == 0 {
            return;
        }

        // В режиме сетки спрос диктует она сама: лента страниц и панель
        // миниатюр спрятаны, и просить их растры незачем.
        if let Some(organise) = self.organise.as_ref() {
            let offset = -f32::from(organise.scroll.offset().y);
            let viewport = f32::from(organise.scroll.bounds().size.height).max(1.0);
            let width = f32::from(organise.scroll.bounds().size.width).max(1.0);
            let ratio = doc
                .info
                .size(0)
                .map(|s| (s.width / s.height.max(1.0)).clamp(0.3, 3.0))
                .unwrap_or(0.7);
            // Ширина ячейки — карточка плюс щель слева, высота — карточка,
            // номер под ней и вертикальный зазор ряда.
            let cell = organise.card + 10.0;
            let row_height = organise.card / ratio + 42.0;
            let columns = (width / cell).floor().max(1.0);
            let column_count = columns as u32;
            let first_row = (offset / row_height).floor().max(0.0);
            let rows = (viewport / row_height).ceil() + 1.0;

            let first = (first_row * columns) as u32;
            // Ряд про запас сверху и снизу: прокрутка не должна упираться в
            // пустые карточки.
            let from = first.saturating_sub(columns as u32);
            let to = (first + (rows as u32 + 1) * columns as u32).min(count);

            let mut wanted = Vec::new();
            let card_height = organise.card / ratio;
            for page in from..to {
                if let Some(key) = doc.preview_key(page, card_height, scale_factor)
                    && !doc.tiles.contains(&key)
                {
                    wanted.push(key);
                }
            }
            if wanted != doc.requested {
                doc.requested = wanted.clone();
                doc.renderer.set_wanted_tiles(wanted);
            }
            if let Some(organise) = self.organise.as_mut()
                && organise.columns != column_count
            {
                organise.columns = column_count;
            }
            return;
        }

        // Верхний ряд ленты — это страница, а в развороте пара страниц.
        let row = doc.pages.logical_scroll_top().item_ix as u32;
        let first = (if spread { row * 2 } else { row }).min(count - 1);
        // Панель миниатюр следует за лентой: иначе на длинном документе она
        // остаётся там, где её оставили, и перестаёт показывать, где ты сейчас.
        if doc.current_page != first {
            doc.thumbs.scroll_to_reveal_item(first as usize);
        }
        doc.current_page = first;

        let mut wanted = Vec::new();
        let start = first.saturating_sub(PREFETCH_BEHIND);
        // В развороте на экране вдвое больше страниц — и запас нужен вдвое.
        let ahead = if spread {
            PREFETCH_AHEAD * 2 + 1
        } else {
            PREFETCH_AHEAD
        };
        for page in start..(first + ahead + 1).min(count) {
            let key = doc.page_key(page, scale_factor);
            // Устаревшая после правки страница просится заново, хотя её
            // старый растр ещё в кэше: он показывается до прихода свежего.
            if !doc.tiles.contains(&key) || doc.stale_pages.contains(&page) {
                wanted.push(key);
            }
        }

        let page0_thumb = doc.thumbnail_key(0, scale_factor);
        doc.thumb_key_page0 = page0_thumb;

        // Миниатюры идут после основных страниц: они дешёвые, но менее срочные.
        let thumb_first = (doc.thumbs.logical_scroll_top().item_ix as u32).min(count - 1);
        for page in thumb_first..(thumb_first + THUMB_PREFETCH).min(count) {
            if let Some(key) = doc.thumbnail_key(page, scale_factor)
                && (!doc.tiles.contains(&key) || doc.stale_pages.contains(&page))
            {
                wanted.push(key);
            }
        }

        if wanted != doc.requested {
            doc.requested = wanted.clone();
            doc.renderer.set_wanted_tiles(wanted);
        }

        // Блоки нужны всегда, а не только при включённой подсветке: по ним
        // работает выделение абзаца кликом. Разбор страницы стоит единицы
        // миллисекунд и делается один раз на страницу. Просятся все страницы
        // видимой полосы, а не только верхняя: на экране почти всегда стык
        // двух страниц, и клик по нижней упирался в «блоков ещё нет» — ни
        // выделения, ни рамки, вообще ничего.
        for page in first..(first + PREFETCH_AHEAD + 1).min(count) {
            if doc.blocks_requested.insert(page) {
                doc.renderer.request_blocks(page);
            }
        }
    }

    /// Выделяет абзац для правки: текст ложится в поле прямо поверх страницы,
    /// над блоком встаёт панель формата.
    pub(crate) fn select_block(
        &mut self,
        page: u32,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(block) = doc.blocks.get(&page).and_then(|blocks| blocks.get(index)) else {
            return;
        };

        // Пока рамку тянут, выделение не меняется ни при каких условиях: под
        // ней проезжают чужие блоки, и перескочить на них посреди переноса
        // значило бы бросить начатое на полпути.
        if self.drag.is_some() {
            return;
        }
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;

        // Тот же абзац уже правится. Выделять его заново нельзя: рамка блока
        // лежит под текстовым элементом и получает нажатие вместе с ним, так
        // что каждый щелчок по правимому тексту — ради переноса каретки —
        // сбрасывал бы всё набранное.
        if self
            .selected
            .as_ref()
            .is_some_and(|current| current.page == page && current.bbox == block.bbox)
        {
            return;
        }

        let style = block.dominant_style();
        let selection = Selection {
            page,
            bbox: block.bbox,
            frame: block.bbox,
            style: style.clone(),
            original_text: block.text(),
            lines: block.lines.len(),
            sample_lines: block
                .lines
                .iter()
                .map(|line| (line.text(), line.bbox.width(), line.dominant_size()))
                .collect(),
            align: block.align,
            needs_retypeset: false,
            line_height: block.leading(),
            page_font: None,
            owner: block.mark,
            touched: false,
            char_spacing_override: None,
            h_scale_override: None,
            seeded_char_spacing: 0.0,
            seeded_h_scale: 100.0,
            para_spacing_override: None,
            fill: None,
            style_id: block.style,
            seeded_style_id: block.style,
            creating: false,
            // Угол приходит из метки блока: повёрнутый ранее абзац открывается
            // с тем же углом, а не с нулём, — иначе первое же «применить»
            // распрямляло бы его.
            rotation: block.rotation,
        };
        // Модель собирается из ранов блока со стилями по кускам: жирное
        // начало остаётся жирным, курсив курсивом — перенабор воспроизводит
        // их своими же шрифтами страницы. Раньше сюда шёл плоский текст, и
        // пёстрый абзац нельзя было ни править, ни растягивать.
        let model = model_from_block(block, &style);
        let size = style.size;
        let family = style.clean_family().to_owned();

        // Оформление читаем из самого документа: имя шрифта из PDF, его
        // встроенную программу, кегль и цвет. Ответ придёт событием.
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer
                .request_block_font(page + 1, block.bbox, block.mark);
        }

        let zoom = doc.zoom;
        let wrap_width = px(block.bbox.width() * zoom);
        tracing::info!(
            family = %family, size, zoom,
            bbox_w = block.bbox.width(), bbox_h = block.bbox.height(),
            "выделен блок: оформление от pdfium"
        );
        let line_height = block.leading().unwrap_or(size * 1.2);
        let (widgets, preselected, hint) = build_widgets(
            model,
            size,
            &family,
            line_height,
            wrap_width,
            block.align,
            zoom,
            window,
            cx,
        );
        // Выбор пикера начинается с гарнитуры блока; точного начертания нет,
        // пока его не выбрали руками.
        self.chosen_family = preselected;
        self.chosen_face = None;

        // Клик по абзацу должен сразу давать печатать. Фокус ставится не
        // здесь, а на следующем кадре: элемент правки в этот миг ещё не
        // существует — он и создаётся этим самым нажатием, — а нажатие мимо
        // всего, что умеет держать фокус, к концу обработки его снимает.
        // Поэтому первое набранное слово раньше пропадало.
        self.focus_editor = true;

        // Подтверждение и отказ приходят от самого элемента.
        cx.subscribe_in(
            &widgets.editor,
            window,
            |this, _, event, _window, cx| match event {
                EditorEvent::Cancel => this.clear_selection(cx),
                // Каждое нажатие перерисовывает саму страницу — так набранное
                // сразу видно в тексте документа, а не в накладке над ним.
                EditorEvent::Changed => this.request_preview(cx),
            },
        )
        .detach();

        // Поля геометрии и метрик: применяются по Enter и уходу фокуса.
        let geometry_subscriptions: [(
            &Entity<InputState>,
            fn(&mut Viewer, f32, &mut Context<Viewer>),
        ); 8] = [
            (&widgets.geometry.x, |this, v, cx| this.set_frame_x(v, cx)),
            (&widgets.geometry.y, |this, v, cx| {
                this.set_frame_y_top(v, cx)
            }),
            (&widgets.geometry.w, |this, v, cx| this.set_frame_w(v, cx)),
            (&widgets.geometry.h, |this, v, cx| this.set_frame_h(v, cx)),
            (&widgets.geometry.angle, |this, v, cx| {
                this.set_rotation(v, cx)
            }),
            (&widgets.geometry.char_spacing, |this, v, cx| {
                this.set_char_spacing(v, cx)
            }),
            (&widgets.geometry.h_scale, |this, v, cx| {
                this.set_h_scale(v, cx)
            }),
            (&widgets.geometry.para_spacing, |this, v, cx| {
                this.set_para_spacing(v, cx)
            }),
        ];
        for (input, apply) in geometry_subscriptions {
            let handle = input.clone();
            cx.subscribe(input, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur)
                    && let Ok(value) = handle
                        .read(cx)
                        .value()
                        .trim()
                        .replace(',', ".")
                        .replace('°', "")
                        .parse::<f32>()
                {
                    apply(this, value, cx);
                }
            })
            .detach();
        }
        cx.subscribe(
            &widgets.color,
            |this, _, event: &gpui_component::color_picker::ColorPickerEvent, cx| {
                let gpui_component::color_picker::ColorPickerEvent::Change(Some(color)) = event
                else {
                    return;
                };
                let rgba = gpui::Rgba::from(*color);
                this.set_color([rgba.r, rgba.g, rgba.b], cx);
            },
        )
        .detach();

        cx.subscribe(&widgets.size, |this, _, event: &InputEvent, cx| {
            // По Enter и уходу фокуса, а не по каждой цифре: пока набирают
            // «23», промежуточная «2» не должна перенабирать блок кеглем два.
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.request_preview(cx);
            }
        })
        .detach();
        self.widgets = Some(widgets);
        self.selected = Some(selection);
        if let Some(hint) = hint {
            self.set_status(hint, cx);
        }
        cx.notify();
    }

    /// Принимает оформление абзаца, прочитанное из самого документа.
    ///
    /// Главное здесь — метрики шрифта. Рисовать текст ими не нужно: его рисует
    /// pdfium на настоящей странице. Нужны они, чтобы каретка и подсветка
    /// выделения вставали ровно между настоящими буквами, а перенос строк на
    /// экране случался там же, где случится в документе.
    fn adopt_page_font(
        &mut self,
        font: pdfcore::stream_edit::BlockFont,
        metrics: Vec<(String, pdfcore::stream_edit::Encoder)>,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(
            base_font = %font.base_font,
            family = ?font.family,
            size = font.size,
            color = ?font.color,
            embedded = font.program.is_some(),
            metrics = font.metrics.is_some(),
            "оформление блока из потока содержимого"
        );

        // Шрифт, которого нет ни в документе, ни в системе, нечем встроить —
        // значит сменить им начертание не выйдет, и сказать об этом надо
        // сразу, а не после нажатия «Применить».
        if font.program.is_none() && !pdfcore::system_fonts().has_family(&font.base_font) {
            self.set_status(
                format!(
                    "Шрифт «{}» не встроен в документ и не установлен в системе — \
                     поставьте его и нажмите ⟳ в панели формата",
                    font.base_font
                ),
                cx,
            );
        }

        if let Some(widgets) = self.widgets.as_ref() {
            let color = font
                .color
                .map(|[r, g, b]| gpui::Rgba { r, g, b, a: 1.0 }.into());
            let (size, mut primary) = (font.size, font.metrics.clone());

            // Средняя ошибка энкодера на строках блока, как они стоят на
            // странице. Эталон — настоящие ширины строк от pdfium.
            let sample = self
                .selected
                .as_ref()
                .map(|selection| selection.sample_lines.clone())
                .unwrap_or_default();
            let error_of = |encoder: &pdfcore::stream_edit::Encoder| -> f32 {
                let mut total = 0.0;
                let mut real_total = 0.0;
                for (text, real, line_size) in &sample {
                    if *real <= 1.0 {
                        continue;
                    }
                    total += (encoder.width(text, *line_size) - real).abs();
                    real_total += real;
                }
                if real_total <= 0.0 {
                    0.0
                } else {
                    total / real_total
                }
            };

            // Метрики раздаются по ключу гарнитуры: кусок ищет свой шрифт тем
            // же ключом при обмере (см. `metrics_for_run`). У ключа может
            // оказаться несколько претендентов — в ресурсах страницы часто
            // живут тёзки вроде «PT Serif Bold Italic» и «PTSerif-BoldItalic»,
            // и у одного из них ширины кириллицы взяты с потолка. Побеждает
            // тот, кто точнее меряет настоящие строки блока: раньше побеждал
            // последний по порядку, и поле правки переносило слова не так,
            // как страница.
            let mut by_family: std::collections::HashMap<String, pdfcore::stream_edit::Encoder> =
                std::collections::HashMap::new();
            for (name, encoder) in metrics {
                let key = pdfcore::fonts::family_key(&name);
                match by_family.entry(key) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(encoder);
                    }
                    std::collections::hash_map::Entry::Occupied(mut slot) => {
                        if error_of(&encoder) < error_of(slot.get()) {
                            slot.insert(encoder);
                        }
                    }
                }
            }
            // Запасной энкодер блока сверяется с тем же эталоном: ему меряют
            // куски без своей гарнитуры, и врать ему так же нельзя.
            if let Some(current) = primary.as_ref() {
                let best = by_family
                    .values()
                    .min_by(|a, b| error_of(a).total_cmp(&error_of(b)));
                if let Some(best) = best
                    && !sample.is_empty()
                    && error_of(best) + 0.005 < error_of(current)
                {
                    primary = Some(best.clone());
                }
            }
            let (char_spacing, h_scale) = (font.char_spacing, font.h_scale);
            widgets.editor.update(cx, |editor, cx| {
                if let Some(color) = color {
                    editor.base.color = color;
                }
                editor.base.size_points = size;
                editor.base.metrics = primary;
                editor.base.metrics_by_family = by_family;
                // Раскладка поля меряет с той же разрядкой и тем же
                // масштабом, что и страница.
                editor.base.char_spacing = char_spacing;
                editor.base.h_scale = h_scale;
                cx.notify();
            });
        }

        if let Some(selection) = self.selected.as_mut() {
            selection.seeded_char_spacing = font.char_spacing;
            selection.seeded_h_scale = font.h_scale;
            selection.page_font = Some(PageFont {
                base_font: font.base_font,
                size: font.size,
                color: font
                    .color
                    .map(|[r, g, b]| gpui::Rgba { r, g, b, a: 1.0 }.into()),
                embedded: font.program.is_some(),
            });
        }
        cx.notify();
    }

    /// Перечитывает системные шрифты и пересобирает список гарнитур.
    pub(crate) fn refresh_fonts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changed = pdfcore::fonts::refresh();
        let count = pdfcore::system_fonts().families().len();

        // Список в выпадашке фиксируется при её создании, поэтому после
        // пересканирования поля надо собрать заново.
        if let (Some(selection), Some(widgets)) = (self.selected.as_ref(), self.widgets.as_ref()) {
            let model = widgets.editor.read(cx).text.clone();
            let size = selection
                .page_font
                .as_ref()
                .map(|f| f.size)
                .unwrap_or(selection.style.size);
            let family = selection
                .page_font
                .as_ref()
                .map(|f| f.base_font.clone())
                .unwrap_or_else(|| selection.style.clean_family().to_owned());

            let zoom = self.doc.as_ref().map(|doc| doc.zoom).unwrap_or(1.0);
            let wrap_width = px(selection.bbox.width() * zoom);
            let line_height = selection.line_height.unwrap_or(size * 1.2);
            let (rebuilt, preselected, _) = build_widgets(
                model,
                size,
                &family,
                line_height,
                wrap_width,
                selection.align,
                zoom,
                window,
                cx,
            );
            self.chosen_family = preselected;
            self.chosen_face = None;
            window.focus(&rebuilt.editor.read(cx).focus_handle(cx));
            self.widgets = Some(rebuilt);
        }

        self.set_status(
            if changed {
                format!("Список шрифтов обновлён: {count} семейств")
            } else {
                format!("Новых шрифтов не найдено, семейств по-прежнему {count}")
            },
            cx,
        );
    }

    /// Нажатие по пустому месту страницы.
    pub(crate) fn press_on_page(&mut self, page: u32, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.press_anywhere(page, None, at, cx);
    }

    /// Единая точка входа для нажатий по странице и блокам.
    ///
    /// Открытая правка сперва завершается, как и раньше. Остальное ждёт
    /// отпускания: без движения нажатие станет щелчком (выделить блок либо
    /// ничего), с движением — рамкой выделения, начатой хоть с пустого места,
    /// хоть прямо с текста.
    pub(crate) fn press_anywhere(
        &mut self,
        page: u32,
        block: Option<usize>,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        match self.selected.as_ref() {
            // Набранное нельзя терять: нажатие целиком уходит на применение
            // правки, а выбор нового блока — уже следующим щелчком. После
            // перенабора состав блоков другой, и нажимать «вслепую» на старый
            // номер значило бы попасть неизвестно куда.
            Some(selection) if selection.touched => {
                self.apply_edit(cx);
                return;
            }
            // Ничего не набрали — переход мгновенный, в один щелчок.
            Some(_) => self.clear_selection(cx),
            None => {}
        }
        if !self.multi.is_empty() {
            // Нажатие на блок самой группы — начало её перетаскивания, а не
            // сброс выделения: группу двигают, ухватив за любой её блок.
            let on_member = block.is_some_and(|index| {
                self.doc
                    .as_ref()
                    .and_then(|doc| doc.blocks.get(&page))
                    .and_then(|blocks| blocks.get(index))
                    .is_some_and(|clicked| {
                        self.multi
                            .iter()
                            .any(|target| target.page == page && target.bbox == clicked.bbox)
                    })
            });
            if on_member {
                self.group_drag = Some(GroupDrag {
                    page,
                    start: at,
                    current: at,
                });
                return;
            }
            self.multi.clear();
            self.multi_size = None;
            self.multi_family = None;
            self.multi_color = None;
            cx.notify();
        }
        self.pending_press = Some(PendingPress { page, block, at });
    }

    /// Завершение резиновой рамки: отбор блоков по режиму.
    /// Угол страницы в окне — тот, внутри которого лежит указанная точка.
    ///
    /// Кандидатов может быть несколько (см. `page_origins`), и различить их
    /// иначе нельзя: у примерочной раскладки те же размеры, что у настоящей.
    /// Зато точка нажатия заведомо попала в настоящую страницу — по ней и
    /// выбирается. Если не подошёл никто, берётся первый: лучше приблизительно,
    /// чем никак.
    pub(crate) fn origin_for(&self, page: u32, at: Point<Pixels>) -> Option<Point<Pixels>> {
        let doc = self.doc.as_ref()?;
        let size = doc.info.size(page)?;
        let zoom = doc.zoom.max(0.01);
        let places = self.page_origins.get(&page)?;
        let width = px(size.width * zoom);
        let height = px(size.height * zoom);
        places
            .iter()
            .copied()
            .find(|origin| {
                at.x >= origin.x
                    && at.x <= origin.x + width
                    && at.y >= origin.y
                    && at.y <= origin.y + height
            })
            .or_else(|| places.first().copied())
    }

    /// Рамка в координатах страницы вместе с режимом отбора.
    ///
    /// Одна и та же арифметика нужна и на отпускании, и на каждом кадре
    /// протяжки — подсветить то, что рамка возьмёт, если её отпустить сейчас.
    pub(crate) fn rubber_band(&self, rubber: &Rubber) -> Option<(Rect, crate::frame::BandMode)> {
        let origin = self.origin_for(rubber.page, rubber.start)?;
        let doc = self.doc.as_ref()?;
        let size = doc.info.size(rubber.page)?;
        let zoom = doc.zoom.max(0.01);

        // Оконные координаты → страничные: у PDF ось игрек смотрит вверх.
        let to_page = |point: Point<Pixels>| {
            (
                f32::from(point.x - origin.x) / zoom,
                size.height - f32::from(point.y - origin.y) / zoom,
            )
        };
        let (x1, y1) = to_page(rubber.start);
        let (x2, y2) = to_page(rubber.current);
        Some((
            Rect::new(x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)),
            crate::frame::BandMode::from_drag(x1, x2),
        ))
    }

    /// Блоки, которые рамка возьмёт прямо сейчас.
    pub(crate) fn rubber_candidates(&self, page: u32) -> Vec<usize> {
        let Some(rubber) = self.rubber.as_ref().filter(|r| r.page == page) else {
            return Vec::new();
        };
        if !rubber.meaningful() || self.tool == Tool::Text {
            return Vec::new();
        }
        let Some((band, mode)) = self.rubber_band(rubber) else {
            return Vec::new();
        };
        let Some(doc) = self.doc.as_ref() else {
            return Vec::new();
        };
        let Some(blocks) = doc.blocks.get(&page) else {
            return Vec::new();
        };
        let boxes: Vec<Rect> = blocks.iter().map(|block| block.bbox).collect();
        crate::frame::blocks_in_band(band, mode, &boxes)
    }

    pub(crate) fn finish_rubber(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rubber) = self.rubber.take() else {
            return;
        };
        // Что бы ни случилось дальше, рамки на экране больше нет — вид обязан
        // перерисоваться. Ранние выходы ниже без этого оставляли «призрак»:
        // последняя покраска с рамкой так и висела на странице.
        cx.notify();
        if !rubber.meaningful() {
            return;
        }
        let page = rubber.page;
        let Some((band, mode)) = self.rubber_band(&rubber) else {
            return;
        };
        let Some(doc) = self.doc.as_ref() else {
            return;
        };

        if self.tool == Tool::Text {
            self.start_new_block(page, band, window, cx);
            return;
        }

        let Some(blocks) = doc.blocks.get(&page) else {
            return;
        };
        let boxes: Vec<Rect> = blocks.iter().map(|block| block.bbox).collect();
        let picked = crate::frame::blocks_in_band(band, mode, &boxes);
        tracing::debug!(
            page,
            ?mode,
            blocks = boxes.len(),
            picked = picked.len(),
            "рамка выделения завершена"
        );

        match picked.as_slice() {
            // Рамка пуста. Если её начали с текста — это был щелчок, которому
            // помешала дрогнувшая рука: выделяется блок под точкой нажатия.
            [] => {
                if let Some(index) = rubber.block {
                    self.select_block(page, index, window, cx);
                }
            }
            // Один блок — обычная правка со всеми возможностями.
            [only] => {
                let index = *only;
                self.select_block(page, index, window, cx);
            }
            _ => {
                self.multi = picked
                    .into_iter()
                    .filter_map(|index| blocks.get(index))
                    .map(|block| MultiTarget {
                        page,
                        bbox: block.bbox,
                        owner: block.mark,
                        size: block.dominant_style().size,
                        line_height: block.leading(),
                        align: block.align,
                    })
                    .collect();
                self.rebuild_multi_size(window, cx);
            }
        }
        cx.notify();
    }

    /// Открывает правку нового блока, растянутого инструментом «Текст».
    ///
    /// Блока на странице ещё нет: рамка пустая, модель пустая. Реальным он
    /// станет при первом применении — перенабор с признаком создания уложит
    /// набранное в эту рамку и поставит блоку метку.
    fn start_new_block(
        &mut self,
        page: u32,
        rect: Rect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Слишком маленькая рамка — это дрогнувшая рука, а не намерение.
        if rect.width() < 20.0 || rect.height() < 8.0 {
            return;
        }
        let mut style = Style::default();
        style.family = "Arial".to_owned();
        style.size = 12.0;

        self.selected = Some(Selection {
            page,
            bbox: rect,
            frame: rect,
            style: style.clone(),
            original_text: String::new(),
            lines: 1,
            seeded_style_id: None,
            sample_lines: Vec::new(),
            align: Align::Left,
            line_height: None,
            page_font: None,
            owner: None,
            touched: false,
            char_spacing_override: None,
            h_scale_override: None,
            seeded_char_spacing: 0.0,
            seeded_h_scale: 100.0,
            para_spacing_override: None,
            fill: None,
            style_id: None,
            creating: true,
            needs_retypeset: false,
            rotation: 0.0,
        });

        let zoom = self.doc.as_ref().map(|doc| doc.zoom).unwrap_or(1.0);
        let (widgets, preselected, hint) = build_widgets(
            RichText::new(String::new(), RunStyle::default()),
            12.0,
            "Arial",
            14.4,
            px(rect.width() * zoom),
            pdfcore::model::Align::Left,
            zoom,
            window,
            cx,
        );
        self.chosen_family = preselected;
        self.chosen_face = None;
        cx.subscribe_in(
            &widgets.editor,
            window,
            |this, _, event, _window, cx| match event {
                EditorEvent::Cancel => this.clear_selection(cx),
                EditorEvent::Changed => this.request_preview(cx),
            },
        )
        .detach();
        // Поля геометрии и метрик: применяются по Enter и уходу фокуса.
        let geometry_subscriptions: [(
            &Entity<InputState>,
            fn(&mut Viewer, f32, &mut Context<Viewer>),
        ); 8] = [
            (&widgets.geometry.x, |this, v, cx| this.set_frame_x(v, cx)),
            (&widgets.geometry.y, |this, v, cx| {
                this.set_frame_y_top(v, cx)
            }),
            (&widgets.geometry.w, |this, v, cx| this.set_frame_w(v, cx)),
            (&widgets.geometry.h, |this, v, cx| this.set_frame_h(v, cx)),
            (&widgets.geometry.angle, |this, v, cx| {
                this.set_rotation(v, cx)
            }),
            (&widgets.geometry.char_spacing, |this, v, cx| {
                this.set_char_spacing(v, cx)
            }),
            (&widgets.geometry.h_scale, |this, v, cx| {
                this.set_h_scale(v, cx)
            }),
            (&widgets.geometry.para_spacing, |this, v, cx| {
                this.set_para_spacing(v, cx)
            }),
        ];
        for (input, apply) in geometry_subscriptions {
            let handle = input.clone();
            cx.subscribe(input, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur)
                    && let Ok(value) = handle
                        .read(cx)
                        .value()
                        .trim()
                        .replace(',', ".")
                        .replace('°', "")
                        .parse::<f32>()
                {
                    apply(this, value, cx);
                }
            })
            .detach();
        }
        cx.subscribe(
            &widgets.color,
            |this, _, event: &gpui_component::color_picker::ColorPickerEvent, cx| {
                let gpui_component::color_picker::ColorPickerEvent::Change(Some(color)) = event
                else {
                    return;
                };
                let rgba = gpui::Rgba::from(*color);
                this.set_color([rgba.r, rgba.g, rgba.b], cx);
            },
        )
        .detach();

        cx.subscribe(&widgets.size, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.request_preview(cx);
            }
        })
        .detach();
        self.widgets = Some(widgets);
        self.focus_editor = true;
        // Набирать удобнее курсором: после создания инструмент отпускается.
        self.tool = Tool::Cursor;
        if let Some(hint) = hint {
            self.set_status(hint, cx);
        }
        cx.notify();
    }

    /// Удаляет выделенный блок со страницы.
    pub(crate) fn erase_selected(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selected.as_ref() else {
            return;
        };
        if selection.creating {
            // Ещё не созданный блок «удаляется» простым отказом.
            self.clear_selection(cx);
            return;
        }
        let erase = BlockEdit::Erase(pdfcore::BlockErase {
            page_number: selection.page + 1,
            bbox: selection.bbox,
            owner: selection.owner,
        });
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.apply_edit(erase);
        }
        self.clear_selection(cx);
    }

    /// Операция над страницей из панели миниатюр.
    pub(crate) fn page_op(&mut self, op: pdfcore::PageOp, cx: &mut Context<Self>) {
        self.clear_selection(cx);
        self.multi.clear();
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.apply_page_op(op);
        }
    }

    /// Общий кегль группы — или пустое поле, когда кегли разные, как в
    /// Acrobat: врать среднее было бы хуже честной пустоты.
    fn rebuild_multi_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut sizes = self.multi.iter().map(|target| seeded_size(target.size));
        let first = sizes.next();
        let shared = match first {
            Some(value) if sizes.all(|other| other == value) => Some(value),
            _ => None,
        };
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            if let Some(value) = shared {
                state.set_value(format!("{value:.1}"), window, cx);
            }
            state
        });
        cx.subscribe(&input, |this, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let parsed = state
                    .read(cx)
                    .value()
                    .trim()
                    .replace(',', ".")
                    .parse::<f32>()
                    .ok()
                    .filter(|value| (1.0..=400.0).contains(value));
                if let Some(size) = parsed {
                    this.multi_set_size(size, cx);
                }
            }
        })
        .detach();
        self.multi_size = Some(input);

        // Поля типографики группы: пустые — общего значения у блоков нет.
        let char_spacing = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe(&char_spacing, |this, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur)
                && let Ok(value) = state
                    .read(cx)
                    .value()
                    .trim()
                    .replace(',', ".")
                    .parse::<f32>()
            {
                this.multi_set_char_spacing(value, cx);
            }
        })
        .detach();
        self.multi_char_spacing = Some(char_spacing);

        let h_scale = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe(&h_scale, |this, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur)
                && let Ok(value) = state
                    .read(cx)
                    .value()
                    .trim()
                    .replace(',', ".")
                    .parse::<f32>()
            {
                this.multi_set_h_scale(value, cx);
            }
        })
        .detach();
        self.multi_h_scale = Some(h_scale);

        let para_spacing = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe(&para_spacing, |this, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur)
                && let Ok(value) = state
                    .read(cx)
                    .value()
                    .trim()
                    .replace(',', ".")
                    .parse::<f32>()
            {
                this.multi_set_para_spacing(value, cx);
            }
        })
        .detach();
        self.multi_para_spacing = Some(para_spacing);

        // Гарнитура группы выбирается пикером шрифтов; пока не выбирали,
        // значения нет — как в Acrobat.
        self.multi_family = None;

        let color = cx.new(|cx| gpui_component::color_picker::ColorPickerState::new(window, cx));
        cx.subscribe(
            &color,
            |this, _, event: &gpui_component::color_picker::ColorPickerEvent, cx| {
                let gpui_component::color_picker::ColorPickerEvent::Change(Some(value)) = event
                else {
                    return;
                };
                let rgba = gpui::Rgba::from(*value);
                this.multi_set_color([rgba.r, rgba.g, rgba.b], cx);
            },
        )
        .detach();
        self.multi_color = Some(color);
    }

    /// Ctrl+клик: добавить блок в группу либо убрать из неё.
    pub(crate) fn toggle_multi(
        &mut self,
        page: u32,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Открытая одиночная правка сперва завершается.
        if self.selected.is_some() {
            self.commit_or_clear(cx);
        }
        let Some(block) = self
            .doc
            .as_ref()
            .and_then(|doc| doc.blocks.get(&page))
            .and_then(|blocks| blocks.get(index))
        else {
            return;
        };
        let bbox = block.bbox;
        if let Some(at) = self
            .multi
            .iter()
            .position(|target| target.page == page && target.bbox == bbox)
        {
            self.multi.remove(at);
        } else {
            self.multi.push(MultiTarget {
                page,
                bbox,
                owner: block.mark,
                size: block.dominant_style().size,
                line_height: block.leading(),
                align: block.align,
            });
        }
        self.rebuild_multi_size(window, cx);
        cx.notify();
    }

    /// Резиновая рамка выделения — как в Компас-3D: слева направо синее
    /// «окно» берёт целиком накрытое, справа налево зелёная пунктирная
    /// «секущая» берёт всё задетое.
    fn render_rubber(&self, page: u32) -> Option<AnyElement> {
        let rubber = self.rubber.as_ref().filter(|rubber| rubber.page == page)?;
        if !rubber.meaningful() {
            return None;
        }
        let origin = self.origin_for(page, rubber.start)?;
        let (x1, y1) = (rubber.start.x - origin.x, rubber.start.y - origin.y);
        let (x2, y2) = (rubber.current.x - origin.x, rubber.current.y - origin.y);
        let window_mode = rubber.current.x >= rubber.start.x;

        let base = div()
            .absolute()
            .left(x1.min(x2))
            .top(y1.min(y2))
            .w((x2 - x1).abs())
            .h((y2 - y1).abs())
            .border_1();
        let styled = if window_mode {
            base.border_color(rgba(0x2563_EBCC)).bg(rgba(0x2563_EB22))
        } else {
            base.border_dashed()
                .border_color(rgba(0x16A3_4ACC))
                .bg(rgba(0x16A3_4A22))
        };
        Some(styled.into_any_element())
    }

    /// Подсветка блоков, которые возьмёт рамка, пока её ещё тянут.
    ///
    /// Цвет — по режиму отбора: голубой у «окна» слева направо, зелёный у
    /// «секущей» справа налево. Так видно заранее, что попадёт в группу, и не
    /// приходится отпускать кнопку наугад.
    fn render_band_candidates(&self, page: u32, page_height: f32, zoom: f32) -> Vec<AnyElement> {
        let candidates = self.rubber_candidates(page);
        if candidates.is_empty() {
            return Vec::new();
        }
        let crossing = self
            .rubber
            .as_ref()
            .and_then(|rubber| self.rubber_band(rubber))
            .map(|(_, mode)| mode == crate::frame::BandMode::Crossing)
            .unwrap_or(false);
        let (border, fill) = if crossing {
            (rgba(0x16A3_4ACC), rgba(0x16A3_4A33))
        } else {
            (rgba(0x2563_EBCC), rgba(0x2563_EB33))
        };

        let Some(doc) = self.doc.as_ref() else {
            return Vec::new();
        };
        let Some(blocks) = doc.blocks.get(&page) else {
            return Vec::new();
        };
        candidates
            .into_iter()
            .filter_map(|index| blocks.get(index))
            .map(|block| {
                div()
                    .absolute()
                    .left(px(block.bbox.left * zoom))
                    .top(px((page_height - block.bbox.top) * zoom))
                    .w(px(block.bbox.width() * zoom))
                    .h(px(block.bbox.height() * zoom))
                    .border_2()
                    .border_color(border)
                    .bg(fill)
                    .into_any_element()
            })
            .collect()
    }

    /// Подсветка блоков, попавших в групповое выделение. Во время
    /// перетаскивания группы метки едут за мышью — это и есть весь показ
    /// переноса до отпускания.
    fn render_multi_marks(&self, page: u32, page_height: f32, zoom: f32) -> Vec<AnyElement> {
        let (offset_x, offset_y) = self.group_drag_offset(page).unwrap_or((0.0, 0.0));
        self.multi
            .iter()
            .filter(|target| target.page == page)
            .map(|target| {
                div()
                    .absolute()
                    .left(px(target.bbox.left * zoom + offset_x))
                    .top(px((page_height - target.bbox.top) * zoom + offset_y))
                    .w(px(target.bbox.width() * zoom))
                    .h(px(target.bbox.height() * zoom))
                    .border_2()
                    .border_color(rgba(0x2563_EBCC))
                    .bg(rgba(0x2563_EB22))
                    .into_any_element()
            })
            .collect()
    }

    /// Пачка перенаборов по всем блокам группы с общей правкой стиля.
    ///
    /// Каждый блок собирается в модель со своими стилями по кускам, правка
    /// накладывается поверх — и блок перенабирается на своём месте. Одна
    /// пачка — один шаг истории: «сделал все крупнее» отменяется одним Ctrl+Z.
    fn multi_rewrites(&self, change: impl Fn(&mut crate::rich_text::RunStyle)) -> Vec<BlockEdit> {
        let Some(doc) = self.doc.as_ref() else {
            return Vec::new();
        };
        let mut edits = Vec::new();
        for target in &self.multi {
            let Some(block) = doc
                .blocks
                .get(&target.page)
                .and_then(|blocks| blocks.iter().find(|block| block.bbox == target.bbox))
            else {
                continue;
            };
            let style = block.dominant_style();
            let mut model = model_from_block(block, &style);
            model.restyle(&change);
            edits.push(BlockEdit::Rewrite(BlockRewrite {
                char_spacing: None,
                h_scale: None,
                para_spacing: None,
                fill: None,
                style_id: None,
                page_number: target.page + 1,
                bbox: target.bbox,
                target: Some(target.bbox),
                spans: model.to_spans(style.clean_family()),
                align: target.align,
                line_height: target.line_height,
                rotation: block.rotation,
                create: false,
                owner: target.owner,
            }));
        }
        edits
    }

    /// Правка типографики группы: разрядка, масштаб, отбивка, интерлиньяж.
    ///
    /// Текст не трогается — каждый блок перенабирается как есть, но с новыми
    /// полями перенабора. Именно так одно значение ложится на разные блоки.
    fn multi_set_metrics(&mut self, apply: impl Fn(&mut BlockRewrite), cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let mut edits = Vec::new();
        for target in &self.multi {
            let Some(block) = doc
                .blocks
                .get(&target.page)
                .and_then(|blocks| blocks.iter().find(|block| block.bbox == target.bbox))
            else {
                continue;
            };
            let style = block.dominant_style();
            let model = model_from_block(block, &style);
            let mut edit = BlockRewrite {
                char_spacing: None,
                h_scale: None,
                para_spacing: None,
                fill: None,
                style_id: None,
                page_number: target.page + 1,
                bbox: target.bbox,
                target: Some(target.bbox),
                spans: model.to_spans(style.clean_family()),
                align: target.align,
                line_height: target.line_height,
                rotation: block.rotation,
                create: false,
                owner: target.owner,
            };
            apply(&mut edit);
            edits.push(BlockEdit::Rewrite(edit));
        }
        if edits.is_empty() {
            return;
        }
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.apply_edits(edits);
        }
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;
        cx.notify();
    }

    pub(crate) fn multi_set_char_spacing(&mut self, value: f32, cx: &mut Context<Self>) {
        self.multi_set_metrics(|edit| edit.char_spacing = Some(value), cx);
    }

    pub(crate) fn multi_set_h_scale(&mut self, value: f32, cx: &mut Context<Self>) {
        if value > 1.0 {
            self.multi_set_metrics(|edit| edit.h_scale = Some(value), cx);
        }
    }

    pub(crate) fn multi_set_para_spacing(&mut self, value: f32, cx: &mut Context<Self>) {
        self.multi_set_metrics(|edit| edit.para_spacing = Some(value), cx);
    }

    /// Интерлиньяж группы задаётся множителем кегля: у блоков разные кегли,
    /// и одно значение в пунктах имело бы смысл лишь для одного из них.
    pub(crate) fn multi_set_leading_factor(&mut self, factor: f32, cx: &mut Context<Self>) {
        let sizes: std::collections::HashMap<(u32, String), f32> = self
            .multi
            .iter()
            .map(|target| {
                (
                    (target.page, format!("{:?}", target.bbox)),
                    target.size.max(1.0),
                )
            })
            .collect();
        self.multi_set_metrics(
            move |edit| {
                let key = (edit.page_number - 1, format!("{:?}", edit.bbox));
                if let Some(size) = sizes.get(&key) {
                    edit.line_height = Some(size * factor);
                }
            },
            cx,
        );
    }

    /// Применяет правку стиля ко всем блокам группы.
    pub(crate) fn multi_restyle(
        &mut self,
        change: impl Fn(&mut crate::rich_text::RunStyle),
        cx: &mut Context<Self>,
    ) {
        let edits = self.multi_rewrites(change);
        if edits.is_empty() {
            return;
        }
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.apply_edits(edits);
        }
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;
        cx.notify();
    }

    /// Общий цвет для всех блоков группы.
    ///
    /// Идёт перестановкой, а не перенабором: цвет — единственное, что меняется,
    /// и трогать раскладку незачем. Ни ширины, ни положение строк, ни
    /// межбуквенные расстояния не могут поехать — буквы вообще не трогаются.
    pub(crate) fn multi_set_color(&mut self, color: [f32; 3], cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let mut edits = Vec::new();
        for target in &self.multi {
            let rotation = doc
                .blocks
                .get(&target.page)
                .and_then(|blocks| blocks.iter().find(|block| block.bbox == target.bbox))
                .map(|block| block.rotation)
                .unwrap_or(0.0);
            edits.push(BlockEdit::Transform(pdfcore::BlockTransform {
                page_number: target.page + 1,
                bbox: target.bbox,
                target: target.bbox,
                rotation,
                color: Some(color),
                owner: target.owner,
            }));
        }
        if edits.is_empty() {
            return;
        }
        doc.renderer.apply_edits(edits);
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;
        cx.notify();
    }

    /// Общий кегль для всех блоков группы.
    pub(crate) fn multi_set_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.multi_restyle(move |style| style.size = Some(size), cx);
    }

    /// Общая выключка для всех блоков группы.
    pub(crate) fn multi_set_align(&mut self, align: Align, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let mut edits = Vec::new();
        for target in &self.multi {
            let Some(block) = doc
                .blocks
                .get(&target.page)
                .and_then(|blocks| blocks.iter().find(|block| block.bbox == target.bbox))
            else {
                continue;
            };
            let style = block.dominant_style();
            let model = model_from_block(block, &style);
            edits.push(BlockEdit::Rewrite(BlockRewrite {
                char_spacing: None,
                h_scale: None,
                para_spacing: None,
                fill: None,
                style_id: None,
                page_number: target.page + 1,
                bbox: target.bbox,
                target: Some(target.bbox),
                spans: model.to_spans(style.clean_family()),
                align,
                line_height: target.line_height,
                rotation: block.rotation,
                create: false,
                owner: target.owner,
            }));
        }
        if edits.is_empty() {
            return;
        }
        doc.renderer.apply_edits(edits);
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;
        cx.notify();
    }

    /// Сливает блоки группы в один абзац.
    ///
    /// Это лекарство от чрезмерного усердия детектора: он геометрический и
    /// иногда разрезает единый абзац. Пакет из «стереть каждый выбранный» и
    /// «создать один новый в общей рамке» применяется одним шагом истории —
    /// и ему безразличны владельцы и стили: каждый кусок несёт свой шрифт,
    /// свой кегль и свой цвет, и всё это переезжает в слитый абзац как есть.
    pub(crate) fn group_multi(&mut self, cx: &mut Context<Self>) {
        if self.multi.len() < 2 {
            return;
        }
        let page = self.multi[0].page;
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let Some(blocks) = doc.blocks.get(&page) else {
            return;
        };

        let union = self
            .multi
            .iter()
            .skip(1)
            .fold(self.multi[0].bbox, |acc, target| acc.union(&target.bbox));

        // Чужой невыбранный блок внутри общей рамки — стоп: новый абзац лёг
        // бы прямо на него.
        let foreign = blocks.iter().any(|block| {
            block.bbox.intersects(&union)
                && !self.multi.iter().any(|target| target.bbox == block.bbox)
        });
        if foreign {
            self.set_status(
                "Между выбранными блоками лежит невыбранный текст — сгруппировать нельзя",
                cx,
            );
            return;
        }

        // Порядок чтения: сверху вниз.
        let mut chosen: Vec<&Block> = self
            .multi
            .iter()
            .filter_map(|target| blocks.iter().find(|block| block.bbox == target.bbox))
            .collect();
        chosen.sort_by(|a, b| b.bbox.top.total_cmp(&a.bbox.top));
        let Some(first) = chosen.first() else {
            return;
        };
        let style = first.dominant_style();

        let mut runs: Vec<crate::rich_text::Run> = Vec::new();
        for (index, block) in chosen.iter().enumerate() {
            let model = model_from_block(block, &style);
            if index > 0
                && let Some(last) = runs.last_mut()
            {
                // Между блоками — пробел: это был один абзац, разрезанный
                // детектором, и шов должен исчезнуть.
                if !last.text.ends_with(' ') {
                    last.text.push(' ');
                }
            }
            runs.extend(model.runs().iter().cloned());
        }
        let model = RichText::from_runs(runs);

        // Сначала стираются все выбранные — каждый у своего владельца, — а
        // затем в опустевшую рамку укладывается один новый абзац.
        let mut edits: Vec<BlockEdit> = chosen
            .iter()
            .map(|block| {
                BlockEdit::Erase(pdfcore::BlockErase {
                    page_number: page + 1,
                    bbox: block.bbox,
                    owner: block.mark,
                })
            })
            .collect();
        edits.push(BlockEdit::Rewrite(BlockRewrite {
            char_spacing: None,
            h_scale: None,
            para_spacing: None,
            fill: None,
            style_id: None,
            page_number: page + 1,
            bbox: union,
            target: Some(union),
            spans: model.to_spans(style.clean_family()),
            align: first.align,
            line_height: first.leading(),
            rotation: 0.0,
            create: true,
            owner: None,
        }));
        doc.renderer.apply_edits(edits);
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;
        self.set_status("Блоки слиты в один абзац", cx);
        cx.notify();
    }

    /// Щелчок по пустому месту страницы: тронутая правка применяется, чистое
    /// выделение просто снимается. Так работает Acrobat — никакой кнопки
    /// «Применить» не нужно.
    /// Высота страницы выделения — для пересчёта Y «сверху» в ось PDF.
    fn selected_page_height(&self) -> Option<f32> {
        let selection = self.selected.as_ref()?;
        let doc = self.doc.as_ref()?;
        doc.info.size(selection.page).map(|size| size.height)
    }

    pub(crate) fn set_frame_x(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.as_mut() {
            let width = selection.frame.width();
            selection.frame.left = x;
            selection.frame.right = x + width;
            selection.touched = true;
            self.request_preview(cx);
        }
    }

    /// Y задаётся от верха страницы — как в панели, а не в оси PDF.
    pub(crate) fn set_frame_y_top(&mut self, y_top: f32, cx: &mut Context<Self>) {
        let Some(page_height) = self.selected_page_height() else {
            return;
        };
        if let Some(selection) = self.selected.as_mut() {
            let height = selection.frame.height();
            selection.frame.top = page_height - y_top;
            selection.frame.bottom = selection.frame.top - height;
            selection.touched = true;
            self.request_preview(cx);
        }
    }

    pub(crate) fn set_frame_w(&mut self, w: f32, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.as_mut()
            && w > 4.0
        {
            selection.frame.right = selection.frame.left + w;
            selection.touched = true;
            self.request_preview(cx);
        }
    }

    pub(crate) fn set_frame_h(&mut self, h: f32, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.as_mut()
            && h > 4.0
        {
            selection.frame.bottom = selection.frame.top - h;
            selection.touched = true;
            self.request_preview(cx);
        }
    }

    pub(crate) fn set_para_spacing(&mut self, value: f32, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.as_mut() {
            selection.para_spacing_override = Some(value.max(0.0));
            selection.touched = true;
            self.request_preview(cx);
        }
    }

    /// Фон за буквами. `None` убирает заливку.
    pub(crate) fn set_fill(&mut self, fill: Option<[f32; 3]>, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.as_mut() {
            selection.fill = fill;
            selection.touched = true;
            self.request_preview(cx);
        }
    }

    pub(crate) fn set_char_spacing(&mut self, value: f32, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.as_mut() {
            selection.char_spacing_override = Some(value);
            selection.touched = true;
            if let Some(widgets) = self.widgets.as_ref() {
                widgets.editor.update(cx, |editor, cx| {
                    editor.base.char_spacing = value;
                    cx.notify();
                });
            }
            self.request_preview(cx);
        }
    }

    pub(crate) fn set_h_scale(&mut self, value: f32, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.as_mut()
            && value > 1.0
        {
            selection.h_scale_override = Some(value);
            selection.touched = true;
            if let Some(widgets) = self.widgets.as_ref() {
                widgets.editor.update(cx, |editor, cx| {
                    editor.base.h_scale = value;
                    cx.notify();
                });
            }
            self.request_preview(cx);
        }
    }

    /// Пересев числовых полей панели свежими значениями выделения.
    ///
    /// Поле, которое сейчас редактируют, не трогается: перебивать набор
    /// пользователя половиной значения — худшее, что может сделать панель.
    pub(crate) fn sync_property_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selected.as_ref() else {
            return;
        };
        let Some(widgets) = self.widgets.as_ref() else {
            return;
        };
        let Some(page_height) = self.selected_page_height() else {
            return;
        };
        let frame = selection.frame;
        let values = [
            (&widgets.geometry.x, format!("{:.1}", frame.left)),
            (
                &widgets.geometry.y,
                format!("{:.1}", page_height - frame.top),
            ),
            (&widgets.geometry.w, format!("{:.1}", frame.width())),
            (&widgets.geometry.h, format!("{:.1}", frame.height())),
            (
                &widgets.geometry.angle,
                format!("{:.0}", selection.rotation),
            ),
            (
                &widgets.geometry.char_spacing,
                format!(
                    "{:.2}",
                    selection
                        .char_spacing_override
                        .unwrap_or(selection.seeded_char_spacing)
                ),
            ),
            (
                &widgets.geometry.h_scale,
                format!(
                    "{:.0}",
                    selection
                        .h_scale_override
                        .unwrap_or(selection.seeded_h_scale)
                ),
            ),
            (
                &widgets.geometry.para_spacing,
                format!("{:.1}", selection.para_spacing_override.unwrap_or(0.0)),
            ),
        ];
        for (input, value) in values {
            let focused = input.focus_handle(cx).is_focused(window);
            if !focused && input.read(cx).value() != value.as_str() {
                input.update(cx, |state, cx| state.set_value(value, window, cx));
            }
        }
    }

    /// Выравнивание блоков группы по общей рамке — как в Figma.
    ///
    /// 0..=2 — левые края, центры, правые края; 3..=5 — верхние, середины,
    /// нижние. Пакет перестановок — один шаг истории.
    pub(crate) fn multi_align(&mut self, mode: u8, cx: &mut Context<Self>) {
        if self.multi.len() < 2 {
            return;
        }
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let union = self
            .multi
            .iter()
            .skip(1)
            .fold(self.multi[0].bbox, |acc, t| acc.union(&t.bbox));

        let mut edits = Vec::new();
        for target in &self.multi {
            let b = target.bbox;
            let (dx, dy) = match mode {
                0 => (union.left - b.left, 0.0),
                1 => ((union.left + union.right - b.left - b.right) / 2.0, 0.0),
                2 => (union.right - b.right, 0.0),
                3 => (0.0, union.top - b.top),
                4 => (0.0, (union.top + union.bottom - b.top - b.bottom) / 2.0),
                _ => (0.0, union.bottom - b.bottom),
            };
            if dx.abs() < 0.01 && dy.abs() < 0.01 {
                continue;
            }
            let moved = Rect::new(b.left + dx, b.bottom + dy, b.right + dx, b.top + dy);
            let rotation = doc
                .blocks
                .get(&target.page)
                .and_then(|blocks| blocks.iter().find(|block| block.bbox == b))
                .map(|block| block.rotation)
                .unwrap_or(0.0);
            edits.push(BlockEdit::Transform(pdfcore::BlockTransform {
                page_number: target.page + 1,
                bbox: b,
                target: moved,
                rotation,
                color: None,
                owner: target.owner,
            }));
        }
        if edits.is_empty() {
            return;
        }
        doc.renderer.apply_edits(edits);
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;
        cx.notify();
    }

    /// Сдвиг перетаскиваемой группы в пикселях окна.
    fn group_drag_offset(&self, page: u32) -> Option<(f32, f32)> {
        let drag = self.group_drag.as_ref().filter(|d| d.page == page)?;
        Some((
            f32::from(drag.current.x - drag.start.x),
            f32::from(drag.current.y - drag.start.y),
        ))
    }

    /// Отпускание группы: сдвиг превращается в пакет перестановок.
    fn finish_group_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.group_drag.take() else {
            return;
        };
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let zoom = doc.zoom.max(0.01);
        let dx = f32::from(drag.current.x - drag.start.x) / zoom;
        let dy = -f32::from(drag.current.y - drag.start.y) / zoom;
        if dx.abs() < 0.5 && dy.abs() < 0.5 {
            cx.notify();
            return;
        }

        let mut edits = Vec::new();
        for target in &self.multi {
            let b = target.bbox;
            let moved = Rect::new(b.left + dx, b.bottom + dy, b.right + dx, b.top + dy);
            tracing::info!(
                page = target.page,
                owner = ?target.owner,
                bbox = format!("{:.1} {:.1} {:.1} {:.1}", b.left, b.bottom, b.right, b.top),
                dx, dy,
                "перенос блока группы"
            );
            let rotation = doc
                .blocks
                .get(&target.page)
                .and_then(|blocks| blocks.iter().find(|block| block.bbox == b))
                .map(|block| block.rotation)
                .unwrap_or(0.0);
            edits.push(BlockEdit::Transform(pdfcore::BlockTransform {
                page_number: target.page + 1,
                bbox: b,
                target: moved,
                rotation,
                color: None,
                owner: target.owner,
            }));
        }
        if edits.is_empty() {
            return;
        }
        doc.renderer.apply_edits(edits);
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;
        self.set_status("Группа перенесена", cx);
        cx.notify();
    }

    /// Ctrl+C: выделенный блок или вся группа уходят в буфер блоков.
    pub(crate) fn copy_blocks(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let mut copied = Vec::new();

        if !self.multi.is_empty() {
            for target in &self.multi {
                let Some(block) = doc
                    .blocks
                    .get(&target.page)
                    .and_then(|blocks| blocks.iter().find(|b| b.bbox == target.bbox))
                else {
                    continue;
                };
                let dominant = block.dominant_style();
                copied.push(CopiedBlock {
                    runs: model_from_block(block, &dominant).runs().to_vec(),
                    bbox: block.bbox,
                    line_height: block.leading(),
                    align: block.align,
                    rotation: block.rotation,
                    base_family: dominant.clean_family().to_owned(),
                    page: target.page,
                });
            }
        } else if let (Some(selection), Some(widgets)) =
            (self.selected.as_ref(), self.widgets.as_ref())
        {
            // Одиночный блок копируется из редактора: с несохранённым
            // набором, каким его видит пользователь.
            copied.push(CopiedBlock {
                runs: widgets.editor.read(cx).text.runs().to_vec(),
                bbox: selection.frame,
                line_height: selection.line_height,
                align: selection.align,
                rotation: selection.rotation,
                base_family: selection.style.clean_family().to_owned(),
                page: selection.page,
            });
        }

        if copied.is_empty() {
            return;
        }
        let count = copied.len();
        clipboard_store(&self.path, copied);
        self.set_status(
            if count == 1 {
                "Блок скопирован — Ctrl+V вставит копию".to_owned()
            } else {
                format!("Скопировано блоков: {count} — Ctrl+V вставит копии")
            },
            cx,
        );
    }

    /// Правый клик по блоку или по пустому месту страницы.
    ///
    /// Невыделенный блок сперва становится выделенным — как в проводнике:
    /// меню всегда говорит о том, на чём его открыли. Клик по пустому месту
    /// выделение не трогает — вставке оно не мешает, а «скопировать» и
    /// «удалить» продолжают говорить о текущем выборе.
    pub(crate) fn open_text_menu(
        &mut self,
        page: u32,
        block: Option<usize>,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = block {
            let already = self
                .doc
                .as_ref()
                .and_then(|doc| doc.blocks.get(&page))
                .and_then(|blocks| blocks.get(index))
                .map(|clicked| {
                    self.multi
                        .iter()
                        .any(|target| target.page == page && target.bbox == clicked.bbox)
                })
                .unwrap_or(false);
            let selected_here = self
                .selected
                .as_ref()
                .is_some_and(|selection| selection.page == page);
            if !already && !selected_here {
                self.commit_or_clear(cx);
                self.multi.clear();
                self.toggle_multi(page, index, window, cx);
            }
        }
        self.panel_menu = Some(PanelMenu::TextOps { at });
        cx.notify();
    }

    /// Ctrl+V: копии выкладываются на видимую страницу пакетом-созданием.
    ///
    /// На той же странице копия сдвигается на 12 пт вправо-вниз — иначе она
    /// легла бы точь-в-точь на оригинал и выглядела бы как «ничего не
    /// случилось». Взаимное расположение блоков группы сохраняется.
    pub(crate) fn paste_blocks(&mut self, cx: &mut Context<Self>) {
        let (same_document, clipboard) = clipboard_take(&self.path);
        if clipboard.is_empty() {
            return;
        }
        // Открытая правка завершается: выделенной останется только копия,
        // иначе панель свойств показывала бы прежний блок, а Delete унёс бы
        // не то.
        self.commit_or_clear(cx);
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let page = doc.current_page;
        let same_page = same_document && clipboard.iter().any(|b| b.page == page);
        let (dx, dy) = if same_page { (12.0, -12.0) } else { (0.0, 0.0) };

        let mut edits = Vec::new();
        let mut pasted = Vec::new();
        for block in &clipboard {
            let b = block.bbox;
            let target = Rect::new(b.left + dx, b.bottom + dy, b.right + dx, b.top + dy);
            pasted.push(target);
            let model = RichText::from_runs(block.runs.clone());
            let mut spans = model.to_spans(&block.base_family);
            // Документная гарнитура спана резолвится в ресурсах конкретной
            // страницы. На чужой странице — даже этого же документа — такого
            // имени может не быть, и вставка падала бы «шрифт не найден».
            // Стили переводятся в явные запросы всюду, кроме родной страницы.
            if !(same_document && block.page == page) {
                for (span, run) in spans.iter_mut().zip(model.runs()) {
                    if span.font.is_none() {
                        let family = run
                            .style
                            .document_family
                            .clone()
                            .unwrap_or_else(|| block.base_family.clone());
                        span.font = Some(pdfcore::FontRequest::new(family, span.bold, span.italic));
                        span.page_family = None;
                    }
                }
            }
            edits.push(BlockEdit::Rewrite(BlockRewrite {
                page_number: page + 1,
                bbox: target,
                target: Some(target),
                spans,
                line_height: block.line_height,
                rotation: block.rotation,
                align: block.align,
                char_spacing: None,
                h_scale: None,
                para_spacing: None,
                fill: None,
                style_id: None,
                create: true,
                owner: None,
            }));
        }
        doc.renderer.apply_edits(edits);
        // Блоков ещё нет: страница перечитывается после правки. Запоминаем
        // места, а выделим, когда придёт свежий разбор.
        self.pending_paste = Some((page, pasted));
        self.set_status("Вставляю копию…", cx);
        cx.notify();
    }

    /// Делает выделенными блоки, только что легшие вставкой.
    ///
    /// Рамка вставленного блока почти совпадает с заказанной, но не буква в
    /// букву: перенабор ставит строки по метрикам шрифта. Поэтому блок
    /// ищется по ближайшему центру, а не по точному совпадению.
    fn select_pasted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((page, wanted)) = self.pending_paste.clone() else {
            return;
        };
        // Разбор страницы после правки сбрасывается и приходит заново; его
        // появление и есть сигнал, что вставленное уже на месте.
        let Some(blocks) = self.doc.as_ref().and_then(|doc| doc.blocks.get(&page)) else {
            return;
        };

        let mut found: Vec<MultiTarget> = Vec::new();
        for rect in &wanted {
            let centre = (
                (rect.left + rect.right) / 2.0,
                (rect.bottom + rect.top) / 2.0,
            );
            let nearest = blocks
                .iter()
                .filter(|block| {
                    // Тот же блок дважды не берём: две копии рядом должны
                    // выделиться каждая своя.
                    !found
                        .iter()
                        .any(|target| target.page == page && target.bbox == block.bbox)
                })
                .min_by(|a, b| {
                    let distance = |block: &pdfcore::Block| {
                        let cx = (block.bbox.left + block.bbox.right) / 2.0 - centre.0;
                        let cy = (block.bbox.bottom + block.bbox.top) / 2.0 - centre.1;
                        cx * cx + cy * cy
                    };
                    distance(a).total_cmp(&distance(b))
                });
            // Разбор страницы после правки сбрасывается, но ответ на прежний
            // запрос может прийти уже после — в нём копии ещё нет. Такой
            // разбор узнаётся по тому, что рядом с заказанным местом ничего
            // не легло: ждём следующего.
            let nearest = nearest.filter(|block| {
                let dx = (block.bbox.left + block.bbox.right) / 2.0 - centre.0;
                let dy = (block.bbox.bottom + block.bbox.top) / 2.0 - centre.1;
                dx * dx + dy * dy <= PASTE_MATCH * PASTE_MATCH
            });
            let Some(block) = nearest else {
                return;
            };
            found.push(MultiTarget {
                page,
                bbox: block.bbox,
                owner: block.mark,
                size: block.dominant_style().size,
                line_height: block.leading(),
                align: block.align,
            });
        }

        self.pending_paste = None;
        if found.is_empty() {
            return;
        }
        let count = found.len();
        self.selected = None;
        self.multi = found;
        self.rebuild_multi_size(window, cx);
        self.set_status(
            if count == 1 {
                "Копия вставлена и выделена — тяните её на место".to_owned()
            } else {
                format!("Вставлено блоков: {count} — выделены, тяните на место")
            },
            cx,
        );
        cx.notify();
    }

    /// Удаление всей группы одним пакетом стираний.
    pub(crate) fn delete_multi(&mut self, cx: &mut Context<Self>) {
        if self.multi.is_empty() {
            return;
        }
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let edits: Vec<BlockEdit> = self
            .multi
            .iter()
            .map(|target| {
                BlockEdit::Erase(pdfcore::BlockErase {
                    page_number: target.page + 1,
                    bbox: target.bbox,
                    owner: target.owner,
                })
            })
            .collect();
        doc.renderer.apply_edits(edits);
        self.multi.clear();
        self.multi_size = None;
        self.multi_family = None;
        self.multi_color = None;
        self.set_status("Группа удалена — Ctrl+Z вернёт", cx);
        cx.notify();
    }

    pub(crate) fn commit_or_clear(&mut self, cx: &mut Context<Self>) {
        match self.selected.as_ref() {
            Some(selection) if selection.touched => self.apply_edit(cx),
            Some(_) => self.clear_selection(cx),
            None => {}
        }
    }

    pub(crate) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.widgets = None;
        self.drag = None;
        self.guides.clear();
        // Поле правки исчезло вместе с выделением, и клавиатура осталась ни
        // при чём: следующий Ctrl+V уходил в никуда. Возвращаем её виду.
        self.focus_root = true;
        // Предпросмотр показывал непринятую правку — без выделения он лжёт.
        // Текстуру снимаем явно: её копия живёт ещё и в атласе спрайтов gpui.
        if let Some((_, texture)) = self.doc.as_mut().and_then(|doc| doc.preview.take()) {
            cx.drop_image(texture, None);
        }
        cx.notify();
    }

    /// Текущее состояние панели формата.
    pub(crate) fn current_format(&self, cx: &App) -> Option<(String, f32, bool, bool, bool)> {
        let selection = self.selected.as_ref()?;
        let widgets = self.widgets.as_ref()?;

        let family = self
            .chosen_family
            .clone()
            .unwrap_or_else(|| selection.style.clean_family().to_owned());
        let size = widgets
            .size
            .read(cx)
            .value()
            .trim()
            .replace(',', ".")
            .parse::<f32>()
            .ok()
            .filter(|v| (1.0..=400.0).contains(v))
            .unwrap_or(selection.style.size);

        // Начертание берётся там, где стоит каретка, — как в текстовом
        // редакторе: панель показывает состояние текущего места, а не то, что
        // нажимали последним.
        let at_caret = widgets.editor.read(cx).text.style_at_caret();
        Some((
            family,
            size,
            at_caret.bold,
            at_caret.italic,
            at_caret.underline,
        ))
    }

    /// Собирает правку из текущего состояния редактора и панели формата.
    ///
    /// Одна и та же сборка идёт и на показ по ходу набора, и на применение:
    /// иначе показанное могло бы разойтись с записанным.
    fn pending_edit(&self, cx: &App) -> Option<BlockEdit> {
        let widgets = self.widgets.as_ref()?;
        let model = widgets.editor.read(cx).text.clone();
        let (family, size, _bold, _italic, _underline) = self.current_format(cx)?;
        let selection = self.selected.as_ref()?;

        // Перестановка вместо перенабора, когда содержимое не трогали. Это не
        // оптимизация, а единственный способ подвинуть пёстрый абзац: перенабор
        // выложил бы его одним оформлением и потому справедливо отклоняется,
        // если внутри есть слова другим шрифтом или кеглем. Перестановка же
        // правит только координаты уже написанного.
        let family_untouched = {
            let original = selection
                .page_font
                .as_ref()
                .map(|font| font.base_font.clone())
                .unwrap_or_else(|| selection.style.clean_family().to_owned());
            // Точное начертание из пикера — смена шрифта, даже когда семейство
            // то же самое: блок «MyriadPro-Semibold» и выбранный «Myriad Pro
            // Light» дают одинаковый ключ, но это разные шрифты.
            self.chosen_face.is_none()
                && pdfcore::fonts::family_key(&family) == pdfcore::fonts::family_key(&original)
        };
        if !selection.creating
            && selection.style_id == selection.seeded_style_id
            && selection.char_spacing_override.is_none()
            && selection.h_scale_override.is_none()
            && selection.para_spacing_override.is_none()
            && selection.fill.is_none()
            && !selection.needs_retypeset
            && family_untouched
            && model.text() == selection.original_text
            && model.runs().iter().all(|run| run.style.keeps_typesetting())
            && size == seeded_size(selection.style.size)
            && (selection.frame.width() - selection.bbox.width()).abs() < 0.5
        {
            return Some(BlockEdit::Transform(pdfcore::BlockTransform {
                page_number: selection.page + 1,
                bbox: selection.bbox,
                target: selection.frame,
                rotation: selection.rotation,
                color: single_color(&model),
                owner: selection.owner,
            }));
        }

        // Кегль исходного абзаца берём из документа, если он уже прочитан:
        // pdfium и поток содержимого иногда расходятся на доли пункта, а
        // сравнение решает, менять ли размер вообще.
        let original_size = selection
            .page_font
            .as_ref()
            .map(|font| font.size)
            .unwrap_or(selection.style.size);

        // Куски берём прямо из модели: у каждого своё оформление, и шрифт
        // встраивается только там, где его правда меняли.
        let mut edit = BlockRewrite {
            char_spacing: selection.char_spacing_override,
            h_scale: selection.h_scale_override,
            para_spacing: selection.para_spacing_override,
            fill: selection.fill,
            style_id: selection.style_id,
            // lopdf нумерует страницы с единицы.
            page_number: selection.page + 1,
            bbox: selection.bbox,
            // Стирать надо там, где текст был, а печатать — там, куда его
            // передвинули маркерами.
            target: Some(selection.frame),
            spans: model.to_spans(&family),
            align: selection.align,
            line_height: selection.line_height,
            rotation: selection.rotation,
            create: selection.creating,
            owner: selection.owner,
        };

        // Кегль, заданный в панели, применяется к тем кускам, где своего нет.
        if (size - original_size).abs() > 0.01 {
            for span in &mut edit.spans {
                span.size.get_or_insert(size);
            }
        }

        // Гарнитура из панели — та же дисциплина. Сравнение по
        // нормализованному ключу: панель показывает «PT Serif», а блок набран
        // «PTSerif-Regular», и это одна и та же гарнитура, а не смена шрифта.
        let original_family = selection
            .page_font
            .as_ref()
            .map(|font| font.base_font.clone())
            .unwrap_or_else(|| selection.style.clean_family().to_owned());
        if selection.creating
            || self.chosen_face.is_some()
            || pdfcore::fonts::family_key(&family) != pdfcore::fonts::family_key(&original_family)
        {
            // Спаны и раны модели идут один в один. Гарнитура панели
            // накрывает каждый кусок, где гарнитуру не выбирали для него
            // отдельно, — включая куски, у которых шрифт уже стоит из-за
            // смены начертания: тот шрифт построен от документной гарнитуры,
            // и оставить его — значит молча потерять выбор. Так и терялся
            // «CMU Serif · Bold» на курсивном блоке: сброс курсива помечал
            // кусок сменившим начертание раньше, чем семейство успевало
            // примениться.
            for (span, run) in edit.spans.iter_mut().zip(model.runs()) {
                if run.style.family.is_none() {
                    // Начертание куска сохраняется: жирное начало абзаца
                    // остаётся жирным и в новой гарнитуре.
                    let mut request = pdfcore::FontRequest::new(&family, span.bold, span.italic);
                    // Точное начертание из пикера главнее флагов — его
                    // выбрали руками из списка семейства.
                    if let Some(face) = self.chosen_face.as_ref() {
                        request = request.with_face(face.clone());
                    }
                    span.font = Some(request);
                    span.page_family = None;
                }
            }
        }

        // Замена шрифта — там, где набирать нечем: программа не вшита в файл
        // и в системе гарнитуры нет. Ручная замена из окна «Шрифты» главнее;
        // без неё берётся автоматическая — ближайшая по духу установленная
        // гарнитура. Какая именно — видно в окне «Шрифты» стрелкой.
        let manual = self.substitute_for(&original_family).cloned();
        let auto = if manual.is_none() {
            let unusable = selection
                .page_font
                .as_ref()
                .is_none_or(|font| !font.embedded)
                && pdfcore::system_fonts()
                    .find_family(&original_family)
                    .is_none();
            unusable
                .then(|| pdfcore::fonts::suggest_substitute(&original_family))
                .flatten()
        } else {
            None
        };
        if let Some(chosen) = manual.or(auto) {
            for (span, run) in edit.spans.iter_mut().zip(model.runs()) {
                // Куски, где гарнитуру выбрали отдельно, не трогаем; куски со
                // сменённым начертанием несут шрифт документной гарнитуры —
                // той самой, которой нет, — и подменяются вместе с прочими.
                if run.style.family.is_none() {
                    span.font = Some(pdfcore::FontRequest::new(&chosen, span.bold, span.italic));
                    span.page_family = None;
                }
            }
        }
        Some(BlockEdit::Rewrite(edit))
    }

    /// Просит перерисовать страницу с непринятой ещё правкой.
    ///
    /// Именно это и делает правку правкой «в самом тексте»: на экране
    /// обновляется настоящая страница, а не накладка над ней. Один такой круг
    /// стоит около 54 мс на книге в 205 МБ — страница вынимается в отдельный
    /// одностраничный документ, и пересобирается только она.
    pub(crate) fn request_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.as_mut() {
            selection.touched = true;
        }
        let Some(edit) = self.pending_edit(cx) else {
            return;
        };
        let Some(doc) = self.doc.as_ref() else { return };
        // Тот же масштаб, что и у тайла, который картинка заменяет.
        let scale = doc
            .page_key(edit.page_number().saturating_sub(1), self.scale_factor)
            .zoom
            .scale();
        doc.renderer.request_preview(edit, scale);
        cx.notify();
    }

    /// Отправляет правку в поток рендера.
    pub(crate) fn apply_edit(&mut self, cx: &mut Context<Self>) {
        let Some(widgets) = self.widgets.as_ref() else {
            return;
        };
        if widgets.editor.read(cx).text.text().trim().is_empty() {
            self.set_status("Пустой текст: блок остался как был", cx);
            return;
        }
        let Some(edit) = self.pending_edit(cx) else {
            return;
        };

        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.apply_edit(edit);
        }
        // Разбор страницы после правки изменится, старое выделение обесценится.
        self.clear_selection(cx);
    }

    /// Шаг истории назад. Открытая правка сбрасывается: откатывается то, что
    /// уже применено, а не то, что ещё набирают.
    pub(crate) fn undo(&mut self, cx: &mut Context<Self>) {
        self.clear_selection(cx);
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.undo();
        }
    }

    pub(crate) fn redo(&mut self, cx: &mut Context<Self>) {
        self.clear_selection(cx);
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.redo();
        }
    }

    pub(crate) fn set_status(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc.as_mut() {
            doc.status = Some(message.into());
        }
        cx.notify();
    }

    /// Сохраняет результат рядом с исходником, а не поверх него.
    ///
    /// Перезаписывать оригинал по нажатию кнопки нельзя: правка PDF
    /// необратима, а исходник — единственная копия книги пользователя.
    fn save(&mut self, cx: &mut Context<Self>) {
        // Сохраняется сам открытый файл — как в любом настольном редакторе.
        // Подмена на диске атомарная, исходник в целости на каждом шаге:
        // см. `save_document` в движке.
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.save_as(self.path.clone());
            self.set_status("Сохраняю…", cx);
        }
        cx.notify();
    }

    /// Сколько рядов в ленте: в развороте на ряд приходится две страницы.
    fn row_count(&self, pages: u32) -> usize {
        if self.spread {
            pages.div_ceil(2) as usize
        } else {
            pages as usize
        }
    }

    /// Ряд, в котором лежит страница.
    fn row_of(&self, page: u32) -> usize {
        if self.spread { page / 2 } else { page }.max(0) as usize
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom - doc.zoom).abs() < 0.0005 {
            return;
        }
        doc.zoom = zoom;

        // Высота страниц изменилась — список обязан перемерить элементы. Но
        // `reset` заодно отматывает его в самое начало, поэтому читаемую
        // страницу приходится возвращать руками: менять масштаб и оказываться
        // на первой странице книги — не то, чего ждёшь.
        //
        // Именно `scroll_to`, а не «показать элемент»: после сброса высоты
        // страниц ещё не измерены, и расчёт «сколько прокрутить, чтобы элемент
        // стал виден» на нулевых высотах всегда выдаёт самое начало ленты.
        // Здесь же нужное положение известно точно — искомая страница просто
        // становится верхней.
        let page = doc.current_page;
        let count = doc.info.page_count;
        let rows = self.row_count(count);
        let row = self.row_of(page).min(rows.saturating_sub(1));
        let doc = self.doc.as_mut().expect("документ проверен выше");
        doc.pages.reset(rows);
        doc.pages.scroll_to(gpui::ListOffset {
            item_ix: row,
            offset_in_item: px(0.0),
        });
        self.clamp_pan();
        cx.notify();
    }

    /// Масштаб, заданный руками, отменяет подгонку: иначе следующая же смена
    /// размера окна молча вернула бы прежний и человек решил бы, что колесо
    /// сломалось.
    fn zoom_manually(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.fit = Fit::Free;
        self.set_zoom(zoom, cx);
    }

    fn zoom_manually_by(&mut self, factor: f32, cx: &mut Context<Self>) {
        let current = self.doc.as_ref().map(|d| d.zoom).unwrap_or(1.0);
        self.zoom_manually(current * factor, cx);
    }

    /// Выбор способа подгонки: по ширине, по высоте или обратно к ручному.
    pub(crate) fn set_fit(&mut self, fit: Fit, cx: &mut Context<Self>) {
        self.fit = if self.fit == fit { Fit::Free } else { fit };
        self.apply_fit(cx);
        cx.notify();
    }

    /// Разворот: две страницы в ряд. Масштаб пересчитывается сразу — в
    /// развороте странице достаётся половина ширины.
    pub(crate) fn toggle_spread(&mut self, cx: &mut Context<Self>) {
        self.spread = !self.spread;
        // Лента считает рядами, и их число только что изменилось.
        if let Some(doc) = self.doc.as_ref() {
            let count = doc.info.page_count;
            let page = doc.current_page;
            let rows = self.row_count(count);
            let row = self.row_of(page).min(rows.saturating_sub(1));
            let doc = self.doc.as_mut().expect("документ проверен выше");
            doc.pages.reset(rows);
            doc.pages.scroll_to(gpui::ListOffset {
                item_ix: row,
                offset_in_item: px(0.0),
            });
        }
        // Развороту нужна вся ширина окна. Если при нынешнем масштабе пара
        // страниц в него не влезает, разъезжаться ей некуда: лента не
        // прокручивается вбок, и края страниц просто обрезало бы. Тогда
        // масштаб подбирается сам — по ширине.
        if self.spread && self.fit == Fit::Free && !self.spread_fits() {
            self.fit = Fit::Width;
        }
        self.apply_fit(cx);
        cx.notify();
    }

    /// Ширина того, что лежит в ряду ленты, при нынешнем масштабе.
    fn content_width(&self) -> f32 {
        let Some(doc) = self.doc.as_ref() else {
            return 0.0;
        };
        let Some(size) = doc.info.size(doc.current_page).or_else(|| doc.info.size(0)) else {
            return 0.0;
        };
        let page = size.width * doc.zoom;
        if self.spread {
            page * 2.0 + SPREAD_GAP
        } else {
            page
        }
    }

    /// Насколько далеко можно увести страницу вбок. Ряд стоит по центру, и
    /// вылезает он поровну с обеих сторон — отсюда половина.
    fn max_pan(&self) -> f32 {
        let room = f32::from(self.canvas_size.width) - PAGE_CHROME_X;
        ((self.content_width() - room) / 2.0).max(0.0)
    }

    /// Сдвигает страницы вбок, не выпуская их края внутрь окна.
    fn set_pan(&mut self, value: f32, cx: &mut Context<Self>) {
        let limit = self.max_pan();
        let value = value.clamp(-limit, limit);
        if (value - self.pan_x).abs() < 0.01 {
            return;
        }
        self.pan_x = value;
        cx.notify();
    }

    /// Возвращает сдвиг в допустимые пределы: масштаб или размер окна
    /// изменились, и вчерашний сдвиг мог оставить страницу за краем.
    fn clamp_pan(&mut self) {
        let limit = self.max_pan();
        self.pan_x = self.pan_x.clamp(-limit, limit);
    }

    /// Влезает ли разворот в окно при нынешнем масштабе.
    fn spread_fits(&self) -> bool {
        let width = f32::from(self.canvas_size.width);
        let Some(doc) = self.doc.as_ref() else {
            return true;
        };
        let Some(size) = doc.info.size(doc.current_page).or_else(|| doc.info.size(0)) else {
            return true;
        };
        size.width * doc.zoom * 2.0 + PAGE_CHROME_X + SPREAD_GAP <= width
    }

    /// Пересчитывает масштаб под текущий способ подгонки и размер окна.
    fn apply_fit(&mut self, cx: &mut Context<Self>) {
        if self.fit == Fit::Free {
            return;
        }
        let width = f32::from(self.canvas_size.width);
        let height = f32::from(self.canvas_size.height);
        if width <= 1.0 || height <= 1.0 {
            return;
        }
        let Some(doc) = self.doc.as_ref() else { return };
        // Меряется по читаемой странице: у книги бывают вклейки другого
        // формата, и подгонять их по первой странице было бы враньём.
        let Some(size) = doc.info.size(doc.current_page).or_else(|| doc.info.size(0)) else {
            return;
        };
        if size.width <= 0.0 || size.height <= 0.0 {
            return;
        }

        let zoom = match self.fit {
            Fit::Width => {
                let room = if self.spread {
                    (width - PAGE_CHROME_X - SPREAD_GAP) / 2.0
                } else {
                    width - PAGE_CHROME_X
                };
                room / size.width
            }
            Fit::Height => (height - PAGE_CHROME_Y) / size.height,
            Fit::Free => return,
        };
        self.set_zoom(zoom, cx);
    }

    /// Место, которое лента занимает в окне. Пишется отрисовкой: до раскладки
    /// размер неизвестен, а подгонка считается именно от него.
    fn note_canvas(&mut self, size: gpui::Size<Pixels>, cx: &mut Context<Self>) {
        if self.canvas_size == size {
            return;
        }
        self.canvas_size = size;
        self.apply_fit(cx);
        self.clamp_pan();
    }

    /// Слежение за мышью, которое не зависит от того, что лежит под курсором.
    ///
    /// Обработчики регистрируются напрямую в окне, а не навешиваются на `div`.
    /// Это принципиально: обработчик на элементе срабатывает, только когда его
    /// собственная область — верхняя под курсором, а во время перетаскивания
    /// рамка как раз уезжает из-под указателя, да и лежат над ней то страница,
    /// то панель формата. Через окно ни одно движение не теряется.
    ///
    /// Здесь же перехватывается щипок на трекпаде. Он ловится в **фазе
    /// перехвата**: лента страниц слушает колесо в фазе всплытия и
    /// прокручивается безусловно, остановить её можно только раньше неё —
    /// иначе жест и увеличивал бы, и уезжал вниз одновременно. Отдельного
    /// события «щипок» в Windows нет: сенсорная панель шлёт его как колесо с
    /// зажатым Ctrl, тем же самым, что и мышь, так что одна обработка
    /// покрывает оба случая.
    fn render_mouse_hooks(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity().downgrade();
        gpui::canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let zoom_view = view.clone();
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture
                        || !event.modifiers.control
                        || !bounds.contains(&event.position)
                    {
                        return;
                    }
                    // Шаг пропорционален величине жеста, а не фиксирован:
                    // щипок приходит частыми маленькими порциями, и от
                    // постоянного множителя масштаб скакал бы.
                    let step = f32::from(event.delta.pixel_delta(px(20.0)).y);
                    if step.abs() > 0.01
                        && let Some(view) = zoom_view.upgrade()
                    {
                        view.update(cx, |this, cx| {
                            this.zoom_manually_by((step / 150.0).exp(), cx)
                        });
                    }
                    cx.stop_propagation();
                });

                // Два пальца поперёк трекпада (и наклон колеса вбок) двигают
                // страницу, когда она шире окна. Событие приходит общим — и
                // вбок, и вниз сразу, — поэтому ленте оно достаётся всегда,
                // кроме случая, когда жест явно горизонтальный.
                let pan_view = view.clone();
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture
                        || event.modifiers.control
                        || !bounds.contains(&event.position)
                    {
                        return;
                    }
                    let delta = event.delta.pixel_delta(px(20.0));
                    let dx = f32::from(delta.x);
                    let dy = f32::from(delta.y);
                    if dx.abs() < 0.01 {
                        return;
                    }
                    let Some(view) = pan_view.upgrade() else {
                        return;
                    };
                    let moved = view.update(cx, |this, cx| {
                        if this.max_pan() <= 0.0 {
                            return false;
                        }
                        this.set_pan(this.pan_x - dx, cx);
                        true
                    });
                    // Прокрутку вниз отдаём ленте: у трекпада в одном событии
                    // приезжают обе оси, и глотать его целиком значило бы
                    // остановить чтение вместе со сдвигом.
                    if moved && dx.abs() > dy.abs() {
                        cx.stop_propagation();
                    }
                });

                // Нажатое колёсико таскает страницу — привычный жест из всех
                // просмотрщиков: вбок сдвигом ряда, вверх-вниз прокруткой.
                let grab_view = view.clone();
                window.on_mouse_event(move |event: &gpui::MouseDownEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture
                        || event.button != MouseButton::Middle
                        || !bounds.contains(&event.position)
                    {
                        return;
                    }
                    if let Some(view) = grab_view.upgrade() {
                        view.update(cx, |this, _| {
                            this.pan_drag = Some(PanDrag {
                                at: event.position,
                                pan: this.pan_x,
                            });
                        });
                    }
                    cx.stop_propagation();
                });

                let drag_view = view.clone();
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture {
                        return;
                    }
                    let position = event.position;
                    if let Some(view) = drag_view.upgrade() {
                        view.update(cx, |this, cx| {
                            let Some(drag) = this.pan_drag else { return };
                            if event.pressed_button != Some(MouseButton::Middle) {
                                this.pan_drag = None;
                                return;
                            }
                            let dx = f32::from(position.x - drag.at.x);
                            let dy = position.y - drag.at.y;
                            this.set_pan(drag.pan - dx, cx);
                            // Лента живёт своей прокруткой, поэтому по вертикали
                            // страница едет ею же: тащим вниз — лента вверх.
                            if let Some(doc) = this.doc.as_ref() {
                                doc.pages.scroll_by(-dy);
                            }
                            // Точка захвата переносится: прокрутка ленты
                            // отсчитывается от прошлого кадра, а не от начала
                            // жеста, и складывать её со сдвигом нельзя.
                            this.pan_drag = Some(PanDrag {
                                at: position,
                                pan: this.pan_x,
                            });
                            cx.notify();
                        });
                    }
                });

                let release_view = view.clone();
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture || event.button != MouseButton::Middle
                    {
                        return;
                    }
                    if let Some(view) = release_view.upgrade() {
                        view.update(cx, |this, _| this.pan_drag = None);
                    }
                });

                let menu_view = view.clone();
                window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    if let Some(view) = menu_view.upgrade() {
                        view.update(cx, |this, cx| {
                            if this.panel_menu.take().is_some() {
                                cx.notify();
                            }
                        });
                    }
                });

                let move_view = view.clone();
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    let position = event.position;
                    if let Some(view) = move_view.upgrade() {
                        view.update(cx, |this, cx| {
                            this.drag_frame(position, window, cx);
                            if event.pressed_button == Some(gpui::MouseButton::Left)
                                && let Some(drag) = this.group_drag.as_mut()
                            {
                                drag.current = position;
                                cx.notify();
                            }
                            if event.pressed_button == Some(gpui::MouseButton::Left) {
                                // Сдвинулись с места нажатия — это рамка.
                                if let Some(press) = this.pending_press.as_ref() {
                                    if press_became_drag(press.at, position) {
                                        tracing::info!(page = press.page, "рамка началась");
                                        this.rubber = Some(Rubber {
                                            page: press.page,
                                            block: press.block,
                                            start: press.at,
                                            current: position,
                                        });
                                        this.pending_press = None;
                                    }
                                }
                                if let Some(rubber) = this.rubber.as_mut() {
                                    rubber.current = position;
                                    cx.notify();
                                }
                            }
                        });
                    }
                });

                let up_view = view.clone();
                window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    if let Some(view) = up_view.upgrade() {
                        view.update(cx, |this, cx| {
                            // Нажатие без движения — щелчок: по блоку —
                            // выделение, по пустому месту — ничего.
                            this.finish_group_drag(cx);
                            if let Some(press) = this.pending_press.take()
                                && let Some(index) = press.block
                            {
                                this.select_block(press.page, index, window, cx);
                            }
                            this.finish_rubber(window, cx);
                            if this.drag.take().is_none() {
                                return;
                            }
                            // Клавиатура возвращается к тексту: рамку подвинули
                            // и тут же продолжают печатать, а Esc обязан
                            // отменять правку и после перетаскивания. Возврат
                            // делается здесь, а не при нажатии: нажатие на
                            // маркер само по себе снимает фокус с текста, и
                            // сделанное раньше не удержалось бы.
                            if let Some(widgets) = this.widgets.as_ref() {
                                window.focus(&widgets.editor.read(cx).focus_handle(cx));
                            }
                            // Линии привязки жили только на время переноса.
                            this.guides.clear();
                            cx.notify();
                        });
                    }
                });
            },
        )
        .absolute()
        // Угол прибит явно: без `top`/`left` «absolute» встаёт на своё место в
        // потоке — то есть ПОД лентой, за нижним краем окна. Область
        // обработчиков оказывалась вне экрана, и проверка «курсор внутри»
        // никогда не срабатывала: щипок и Ctrl+колесо не делали ничего.
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    fn toggle_blocks(&mut self, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc.as_mut() {
            doc.show_blocks = !doc.show_blocks;
            if doc.show_blocks {
                let page = doc.current_page;
                if doc.blocks_requested.insert(page) {
                    doc.renderer.request_blocks(page);
                }
            }
            cx.notify();
        }
    }

    fn go_to_page(&mut self, page: u32, cx: &mut Context<Self>) {
        let row = self.row_of(page);
        if let Some(doc) = self.doc.as_mut() {
            // Именно `scroll_to`, а не «показать элемент»: высоты далёких
            // страниц ещё не измерены, и «показать» честно считал смещение по
            // нулям — клик по дальней миниатюре уводил ленту к началу.
            doc.pages.scroll_to(gpui::ListOffset {
                item_ix: row,
                offset_in_item: px(0.0),
            });
            doc.current_page = page;
            cx.notify();
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = &event.keystroke.modifiers;
        // Открытое меню перехватывает Esc: сперва закрывается оно.
        if self.panel_menu.is_some() && event.keystroke.key == "escape" {
            self.panel_menu = None;
            cx.notify();
            return;
        }
        if self.font_picker.is_some() && event.keystroke.key == "escape" {
            self.font_picker = None;
            cx.notify();
            return;
        }
        // В сетке страниц те же горячие клавиши работают со страницами, а не
        // с блоками текста: там страницы и есть содержимое.
        if self.organise.is_some() {
            let anchor = self
                .organise
                .as_ref()
                .and_then(|o| o.picked.first().copied())
                .unwrap_or(0);
            match event.keystroke.key.as_str() {
                "c" if modifiers.control => self.organise_copy(cx),
                "v" if modifiers.control => self.organise_paste(cx),
                "x" if modifiers.control => {
                    self.organise_copy(cx);
                    self.organise_delete(anchor, cx);
                }
                "a" if modifiers.control => {
                    let count = self.doc.as_ref().map(|d| d.info.page_count).unwrap_or(0);
                    if let Some(organise) = self.organise.as_mut() {
                        organise.picked = (0..count).collect();
                        organise.anchor = Some(0);
                    }
                    cx.notify();
                }
                "delete" | "backspace" => self.organise_delete(anchor, cx),
                "z" if modifiers.control => self.undo(cx),
                "y" if modifiers.control => self.redo(cx),
                "s" if modifiers.control => self.save(cx),
                "escape" => self.toggle_organise(cx),
                "enter" => {
                    self.organise = None;
                    self.go_to_page(anchor, cx);
                }
                _ => {}
            }
            return;
        }
        match event.keystroke.key.as_str() {
            "=" | "+" if modifiers.control => self.zoom_manually_by(ZOOM_STEP, cx),
            "-" if modifiers.control => self.zoom_manually_by(1.0 / ZOOM_STEP, cx),
            "0" if modifiers.control => self.zoom_manually(1.0, cx),
            "s" if modifiers.control => self.save(cx),
            "z" if modifiers.control => self.undo(cx),
            "y" if modifiers.control => self.redo(cx),
            "escape" => self.clear_selection(cx),
            "c" if modifiers.control => self.copy_blocks(cx),
            "v" if modifiers.control => self.paste_blocks(cx),
            "delete" | "backspace" if !self.multi.is_empty() => self.delete_multi(cx),
            // Пока идёт правка, буквы уходят в поле ввода, а не в горячие клавиши.
            "b" if self.selected.is_none() => self.toggle_blocks(cx),
            _ => {}
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let zoom = self.doc.as_ref().map(|d| d.zoom).unwrap_or(1.0);
        let fit = self.fit;
        let spread = self.spread;
        let show_blocks = self.doc.as_ref().is_some_and(|d| d.show_blocks);
        let snapping = self.snapping;
        let show_properties = self.show_properties;
        let dirty = self.doc.as_ref().is_some_and(|d| d.dirty);
        let title = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string());

        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("go-home")
                    .small()
                    .ghost()
                    .label("← Недавние")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ViewerEvent::GoHome))),
            )
            .child(div().flex_1().text_sm().child(if dirty {
                format!("{title} — есть несохранённые правки")
            } else {
                title
            }))
            .child(
                Button::new("styles")
                    .small()
                    .ghost()
                    .label("Стили")
                    .tooltip("Именованные стили: правка перенабирает все блоки стиля")
                    .on_click(cx.listener(|this, _, _, cx| this.open_styles(cx))),
            )
            .child(
                Button::new("fonts")
                    .small()
                    .ghost()
                    .label("Шрифты")
                    .tooltip("Чем набран документ и чего не хватает системе")
                    .on_click(cx.listener(|this, _, _, cx| this.open_fonts(cx))),
            )
            .child(
                Button::new("save")
                    .small()
                    .when(dirty, |b| b.primary())
                    .when(!dirty, |b| b.ghost())
                    .label("Сохранить")
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
            .child(
                Button::new("undo")
                    .small()
                    .ghost()
                    .icon(gpui_component::Icon::empty().path("icons/undo-03.svg"))
                    .tooltip("Отменить правку (Ctrl+Z) — история на сто шагов")
                    .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
            )
            .child(
                Button::new("redo")
                    .small()
                    .ghost()
                    .icon(gpui_component::Icon::empty().path("icons/redo-03.svg"))
                    .tooltip("Вернуть правку (Ctrl+Y)")
                    .on_click(cx.listener(|this, _, _, cx| this.redo(cx))),
            )
            .child(
                Button::new("zoom-out").small().ghost().label("−").on_click(
                    cx.listener(|this, _, _, cx| this.zoom_manually_by(1.0 / ZOOM_STEP, cx)),
                ),
            )
            .child(
                Button::new("zoom-reset")
                    .small()
                    .ghost()
                    .label(format!("{:.0}%", zoom * 100.0))
                    .tooltip("Вернуть натуральную величину (Ctrl+0)")
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_manually(1.0, cx))),
            )
            .child(
                Button::new("zoom-in")
                    .small()
                    .ghost()
                    .label("+")
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_manually_by(ZOOM_STEP, cx))),
            )
            // Как показывать документ: во всю ширину, целиком по высоте и
            // разворотом в две страницы. Первые две — взаимоисключающие
            // способы подгонки, разворот работает с любым из них.
            .child(
                Button::new("fit-width")
                    .small()
                    .when(fit == Fit::Width, |b| b.primary())
                    .when(fit != Fit::Width, |b| b.ghost())
                    .icon(gpui_component::Icon::empty().path("icons/fit-width.svg"))
                    .tooltip("На всю ширину: страница по ширине окна")
                    .on_click(cx.listener(|this, _, _, cx| this.set_fit(Fit::Width, cx))),
            )
            .child(
                Button::new("fit-height")
                    .small()
                    .when(fit == Fit::Height, |b| b.primary())
                    .when(fit != Fit::Height, |b| b.ghost())
                    .icon(gpui_component::Icon::empty().path("icons/fit-height.svg"))
                    .tooltip("На всю высоту: страница целиком в окне")
                    .on_click(cx.listener(|this, _, _, cx| this.set_fit(Fit::Height, cx))),
            )
            .child(
                Button::new("spread")
                    .small()
                    .when(spread, |b| b.primary())
                    .when(!spread, |b| b.ghost())
                    .icon(gpui_component::Icon::empty().path("icons/page-spread.svg"))
                    .tooltip("В две страницы: разворот, как в раскрытой книге")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_spread(cx))),
            )
            .child(
                Button::new("toggle-properties")
                    .small()
                    .when(show_properties, |b| b.primary())
                    .when(!show_properties, |b| b.ghost())
                    .label("Свойства")
                    .tooltip("Панель свойств блока и текста")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_properties = !this.show_properties;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("toggle-snap")
                    .small()
                    .when(snapping, |b| b.primary())
                    .when(!snapping, |b| b.ghost())
                    .icon(gpui_component::Icon::empty().path("icons/magnet-02.svg"))
                    .tooltip(if snapping {
                        "Привязка включена: рамка выравнивается по соседним блокам"
                    } else {
                        "Привязка выключена: рамка двигается свободно"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.snapping = !this.snapping;
                        this.guides.clear();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("toggle-blocks")
                    .small()
                    .when(show_blocks, |b| b.primary())
                    .when(!show_blocks, |b| b.ghost())
                    .label("Блоки")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_blocks(cx))),
            )
            .into_any_element()
    }

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // В режиме сетки панель миниатюр не нужна: сетка и есть страницы, а
        // место лучше отдать ей.
        if self.doc.is_none() || self.organise.is_some() {
            return div().into_any_element();
        }
        let state = self
            .doc
            .as_ref()
            .expect("документ проверен выше")
            .thumbs
            .clone();
        let scale_factor = window.scale_factor();
        let this = cx.entity().downgrade();

        v_flex()
            .id("thumbs-panel")
            .w(px(THUMB_PANEL_WIDTH))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            // Вход в режим управления страницами — там, где страницы и живут.
            .child(
                div().px_2().py_1p5().child(
                    Button::new("organise")
                        .small()
                        .ghost()
                        .w_full()
                        .label("Систематизировать")
                        .tooltip("Сетка страниц: порядок, повороты, вставка и удаление")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_organise(cx))),
                ),
            )
            // Файл, брошенный на панель страниц, вставляется в документ, а не
            // открывается вместо него: панель — это состав книги, и жест над
            // ней читается однозначно.
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.drop_document(paths, cx);
                cx.stop_propagation();
            }))
            .child(
                list(state, move |ix, window, cx| {
                    this.update(cx, |viewer, cx| {
                        viewer.render_thumb(ix, scale_factor, window, cx)
                    })
                    .unwrap_or_else(|_| div().into_any_element())
                })
                .with_sizing_behavior(ListSizingBehavior::Auto)
                .size_full(),
            )
            .into_any_element()
    }

    fn render_thumb(
        &mut self,
        ix: usize,
        scale_factor: f32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(doc) = self.doc.as_mut() else {
            return div().into_any_element();
        };
        let page = ix as u32;
        let Some(size) = doc.info.size(page) else {
            return div().into_any_element();
        };

        let is_current = doc.current_page == page;
        let total = doc.info.page_count;
        let height = THUMBNAIL_HEIGHT_PX;
        let width = size.width / size.height * height;
        let texture = doc
            .thumbnail_key(page, scale_factor)
            .and_then(|k| doc.tiles.get(&k));

        v_flex()
            .w_full()
            .items_center()
            .gap_1()
            .py_1()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| this.go_to_page(page, cx)),
            )
            // Правая кнопка — контекстное меню страницы, как в настольных
            // редакторах: повороты, экспорт, копирование, удаление.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.panel_menu = Some(PanelMenu::Page {
                        page,
                        at: event.position,
                    });
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            // Бейдж «номер из всех» над карточкой.
            .child(
                div()
                    .px_2()
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{}/{}", page + 1, total)),
            )
            .child(
                div()
                    .w(px(width))
                    .h(px(height))
                    .bg(white())
                    .rounded_sm()
                    .shadow_sm()
                    .overflow_hidden()
                    // Рамка есть всегда, но у невыбранной страницы она
                    // прозрачная: так лист не дёргается на пару пикселей в
                    // момент выбора, а глазу видна только сама страница.
                    .border_2()
                    .border_color(if is_current {
                        accent_blue()
                    } else {
                        gpui::transparent_black()
                    })
                    .when_some(texture, |el, tex| {
                        el.child(img(tex).w(px(width)).h(px(height)))
                    }),
            )
            // Разделитель-вставка: синяя линия с плюсом между страницами.
            // В покое её не видно — она проявляется под курсором, чтобы не
            // рябить в глазах на длинной ленте. Клик открывает меню
            // «Пустой лист / Выбрать документ».
            .child({
                let accent = accent_blue();
                let line = || div().flex_1().h(px(2.0)).rounded_full().bg(accent);
                h_flex()
                    .id(("divider", ix))
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(18.0))
                    // В покое разделителя не видно совсем; под курсором он
                    // проявляется целиком — линия, кружок и плюс разом.
                    // Ховер висит на самом разделителе: групповой ховер
                    // с прозрачностью в gpui не срабатывал.
                    .opacity(0.0)
                    .hover(|el| el.opacity(1.0))
                    .child(line())
                    .child(
                        div()
                            .id(("insert-plus", ix))
                            .w(px(18.0))
                            .h(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(accent)
                            .text_color(gpui::white())
                            .cursor_pointer()
                            .child(
                                gpui_component::Icon::empty()
                                    .path("icons/add-01.svg")
                                    .size_3()
                                    .text_color(gpui::white()),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.panel_menu = Some(PanelMenu::Insert {
                                        after: page,
                                        at: event.position,
                                        before_first: false,
                                    });
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(line())
            })
            .into_any_element()
    }

    /// Окно «Стили» поверх документа.
    fn render_styles_window(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let window = self.styles_window.as_ref()?;
        let loading = window.loading;
        let muted = cx.theme().muted_foreground;
        let has_selection = self.selected.is_some();

        let rows: Vec<AnyElement> = window
            .defs
            .iter()
            .zip(&window.rows)
            .map(|(def, inputs)| {
                let id = def.id;
                let shown_family = def.family.clone().map(gpui::SharedString::from);
                v_flex()
                    .w_full()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(200.0))
                                    .when_some(shown_family, |el, family| el.font_family(family))
                                    .child(gpui_component::input::Input::new(&inputs.name).small()),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new(("style-bind", id as usize))
                                    .small()
                                    .primary()
                                    .label("Применить к блоку")
                                    .tooltip(if has_selection {
                                        "Привязать выделенный абзац к стилю"
                                    } else {
                                        "Сначала выделите абзац на странице"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.bind_style_to_selection(id, cx)
                                    })),
                            )
                            .child(
                                Button::new(("style-delete", id as usize))
                                    .small()
                                    .ghost()
                                    .icon(gpui_component::Icon::empty().path("icons/delete-02.svg"))
                                    .tooltip("Удалить стиль: блоки останутся, связь порвётся")
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.delete_style(id, cx)
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child("Гарнитура (пусто — как в блоке)"),
                                    )
                                    .child(div().w(px(200.0)).child(
                                        gpui_component::input::Input::new(&inputs.family).small(),
                                    )),
                            )
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(div().text_xs().text_color(muted).child("Кегль, пт"))
                                    .child(div().w(px(70.0)).child(
                                        gpui_component::input::Input::new(&inputs.size).small(),
                                    )),
                            )
                            .child(div().flex_1())
                            .children(
                                [
                                    ("Ж", def.bold, 0u8),
                                    ("К", def.italic, 1),
                                    ("Ч", def.underline, 2),
                                ]
                                .map(|(title, on, kind)| {
                                    Button::new(("style-flag", (id as usize) * 4 + kind as usize))
                                        .small()
                                        .when(on, |b| b.primary())
                                        .when(!on, |b| b.ghost())
                                        .label(title)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.change_style(id, cx, move |def| match kind {
                                                0 => def.bold = !def.bold,
                                                1 => def.italic = !def.italic,
                                                _ => def.underline = !def.underline,
                                            })
                                        }))
                                }),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(div().text_xs().text_color(muted).child("Цвет:"))
                            .children(crate::properties::STYLE_SWATCHES.iter().enumerate().map(
                                |(index, value)| {
                                    let value = *value;
                                    let active =
                                        def.color == Some(crate::properties::unpack_rgb(value));
                                    div()
                                        .id(("style-swatch", (id as usize) * 16 + index))
                                        .w(px(16.0))
                                        .h(px(16.0))
                                        .rounded_sm()
                                        .bg(gpui::rgb(value))
                                        .border_2()
                                        .border_color(if active {
                                            cx.theme().primary
                                        } else {
                                            cx.theme().border
                                        })
                                        .cursor_pointer()
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                                this.change_style(id, cx, move |def| {
                                                    def.color =
                                                        Some(crate::properties::unpack_rgb(value));
                                                });
                                                cx.stop_propagation();
                                            }),
                                        )
                                },
                            )),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .absolute()
                .inset_0()
                .bg(gpui::black().opacity(0.55))
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.styles_window = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    v_flex()
                        .absolute()
                        .left(gpui::relative(0.5))
                        .top(px(70.0))
                        .ml(px(-330.0))
                        .w(px(660.0))
                        .max_h(px(640.0))
                        .rounded_lg()
                        .bg(cx.theme().background)
                        .border_1()
                        .border_color(cx.theme().border)
                        .shadow_lg()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .justify_between()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(div().text_sm().child("Стили документа"))
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new("styles-create")
                                                .small()
                                                .primary()
                                                .label("Создать стиль")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.create_style(cx)
                                                })),
                                        )
                                        .child(
                                            Button::new("styles-close")
                                                .small()
                                                .ghost()
                                                .label("Закрыть")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.styles_window = None;
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        )
                        .child(
                            v_flex()
                                .id("styles-list")
                                .flex_1()
                                .min_h(px(0.0))
                                .overflow_y_scroll()
                                .when(loading, |el| {
                                    el.child(
                                        div()
                                            .px_3()
                                            .py_3()
                                            .text_sm()
                                            .text_color(muted)
                                            .child("Читаю каталог стилей…"),
                                    )
                                })
                                .when(!loading && rows.is_empty(), |el| {
                                    el.child(
                                        div().px_3().py_3().text_sm().text_color(muted).child(
                                            "Стилей пока нет. Создайте первый и привяжите к                                              нему блоки — правка стиля будет перенабирать их                                              все разом.",
                                        ),
                                    )
                                })
                                .children(rows),
                        )
                        .child(
                            div().px_3().py_2().text_xs().text_color(muted).child(
                                "Правка полей стиля сразу перенабирает все его блоки.                                  Ctrl+Z откатывает и правку, и каскад.",
                            ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Окно «Шрифты» поверх документа.
    fn render_fonts_window(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let window = self.fonts_window.as_ref()?;
        let loading = window.loading;
        let fonts = window.fonts.clone();
        let muted = cx.theme().muted_foreground;
        let warn = gpui_component::amber_500();

        let rows: Vec<AnyElement> = fonts
            .iter()
            .map(|font| {
                // Предупреждение — когда гарнитуры нет в системе. Даже
                // вшитое подмножество несёт лишь те знаки, что уже стояли на
                // странице: наберёшь новую букву — и её нечем нарисовать.
                // Поставленный в систему шрифт снимает это ограничение.
                let missing = font.system_family.is_none();
                let name = font.base_font.clone();
                let manual = self.substitute_for(&name).cloned();
                // Без ручной замены отсутствующему шрифту подбирается
                // автоматическая — и стрелка честно показывает, чем на деле
                // будет набираться правка.
                let auto = (manual.is_none() && missing)
                    .then(|| pdfcore::fonts::suggest_substitute(&name))
                    .flatten();
                let is_auto = manual.is_none() && auto.is_some();
                let substitute = manual.clone().or(auto);
                // Имя гарнитуры пишется ею же — так его узнают в лицо, а не
                // по буквам. Если в системе шрифта нет, писать нечем: остаётся
                // шрифт интерфейса.
                let shown_in = font.system_family.clone().map(gpui::SharedString::from);

                let mut marks: Vec<String> = Vec::new();
                marks.push(if font.embedded {
                    if font.subset {
                        "подмножество".into()
                    } else {
                        "вшит целиком".into()
                    }
                } else {
                    "не вшит".into()
                });
                if let Some(system) = font.system_family.as_ref() {
                    marks.push(format!("в системе: {system}"));
                } else {
                    marks.push("в системе нет".into());
                }
                if !font.subtype.is_empty() {
                    marks.push(font.subtype.clone());
                }
                marks.push(format!("{} стр.", font.pages));

                let for_menu = name.clone();
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().w(px(18.0)).child(if missing {
                        gpui_component::Icon::empty()
                            .path("icons/alert-02.svg")
                            .size_4()
                            .text_color(warn)
                            .into_any_element()
                    } else {
                        div().into_any_element()
                    }))
                    .child(
                        // Колонка ужимается: длинная подпись не должна
                        // растягивать строку и выталкивать кнопки за край окна.
                        v_flex()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_sm()
                                            .when_some(shown_in, |el, family| {
                                                el.font_family(family)
                                            })
                                            .child(name.clone()),
                                    )
                                    // Стрелка показывает, чем шрифт будет
                                    // набираться на деле, — и сама замена
                                    // написана собственной гарнитурой.
                                    .when_some(substitute.clone(), |el, chosen| {
                                        el.child(div().text_sm().text_color(muted).child("→"))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_family(gpui::SharedString::from(
                                                        chosen.clone(),
                                                    ))
                                                    .child(chosen),
                                            )
                                            .when(is_auto, |el| {
                                                el.child(
                                                    div().text_xs().text_color(muted).child("авто"),
                                                )
                                            })
                                    }),
                            )
                            .child(div().text_xs().text_color(muted).child(marks.join(" · ")))
                            .when(missing, |el| {
                                el.child(div().text_xs().text_color(warn).child(if font.embedded {
                                    "поставьте шрифт в систему — иначе новых знаков не набрать"
                                } else {
                                    "поставьте шрифт в систему — набирать нечем"
                                }))
                            }),
                    )
                    .child(
                        Button::new(gpui::SharedString::from(format!("replace-{name}")))
                            .flex_shrink_0()
                            .small()
                            .ghost()
                            .label("Заменить")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                // Тот же список, что и в панели свойств:
                                // семейства с начертаниями внутри и поиском.
                                this.toggle_font_picker(
                                    FontTarget::Substitute(for_menu.clone()),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new(gpui::SharedString::from(format!("load-{name}")))
                            .small()
                            .ghost()
                            .label("Загрузить")
                            .tooltip("Установка шрифта из файла появится позже"),
                    )
                    .into_any_element()
            })
            .collect();

        // Список системных гарнитур для замены. Он показывается вместо описи:
        // так его видно целиком, а не в остатке высоты под длинным списком.
        Some(
            // Затемнение позади окна: щелчок мимо закрывает.
            div()
                .absolute()
                .inset_0()
                .bg(gpui::black().opacity(0.55))
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.fonts_window = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    v_flex()
                        .absolute()
                        .left(gpui::relative(0.5))
                        .top(px(80.0))
                        .ml(px(-320.0))
                        .w(px(640.0))
                        .max_h(px(620.0))
                        .rounded_lg()
                        .bg(cx.theme().background)
                        .border_1()
                        .border_color(cx.theme().border)
                        .shadow_lg()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .justify_between()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(div().text_sm().child("Шрифты документа"))
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new("fonts-refresh")
                                                .small()
                                                .ghost()
                                                .label("Обновить")
                                                .tooltip("Перечитать систему и документ")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.rescan_fonts(cx)
                                                })),
                                        )
                                        .child(
                                            Button::new("fonts-close")
                                                .small()
                                                .ghost()
                                                .label("Закрыть")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.fonts_window = None;
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        )
                        .child(
                            v_flex()
                                .id("fonts-list")
                                .flex_1()
                                .min_h(px(0.0))
                                .overflow_y_scroll()
                                .when(loading, |el| {
                                    el.child(
                                        div()
                                            .px_3()
                                            .py_3()
                                            .text_sm()
                                            .text_color(muted)
                                            .child("Читаю шрифты документа…"),
                                    )
                                })
                                .when(!loading && rows.is_empty(), |el| {
                                    el.child(
                                        div()
                                            .px_3()
                                            .py_3()
                                            .text_sm()
                                            .text_color(muted)
                                            .child("В документе нет текстовых шрифтов"),
                                    )
                                })
                                .children(rows),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Меню панели миниатюр: карточка с пунктами на позиции курсора.
    fn render_panel_menu(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.panel_menu.as_ref()?;

        // Пункт меню: иконка, подпись и действие. Удаление красное — как во
        // всех настольных редакторах, где эта строка отделена от прочих.
        struct Item {
            id: &'static str,
            icon: &'static str,
            title: String,
            enabled: bool,
            danger: bool,
            action: std::rc::Rc<dyn Fn(&mut Viewer, &mut Context<Viewer>)>,
        }

        // Сколько страниц выбрано в сетке: от этого зависят подписи пунктов.
        let picked = self
            .organise
            .as_ref()
            .map(|organise| organise.picked.len())
            .unwrap_or(0);
        let buffered = self.clipboard_pages().len();

        let (at, items): (Point<Pixels>, Vec<Item>) = match menu {
            PanelMenu::Page { page, at } => {
                let page = *page;
                (
                    *at,
                    vec![
                        Item {
                            id: "menu-rot-r",
                            icon: "icons/rotate-right-01.svg",
                            title: "Повернуть вправо".to_owned(),
                            enabled: true,
                            danger: false,
                            action: std::rc::Rc::new(move |this, cx| {
                                this.page_op(
                                    pdfcore::PageOp::Rotate {
                                        page_number: page + 1,
                                        clockwise: true,
                                    },
                                    cx,
                                )
                            }),
                        },
                        Item {
                            id: "menu-rot-l",
                            icon: "icons/rotate-left-01.svg",
                            title: "Повернуть влево".to_owned(),
                            enabled: true,
                            danger: false,
                            action: std::rc::Rc::new(move |this, cx| {
                                this.page_op(
                                    pdfcore::PageOp::Rotate {
                                        page_number: page + 1,
                                        clockwise: false,
                                    },
                                    cx,
                                )
                            }),
                        },
                        Item {
                            id: "menu-export",
                            icon: "icons/file-export.svg",
                            title: "Экспорт страницы".to_owned(),
                            enabled: true,
                            danger: false,
                            action: std::rc::Rc::new(move |this, cx| this.export_page(page, cx)),
                        },
                        Item {
                            id: "menu-copy",
                            icon: "icons/copy-01.svg",
                            title: if picked > 1 {
                                "Копировать выбранные".to_owned()
                            } else {
                                "Копировать".to_owned()
                            },
                            enabled: true,
                            danger: false,
                            action: std::rc::Rc::new(move |this, cx| {
                                // В сетке копируется весь выбранный набор, в
                                // панели миниатюр — та страница, по которой
                                // щёлкнули.
                                if this.organise.is_some() {
                                    this.organise_copy(cx);
                                } else {
                                    this.copied_page = Some(page);
                                    this.set_status(
                                        format!("Страница {} скопирована", page + 1),
                                        cx,
                                    );
                                }
                            }),
                        },
                        Item {
                            id: "menu-paste",
                            icon: "icons/clipboard.svg",
                            title: "Вставить".to_owned(),
                            enabled: !self.clipboard_pages().is_empty(),
                            danger: false,
                            action: std::rc::Rc::new(move |this, cx| {
                                this.paste_pages_after(page + 1, cx)
                            }),
                        },
                        Item {
                            id: "menu-delete",
                            icon: "icons/delete-02.svg",
                            title: "Удалить".to_owned(),
                            enabled: true,
                            danger: true,
                            action: std::rc::Rc::new(move |this, cx| {
                                this.page_op(
                                    pdfcore::PageOp::Delete {
                                        page_number: page + 1,
                                    },
                                    cx,
                                )
                            }),
                        },
                    ],
                )
            }
            PanelMenu::TextOps { at } => {
                let has_selection = !self.multi.is_empty() || self.selected.is_some();
                let has_clipboard = !clipboard_is_empty();
                (
                    *at,
                    vec![
                        Item {
                            id: "menu-copy-blocks",
                            icon: "icons/copy-01.svg",
                            title: "Копировать".to_owned(),
                            enabled: has_selection,
                            danger: false,
                            action: std::rc::Rc::new(|this, cx| this.copy_blocks(cx)),
                        },
                        Item {
                            id: "menu-paste-blocks",
                            icon: "icons/clipboard.svg",
                            title: "Вставить".to_owned(),
                            enabled: has_clipboard,
                            danger: false,
                            action: std::rc::Rc::new(|this, cx| this.paste_blocks(cx)),
                        },
                        Item {
                            id: "menu-delete-blocks",
                            icon: "icons/delete-02.svg",
                            title: "Удалить".to_owned(),
                            enabled: has_selection,
                            danger: true,
                            action: std::rc::Rc::new(|this, cx| {
                                if this.multi.is_empty() {
                                    this.erase_selected(cx);
                                } else {
                                    this.delete_multi(cx);
                                }
                            }),
                        },
                    ],
                )
            }
            PanelMenu::Insert {
                after,
                at,
                before_first,
            } => {
                // Ноль для движка означает «перед первой страницей».
                let after = if *before_first { 0 } else { *after + 1 };
                (
                    *at,
                    vec![
                        Item {
                            id: "menu-blank",
                            icon: "icons/add-01.svg",
                            title: "Пустой лист".to_owned(),
                            enabled: true,
                            danger: false,
                            action: std::rc::Rc::new(move |this, cx| {
                                this.page_op(
                                    pdfcore::PageOp::InsertAfter { page_number: after },
                                    cx,
                                )
                            }),
                        },
                        Item {
                            id: "menu-paste-slot",
                            icon: "icons/clipboard.svg",
                            title: match buffered {
                                0 => "Вставить".to_owned(),
                                1 => "Вставить страницу".to_owned(),
                                n => format!("Вставить страниц: {n}"),
                            },
                            enabled: buffered > 0,
                            danger: false,
                            action: std::rc::Rc::new(move |this, cx| {
                                this.paste_pages_after(after, cx)
                            }),
                        },
                        Item {
                            id: "menu-doc",
                            icon: "icons/file-export.svg",
                            title: "Выбрать документ…".to_owned(),
                            enabled: true,
                            danger: false,
                            action: std::rc::Rc::new(move |this, cx| {
                                this.insert_document_at(after, cx)
                            }),
                        },
                    ],
                )
            }
        };

        // Меню целиком в видимой области: у нижних страниц оно иначе уезжало
        // бы за край окна вместе с половиной пунктов.
        let height = px(items.len() as f32 * 32.0 + 10.0);
        let left =
            at.x.min(viewport.width - px(MENU_WIDTH) - px(8.0))
                .max(px(0.0));
        let top = at.y.min(viewport.height - height - px(8.0)).max(px(0.0));

        // Тёмная тема даёт `danger` приглушённым — на чёрной карточке он
        // читается как выключенный пункт. Берём открытый красный.
        let danger_color = gpui_component::red_500();
        let muted = cx.theme().muted_foreground;
        let hover = cx.theme().accent;
        let foreground = cx.theme().foreground;
        let rows: Vec<AnyElement> = items
            .into_iter()
            .map(|item| {
                let color = if !item.enabled {
                    muted
                } else if item.danger {
                    danger_color
                } else {
                    foreground
                };
                let action = item.action.clone();
                h_flex()
                    .id(item.id)
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .text_sm()
                    .text_color(color)
                    .child(
                        gpui_component::Icon::empty()
                            .path(item.icon)
                            .size_4()
                            .text_color(color),
                    )
                    .child(item.title)
                    .when(item.enabled, |el| {
                        el.cursor_pointer().hover(|el| el.bg(hover)).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                this.panel_menu = None;
                                action(this, cx);
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        // Подложка во весь экран: щелчок мимо меню закрывает его, как в любом
        // настольном редакторе. Без неё меню оставалось висеть, пока не
        // выберешь пункт.
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .w(viewport.width)
                .h(viewport.height)
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.panel_menu = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.panel_menu = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .left(left)
                        .top(top)
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .child(
                            v_flex()
                                .min_w(px(MENU_WIDTH))
                                .p_1()
                                .gap_0p5()
                                .rounded_lg()
                                .bg(cx.theme().popover)
                                .border_1()
                                .border_color(cx.theme().border)
                                .shadow_lg()
                                .children(rows),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Открывает и закрывает режим «Систематизировать».
    pub(crate) fn toggle_organise(&mut self, cx: &mut Context<Self>) {
        // И вход, и выход возвращают клавиатуру виду: иначе Ctrl+C и Delete
        // уходят в поле, из которого только что вышли.
        self.focus_root = true;
        if self.organise.take().is_none() {
            // Правка блока и сетка страниц несовместимы: сетка показывает
            // документ целиком, и незакрытая правка повисла бы в воздухе.
            self.commit_or_clear(cx);
            self.multi.clear();
            self.organise = Some(Organise::default());
        }
        cx.notify();
    }

    /// Страницы, лежащие в буфере: в сетке это её набор, в панели миниатюр —
    /// одна скопированная страница. Нумерация с нуля.
    fn clipboard_pages(&self) -> Vec<u32> {
        match self.organise.as_ref() {
            Some(organise) if !organise.clipboard.is_empty() => organise.clipboard.clone(),
            _ => self.copied_page.into_iter().collect(),
        }
    }

    /// Вставляет буфер после страницы `after` — нумерация с единицы, ноль
    /// означает «в самое начало документа».
    fn paste_pages_after(&mut self, after: u32, cx: &mut Context<Self>) {
        let sources = self.clipboard_pages();
        if sources.is_empty() {
            self.set_status("Буфер страниц пуст", cx);
            return;
        }
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        // Копии ложатся друг за другом: каждая следующая — на позицию правее.
        for (shift, source) in sources.iter().enumerate() {
            doc.renderer.apply_page_op(pdfcore::PageOp::Duplicate {
                source: source + 1,
                after: after + shift as u32,
            });
        }
        self.set_status(
            if sources.len() == 1 {
                "Страница вставлена".to_owned()
            } else {
                format!("Вставлено страниц: {}", sources.len())
            },
            cx,
        );
        cx.notify();
    }

    /// Щелчок по странице в сетке.
    ///
    /// Ctrl (или включённый режим «несколько страниц») — добавить или убрать
    /// одну; Shift — весь ряд от предыдущей выбранной до этой; без клавиш —
    /// выбрать только её.
    fn organise_pick(&mut self, page: u32, add: bool, range: bool, cx: &mut Context<Self>) {
        let Some(organise) = self.organise.as_mut() else {
            return;
        };
        if range && let Some(from) = organise.anchor {
            let (lo, hi) = if from <= page {
                (from, page)
            } else {
                (page, from)
            };
            organise.picked = (lo..=hi).collect();
            cx.notify();
            return;
        }
        let together = add || organise.multi;
        if together {
            match organise.picked.iter().position(|p| *p == page) {
                Some(at) => {
                    organise.picked.remove(at);
                }
                None => organise.picked.push(page),
            }
        } else {
            organise.picked = vec![page];
        }
        organise.anchor = Some(page);
        cx.notify();
    }

    /// Копирование выбранных страниц: запоминаются только их номера, сами
    /// страницы берутся из документа в момент вставки.
    fn organise_copy(&mut self, cx: &mut Context<Self>) {
        let Some(organise) = self.organise.as_mut() else {
            return;
        };
        let mut pages = organise.picked.clone();
        pages.sort_unstable();
        if pages.is_empty() {
            return;
        }
        organise.clipboard = pages.clone();
        self.set_status(
            format!("Скопировано страниц: {} — Ctrl+V вставит", pages.len()),
            cx,
        );
        cx.notify();
    }

    /// Вставка скопированных страниц следом за выбранной.
    fn organise_paste(&mut self, cx: &mut Context<Self>) {
        let Some(organise) = self.organise.as_ref() else {
            return;
        };
        // Вставляем после последней выбранной; без выбора — в самый конец.
        let after = organise
            .picked
            .iter()
            .copied()
            .max()
            .map(|page| page + 1)
            .or_else(|| self.doc.as_ref().map(|doc| doc.info.page_count))
            .unwrap_or(0);
        self.paste_pages_after(after, cx);
    }

    /// Страницы, к которым относится действие: выбранные или та, на которой
    /// нажали кнопку.
    fn organise_targets(&self, page: u32) -> Vec<u32> {
        match self.organise.as_ref() {
            Some(organise) if organise.picked.contains(&page) => {
                let mut all = organise.picked.clone();
                all.sort_unstable();
                all
            }
            _ => vec![page],
        }
    }

    /// Поворот выбранных страниц одной пачкой заданий.
    fn organise_rotate(&mut self, page: u32, clockwise: bool, cx: &mut Context<Self>) {
        let targets = self.organise_targets(page);
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        for target in targets {
            doc.renderer.apply_page_op(pdfcore::PageOp::Rotate {
                page_number: target + 1,
                clockwise,
            });
        }
        cx.notify();
    }

    /// Удаление выбранных страниц. Идём с конца: удаление сдвигает номера.
    fn organise_delete(&mut self, page: u32, cx: &mut Context<Self>) {
        let mut targets = self.organise_targets(page);
        targets.sort_unstable_by(|a, b| b.cmp(a));
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        for target in targets {
            doc.renderer.apply_page_op(pdfcore::PageOp::Delete {
                page_number: target + 1,
            });
        }
        if let Some(organise) = self.organise.as_mut() {
            organise.picked.clear();
        }
        self.set_status("Страницы удалены — Ctrl+Z вернёт", cx);
        cx.notify();
    }

    /// Отпускание перетаскиваемой страницы в щель `slot`.
    fn organise_drop(&mut self, slot: u32, cx: &mut Context<Self>) {
        let Some(organise) = self.organise.as_mut() else {
            return;
        };
        let Some(from) = organise.dragging.take() else {
            organise.drop_at = None;
            return;
        };
        organise.drop_at = None;
        organise.press = None;
        // Несут весь выбранный набор, если взялись за одну из выбранных.
        let mut moving = organise.picked.clone();
        if !moving.contains(&from) {
            moving = vec![from];
        }
        moving.sort_unstable();
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let count = doc.info.page_count;

        // Перестановка считается на модели списка: движок переставляет по
        // одной странице, и каждый перенос сдвигает номера остальных.
        let mut order: Vec<u32> = (0..count).collect();
        let base = order
            .iter()
            .take(slot.min(count) as usize)
            .filter(|page| !moving.contains(page))
            .count();
        let mut ops = Vec::new();
        for (step, page) in moving.iter().enumerate() {
            let Some(at) = order.iter().position(|p| p == page) else {
                continue;
            };
            let target = (base + step).min(order.len() - 1);
            if at == target {
                continue;
            }
            ops.push(pdfcore::PageOp::Move {
                from: at as u32 + 1,
                to: target as u32 + 1,
            });
            let moved = order.remove(at);
            order.insert(target, moved);
        }
        // Выбор переезжает вместе со страницами на их новые места.
        organise.picked = (base..base + moving.len())
            .map(|at| at.min(count.saturating_sub(1) as usize) as u32)
            .collect();
        organise.anchor = organise.picked.first().copied();
        if ops.is_empty() {
            cx.notify();
            return;
        }
        let moved = ops.len();
        for op in ops {
            doc.renderer.apply_page_op(op);
        }
        self.set_status(
            if moved == 1 {
                "Страница переставлена".to_owned()
            } else {
                format!("Переставлено страниц: {moved}")
            },
            cx,
        );
        cx.notify();
    }

    /// Применяет готовое значение из списка пресетов.
    fn apply_preset(
        &mut self,
        kind: PresetKind,
        target: PresetTarget,
        value: f32,
        cx: &mut Context<Self>,
    ) {
        self.preset_menu = None;
        match (target, kind) {
            (PresetTarget::Editor, PresetKind::CharSpacing) => self.set_char_spacing(value, cx),
            (PresetTarget::Editor, PresetKind::HScale) => self.set_h_scale(value, cx),
            (PresetTarget::Editor, PresetKind::LineHeight) => {
                // Пресеты интерлиньяжа — множители кегля, как в настольных
                // редакторах: 1.2 значит «120 % кегля».
                let size = self
                    .selected
                    .as_ref()
                    .map(|selection| {
                        selection
                            .page_font
                            .as_ref()
                            .map(|font| font.size)
                            .unwrap_or(selection.style.size)
                    })
                    .unwrap_or(12.0);
                self.set_line_height(size * value, cx);
            }
            (PresetTarget::Multi, PresetKind::CharSpacing) => {
                self.multi_set_char_spacing(value, cx)
            }
            (PresetTarget::Multi, PresetKind::HScale) => self.multi_set_h_scale(value, cx),
            (PresetTarget::Multi, PresetKind::LineHeight) => {
                self.multi_set_leading_factor(value, cx)
            }
        }
        cx.notify();
    }

    /// Список готовых значений у числового поля: поле остаётся полем — сюда
    /// выносятся типовые значения, чтобы не набирать их руками.
    fn render_preset_menu(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.preset_menu.as_ref()?;
        let kind = menu.kind;
        let target = menu.target;
        let values: &[f32] = match kind {
            PresetKind::CharSpacing => &[
                -1.0, -0.75, -0.5, -0.25, -0.1, 0.0, 0.1, 0.25, 0.5, 0.75, 1.0, 2.0,
            ],
            PresetKind::HScale => &[80.0, 85.0, 90.0, 95.0, 100.0, 105.0, 110.0, 115.0, 120.0],
            PresetKind::LineHeight => &[1.0, 1.2, 1.4, 1.5, 2.0, 2.5, 3.0],
        };
        let shown = |value: f32| -> String {
            match kind {
                PresetKind::CharSpacing => format!("{value:.2}"),
                PresetKind::HScale => format!("{value:.0}"),
                PresetKind::LineHeight => format!("{value:.2}"),
            }
        };

        let height = values.len() as f32 * 26.0 + 10.0;
        let left = menu.at.x.min(viewport.width - px(96.0)).max(px(0.0));
        let top = menu
            .at
            .y
            .min(viewport.height - px(height) - px(8.0))
            .max(px(0.0));

        let rows: Vec<AnyElement> = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let value = *value;
                div()
                    .id(("preset", index))
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .text_sm()
                    .cursor_pointer()
                    .hover(|el| el.bg(cx.theme().accent))
                    .child(shown(value))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.apply_preset(kind, target, value, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.preset_menu = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    v_flex()
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(px(88.0))
                        .p_1()
                        .gap_0p5()
                        .rounded_md()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .shadow_lg()
                        .occlude()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    /// Открывает и закрывает список выбора шрифта.
    pub(crate) fn toggle_font_picker(
        &mut self,
        target: FontTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .font_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target)
        {
            // Повторное нажатие той же кнопки закрывает список.
            self.font_picker = None;
            cx.notify();
            return;
        }
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Поиск шрифта…"));
        // Набор в поиске перерисовывает список на каждом знаке.
        cx.observe(&query, |_, _, cx| cx.notify()).detach();
        // Клавиатура сразу в поиск: иначе набор уйдёт в правимый блок.
        window.focus(&query.read(cx).focus_handle(cx));
        self.font_picker = Some(FontPickerUi {
            target,
            query,
            expanded: HashSet::new(),
            scroll: gpui::ScrollHandle::new(),
        });
        cx.notify();
    }

    /// Выбор в пикере: семейство целиком либо конкретное начертание.
    fn pick_font(
        &mut self,
        family: String,
        face: Option<pdfcore::FaceInfo>,
        cx: &mut Context<Self>,
    ) {
        let target = match self.font_picker.as_ref() {
            Some(picker) => picker.target.clone(),
            None => return,
        };
        self.font_picker = None;
        // Выбранное всплывает в шапку недавних; больше четырёх не держим.
        self.recent_fonts.retain(|name| name != &family);
        self.recent_fonts.insert(0, family.clone());
        self.recent_fonts.truncate(4);
        match target {
            FontTarget::Editor => {
                self.chosen_family = Some(family.clone());
                self.chosen_face = face.as_ref().map(|f| f.style.clone());
                // Точное начертание тянет за собой флаги: выбрал «Bold
                // Italic» — кнопки Ж и К загораются сами.
                if let Some(face) = face {
                    let (bold, italic) = (face.bold, face.italic);
                    if let Some(widgets) = self.widgets.as_ref() {
                        widgets.editor.update(cx, |editor, cx| {
                            editor.restyle(
                                move |style| {
                                    style.bold = bold;
                                    style.italic = italic;
                                },
                                cx,
                            );
                        });
                    }
                }
                if let Some(selection) = self.selected.as_mut() {
                    selection.touched = true;
                }
                self.request_preview(cx);
            }
            FontTarget::Substitute(document_font) => {
                // Начертание в замене не участвует: подменяется гарнитура
                // целиком, а начертание каждый кусок несёт своё.
                self.substitute_font(&document_font, family, cx);
            }
            FontTarget::Multi => {
                self.multi_family = Some(family.clone());
                let face_style = face.clone();
                self.multi_restyle(
                    move |style| {
                        style.family = Some(family.clone());
                        if let Some(face) = &face_style {
                            style.bold = face.bold;
                            style.italic = face.italic;
                        }
                    },
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Список шрифтов поверх окна: семейства с раскрывающимися начертаниями.
    fn render_font_picker(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let picker = self.font_picker.as_ref()?;
        let height = (f32::from(viewport.height) - 160.0).max(240.0);
        // Для подмены карточка встаёт по центру: окно «Шрифты» само по
        // центру, и список у правого края улетал бы от него.
        let centred = matches!(picker.target, FontTarget::Substitute(_));
        let query = picker.query.read(cx).value().trim().to_lowercase();
        let expanded = picker.expanded.clone();
        let scroll = picker.scroll.clone();
        let query_state = picker.query.clone();

        let fonts = pdfcore::system_fonts();
        let families: Vec<String> = fonts
            .families()
            .into_iter()
            .filter(|family| query.is_empty() || family.to_lowercase().contains(&query))
            .collect();

        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let hover = cx.theme().accent;

        // Список виртуализирован: в системе сотни семейств, и каждое рисуется
        // собственной гарнитурой. Разложить их все разом — значит грузить и
        // шейпить сотни шрифтов на каждый кадр: прокрутка захлёбывалась.
        // Начертания запрашиваются тоже только у видимых семейств — обход
        // всего индекса на каждое семейство стоил дороже самой отрисовки.
        const ROW_H: f32 = 28.0;
        let opened: Vec<(String, Vec<pdfcore::FaceInfo>)> = families
            .iter()
            .filter(|family| expanded.contains(*family))
            .map(|family| (family.clone(), fonts.faces_of(family)))
            .collect();
        let row_count: usize =
            families.len() + opened.iter().map(|(_, faces)| faces.len()).sum::<usize>();

        // Шапка недавних прокручивается вместе со списком и сдвигает его
        // начало вниз; расчёт видимых рядов ведётся от места после неё.
        let header_px = if query.is_empty() && !self.recent_fonts.is_empty() {
            self.recent_fonts.len() as f32 * ROW_H + 9.0
        } else {
            0.0
        };
        let offset = (-f32::from(scroll.offset().y) - header_px).max(0.0);
        let list_height = {
            let measured = f32::from(scroll.bounds().size.height);
            // Пока список не разложен ни разу, его высота нулевая — берём
            // высоту карточки, чтобы первый кадр не остался пустым.
            if measured > 1.0 { measured } else { height }
        };
        let first_row = ((offset / ROW_H).floor() as usize).saturating_sub(4);
        let visible_rows = (list_height / ROW_H).ceil() as usize + 8;
        let last_row = (first_row + visible_rows).min(row_count);

        let mut rows: Vec<AnyElement> = Vec::new();

        // Шапка недавних: три-четыре последних выбора и разделитель. Только
        // без поиска — запрос сужает список, и дубли сверху лишь мешали бы.
        if query.is_empty() && !self.recent_fonts.is_empty() {
            let recents: Vec<String> = self
                .recent_fonts
                .iter()
                .filter(|name| fonts.has_family(name))
                .cloned()
                .collect();
            for (index, family) in recents.iter().enumerate() {
                let family_for_pick = family.clone();
                rows.push(
                    h_flex()
                        .id(("font-recent", index))
                        .w_full()
                        .h(px(ROW_H))
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|el| el.bg(hover))
                        .child(div().w(px(18.0)).flex_none())
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .truncate()
                                .font_family(gpui::SharedString::from(family.clone()))
                                .child(family.clone()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                this.pick_font(family_for_pick.clone(), None, cx);
                                cx.stop_propagation();
                            }),
                        )
                        .into_any_element(),
                );
            }
            if !recents.is_empty() {
                rows.push(
                    div()
                        .w_full()
                        .h(px(1.0))
                        .flex_none()
                        .my_1()
                        .bg(border)
                        .into_any_element(),
                );
            }
        }

        let mut flat = 0usize;
        for (index, family) in families.iter().enumerate() {
            let open = expanded.contains(family);
            let faces: &[pdfcore::FaceInfo] = if open {
                opened
                    .iter()
                    .find(|(name, _)| name == family)
                    .map(|(_, faces)| faces.as_slice())
                    .unwrap_or(&[])
            } else {
                &[]
            };

            // Ряд семейства.
            if (first_row..last_row).contains(&flat) {
                let family_for_toggle = family.clone();
                let family_for_pick = family.clone();
                let face_count = if open {
                    faces.len()
                } else {
                    fonts.faces_of(family).len()
                };
                rows.push(
                    h_flex()
                        .id(("font-family", index))
                        .w_full()
                        .h(px(ROW_H))
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|el| el.bg(hover))
                        // Стрелка раскрывает начертания, не выбирая семейства.
                        .child(
                            div()
                                .id(("font-expand", index))
                                .w(px(18.0))
                                .h(px(18.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_sm()
                                .hover(|el| el.bg(border))
                                .child(
                                    gpui_component::Icon::empty()
                                        .path(if open {
                                            "icons/arrow-down-01.svg"
                                        } else {
                                            "icons/arrow-right-01.svg"
                                        })
                                        .size_4()
                                        .text_color(cx.theme().foreground),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                        if let Some(picker) = this.font_picker.as_mut() {
                                            if !picker.expanded.remove(&family_for_toggle) {
                                                picker.expanded.insert(family_for_toggle.clone());
                                            }
                                            cx.notify();
                                        }
                                        cx.stop_propagation();
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .truncate()
                                .font_family(gpui::SharedString::from(family.clone()))
                                .child(family.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{face_count}")),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                this.pick_font(family_for_pick.clone(), None, cx);
                                cx.stop_propagation();
                            }),
                        )
                        .into_any_element(),
                );
            }
            flat += 1;

            for (face_ix, face) in faces.iter().enumerate() {
                if (first_row..last_row).contains(&flat) {
                    let family_for_face = family.clone();
                    let shown_face = face.clone();
                    rows.push(
                        h_flex()
                            .id(("font-face", index * 100 + face_ix))
                            .w_full()
                            .h(px(ROW_H))
                            .flex_none()
                            .items_center()
                            // Отступ вдвое больше стрелки: начертания читаются
                            // как вложенный список, а не как соседние строки.
                            .pl(px(48.0))
                            .pr_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|el| el.bg(hover))
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .truncate()
                                    .font_family(gpui::SharedString::from(family.clone()))
                                    .when(face.bold, |el| el.font_weight(gpui::FontWeight::BOLD))
                                    .when(face.italic, |el| el.italic())
                                    .child(face.style.clone()),
                            )
                            .children(
                                face.variable
                                    .then(|| div().text_xs().text_color(muted).child("VAR")),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    this.pick_font(
                                        family_for_face.clone(),
                                        Some(shown_face.clone()),
                                        cx,
                                    );
                                    cx.stop_propagation();
                                }),
                            )
                            .into_any_element(),
                    );
                }
                flat += 1;
            }

            if flat >= last_row {
                break;
            }
        }

        // Распорки восстанавливают полную длину списка: полоса прокрутки
        // обязана знать про невидимые ряды.
        let above = px(first_row.min(row_count) as f32 * ROW_H);
        let below = px(row_count.saturating_sub(last_row) as f32 * ROW_H);

        // Карточка стоит у правой панели — там живут обе кнопки выбора.
        let width = px(340.0);
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.font_picker = None;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    v_flex()
                        .absolute()
                        .top(px(96.0))
                        .when(centred, |el| el.left(gpui::relative(0.5)).ml(px(-170.0)))
                        .when(!centred, |el| {
                            el.right(px(crate::properties::PANEL_WIDTH + 12.0))
                        })
                        .w(width)
                        .h(px(height))
                        .rounded_lg()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(border)
                        .shadow_lg()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .child(
                            div()
                                .p_2()
                                .border_b_1()
                                .border_color(border)
                                .child(Input::new(&query_state).small()),
                        )
                        .child(
                            v_flex()
                                .id("font-picker-list")
                                .flex_1()
                                .min_h(px(0.0))
                                .p_1()
                                .track_scroll(&scroll)
                                .overflow_y_scroll()
                                .child(div().h(above).flex_none())
                                .children(rows)
                                .child(div().h(below).flex_none()),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Открывает окно «Стили» и просит каталог у движка.
    pub(crate) fn open_styles(&mut self, cx: &mut Context<Self>) {
        self.styles_window = Some(StylesWindow {
            defs: Vec::new(),
            loading: true,
            rows: Vec::new(),
        });
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.request_styles();
        }
        cx.notify();
    }

    /// Пересобирает поля рядов окна стилей под свежий каталог.
    fn sync_styles_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(styles) = self.styles_window.as_ref() else {
            return;
        };
        if styles.rows.len() == styles.defs.len()
            && styles
                .rows
                .iter()
                .zip(&styles.defs)
                .all(|(row, def)| row.id == def.id)
        {
            return;
        }
        let defs = styles.defs.clone();
        let mut rows = Vec::with_capacity(defs.len());
        for def in &defs {
            let seed = |value: String, window: &mut Window, cx: &mut Context<Viewer>| {
                cx.new(|cx| {
                    let mut state = InputState::new(window, cx);
                    state.set_value(value, window, cx);
                    state
                })
            };
            let row = StyleRowInputs {
                id: def.id,
                name: seed(def.name.clone(), window, cx),
                family: seed(def.family.clone().unwrap_or_default(), window, cx),
                size: seed(
                    def.size.map(|v| format!("{v:.1}")).unwrap_or_default(),
                    window,
                    cx,
                ),
            };
            let id = def.id;
            let name_input = row.name.clone();
            cx.subscribe(&row.name, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let value = name_input.read(cx).value().trim().to_owned();
                    this.rename_style(id, value, cx);
                }
            })
            .detach();
            let family_input = row.family.clone();
            cx.subscribe(&row.family, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let value = family_input.read(cx).value().trim().to_owned();
                    this.change_style(id, cx, move |def| {
                        def.family = (!value.is_empty()).then_some(value.clone());
                    });
                }
            })
            .detach();
            let size_input = row.size.clone();
            cx.subscribe(&row.size, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let parsed = size_input
                        .read(cx)
                        .value()
                        .trim()
                        .replace(',', ".")
                        .parse::<f32>()
                        .ok();
                    this.change_style(id, cx, move |def| def.size = parsed);
                }
            })
            .detach();
            rows.push(row);
        }
        if let Some(styles) = self.styles_window.as_mut() {
            styles.rows = rows;
        }
    }

    /// Переименование не трогает блоки — только каталог.
    fn rename_style(&mut self, id: i64, name: String, _cx: &mut Context<Self>) {
        let Some(window) = self.styles_window.as_ref() else {
            return;
        };
        if name.is_empty() || window.defs.iter().any(|d| d.id == id && d.name == name) {
            return;
        }
        let mut defs = window.defs.clone();
        if let Some(def) = defs.iter_mut().find(|d| d.id == id) {
            def.name = name;
        }
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.save_styles(defs);
        }
    }

    /// Правка спека стиля: каскадом перенабираются все его блоки.
    pub(crate) fn change_style(
        &mut self,
        id: i64,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut pdfcore::StyleDef),
    ) {
        // Каталог из документа: правка стиля доступна и из панели свойств,
        // когда окно стилей закрыто.
        let def = self
            .styles_window
            .as_ref()
            .and_then(|window| window.defs.iter().find(|d| d.id == id))
            .or_else(|| {
                self.doc
                    .as_ref()
                    .and_then(|doc| doc.styles.iter().find(|d| d.id == id))
            })
            .cloned();
        let Some(mut def) = def else {
            return;
        };
        change(&mut def);
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.apply_style(def);
            self.set_status("Применяю стиль ко всем его блокам…", cx);
        }
    }

    /// Удаление стиля рвёт связь: блоки остаются как есть.
    pub(crate) fn delete_style(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(window) = self.styles_window.as_ref() else {
            return;
        };
        let defs: Vec<_> = window.defs.iter().filter(|d| d.id != id).cloned().collect();
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.save_styles(defs);
        }
        cx.notify();
    }

    /// Новый пустой стиль в конец каталога.
    pub(crate) fn create_style(&mut self, cx: &mut Context<Self>) {
        let Some(window) = self.styles_window.as_ref() else {
            return;
        };
        let mut defs = window.defs.clone();
        defs.push(pdfcore::StyleDef::fresh(&defs));
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.save_styles(defs);
        }
        cx.notify();
    }

    /// Привязывает выделенный блок к стилю и применяет спек к его тексту.
    pub(crate) fn bind_style_to_selection(&mut self, id: i64, cx: &mut Context<Self>) {
        // Каталог берётся из документа: привязать стиль можно и из панели
        // свойств, когда окно «Стили» закрыто.
        let def = self
            .styles_window
            .as_ref()
            .and_then(|w| w.defs.iter().find(|d| d.id == id))
            .or_else(|| {
                self.doc
                    .as_ref()
                    .and_then(|doc| doc.styles.iter().find(|d| d.id == id))
            })
            .cloned();
        let Some(def) = def else {
            return;
        };
        let Some(selection) = self.selected.as_mut() else {
            self.set_status("Сначала выделите абзац на странице", cx);
            return;
        };
        selection.style_id = Some(def.id);
        selection.touched = true;
        if let Some(widgets) = self.widgets.as_ref() {
            widgets.editor.update(cx, |editor, cx| {
                editor.restyle(
                    |style| {
                        if let Some(family) = &def.family {
                            style.family = Some(family.clone());
                        }
                        if let Some(size) = def.size {
                            style.size = Some(size);
                        }
                        if let Some(color) = def.color {
                            style.color = Some(color);
                        }
                        style.bold = def.bold;
                        style.italic = def.italic;
                        style.underline = def.underline;
                    },
                    cx,
                );
            });
        }
        // Привязка применяется сразу: каскад ищет метку стиля в самом
        // документе, и незакоммиченная правка для него невидима.
        self.apply_edit(cx);
        self.set_status(format!("Блок привязан к стилю «{}»", def.name), cx);
    }

    /// Снимает со блока метку стиля: текст остаётся как есть, каскад его
    /// больше не трогает.
    pub(crate) fn unbind_style(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selected.as_mut() else {
            return;
        };
        if selection.style_id.take().is_none() {
            return;
        }
        selection.touched = true;
        self.apply_edit(cx);
        self.set_status("Стиль отвязан — текст остался как был", cx);
    }

    /// Открывает окно «Шрифты» и просит опись у движка.
    pub(crate) fn open_fonts(&mut self, cx: &mut Context<Self>) {
        self.fonts_window = Some(FontsWindow {
            fonts: Vec::new(),
            loading: true,
        });
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.request_fonts();
        }
        cx.notify();
    }

    /// «Обновить» в окне шрифтов: система пересматривается заново и опись
    /// перечитывается.
    ///
    /// Нужно ровно для того, ради чего кнопка и заведена: поставил шрифт в
    /// систему — и увидел это, не перезапуская редактор.
    pub(crate) fn rescan_fonts(&mut self, cx: &mut Context<Self>) {
        pdfcore::fonts::refresh();
        if let Some(window) = self.fonts_window.as_mut() {
            window.loading = true;
        }
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.request_fonts();
        }
        cx.notify();
    }

    /// Запоминает подмену шрифта и закрывает список.
    pub(crate) fn substitute_font(
        &mut self,
        document_font: &str,
        family: String,
        cx: &mut Context<Self>,
    ) {
        let key = pdfcore::fonts::family_key(document_font);
        self.font_substitutes.insert(key, family.clone());
        self.set_status(
            format!("«{document_font}» будет набираться шрифтом {family}"),
            cx,
        );
        cx.notify();
    }

    /// Подмена для гарнитуры из документа, если она назначена.
    pub(crate) fn substitute_for(&self, family: &str) -> Option<&String> {
        self.font_substitutes
            .get(&pdfcore::fonts::family_key(family))
    }

    /// Экспорт страницы в отдельный файл рядом с исходником.
    fn export_page(&mut self, page: u32, cx: &mut Context<Self>) {
        let target = self.path.with_file_name(format!(
            "{}_p{}.pdf",
            self.path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "страница".into()),
            page + 1
        ));
        if let Some(doc) = self.doc.as_ref() {
            doc.renderer.export_page(page + 1, target.clone());
            self.set_status(format!("Экспорт: {}", target.display()), cx);
        }
    }

    /// PDF, брошенный на панель страниц, вставляется после текущей страницы.
    fn drop_document(&mut self, paths: &gpui::ExternalPaths, cx: &mut Context<Self>) {
        let pdf = paths
            .paths()
            .iter()
            .find(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
            })
            .cloned();
        let Some(path) = pdf else {
            self.set_status("Вставить можно только файл PDF", cx);
            return;
        };
        let after = self.doc.as_ref().map(|doc| doc.current_page).unwrap_or(0);
        self.page_op(
            pdfcore::PageOp::InsertDocument {
                page_number: after + 1,
                path,
            },
            cx,
        );
    }

    /// «Выбрать документ…» из панели миниатюр: after — номер с нуля.
    fn insert_document_after(&mut self, after: u32, cx: &mut Context<Self>) {
        self.insert_document_at(after + 1, cx);
    }

    /// Вставка документа после страницы `page_number` (ноль — в начало).
    fn insert_document_at(&mut self, page_number: u32, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(path) = paths.pop()
            {
                this.update(cx, |this, cx| {
                    this.page_op(pdfcore::PageOp::InsertDocument { page_number, path }, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Полоса инструментов режима «Систематизировать».
    fn render_organise_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(organise) = self.organise.as_ref() else {
            return div().into_any_element();
        };
        let picked = organise.picked.len();
        let anchor = organise.picked.first().copied().unwrap_or(0);
        let copied = organise.clipboard.len();
        let multi = organise.multi;
        let card = organise.card;
        let muted = cx.theme().muted_foreground;

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().child("Систематизировать страницы"))
                    .child(div().text_xs().text_color(muted).child(match picked {
                        0 => "страница не выбрана".to_owned(),
                        1 => format!("выбрана страница {}", anchor + 1),
                        n => format!("выбрано страниц: {n}"),
                    }))
                    .when(copied > 0, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("в буфере: {copied}")),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("org-rotate-left")
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path("icons/rotate-left-01.svg"))
                            .tooltip("Повернуть влево")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.organise_rotate(anchor, false, cx)
                            })),
                    )
                    .child(
                        Button::new("org-rotate-right")
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path("icons/rotate-right-01.svg"))
                            .tooltip("Повернуть вправо")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.organise_rotate(anchor, true, cx)
                            })),
                    )
                    .child(
                        Button::new("org-copy")
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path("icons/copy-01.svg"))
                            .tooltip("Копировать выбранные (Ctrl+C)")
                            .on_click(cx.listener(move |this, _, _, cx| this.organise_copy(cx))),
                    )
                    .child(
                        Button::new("org-paste")
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path("icons/clipboard.svg"))
                            .tooltip(if copied == 0 {
                                "Буфер страниц пуст".to_owned()
                            } else {
                                format!("Вставить страниц: {copied} (Ctrl+V)")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.organise_paste(cx))),
                    )
                    .child(
                        Button::new("org-delete")
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path("icons/delete-02.svg"))
                            .tooltip("Удалить выбранные (Del)")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.organise_delete(anchor, cx)),
                            ),
                    )
                    .child(
                        Button::new("org-extract")
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path("icons/file-export.svg"))
                            .label("Извлечь")
                            .tooltip("Сохранить выбранную страницу отдельным файлом")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.export_page(anchor, cx)),
                            ),
                    )
                    .child(
                        Button::new("org-insert-blank")
                            .small()
                            .ghost()
                            .icon(gpui_component::Icon::empty().path("icons/add-01.svg"))
                            .label("Пустая")
                            .tooltip("Вставить пустой лист следом")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.page_op(
                                    pdfcore::PageOp::InsertAfter {
                                        page_number: anchor + 1,
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("org-insert-doc")
                            .small()
                            .ghost()
                            .label("Вставка из файла…")
                            .tooltip("Вставить страницы другого PDF следом")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.insert_document_after(anchor, cx)
                            })),
                    )
                    .child(
                        Button::new("org-close")
                            .small()
                            .primary()
                            .label("Закрыть")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_organise(cx))),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("org-multi")
                            .small()
                            .when(multi, |b| b.primary())
                            .when(!multi, |b| b.ghost())
                            .label(if multi {
                                "☑ Выделение нескольких страниц"
                            } else {
                                "☐ Выделение нескольких страниц"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(organise) = this.organise.as_mut() {
                                    organise.multi = !organise.multi;
                                    if !organise.multi {
                                        organise.picked.truncate(1);
                                    }
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Двойной щелчок — перейти к странице"),
                    )
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(muted).child("Размер"))
                    .children(
                        [(-30.0f32, "org-smaller", "−"), (30.0, "org-bigger", "+")].map(
                            |(step, id, glyph)| {
                                Button::new(id)
                                    .small()
                                    .ghost()
                                    .label(glyph)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(organise) = this.organise.as_mut() {
                                            organise.card =
                                                (organise.card + step).clamp(90.0, 320.0);
                                        }
                                        cx.notify();
                                    }))
                            },
                        ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("{card:.0} px")),
                    ),
            )
            .into_any_element()
    }

    /// Половина карточки, принимающая перетаскиваемую страницу.
    ///
    /// Левая говорит «встать перед этой страницей», правая — «после неё».
    /// Обе живут только во время переноса и потому не мешают обычным
    /// щелчкам по карточке.
    fn drop_half(
        id: (&'static str, usize),
        slot: u32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let left = id.0 == "org-left";
        div()
            .id(id)
            .absolute()
            .top_0()
            .when(left, |el| el.left_0())
            .when(!left, |el| el.right_0())
            .w(gpui::relative(0.5))
            .h_full()
            .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _, cx| {
                if let Some(organise) = this.organise.as_mut()
                    && organise.dragging.is_some()
                    && organise.drop_at != Some(slot)
                {
                    organise.drop_at = Some(slot);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseUpEvent, _, cx| {
                    this.organise_drop(slot, cx)
                }),
            )
    }

    /// Сетка страниц режима «Систематизировать».
    ///
    /// Ячейка сетки — щель вместе с карточкой, одним куском постоянной
    /// ширины. Порознь они переносились на новую строку вразнобой, и колонки
    /// разъезжались; вместе — перенос по месту сам ставит их друг под друга.
    fn render_organise_grid(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(organise) = self.organise.as_ref() else {
            return div().into_any_element();
        };
        let card = organise.card;
        let picked = organise.picked.clone();
        let dragging = organise.dragging;
        let drop_at = organise.drop_at;
        let scroll = organise.scroll.clone();
        let columns = organise.columns.max(1);
        let scale_factor = self.scale_factor;
        let border = cx.theme().border;
        let accent = accent_blue();
        let muted = cx.theme().muted_foreground;
        // Кэш растров просят изменяемым: обращение к тайлу освежает его место
        // в очереди вытеснения.
        let Some(doc) = self.doc.as_mut() else {
            return div().into_any_element();
        };
        let count = doc.info.page_count;
        let carrying = dragging.is_some();

        // Щель: место, куда ляжет перетаскиваемая страница, и место вставки
        // новой — та же линия с плюсом, что между миниатюрами, вертикальная.
        let slot = |page: u32, height: f32, cx: &mut Context<Self>| {
            div()
                .id(("org-slot", page as usize))
                .relative()
                .w(px(26.0))
                .h(px(height))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                // В покое щели не видно; она проявляется под курсором — и сама
                // собой, когда над ней несут страницу.
                .when(drop_at != Some(page), |el| {
                    el.opacity(0.0).hover(|el| el.opacity(1.0))
                })
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left(px(11.0))
                        .w(px(3.0))
                        .h_full()
                        .rounded_full()
                        .bg(accent),
                )
                .child(
                    div()
                        .id(("org-plus", page as usize))
                        .w(px(20.0))
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(accent)
                        .cursor_pointer()
                        // Пока страницу несут, плюс мешает: щель тогда — только
                        // место, куда её положить.
                        .when(carrying, |el| el.invisible())
                        .child(
                            gpui_component::Icon::empty()
                                .path("icons/add-01.svg")
                                .size_3()
                                .text_color(gpui::white()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                // Щель перед страницей `page` — вставка после
                                // предыдущей; перед первой это ноль.
                                this.panel_menu = Some(PanelMenu::Insert {
                                    after: page.saturating_sub(1),
                                    at: event.position,
                                    before_first: page == 0,
                                });
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                )
                .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _, cx| {
                    if let Some(organise) = this.organise.as_mut()
                        && organise.dragging.is_some()
                        && organise.drop_at != Some(page)
                    {
                        organise.drop_at = Some(page);
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _: &gpui::MouseUpEvent, _, cx| {
                        this.organise_drop(page, cx)
                    }),
                )
        };

        // Рисуются только видимые ряды: 376 карточек разом переполняют атлас
        // текстур, и растры начинают выгружаться и грузиться заново — экран
        // «перезагружается» на каждый щелчок. Сверху и снизу вместо
        // невидимых карточек стоят распорки, поэтому полоса прокрутки знает
        // настоящую длину документа.
        let ratio0 = doc
            .info
            .size(0)
            .map(|s| (s.width / s.height.max(1.0)).clamp(0.3, 3.0))
            .unwrap_or(0.7);
        let row_height = card / ratio0 + 42.0;
        let offset = -f32::from(scroll.offset().y);
        let viewport = f32::from(scroll.bounds().size.height).max(1.0);
        let rows_total = count.div_ceil(columns);
        let first_row = (offset / row_height).floor().max(0.0) as u32;
        let first_row = first_row
            .saturating_sub(1)
            .min(rows_total.saturating_sub(1));
        let visible_rows = (viewport / row_height).ceil() as u32 + 3;
        let last_row = (first_row + visible_rows).min(rows_total);

        let from = first_row * columns;
        let to = (last_row * columns).min(count);
        let above = first_row as f32 * row_height;
        let below = (rows_total.saturating_sub(last_row)) as f32 * row_height;

        let mut cells: Vec<AnyElement> = Vec::new();
        let mut last_height = card / 0.7 + 30.0;
        for page in from..to {
            let ratio = doc
                .info
                .size(page)
                .map(|s| (s.width / s.height.max(1.0)).clamp(0.3, 3.0))
                .unwrap_or(0.7);
            let height = card / ratio;
            last_height = height + 30.0;
            let texture = doc
                .preview_key(page, height, scale_factor)
                .and_then(|key| doc.tiles.get(&key));
            let chosen = picked.contains(&page);
            let carried = dragging == Some(page);

            let card_view = v_flex()
                .id(("org-card", page as usize))
                .items_center()
                .gap_1()
                .p_1()
                .rounded_md()
                .when(chosen, |el| el.bg(accent.opacity(0.25)))
                .when(carried, |el| el.opacity(0.4))
                .child(
                    div()
                        .relative()
                        .w(px(card))
                        .h(px(height))
                        .bg(white())
                        .rounded_sm()
                        .shadow_sm()
                        .overflow_hidden()
                        .border_2()
                        .border_color(if chosen { accent } else { border })
                        .when_some(texture, |el, tex| {
                            el.child(img(tex).w(px(card)).h(px(height)))
                        })
                        // Пока страницу несут, половинки карточки сами
                        // говорят, куда её положить: щель между страницами
                        // узкая, и курсор её перескакивает.
                        .when(carrying, |el| {
                            el.child(Self::drop_half(("org-left", page as usize), page, cx))
                                .child(Self::drop_half(("org-right", page as usize), page + 1, cx))
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{}", page + 1)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        // Двойной щелчок — уйти к этой странице в чтение.
                        if event.click_count >= 2 {
                            this.organise = None;
                            this.go_to_page(page, cx);
                            // Панель миниатюр возвращается вместе с чтением —
                            // пусть открывается на той же странице.
                            if let Some(doc) = this.doc.as_mut() {
                                doc.thumbs.scroll_to(gpui::ListOffset {
                                    item_ix: page as usize,
                                    offset_in_item: px(0.0),
                                });
                            }
                            cx.stop_propagation();
                            return;
                        }
                        // Одиночное нажатие только запоминается: выбор случится
                        // на отпускании, перенос — если курсор уйдёт с места.
                        this.focus_root = true;
                        if let Some(organise) = this.organise.as_mut() {
                            organise.press = Some((page, event.position));
                        }
                        // Ctrl и Shift меняют набор сразу: так набор виден,
                        // пока кнопка ещё нажата.
                        if event.modifiers.control || event.modifiers.shift {
                            this.organise_pick(
                                page,
                                event.modifiers.control,
                                event.modifiers.shift,
                                cx,
                            );
                        }
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseUpEvent, _, cx| {
                        let Some(organise) = this.organise.as_mut() else {
                            return;
                        };
                        let pressed = organise.press.take();
                        // Отпустили там же, где нажали, и ничего не несли —
                        // это простой выбор страницы.
                        if organise.dragging.is_none()
                            && pressed.is_some_and(|(from, _)| from == page)
                            && !event.modifiers.control
                            && !event.modifiers.shift
                        {
                            this.organise_pick(page, false, false, cx);
                        }
                    }),
                )
                // Правая кнопка — то же меню страницы, что и в панели
                // миниатюр: повороты, экспорт, копирование, удаление.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        if this
                            .organise
                            .as_ref()
                            .is_some_and(|o| !o.picked.contains(&page))
                        {
                            this.organise_pick(page, false, false, cx);
                        }
                        this.panel_menu = Some(PanelMenu::Page {
                            page,
                            at: event.position,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }),
                );

            cells.push(
                h_flex()
                    .flex_none()
                    .items_start()
                    .child(slot(page, height + 30.0, cx))
                    .child(card_view)
                    .into_any_element(),
            );
        }
        // Последняя щель — «положить в самый конец».
        if to == count {
            cells.push(slot(count, last_height, cx).into_any_element());
        }

        div()
            .id("organise-grid")
            .track_scroll(&scroll)
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .bg(cx.theme().muted)
            // Перенос начинается, когда курсор ушёл от места нажатия: щелчок
            // на месте остаётся щелчком.
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                let Some(organise) = this.organise.as_mut() else {
                    return;
                };
                if organise.dragging.is_some() {
                    return;
                }
                let Some((page, at)) = organise.press else {
                    return;
                };
                let moved = (f32::from(event.position.x - at.x)).abs()
                    + (f32::from(event.position.y - at.y)).abs();
                if moved > 8.0 {
                    // Несут либо весь выбранный набор, либо одну страницу —
                    // ту, за которую взялись.
                    if !organise.picked.contains(&page) {
                        organise.picked = vec![page];
                        organise.anchor = Some(page);
                    }
                    organise.dragging = Some(page);
                    cx.notify();
                }
            }))
            // Отпускание мимо щели просто отменяет перенос: страница
            // возвращается на место, ничего не происходит.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    if let Some(organise) = this.organise.as_mut() {
                        organise.press = None;
                        if organise.dragging.take().is_some() {
                            organise.drop_at = None;
                            cx.notify();
                        }
                    }
                }),
            )
            .child(
                v_flex()
                    .p_3()
                    .child(div().h(px(above)).flex_none())
                    .child(h_flex().flex_wrap().items_start().gap_y_3().children(cells))
                    .child(div().h(px(below)).flex_none()),
            )
            .into_any_element()
    }

    fn render_canvas(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.doc.is_none() {
            return self.render_placeholder(cx);
        }
        // Сетка страниц заменяет собой всё чтение: и ленту, и панели правки.
        if self.organise.is_some() {
            let bar = self.render_organise_bar(cx);
            let grid = self.render_organise_grid(cx);
            return v_flex()
                .flex_1()
                .h_full()
                .bg(cx.theme().background)
                .child(bar)
                .child(grid)
                .into_any_element();
        }
        let state = self
            .doc
            .as_ref()
            .expect("документ проверен выше")
            .pages
            .clone();
        let scale_factor = window.scale_factor();
        let this = cx.entity().downgrade();

        let hooks = self.render_mouse_hooks(cx);
        // Замер места под ленту: по нему считается подгонка страницы под окно.
        let measure = {
            let view = cx.entity().downgrade();
            gpui::canvas(
                |bounds, _, _| bounds,
                move |bounds, _, _, cx| {
                    if let Some(view) = view.upgrade() {
                        view.update(cx, |this, cx| this.note_canvas(bounds.size, cx));
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
        };
        let tool = self.tool;
        let toolbar = div()
            .absolute()
            .bottom(px(14.0))
            .left(gpui::relative(0.5))
            .ml(px(-56.0))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_lg()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_lg()
                    .child(
                        Button::new("tool-cursor")
                            .small()
                            .when(tool == Tool::Cursor, |b| b.primary())
                            .when(tool != Tool::Cursor, |b| b.ghost())
                            .icon(gpui_component::Icon::empty().path("icons/cursor-01.svg"))
                            .tooltip("Курсор: выделение и правка блоков")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tool = Tool::Cursor;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("tool-text")
                            .small()
                            .when(tool == Tool::Text, |b| b.primary())
                            .when(tool != Tool::Text, |b| b.ghost())
                            .icon(gpui_component::Icon::empty().path("icons/text-font.svg"))
                            .tooltip("Текст: растяните рамку на пустом месте — будет новый блок")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tool = Tool::Text;
                                cx.notify();
                            })),
                    ),
            );

        div()
            .relative()
            .flex_1()
            .h_full()
            .bg(cx.theme().muted)
            .child(
                // Размер ленте задаёт не содержимое, а место в окне: при
                // «Auto» она лишний раз раскладывает страницы вне кадра, и
                // угол страницы приходил то настоящий, то из этой примерки.
                list(state, move |ix, window, cx| {
                    this.update(cx, |viewer, cx| {
                        viewer.render_row(ix, scale_factor, window, cx)
                    })
                    .unwrap_or_else(|_| div().into_any_element())
                })
                .size_full(),
            )
            .child(measure)
            .child(hooks)
            .child(toolbar)
            .into_any_element()
    }

    /// Ряд ленты: одна страница, а в развороте — пара, как в раскрытой книге.
    ///
    /// Разворот собирается именно рядом, а не двумя элементами списка: лента
    /// меряет высоту поэлементно и укладывает элементы строго друг под друга,
    /// так что поставить две страницы бок о бок можно только внутри одного.
    fn render_row(
        &mut self,
        ix: usize,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Сдвиг вбок делается перекосом полей ряда: содержимое стоит по
        // центру, и поле с одной стороны уводит его в другую ровно на
        // половину. Смещением (`left`) не выйдет: ряды — элементы ленты, она
        // кладёт их по своим координатам и вложенный отступ теряется.
        let (pad_left, pad_right) = if self.pan_x >= 0.0 {
            (px(0.0), px(self.pan_x * 2.0))
        } else {
            (px(-self.pan_x * 2.0), px(0.0))
        };
        if !self.spread {
            return v_flex()
                .w_full()
                .items_center()
                .py_3()
                .pl(pad_left)
                .pr(pad_right)
                .child(self.render_page(ix as u32, scale_factor, window, cx))
                .into_any_element();
        }

        let count = self
            .doc
            .as_ref()
            .map(|doc| doc.info.page_count)
            .unwrap_or(0);
        let left = ix as u32 * 2;
        let mut row = h_flex()
            .w_full()
            .items_start()
            .justify_center()
            .py_3()
            .pl(pad_left)
            .pr(pad_right)
            .gap(px(SPREAD_GAP));
        for page in [left, left + 1] {
            if page < count {
                row = row.child(self.render_page(page, scale_factor, window, cx));
            }
        }
        row.into_any_element()
    }

    fn render_page(
        &mut self,
        page: u32,
        scale_factor: f32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let Some(doc) = self.doc.as_mut() else {
            return div().into_any_element();
        };
        let Some(size) = doc.info.size(page) else {
            return div().into_any_element();
        };

        let zoom = doc.zoom;
        let width = px(size.width * zoom);
        let height = px(size.height * zoom);
        let key = doc.page_key(page, scale_factor);
        // Пока абзац правят, страница показывается из предпросмотра: там уже
        // набранный текст, отрисованный тем же pdfium и потому неотличимый от
        // остальной страницы. Тайл в кэше при этом остаётся прежним — правку
        // ещё не приняли.
        let texture = match &doc.preview {
            Some((preview_page, texture)) if *preview_page == page => Some(texture.clone()),
            _ => doc.tiles.get(&key),
        };

        // Рамки найденных абзацев. Они существуют всегда — по ним работает
        // выделение кликом; видимыми делает либо режим подсветки, либо
        // собственно выделение. Координаты PDF растут вверх, экранные — вниз,
        // поэтому верх блока отсчитывается от высоты страницы.
        // Выделенный абзац обведён своей рамкой с маркерами — её рисует
        // `render_editing`. Второй обводки по найденному блоку не нужно: она
        // оставалась бы на старом месте, когда рамку переносят.
        let show_blocks = doc.show_blocks;

        let doc = self.doc.as_mut().expect("документ проверен выше");
        let mut rects: Vec<(usize, Rect)> = doc
            .blocks
            .get(&page)
            .map(|blocks| {
                blocks
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (i, b.bbox))
                    .collect()
            })
            .unwrap_or_default();
        // Выделенные блоки ловят мышь первыми: они рисуются последними и
        // потому лежат сверху. Иначе копию, легшую поверх чужого текста, не
        // ухватить — нажатие уходило нижнему блоку, и выделение слетало.
        let picked = self.multi.clone();
        rects.sort_by_key(|(_, bbox)| {
            picked
                .iter()
                .any(|target| target.page == page && target.bbox == *bbox)
        });

        let overlays: Vec<Div> = rects
            .into_iter()
            .map(|(index, bbox)| {
                div()
                    .absolute()
                    .left(px(bbox.left * zoom))
                    .top(px((size.height - bbox.top) * zoom))
                    .w(px(bbox.width() * zoom))
                    .h(px(bbox.height() * zoom))
                    .cursor_pointer()
                    .when(show_blocks, |el| {
                        el.border_1().border_color(rgba(0x2563_EBAA))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            if event.modifiers.control {
                                // Ctrl+клик собирает группу поштучно, сразу.
                                this.toggle_multi(page, index, window, cx);
                            } else {
                                // Не выделять сходу: возможно, отсюда потянут
                                // рамку выделения — прямо по тексту, как в
                                // чертёжных пакетах. Судьба нажатия решится
                                // при отпускании.
                                this.press_anywhere(page, Some(index), event.position, cx);
                            }
                            // До страницы нажатие не идёт: там оно значило бы
                            // «мимо всех блоков».
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.open_text_menu(page, Some(index), event.position, window, cx);
                            cx.stop_propagation();
                        }),
                    )
            })
            .collect();

        let total = doc.info.page_count;
        let editing_here = self.selected.as_ref().is_some_and(|s| s.page == page);
        let guides = if editing_here {
            self.render_guides(size.height, zoom)
        } else {
            Vec::new()
        };
        // Угол страницы в окне нужен всем, кто переводит мышь в страничные
        // координаты: повороту рамки и резиновому выделению. Пишется на каждой
        // отрисовке — прокрутка и зум двигают страницы каждый кадр.
        let anchor = {
            let view = cx.entity().downgrade();
            Some(
                gpui::canvas(
                    |bounds, _, _| bounds,
                    // Угол берётся из отрисовки, а не из подготовки. Лента
                    // страниц с автоизмерением прогоняет подготовку и по
                    // невидимым страницам, подставляя им место где-то за
                    // кадром, — и записанный оттуда угол был ложным: рамка
                    // выделения считалась от него и улетала за пределы листа,
                    // а поворот мышью брал не ту точку отсчёта.
                    move |bounds, _, _, cx| {
                        if let Some(view) = view.upgrade() {
                            view.update(cx, |this, _| {
                                // Копятся все места, куда лента положила эту
                                // страницу в этом кадре. Какое из них
                                // настоящее — решается позже, по точке
                                // нажатия: см. `origin_for`.
                                let places = this.page_origins_next.entry(page).or_default();
                                if !places.contains(&bounds.origin) {
                                    places.push(bounds.origin);
                                }
                            });
                        }
                    },
                )
                .absolute()
                // Угол прибит явно. Без top/left «absolute» встаёт на своё
                // место в потоке — ПОСЛЕ картинки страницы, то есть в самый
                // низ листа. Пока текстура не загрузилась, поток пуст и якорь
                // случайно совпадал с углом — потому рамка выделения то
                // работала на свежеоткрытой странице, то мазала на прокрученной.
                .top_0()
                .left_0()
                .size_full()
                .into_any_element(),
            )
        };
        // Поле правки и панель формата лежат в той же системе координат, что
        // и страница, поэтому они и рисуются здесь, а не поверх всего окна.
        let editing = self.render_editing(page, size.width, size.height, zoom, cx);

        v_flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .relative()
                    .w(width)
                    .h(height)
                    .bg(white())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.press_on_page(page, event.position, cx)
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.open_text_menu(page, None, event.position, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .border_1()
                    .border_color(border)
                    .when_some(texture, |el, tex| el.child(img(tex).w(width).h(height)))
                    // Метки группы рисуются раньше рамок блоков: иначе они
                    // накрывают их собой и глушат мышь — выделенный блок
                    // переставал ловить нажатие, и перенос группы срывался
                    // в резиновую рамку.
                    .children(self.render_multi_marks(page, size.height, zoom))
                    .children(overlays)
                    .children(self.render_band_candidates(page, size.height, zoom))
                    .children(editing)
                    .children(guides)
                    .children(self.render_rubber(page))
                    .children(anchor),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{} / {}", page + 1, total)),
            )
            .into_any_element()
    }

    fn render_placeholder(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let message = match &self.error {
            Some(error) => format!("Не удалось открыть документ:\n{error}"),
            None => "Передайте путь к PDF аргументом:\ncargo run -p app -- book.pdf".to_owned(),
        };
        div()
            .flex_1()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().muted)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(message),
            )
            .into_any_element()
    }

    /// Полоса отказа поверх страницы: что именно не вышло и как закрыть.
    fn render_failure(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let message = self.failure.clone()?;
        Some(
            div()
                .absolute()
                .top(px(64.0))
                .left(gpui::relative(0.5))
                .ml(px(-260.0))
                .w(px(520.0))
                .occlude()
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(gpui_component::amber_500())
                        .shadow_lg()
                        .child(
                            gpui_component::Icon::empty()
                                .path("icons/alert-02.svg")
                                .size_4()
                                .text_color(gpui_component::amber_500()),
                        )
                        .child(div().flex_1().text_sm().child(message))
                        .child(
                            Button::new("failure-close")
                                .small()
                                .ghost()
                                .label("×")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.failure = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_status(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let text = match self.doc.as_ref() {
            // Сообщение о сохранении или отказе в правке важнее счётчиков.
            Some(doc) => doc.status.clone().unwrap_or_else(|| {
                // Ширина полей закреплена: счётчики меняются каждый кадр, и
                // на «плавающей» ширине вся строка дёргается туда-сюда.
                format!(
                    "стр. {:>4} из {:<4}   ·   кэш {:>4.0} МБ в {:>3} тайлах   ·   в очереди {:>3}",
                    doc.current_page + 1,
                    doc.info.page_count,
                    doc.tiles.bytes() as f64 / (1024.0 * 1024.0),
                    doc.tiles.len(),
                    doc.renderer.pending_tiles(),
                )
            }),
            None => String::new(),
        };

        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(border)
            .child(div().text_xs().text_color(muted).child(text))
            .into_any_element()
    }
}

impl Focusable for Viewer {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.scale_factor = window.scale_factor();
        self.sync_requests(self.scale_factor);

        if std::mem::take(&mut self.focus_editor)
            && let Some(widgets) = self.widgets.as_ref()
        {
            window.focus(&widgets.editor.read(cx).focus_handle(cx));
        }
        if std::mem::take(&mut self.focus_root) {
            window.focus(&self.focus);
        }

        // Новый кадр: собранное отрисовкой прошлого кадра становится тем, по
        // чему считают координаты, а копилка начинается заново.
        self.page_origins = std::mem::take(&mut self.page_origins_next);

        self.sync_property_inputs(window, cx);
        self.sync_styles_rows(window, cx);
        self.select_pasted(window, cx);

        let toolbar = self.render_toolbar(cx);
        let sidebar = self.render_sidebar(window, cx);
        let canvas = self.render_canvas(window, cx);
        let status = self.render_status(cx);
        let properties = self.render_properties(cx);
        let panel_menu = self.render_panel_menu(window.viewport_size(), cx);
        let font_picker = self.render_font_picker(window.viewport_size(), cx);
        let preset_menu = self.render_preset_menu(window.viewport_size(), cx);
        let failure = self.render_failure(cx);
        let fonts_window = self.render_fonts_window(cx);
        let styles_window = self.render_styles_window(cx);

        v_flex()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(sidebar)
                    .child(canvas)
                    .child(properties),
            )
            .child(status)
            .children(fonts_window)
            .children(styles_window)
            .children(panel_menu)
            .children(font_picker)
            .children(preset_menu)
            .children(failure)
    }
}

/// Модель текста из ранов блока: стили по кускам, как в документе.
///
/// Строки склеиваются пробелом, перенос по дефису сшивается — так же, как в
/// `Block::text()`, — но оформление каждого куска сохраняется. Документные
/// гарнитура и кегль кладутся в память рана и правкой не считаются; цвет
/// запоминается, только когда кусок отличается от доминантного цвета блока, —
/// иначе каждый спан таскал бы за собой запись цвета без нужды.
fn model_from_block(block: &pdfcore::Block, dominant: &Style) -> RichText {
    let mut runs: Vec<crate::rich_text::Run> = Vec::new();

    for (index, line) in block.lines.iter().enumerate() {
        if index > 0 {
            match runs.last_mut() {
                // Дефис на конце строки — перенос слова: сшивается без шва.
                Some(last) if last.text.ends_with('-') => {
                    last.text.pop();
                }
                Some(last) => last.text.push(' '),
                None => {}
            }
        }
        // Кегль строки задают её полноразмерные раны: индекс не в счёт.
        let base_size = line.dominant_size().max(1.0);
        for run in &line.runs {
            let colour_differs = run.style.color != dominant.color;
            let colour = colour_differs.then(|| {
                let c = run.style.color;
                [
                    f32::from(c.r) / 255.0,
                    f32::from(c.g) / 255.0,
                    f32::from(c.b) / 255.0,
                ]
            });
            // Под- и надстрочные знаки узнаются по геометрии: заметно мельче
            // строки и сдвинуты с её базовой линии. Такой ран запоминается
            // индексом при полном кегле — перенабор сам уменьшит его и
            // сдвинет, как делает `Ts` в самом PDF.
            let offset = run.baseline() - line.baseline;
            let script = if run.style.size < base_size * 0.85 && offset.abs() > base_size * 0.04 {
                if offset > 0.0 {
                    pdfcore::stream_edit::Script::Superscript
                } else {
                    pdfcore::stream_edit::Script::Subscript
                }
            } else {
                pdfcore::stream_edit::Script::Baseline
            };
            let size = if script == pdfcore::stream_edit::Script::Baseline {
                run.style.size
            } else {
                base_size
            };
            let mut style = RunStyle::from_document(run.style.clean_family(), size, colour);
            style.document_script = script;
            // Начертание из документа: кнопки Ж и К горят у жирного и
            // курсивного текста, и их можно выключить — это станет правкой.
            style.bold = run.style.is_bold();
            style.italic = run.style.italic;
            style.document_bold = style.bold;
            style.document_italic = style.italic;
            runs.push(crate::rich_text::Run {
                text: run.text.clone(),
                style,
            });
        }
    }

    RichText::from_runs(runs)
}

/// Кегль в том виде, в каком его показывает поле панели формата.
///
/// Сравнивать с исходным кеглем напрямую нельзя: в поле он выводится с одним
/// знаком после запятой, и у блока с кеглем 20.56 обратно читается 20.6. Такое
/// расхождение выглядело как «пользователь изменил размер» — и перенос блока
/// уходил в перенабор, который на пёстром абзаце справедливо отказывает. Так
/// что сравнивать надо ровно с тем, что в поле и написано.
fn seeded_size(size: f32) -> f32 {
    format!("{size:.1}").parse().unwrap_or(size)
}

/// Цвет, если он один на весь абзац; `None`, если цвет не задавали.
///
/// Перестановка меняет цвет разом у всего блока — по кускам его красить
/// нечем, там правятся только координаты. Поэтому разноцветный набор она не
/// берёт на себя и оставляет цвет как был.
fn single_color(text: &RichText) -> Option<[f32; 3]> {
    let mut found: Option<[f32; 3]> = None;
    for run in text.runs() {
        match (run.style.color, found) {
            (None, _) => return None,
            (Some(color), None) => found = Some(color),
            (Some(color), Some(current)) if color == current => {}
            _ => return None,
        }
    }
    found
}

/// Создаёт поля ввода для выделенного блока.
fn build_widgets(
    model: RichText,
    size: f32,
    family: &str,
    line_height: f32,
    wrap_width: Pixels,
    align: pdfcore::model::Align,
    zoom: f32,
    window: &mut Window,
    cx: &mut Context<Viewer>,
) -> (EditWidgets, Option<String>, Option<String>) {
    let base = BaseStyle {
        metrics_by_family: std::collections::HashMap::new(),
        align,
        char_spacing: 0.0,
        h_scale: 100.0,
        size_points: size,
        line_height_points: line_height,
        // Рамка лежит поверх страницы, отрисованной с этим масштабом, — значит
        // и меряется она теми же пикселями на пункт.
        points_to_px: zoom,
        color: gpui::rgb(0x11_1111).into(),
        // Метрики шрифта страницы приходят из документа отдельным событием.
        metrics: None,
    };
    let editor = cx
        .new(|cx| RichTextEditor::new(model, base, wrap_width, gpui::rgba(0x2563_EB55).into(), cx));

    let size_state = cx.new(|cx| {
        let mut state = InputState::new(window, cx);
        state.set_value(format!("{size:.1}"), window, cx);
        state
    });

    // Первое обращение к списку может подождать окончания фонового
    // сканирования шрифтов, которое стартует при открытии документа.
    let families = pdfcore::system_fonts().families();
    let (selected, hint) = preselect_family(&families, family);
    let preselected = selected.map(|index| families[index].clone());

    let number =
        |window: &mut Window, cx: &mut Context<Viewer>| cx.new(|cx| InputState::new(window, cx));
    let geometry = GeometryInputs {
        x: number(window, cx),
        y: number(window, cx),
        w: number(window, cx),
        h: number(window, cx),
        angle: number(window, cx),
        char_spacing: number(window, cx),
        h_scale: number(window, cx),
        para_spacing: number(window, cx),
    };
    let color = cx.new(|cx| gpui_component::color_picker::ColorPickerState::new(window, cx));
    let fill = cx.new(|cx| gpui_component::color_picker::ColorPickerState::new(window, cx));

    (
        EditWidgets {
            editor,
            size: size_state,
            geometry,
            color,
            fill,
        },
        preselected,
        hint,
    )
}

/// Подбирает гарнитуру в списке системных шрифтов под шрифт блока.
///
/// В книгах шрифт почти всегда встроен подмножеством с именем вроде
/// `Gotham-Bold`, которого в системе нет. Оставить выбор пустым нельзя: тогда
/// первое же нажатие `B` упрётся в «шрифт не найден». Поэтому подбирается
/// ближайшее совпадение, а о подмене честно сообщается в строке состояния.
fn preselect_family(families: &[String], block_family: &str) -> (Option<usize>, Option<String>) {
    // Сравнение идёт по нормализованному ключу: в PDF пишут `PTSerif-Regular`,
    // а в системе стоит «PT Serif». Буквальное сравнение их не сводит.
    let wanted = pdfcore::fonts::family_key(block_family);
    if !wanted.is_empty()
        && let Some(index) = families
            .iter()
            .position(|name| pdfcore::fonts::family_key(name) == wanted)
    {
        return (Some(index), None);
    }

    let fallback = families
        .iter()
        .position(|f| f.eq_ignore_ascii_case("Arial"))
        .or_else(|| if families.is_empty() { None } else { Some(0) });

    let hint = fallback.map(|index| {
        format!(
            "Шрифта «{block_family}» нет в системе — при смене оформления будет встроен «{}»",
            families[index]
        )
    });
    (fallback, hint)
}

pub(crate) fn align_label(align: Align) -> &'static str {
    match align {
        Align::Left => "по левому краю",
        Align::Center => "по центру",
        Align::Right => "по правому краю",
        Align::Justify => "по формату",
    }
}

/// Превращает растр pdfium в текстуру gpui.
///
/// Перекладки каналов нет: растр приходит в BGRA (см. `set_reverse_byte_order`
/// в `pdfcore::render`), и `RenderImage` хранит BGRA — `RgbaImage` здесь
/// выступает лишь контейнером байтов нужной формы, а не описанием их порядка.
fn to_texture(bitmap: Arc<Bitmap>) -> Option<Arc<RenderImage>> {
    let (width, height) = (bitmap.width, bitmap.height);
    // Ссылка обычно единственная, и буфер переезжает без копирования.
    let pixels = match Arc::try_unwrap(bitmap) {
        Ok(owned) => owned.pixels,
        Err(shared) => shared.pixels.clone(),
    };
    let buffer = RgbaImage::from_raw(width, height, pixels)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

/// Кладёт растр на диск как PNG — превью для стартовой страницы.
///
/// Здесь перестановка каналов всё же нужна: внутри приложения байты ходят в
/// BGRA (так их отдаёт pdfium и так их ждёт gpui), а PNG хранит RGBA.
fn save_thumbnail(target: &Path, bitmap: &Bitmap) -> anyhow::Result<()> {
    let mut rgba = bitmap.pixels.clone();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    let buffer = RgbaImage::from_raw(bitmap.width, bitmap.height, rgba).ok_or_else(|| {
        anyhow::anyhow!(
            "буфер не соответствует размеру {}x{}",
            bitmap.width,
            bitmap.height
        )
    })?;

    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    buffer.save(target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich_text::RunStyle;
    use gpui::point;

    #[test]
    fn a_size_left_alone_reads_back_exactly_as_it_was_shown() {
        // Поле показывает кегль с одним знаком после запятой. Если сравнивать с
        // исходным значением напрямую, у блока с кеглем 20.56 обратно читается
        // 20.6 — и выходит, будто размер меняли. Из-за этого перенос блока
        // сваливался в перенабор и отклонялся на любом пёстром абзаце.
        for size in [11.0, 20.56, 59.2016, 8.0, 49.33] {
            let shown: f32 = format!("{size:.1}").parse().expect("поле числовое");
            assert_eq!(
                seeded_size(size),
                shown,
                "кегль {size} читается не так, как показан"
            );
        }
    }

    #[test]
    fn a_still_press_is_a_click_and_a_moved_one_is_a_band() {
        let start = point(px(100.0), px(100.0));
        // Дрожь руки щелчок не отменяет: пара-тройка пикселей смещения при
        // нажатии — обычное дело, и раньше из-за них щелчок по абзацу
        // превращался в пустую рамку и ничего не выделял.
        assert!(!press_became_drag(start, point(px(102.0), px(103.0))));
        assert!(!press_became_drag(start, point(px(106.0), px(105.0))));
        assert!(!press_became_drag(start, point(px(100.0), px(100.0))));
        // Осознанная протяжка — в любую сторону, в том числе справа налево:
        // именно ею тянут «секущую» рамку.
        assert!(press_became_drag(start, point(px(140.0), px(100.0))));
        assert!(press_became_drag(start, point(px(60.0), px(100.0))));
        assert!(press_became_drag(start, point(px(100.0), px(40.0))));
    }

    #[test]
    fn a_changed_size_is_still_seen_as_changed() {
        assert_ne!(seeded_size(11.0), 12.0);
        assert_ne!(seeded_size(20.56), 20.5);
    }

    #[test]
    fn one_colour_for_the_whole_block_is_taken_up() {
        let mut style = RunStyle::default();
        style.color = Some([1.0, 0.0, 0.0]);
        let text = RichText::new("красный".to_owned(), style);
        assert_eq!(single_color(&text), Some([1.0, 0.0, 0.0]));
    }

    #[test]
    fn a_block_without_a_chosen_colour_keeps_the_one_it_had() {
        let text = RichText::new("как было".to_owned(), RunStyle::default());
        assert_eq!(single_color(&text), None);
    }

    #[test]
    fn a_multicoloured_block_is_not_recoloured_at_once() {
        // Перестановка красит блок целиком и по кускам не умеет, поэтому
        // разноцветный набор она не берёт на себя вовсе.
        let mut red = RunStyle::default();
        red.color = Some([1.0, 0.0, 0.0]);
        let mut text = RichText::new("красный синий".to_owned(), red);
        // Синим красится только слово «синий» — через выделение: перекраска
        // без выделения теперь честно красит весь блок.
        let start = "красный ".len();
        text.set_caret(start);
        text.extend_to(text.len());
        text.restyle(|style| style.color = Some([0.0, 0.0, 1.0]));
        text.set_caret(text.len());

        assert_eq!(single_color(&text), None);
    }
}
