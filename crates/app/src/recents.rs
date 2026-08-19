//! Список недавних документов.
//!
//! Хранится в JSON рядом с настройками приложения. Миниатюра первой страницы
//! кладётся на диск отдельным PNG с именем, выведенным из пути документа, —
//! поэтому стартовая страница не открывает ни одного PDF, чтобы показать
//! сетку превью, и остаётся мгновенной независимо от числа записей.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Сколько записей храним. Больше на экран всё равно не помещается осмысленно.
const MAX_ENTRIES: usize = 36;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecentDoc {
    pub path: PathBuf,
    pub title: String,
    pub pages: u32,
    /// Unix-время последнего открытия, в секундах.
    pub opened_at: u64,
}

impl RecentDoc {
    /// Существует ли файл сейчас. Недавние легко переживают своё содержимое:
    /// документ переименовали, удалили или он лежал на съёмном диске.
    pub fn is_available(&self) -> bool {
        self.path.is_file()
    }

    /// Человекочитаемая давность открытия.
    pub fn opened_ago(&self) -> String {
        let now = unix_now();
        let seconds = now.saturating_sub(self.opened_at);
        match seconds {
            0..=59 => "только что".to_owned(),
            60..=3599 => format!("{} мин назад", seconds / 60),
            3600..=86_399 => format!("{} ч назад", seconds / 3600),
            86_400..=172_799 => "вчера".to_owned(),
            _ => format!("{} дн назад", seconds / 86_400),
        }
    }
}

#[derive(Default)]
pub struct Recents {
    entries: Vec<RecentDoc>,
    /// Файл индекса. `None` — список живёт только в памяти и на диск не
    /// пишется. Такой список создают тесты: иначе прогон `cargo test` менял бы
    /// настоящие недавние документы пользователя.
    storage: Option<PathBuf>,
}

impl Recents {
    pub fn load() -> Recents {
        let storage = index_file();
        let entries = storage
            .as_ref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|raw| match serde_json::from_str::<Vec<RecentDoc>>(&raw) {
                Ok(entries) => Some(entries),
                Err(e) => {
                    // Битый индекс не повод падать: список недавних —
                    // удобство, а не данные пользователя.
                    tracing::warn!("список недавних повреждён, начинаю заново: {e}");
                    None
                }
            })
            .unwrap_or_default();

        Recents { entries, storage }
    }

    pub fn entries(&self) -> &[RecentDoc] {
        &self.entries
    }

    /// Поднимает документ наверх списка, обновляя данные.
    pub fn touch(&mut self, path: &Path, pages: u32) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());

        self.entries.retain(|e| e.path != path);
        self.entries.insert(
            0,
            RecentDoc {
                path,
                title,
                pages,
                opened_at: unix_now(),
            },
        );
        self.entries.truncate(MAX_ENTRIES);
        self.save();
    }

    pub fn remove(&mut self, path: &Path) {
        if let Some(pos) = self.entries.iter().position(|e| e.path == path) {
            let removed = self.entries.remove(pos);
            let _ = std::fs::remove_file(thumbnail_path(&removed.path));
            self.save();
        }
    }

    fn save(&self) {
        let Some(file) = self.storage.as_ref() else {
            return;
        };
        if let Some(dir) = file.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            tracing::warn!("не удалось создать каталог данных: {e}");
            return;
        }
        match serde_json::to_string_pretty(&self.entries) {
            Ok(json) => {
                if let Err(e) = std::fs::write(file, json) {
                    tracing::warn!("не удалось сохранить список недавних: {e}");
                }
            }
            Err(e) => tracing::warn!("не удалось сериализовать список недавних: {e}"),
        }
    }
}

