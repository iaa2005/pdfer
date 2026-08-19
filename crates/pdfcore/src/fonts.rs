//! Системные шрифты и их встраивание в документ.
//!
//! Правка текста прежним шрифтом блока не требует ничего этого — и до тех
//! пор, пока пользователь не трогает оформление, ничего и не происходит. Но
//! стоит нажать «полужирный», сменить гарнитуру или набрать символ, которого
//! нет во встроенном подмножестве, — нужен настоящий файл шрифта.
//!
//! Индекс строится один раз за сеанс: около 550 файлов на типичной Windows,
//! почти полгигабайта, — поэтому сканирование стоит прогревать в фоне
//! ([`warm_up`]), а не ждать его при первом клике по кнопке «Ж».

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};
use pdfium_render::prelude::*;

/// Начертание, найденное в системе.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaceInfo {
    pub family: String,
    pub bold: bool,
    pub italic: bool,
    pub path: PathBuf,
}

/// Запрошенное оформление.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FontRequest {
    pub family: String,
    pub bold: bool,
    pub italic: bool,
}

impl FontRequest {
    pub fn new(family: impl Into<String>, bold: bool, italic: bool) -> FontRequest {
        FontRequest {
            family: family.into(),
            bold,
            italic,
        }
    }
}

#[derive(Default)]
pub struct SystemFonts {
    faces: Vec<FaceInfo>,
}

/// Индекс живёт под `RwLock`, а не в `OnceLock`: пользователь ставит шрифт в
/// систему и вправе ожидать, что тот появится в списке без перезапуска
/// программы. Отдаём `Arc`, чтобы читатели не держали замок.
static SYSTEM_FONTS: RwLock<Option<Arc<SystemFonts>>> = RwLock::new(None);

/// Индекс системных шрифтов. Первый вызов сканирует каталоги и может занять
/// секунду-другую; дальше отдаётся готовый.
pub fn system_fonts() -> Arc<SystemFonts> {
    if let Some(fonts) = SYSTEM_FONTS.read().ok().and_then(|guard| guard.clone()) {
        return fonts;
    }

    // Сканируем вне замка: держать его секунду значило бы подвесить UI.
    let scanned = Arc::new(SystemFonts::scan());
    match SYSTEM_FONTS.write() {
        Ok(mut guard) => {
            // Пока сканировали, кто-то мог успеть раньше — берём его результат,
            // чтобы у всех был один и тот же список.
            if let Some(existing) = guard.clone() {
                return existing;
            }
            *guard = Some(Arc::clone(&scanned));
            scanned
        }
        Err(_) => scanned,
    }
}

/// Перечитывает каталоги шрифтов. Возвращает `true`, если состав семейств
/// изменился — тогда список в интерфейсе стоит пересобрать.
pub fn refresh() -> bool {
    let scanned = Arc::new(SystemFonts::scan());
    let previous = SYSTEM_FONTS.read().ok().and_then(|guard| guard.clone());
    let changed = previous
        .map(|old| old.families() != scanned.families())
        .unwrap_or(true);

    if let Ok(mut guard) = SYSTEM_FONTS.write() {
        *guard = Some(scanned);
    }
    changed
}

/// Запускает сканирование в фоне, чтобы к моменту первого обращения индекс уже
/// был готов.
pub fn warm_up() {
    if SYSTEM_FONTS
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .is_some()
    {
        return;
    }
    std::thread::Builder::new()
        .name("font-scan".to_owned())
        .spawn(|| {
            let started = std::time::Instant::now();
            let count = system_fonts().len();
            tracing::info!(
                faces = count,
                "индекс шрифтов готов за {:?}",
                started.elapsed()
            );
        })
        .ok();
}

