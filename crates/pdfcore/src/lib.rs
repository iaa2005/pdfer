//! Ядро редактора PDF: открытие документа, фоновый рендер, кэш растров и
//! восстановление текстовых блоков.
//!
//! Крейт headless — он ничего не знает про UI и тестируется отдельно.
//!
//! # Устройство
//!
//! [`render::Renderer`] владеет выделенным потоком, в котором живут pdfium и
//! открытый документ; наружу он отдаёт только `Send`-данные — растры
//! ([`cache::Bitmap`]) и разобранные абзацы ([`model::Block`]). Всё остальное
//! — геометрия ([`geom`]), модель ([`model`]), склейка блоков ([`blocks`]) и
//! кэш ([`cache`]) — чистый Rust без зависимости от pdfium, поэтому покрыто
//! обычными юнит-тестами.
//!
//! Модель документа намеренно не повторяет структуру PDF: в файле нет ни
//! абзацев, ни стилей, только позиционированные глифы. Абзацы восстанавливаются
//! при открытии и переписываются обратно при сохранении — это единственный
//! способ дать предсказуемую правку текста с перевёрсткой.

pub mod blocks;
pub mod cache;
pub mod embed;
pub mod engine;
pub mod extract;
pub mod fonts;
pub mod geom;
pub mod model;
pub mod pages;
pub mod render;
pub mod stream_edit;
pub mod styles;

pub use blocks::{BlockOptions, detect_blocks};
pub use cache::{Bitmap, DEFAULT_TILE_BUDGET, TileCache, TileKey, ZoomBucket};
pub use fonts::{DocumentFont, FontRequest, system_fonts};
pub use geom::{PageSize, Rect, Rotation};
pub use model::{Align, Block, Line, Rgba, Style, StyleTemplate, TextRun};
pub use pages::PageOp;
pub use render::{BlockEdit, DocumentInfo, RenderEvent, Renderer};
pub use stream_edit::{
    BlockErase, BlockMark, BlockRewrite, BlockTransform, RewriteOutcome, TransformOutcome,
    document_fonts,
};
pub use styles::StyleDef;