/// Каталог данных приложения.
///
/// Переименование продукта не должно стоить пользователю списка недавних:
/// если рядом лежит каталог под старым именем, а нового ещё нет, он
/// переезжает целиком — со списком и миниатюрами.
pub fn data_dir() -> Option<PathBuf> {
    let root = dirs::data_dir()?;
    let dir = root.join("PDFer");
    let legacy = root.join("pdf-editor");
    if !dir.exists() && legacy.exists() {
        if let Err(e) = std::fs::rename(&legacy, &dir) {
            tracing::warn!("не удалось перенести данные из pdf-editor: {e}");
        }
    }
    Some(dir)
}

fn index_file() -> Option<PathBuf> {
    data_dir().map(|d| d.join("recents.json"))
}

/// Путь миниатюры документа. Выводится из пути детерминированно, поэтому
/// связывать его с записью в индексе не нужно: файл либо есть, либо нет.
pub fn thumbnail_path(document: &Path) -> PathBuf {
    let canonical = document
        .canonicalize()
        .unwrap_or_else(|_| document.to_path_buf());
    let name = format!("{:016x}.png", path_hash(&canonical));
    data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("thumbnails")
        .join(name)
}

/// FNV-1a по строковому представлению пути. Криптостойкость здесь не нужна —
/// нужна лишь стабильность между запусками, чего `DefaultHasher` не гарантирует.
fn path_hash(path: &Path) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_list_never_touches_disk() {
        // Регрессия: `touch` сохраняет список, и раньше он писал в настоящий
        // профиль пользователя прямо из тестов, затирая недавние документы.
        let mut recents = Recents::default();
        assert!(recents.storage.is_none());
        recents.touch(Path::new("whatever.pdf"), 1);
        assert!(
            recents.storage.is_none(),
            "список без файла обязан оставаться в памяти"
        );
    }

    #[test]
    fn touch_moves_existing_document_to_the_front() {
        let mut recents = Recents::default();
        recents.entries = vec![
            RecentDoc {
                path: "a.pdf".into(),
                title: "a.pdf".into(),
                pages: 1,
                opened_at: 1,
            },
            RecentDoc {
                path: "b.pdf".into(),
                title: "b.pdf".into(),
                pages: 2,
                opened_at: 2,
            },
        ];

        recents.touch(Path::new("b.pdf"), 7);

        assert_eq!(
            recents.entries().len(),
            2,
            "повторное открытие не должно плодить дубликаты"
        );
        assert_eq!(recents.entries()[0].title, "b.pdf");
        assert_eq!(recents.entries()[0].pages, 7, "данные обновились");
    }

    #[test]
    fn list_is_capped() {
        let mut recents = Recents::default();
        for i in 0..(MAX_ENTRIES + 10) {
            recents.touch(Path::new(&format!("doc{i}.pdf")), 1);
        }
        assert_eq!(recents.entries().len(), MAX_ENTRIES);
        // Наверху — последний открытый.
        assert_eq!(
            recents.entries()[0].title,
            format!("doc{}.pdf", MAX_ENTRIES + 9)
        );
    }

    #[test]
    fn thumbnail_path_is_stable_and_distinct() {
        let a = thumbnail_path(Path::new("book.pdf"));
        let b = thumbnail_path(Path::new("book.pdf"));
        let c = thumbnail_path(Path::new("other.pdf"));
        assert_eq!(
            a, b,
            "имя миниатюры обязано быть стабильным между запусками"
        );
        assert_ne!(a, c);
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("png"));
    }

    #[test]
    fn relative_time_reads_naturally() {
        let now = unix_now();
        let make = |ago: u64| RecentDoc {
            path: "x.pdf".into(),
            title: "x".into(),
            pages: 1,
            opened_at: now - ago,
        };
        assert_eq!(make(10).opened_ago(), "только что");
        assert_eq!(make(600).opened_ago(), "10 мин назад");
        assert_eq!(make(7200).opened_ago(), "2 ч назад");
        assert_eq!(make(90_000).opened_ago(), "вчера");
        assert_eq!(make(300_000).opened_ago(), "3 дн назад");
    }
}