impl SystemFonts {
    fn scan() -> SystemFonts {
        let mut faces = Vec::new();
        for dir in font_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !is_font_file(&path) {
                    continue;
                }
                if let Some(face) = read_face(&path) {
                    faces.push(face);
                }
            }
        }
        faces.sort_by(|a, b| {
            a.family
                .cmp(&b.family)
                .then(a.bold.cmp(&b.bold))
                .then(a.italic.cmp(&b.italic))
        });
        SystemFonts { faces }
    }

    pub fn len(&self) -> usize {
        self.faces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// Список семейств без повторов, в алфавитном порядке.
    pub fn families(&self) -> Vec<String> {
        let mut families: Vec<String> = Vec::new();
        for face in &self.faces {
            if families
                .last()
                .map(|last| last != &face.family)
                .unwrap_or(true)
            {
                families.push(face.family.clone());
            }
        }
        families
    }

    /// Подбирает начертание под запрос.
    ///
    /// Точное совпадение предпочтительнее, но если у семейства нет, скажем,
    /// курсива, лучше отдать обычное начертание, чем отказать: пользователь
    /// увидит, что курсив не применился, а не пустое место вместо текста.
    pub fn resolve(&self, request: &FontRequest) -> Option<&FaceInfo> {
        let wanted = family_key(&request.family);
        let same_family: Vec<&FaceInfo> = self
            .faces
            .iter()
            .filter(|face| family_key(&face.family) == wanted)
            .collect();

        if same_family.is_empty() {
            return None;
        }
        same_family
            .iter()
            .find(|f| f.bold == request.bold && f.italic == request.italic)
            .or_else(|| same_family.iter().find(|f| f.bold == request.bold))
            .or_else(|| same_family.iter().find(|f| !f.bold && !f.italic))
            .or(same_family.first())
            .copied()
    }

    /// Есть ли такое семейство в системе.
    pub fn has_family(&self, family: &str) -> bool {
        self.find_family(family).is_some()
    }

    /// Ищет семейство по имени из PDF.
    ///
    /// Имена в документе и в системе почти никогда не совпадают буквально:
    /// в PDF пишут `PTSerif-Regular`, а установлено «PT Serif». Поэтому
    /// сравниваются нормализованные ключи — без пробелов, дефисов и хвоста с
    /// начертанием.
    pub fn find_family(&self, family: &str) -> Option<&str> {
        let wanted = family_key(family);
        if wanted.is_empty() {
            return None;
        }
        self.faces
            .iter()
            .find(|face| family_key(&face.family) == wanted)
            .map(|face| face.family.as_str())
    }
}

/// Шрифт документа: что о нём известно из самого файла и из системы.
///
/// Собирается по ресурсам страниц — это ровно те шрифты, которыми набран
/// текст. Редактору важны две вещи: вшита ли программа шрифта в файл (иначе
/// набирать нечем) и есть ли гарнитура в системе (иначе нечем подменить).
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentFont {
    /// Имя из PDF без префикса подмножества: `Inter-SemiBold`.
    pub base_font: String,
    /// Подтип словаря шрифта: `TrueType`, `Type0`, `Type1`.
    pub subtype: String,
    /// Программа шрифта лежит в самом файле.
    pub embedded: bool,
    /// В файле только часть глифов (префикс вида `FNPBYN+`).
    pub subset: bool,
    /// Имя семейства в системе, если оно там нашлось.
    pub system_family: Option<String>,
    /// На скольких страницах встречается.
    pub pages: u32,
}

impl DocumentFont {
    /// Шрифт, которым нельзя ни набирать, ни подменить: ни в файле, ни в
    /// системе. Именно такие и портят правку — их редактор помечает
    /// предупреждением.
    pub fn is_missing(&self) -> bool {
        !self.embedded && self.system_family.is_none()
    }
}

/// Подбирает установленную гарнитуру, похожую на шрифт документа.
///
/// Порядок честный, от точного к грубому: само семейство, если оно есть в
/// системе; родственник с тем же именем без цифрового хвоста (оптические
/// размеры вроде `LMRoman9`); и наконец — первый установленный из списка
/// той же классификации: моноширинный, гротеск или антиква, угаданная по
/// имени. Классификация по имени не идеальна, но для подмены недостающего
/// шрифта «похожий по духу» лучше, чем произвольный.
pub fn suggest_substitute(family: &str) -> Option<String> {
    let fonts = system_fonts();
    if let Some(found) = fonts.find_family(family) {
        return Some(found.to_owned());
    }

    // Родственник по имени: LMRoman9 → любой установленный LMRoman*.
    let wanted = family_key(family);
    let stem: String = wanted
        .trim_end_matches(|ch: char| ch.is_ascii_digit())
        .to_owned();
    if !stem.is_empty() {
        for candidate in fonts.families() {
            let key = family_key(&candidate);
            let candidate_stem = key.trim_end_matches(|ch: char| ch.is_ascii_digit());
            if candidate_stem == stem {
                return Some(candidate);
            }
        }
    }

    // Классификация по имени и первый установленный кандидат из её списка.
    let lower = family.to_lowercase();
    let contains = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    let candidates: &[&str] = if contains(&["mono", "courier", "consol", "menlo", "code"]) {
        &["Consolas", "Cascadia Mono", "Courier New"]
    } else if contains(&[
        "sans",
        "grotesk",
        "gothic",
        "helvetica",
        "arial",
        "myriad",
        "futura",
        "calibri",
        "segoe",
        "verdana",
        "tahoma",
        "inter",
        "roboto",
        "lato",
        "frutiger",
        "univers",
    ]) {
        &["Segoe UI", "Arial", "Calibri", "Verdana"]
    } else {
        // Книжный набор почти всегда антиква: minion, roman, times, garamond…
        &[
            "Georgia",
            "Times New Roman",
            "Cambria",
            "Palatino Linotype",
            "Book Antiqua",
        ]
    };
    candidates
        .iter()
        .find_map(|candidate| fonts.find_family(candidate).map(|f| f.to_owned()))
}

/// Хвосты, обозначающие начертание, а не семейство.
/// Хвосты перечислены от длинных к коротким и снимаются в этом порядке:
/// иначе у «Semibold» первым отрезался бы «bold», оставляя бессмысленное
/// «semi», — и MyriadPro-Semibold не находил установленный Myriad Pro.
/// «It» — адобовское сокращение курсива: MinionPro-It.
const STYLE_SUFFIXES: &[&str] = &[
    "semibold",
    "demibold",
    "extrabold",
    "ultrabold",
    "oblique",
    "regular",
    "italic",
    "medium",
    "light",
    "black",
    "heavy",
    "roman",
    "bold",
    "thin",
    "book",
    "it",
    "mt",
    "ps",
];

/// Ключ для сравнения имён семейств.
pub fn family_key(name: &str) -> String {
    let mut key: String = name
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect();

    // Хвосты снимаются по очереди: `PTSerif-BoldItalic` → `ptserif`.
    loop {
        let before = key.len();
        for suffix in STYLE_SUFFIXES {
            if key.len() > suffix.len() && key.ends_with(suffix) {
                key.truncate(key.len() - suffix.len());
            }
        }
        if key.len() == before {
            break;
        }
    }
    key
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(windir) = std::env::var("WINDIR") {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Microsoft/Windows/Fonts"));
    }
    dirs
}

/// Коллекции `.ttc` намеренно пропускаются: pdfium принимает файл целиком и
/// возьмёт из него первое начертание, а какое именно — не наше решение.
/// Отдельные `.ttf` и `.otf` покрывают подавляющее большинство гарнитур.
fn is_font_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ttf") || e.eq_ignore_ascii_case("otf"))
        .unwrap_or(false)
}

fn read_face(path: &Path) -> Option<FaceInfo> {
    let data = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&data, 0).ok()?;

    // Идентификатор 16 — типографское семейство: у него «Arial» вместо
    // «Arial Narrow Bold», то есть ровно то, что показывают в списке шрифтов.
    // Идентификатор 1 — запасной вариант.
    let family = face_name(&face, 16).or_else(|| face_name(&face, 1))?;

    Some(FaceInfo {
        family,
        bold: face.is_bold(),
        italic: face.is_italic() || face.is_oblique(),
        path: path.to_path_buf(),
    })
}

fn face_name(face: &ttf_parser::Face, name_id: u16) -> Option<String> {
    face.names()
        .into_iter()
        .filter(|name| name.name_id == name_id)
        .find_map(|name| name.to_string())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Шрифты, уже встроенные в открытый документ.
///
/// Один и тот же файл нельзя грузить повторно на каждую правку: pdfium
/// добавлял бы копию шрифта в документ при каждом нажатии «Ж».
#[derive(Default)]
pub struct FontCache {
    tokens: HashMap<FontRequest, PdfFontToken>,
}

impl FontCache {
    pub fn new() -> FontCache {
        FontCache::default()
    }

    pub fn embed(
        &mut self,
        document: &mut PdfDocument<'static>,
        request: &FontRequest,
    ) -> Result<PdfFontToken> {
        if let Some(token) = self.tokens.get(request) {
            return Ok(*token);
        }

        let fonts = system_fonts();
        let face = fonts
            .resolve(request)
            .ok_or_else(|| anyhow!("шрифт «{}» не найден в системе", request.family))?;
        let path = face.path.clone();

        // is_cid = true обязательно: без него шрифт кодируется однобайтно и
        // кириллица превращается в мусор.
        let token = document
            .fonts_mut()
            .load_true_type_from_file(&path, true)
            .map_err(|e| anyhow!("не удалось встроить {}: {e}", path.display()))?;

        self.tokens.insert(request.clone(), token);
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_tails_come_off_whole() {
        // «Semibold» снимается целиком, а не по кускам: раньше первым
        // отрезался «bold», оставляя «myriadprosemi», — и установленный
        // Myriad Pro не находился. «It» — адобовский курсив.
        assert_eq!(family_key("MyriadPro-Semibold"), "myriadpro");
        assert_eq!(family_key("MinionPro-It"), "minionpro");
        assert_eq!(family_key("MinionPro-BoldIt"), "minionpro");
        assert_eq!(family_key("PTSerif-BoldItalic"), "ptserif");
        assert_eq!(family_key("LMRoman9-Italic"), "lmroman9");
    }

    #[test]
    fn system_index_is_not_empty_and_lists_families() {
        let fonts = system_fonts();
        assert!(!fonts.is_empty(), "в системе не найдено ни одного шрифта");

        let families = fonts.families();
        assert!(
            families.len() > 5,
            "семейств подозрительно мало: {}",
            families.len()
        );
        // Список без повторов и отсортирован.
        let mut sorted = families.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            families, sorted,
            "семейства должны идти без повторов, по алфавиту"
        );
    }

    #[test]
    fn resolve_falls_back_within_the_family() {
        let fonts = system_fonts();
        let Some(family) = fonts.families().into_iter().find(|f| fonts.has_family(f)) else {
            return;
        };

        // Какое бы начертание ни попросили, ответ должен быть из того же
        // семейства, а не пустота.
        for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
            let request = FontRequest::new(&family, bold, italic);
            let face = fonts
                .resolve(&request)
                .expect("должно найтись хоть какое-то начертание");
            assert!(face.family.eq_ignore_ascii_case(&family));
        }
    }

    #[test]
    fn family_key_ignores_spacing_and_style_suffixes() {
        // Настоящий случай: в документе `PTSerif-Regular`, в системе «PT Serif».
        assert_eq!(family_key("PTSerif-Regular"), family_key("PT Serif"));
        assert_eq!(
            family_key("TimesNewRomanPSMT"),
            family_key("Times New Roman")
        );
        assert_eq!(family_key("Arial-BoldItalic"), family_key("Arial"));
        // А разные семейства остаются разными.
        assert_ne!(family_key("PT Serif"), family_key("PT Sans"));
        assert_ne!(family_key("Arial"), family_key("Arial Narrow"));
    }

    #[test]
    fn system_lookup_finds_font_named_as_in_a_pdf() {
        let fonts = system_fonts();
        let Some(installed) = fonts.families().into_iter().find(|f| f.contains(' ')) else {
            return;
        };
        // Имя без пробелов и с хвостом начертания обязано находиться.
        let as_in_pdf = format!("{}-Regular", installed.replace(' ', ""));
        assert_eq!(fonts.find_family(&as_in_pdf), Some(installed.as_str()));
    }

    #[test]
    fn unknown_family_resolves_to_nothing() {
        let request = FontRequest::new("Такого Шрифта Нет 12345", false, false);
        assert!(system_fonts().resolve(&request).is_none());
    }

    #[test]
    fn refresh_rebuilds_the_index_and_keeps_it_usable() {
        let before = system_fonts();
        // Ничего не устанавливали — состав обязан совпасть, а индекс остаться
        // рабочим. Проверяем именно это: пересканирование не должно быть
        // разрушительным само по себе.
        let changed = refresh();
        let after = system_fonts();

        assert!(
            !changed,
            "без установки новых шрифтов состав меняться не должен"
        );
        assert_eq!(before.families(), after.families());
        assert!(!after.is_empty());
    }

    #[test]
    fn readers_hold_their_own_snapshot_across_refresh() {
        // Читатель держит `Arc`, поэтому перечитывание индекса не выдёргивает
        // у него список из-под ног посреди работы.
        let snapshot = system_fonts();
        let families = snapshot.families();
        refresh();
        assert_eq!(
            snapshot.families(),
            families,
            "снимок обязан пережить обновление"
        );
    }

    #[test]
    fn family_lookup_ignores_case_and_padding() {
        let fonts = system_fonts();
        let Some(family) = fonts.families().first().cloned() else {
            return;
        };
        assert!(fonts.has_family(&format!("  {}  ", family.to_uppercase())));
    }
}
