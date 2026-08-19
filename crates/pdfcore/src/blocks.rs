//! Восстановление абзацев из позиционированных ранов.
//!
//! В PDF нет ни строк, ни абзацев — только глифы с координатами. Всё, что
//! видит редактор как «блок текста», собирается здесь по геометрии.
//!
//! Сборка идёт в два шага:
//!
//! 1. Раны → строки: объединяются по совпадению базовой линии, но разрываются
//!    на широких горизонтальных зазорах, иначе левая и правая колонки
//!    развёрнутого разворота склеятся в одну строку.
//! 2. Строки → абзацы: строка не приписывается к последнему блоку подряд, а
//!    ищет среди *открытых* блоков ближайший совместимый сверху. Именно это
//!    даёт корректный результат в многоколоночной вёрстке, где строки соседних
//!    колонок чередуются в порядке обхода сверху вниз.

use crate::geom::{Rect, union_all};
use crate::model::{Align, Block, Line, TextRun};

/// Пороги склейки. Все расстояния заданы в долях кегля, а не в пунктах, —
/// иначе настройки, подобранные на книге 10 pt, разваливаются на плакате 40 pt.
#[derive(Clone, Debug)]
pub struct BlockOptions {
    /// Допуск расхождения базовых линий внутри одной строки.
    pub baseline_tol: f32,
    /// Зазор, начиная с которого между ранами вставляется пробел.
    pub space_gap: f32,
    /// Зазор, начиная с которого строка безусловно считается началом другой
    /// колонки, в долях кегля.
    pub column_gap: f32,
    /// Во сколько раз зазор должен превысить обычный для этой полосы, чтобы
    /// считаться границей колонки.
    ///
    /// Абсолютного порога мало: в подписях к рисункам кегль мелкий, колонки
    /// стоят близко, и зазор между ними бывает меньше, чем допустимый пробел в
    /// выключенном по формату абзаце. Зато он всегда резко больше обычного
    /// межсловного на той же строке.
    pub column_gap_ratio: f32,
    /// Кегль, ниже которого ран считается подстрочным или надстрочным знаком,
    /// в долях кегля полосы.
    pub script_size: f32,
    /// Предельное смещение такого знака по вертикали, в долях кегля полосы.
    pub script_offset: f32,
    /// Допустимое расхождение кеглей внутри одной строки, в долях большего.
    ///
    /// Строка абзаца бывает пёстрой: полужирный зачин, курсив, вставка чужой
    /// гарнитурой в формуле — но кегль у всего этого один. А вот номер строки
    /// таблицы, легший на ту же базовую линию, набран и другим шрифтом, и
    /// заметно мельче. Одно из двух совпадений — гарнитура или кегль — и
    /// строка своя; ни одного — это соседняя колонка.
    pub line_size_tol: f32,
    /// Насколько близко к строке должен стоять мелкий ран, чтобы считаться её
    /// подстрочным знаком, в долях кегля.
    ///
    /// Одного кегля мало: номер в таблице набран той же мелочью, что и индекс
    /// у переменной, и по высоте от строки отличается так же. Разница в том,
    /// что индекс стоит вплотную к своей букве, а колонка таблицы — за
    /// десяток пунктов. Без этого порога подписи к рисункам утаскивали в себя
    /// номера соседней таблицы, и правка абзаца рассыпала таблицу.
    pub script_gap: f32,
    /// Во сколько раз шаг строк должен превысить интерлиньяж блока, чтобы
    /// считаться границей абзаца.
    pub para_break: f32,
    /// Допуск совпадения краёв при проверке выравнивания.
    pub align_tol: f32,
    /// Предельное отношение кеглей соседних строк одного блока.
    pub size_ratio: f32,
    /// Минимальная доля горизонтального перекрытия строк одного блока.
    ///
    /// Строки одного абзаца перекрываются почти полностью: доля считается от
    /// более узкой из двух, поэтому даже короткая последняя строка даёт около
    /// единицы. Заметно меньшие значения означают соседние колонки, а не
    /// продолжение абзаца.
    pub min_h_overlap: f32,
    /// Предельный абзацный отступ первой строки.
    pub max_indent: f32,
}

impl Default for BlockOptions {
    fn default() -> Self {
        BlockOptions {
            baseline_tol: 0.30,
            space_gap: 0.22,
            // Полтора кегля. Даже щедро выключенный по формату пробел столько
            // не занимает, а вот колонки подписей расходятся уже на столько.
            column_gap: 1.5,
            column_gap_ratio: 3.5,
            line_size_tol: 0.12,
            script_size: 0.8,
            script_offset: 0.6,
            script_gap: 0.6,
            // Шаг базовых линий внутри абзаца постоянен с точностью до
            // округления (13, 13, 14), а отбивка между абзацами даёт скачок
            // в полтора раза. Порог 1.55 оказался выше реальной отбивки в
            // книгах со скромным межабзацным интервалом — например 19 при
            // интерлиньяже 13.
            para_break: 1.3,
            align_tol: 0.6,
            size_ratio: 1.25,
            min_h_overlap: 0.55,
            max_indent: 4.0,
        }
    }
}

/// Полный разбор страницы: раны → абзацы.
pub fn detect_blocks(runs: Vec<TextRun>, opts: &BlockOptions) -> Vec<Block> {
    group_lines(build_lines(runs, opts), opts)
}

/// Шаг 1: раны → строки.
pub fn build_lines(mut runs: Vec<TextRun>, opts: &BlockOptions) -> Vec<Line> {
    runs.retain(|r| !r.text.trim().is_empty());
    // Порядок чтения: сверху вниз (базовая линия убывает), слева направо.
    runs.sort_by(|a, b| {
        b.baseline()
            .total_cmp(&a.baseline())
            .then_with(|| a.bbox.left.total_cmp(&b.bbox.left))
    });

    // Шаг 1: раны собираются в заготовки строк.
    //
    // Ран ищет среди уже открытых заготовок ближайшую подходящую, а не
    // приписывается к последней. Последовательная сборка здесь разваливается:
    // колонки на одной высоте расходятся базовыми линиями на доли пункта, и
    // подпись из дальней колонки успевала открыть новую полосу, уводя в неё
    // подындексы соседней.
    let mut open: Vec<Vec<TextRun>> = Vec::new();
    for run in runs {
        let best = open
            .iter()
            .enumerate()
            .filter_map(|(index, band)| {
                let gap = horizontal_gap(&band_bbox(band), &run.bbox);
                let reach = opts.column_gap * band_size(band);
                (joins_band(band, &run, opts) && gap <= reach).then_some((index, gap))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index);

        match best {
            Some(index) => open[index].push(run),
            None => open.push(vec![run]),
        }
    }

    // Шаг 1½: сшивка полос-соседок.
    //
    // В вёрстке с формулами строка — «молния»: куски основного текста лежат
    // на одной базовой линии, математические островки — на другой, выше на
    // доли пункта. Островки идут в сортировке первыми, а между ними десятки
    // пунктов — каждый открывает собственную полосу, и до основного текста
    // строка уже разодрана. Прямо в цикле это не лечится: недостающие куски
    // ещё не обработаны. Зато после цикла соседство видно целиком — полосы,
    // лежащие на одной высоте и впритык по горизонтали, сшиваются обратно.
    let open = merge_bands(open, opts);

    // Шаг 2: внутри заготовки — разрез по близко стоящим колонкам.
    let mut lines: Vec<Line> = Vec::new();
    for band in open {
        for column in split_columns(band, opts) {
            lines.push(finish_line(column, opts));
        }
    }

    // Порядок чтения: сверху вниз, а внутри одной полосы — слева направо.
    // Базовые линии соседних колонок расходятся на доли пункта, поэтому
    // сравнивать их напрямую нельзя: правая колонка оказывалась «выше» левой.
    // Квантование в полосы даёт настоящий порядок сравнения, в отличие от
    // сравнения с допуском, которое нетранзитивно.
    let quantum = lines
        .iter()
        .map(Line::dominant_size)
        .fold(0.0f32, f32::max)
        .max(1.0)
        * opts.baseline_tol;
    lines.sort_by_key(|line| {
        let band = (line.baseline / quantum).round() as i64;
        (-band, ordered_float(line.bbox.left))
    });
    lines
}

/// Целочисленный ключ сортировки для `f32`: сравнение чисел с плавающей точкой
/// не даёт `Ord`, а порядок нужен полный.
/// Сшивает полосы, оказавшиеся кусками одной строки.
///
/// Правила те же, что и при сборке: общая высота с допуском по базовой линии,
/// горизонтальный зазор не больше полутора кеглей — и либо близкие кегли,
/// либо отношение «строка и её индекс». Родство гарнитур здесь не требуется
/// нарочно: молния как раз и состоит из чередования разных шрифтов.
/// Настоящие колонки таблиц защищены зазором: они расходятся дальше.
fn merge_bands(mut open: Vec<Vec<TextRun>>, opts: &BlockOptions) -> Vec<Vec<TextRun>> {
    loop {
        let mut merged_any = false;
        let mut result: Vec<Vec<TextRun>> = Vec::new();

        'bands: for band in open {
            for target in result.iter_mut() {
                if bands_belong_together(target, &band, opts) {
                    target.extend(band);
                    merged_any = true;
                    continue 'bands;
                }
            }
            result.push(band);
        }

        open = result;
        if !merged_any {
            return open;
        }
    }
}

/// Куски одной строки, разорванной чередованием шрифтов?
fn bands_belong_together(a: &[TextRun], b: &[TextRun], opts: &BlockOptions) -> bool {
    let size_a = band_size(a);
    let size_b = band_size(b);
    let reference = size_a.max(size_b);

    let offset = (band_baseline(a) - band_baseline(b)).abs();
    let gap = horizontal_gap(&band_bbox(a), &band_bbox(b));
    if gap > opts.column_gap * reference {
        return false;
    }

    // Обычные куски строки: одна высота, близкие кегли.
    if offset <= opts.baseline_tol * reference {
        let size_gap = (size_a - size_b).abs() / reference.max(0.01);
        if size_gap <= opts.line_size_tol {
            return true;
        }
    }
    // Строка и её индексы: один кусок заметно мельче, слегка смещён и набран
    // гарнитурой, родственной ближайшему соседу. Родство обязательно: без
    // него номер строки таблицы, стоящий вплотную к подписи, сливался бы с
    // ней — он такой же мелкий и на той же высоте.
    if size_a.min(size_b) >= reference * opts.script_size
        || offset > reference * opts.script_offset
        || gap > reference * opts.script_gap
    {
        return false;
    }
    let (small, big) = if size_a < size_b { (a, b) } else { (b, a) };
    let small_key = band_family(small);
    let host_key = big
        .iter()
        .min_by(|x, y| {
            horizontal_gap(&x.bbox, &band_bbox(small))
                .total_cmp(&horizontal_gap(&y.bbox, &band_bbox(small)))
        })
        .map(|host| crate::fonts::family_key(&host.style.family))
        .unwrap_or_default();
    related_families(&host_key, &small_key)
}

fn ordered_float(value: f32) -> i64 {
    (value * 100.0).round() as i64
}

/// Кегль полосы — по её самому длинному рану.
fn band_size(band: &[TextRun]) -> f32 {
    band.iter()
        .max_by_key(|r| r.text.chars().count())
        .map(|r| r.style.size)
        .unwrap_or(1.0)
        .max(1.0)
}

fn band_bbox(band: &[TextRun]) -> Rect {
    union_all(band.iter().map(|r| &r.bbox)).unwrap_or(Rect::ZERO)
}

/// Гарнитура полосы — по самому длинному рану, как кегль и базовая линия.
fn band_family(band: &[TextRun]) -> String {
    band.iter()
        .max_by_key(|r| r.text.chars().count())
        .map(|r| crate::fonts::family_key(&r.style.family))
        .unwrap_or_default()
}

fn band_baseline(band: &[TextRun]) -> f32 {
    band.iter()
        .max_by_key(|r| r.text.chars().count())
        .map(|r| r.baseline())
        .unwrap_or(0.0)
}

/// Лежит ли ран на той же высоте, что и полоса.
///
/// Масштаб задаёт **больший** из двух кеглей, а не кегль полосы. Иначе правило
/// работает только в одну сторону: подстрочный знак идёт в порядке обхода
/// после своей строки и примыкает к ней, а надстрочный — раньше, и полоса на
/// момент прихода основного текста состоит из одного мелкого знака. Сравнение
/// «11 pt мельче 6.4 pt» проваливалось, и сноски оставались сиротами.
fn joins_band(band: &[TextRun], run: &TextRun, opts: &BlockOptions) -> bool {
    let band_size = band_size(band);
    let reference = band_size.max(run.style.size);
    let offset = (run.baseline() - band_baseline(band)).abs();

    if offset <= opts.baseline_tol * reference {
        // Совпадения базовых линий мало: у таблицы с подписью они общие.
        // Нужна ещё родственность оформления — та же гарнитура либо тот же
        // кегль. Провал этой проверки — не приговор: подстрочный знак смещён
        // так слабо, что попадает и сюда, и его ещё рассмотрит ветка индекса
        // ниже.
        let same_family = band_family(band) == crate::fonts::family_key(&run.style.family);
        let size_gap = (band_size - run.style.size).abs() / reference.max(0.01);
        if same_family || size_gap <= opts.line_size_tol {
            return true;
        }
    }
    // Знак индекса: один из двух заметно мельче другого и смещён по вертикали.
    // Без этого «w₂» разваливалось на «w» в строке и «₂» отдельным блоком — на
    // страницах с формулами такого мусора набиралось больше, чем абзацев.
    // Индекс набирают той же гарнитурой, что и саму переменную, и ставят
    // вплотную к ней. Мелкий ран чужого шрифта в стороне — это не индекс, а
    // соседняя колонка: на страницах с таблицами номера строк ложатся на те
    // же базовые линии, что и подпись под рисунком, и по одному кеглю их от
    // подстрочных знаков не отличить.
    let smaller = band_size.min(run.style.size);
    let gap = horizontal_gap(&band_bbox(band), &run.bbox);
    if smaller >= reference * opts.script_size
        || offset > reference * opts.script_offset
        || gap > reference * opts.script_gap
    {
        return false;
    }
    // Гарнитуру индекса сверяем не с доминантой полосы, а с ближайшим по
    // горизонтали раном — тем самым, к которому индекс прижат. В латеховской
    // вёрстке строка пёстрая: текст MinionPro, переменная LMRoman9-Italic,
    // её индекс LMRoman7 — доминанта полосы тут ни при чём.
    let run_key = crate::fonts::family_key(&run.style.family);
    let host_key = band
        .iter()
        .min_by(|a, b| {
            horizontal_gap(&a.bbox, &run.bbox).total_cmp(&horizontal_gap(&b.bbox, &run.bbox))
        })
        .map(|host| crate::fonts::family_key(&host.style.family))
        .unwrap_or_default();
    related_families(&host_key, &run_key)
}

/// Родственны ли два ключа гарнитур.
///
/// Точное совпадение — родство. Совпадение после отрезания цифрового хвоста —
/// тоже: оптические размеры Latin Modern зовутся `LMRoman5`, `LMRoman7`,
/// `LMRoman9`, и индекс при переменной почти всегда набран соседним оптическим
/// размером того же семейства. Пустые ключи не считаются: у безымянного шрифта
/// родственников нет.
fn related_families(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let strip = |key: &str| {
        key.trim_end_matches(|ch: char| ch.is_ascii_digit())
            .to_owned()
    };
    let (a, b) = (strip(a), strip(b));
    !a.is_empty() && a == b
}

/// Режет полосу на колонки по горизонтальным разрывам.
///
/// Порог двойной: безусловный по кеглю и относительный — во сколько раз зазор
/// больше обычного на этой же полосе. Второй и ловит близко стоящие колонки
/// подписей, где абсолютный зазор мал, но всё равно на порядок больше
/// межсловного.
fn split_columns(mut band: Vec<TextRun>, opts: &BlockOptions) -> Vec<Vec<TextRun>> {
    band.sort_by(|a, b| a.bbox.left.total_cmp(&b.bbox.left));
    if band.len() < 2 {
        return vec![band];
    }

    let size = band_size(&band);
    let gaps: Vec<f32> = band
        .windows(2)
        .map(|pair| horizontal_gap(&pair[0].bbox, &pair[1].bbox))
        .collect();

    // За «обычный» зазор берём наименьший, а не медианный.
    //
    // Медиана здесь обманывает: если полоса состоит из трёх колонок по два
    // рана, половина зазоров сама является межколоночной, и медиана
    // оказывается равна тому, что мы пытаемся обнаружить. Наименьший зазор —
    // это всегда межсловный пробел, и он честно задаёт масштаб строки.
    let typical = gaps
        .iter()
        .copied()
        .filter(|g| *g > 0.0)
        .fold(f32::INFINITY, f32::min);
    let typical = if typical.is_finite() { typical } else { 0.0 };
    let absolute = opts.column_gap * size;
    // Относительный порог не опускаем ниже разумного: на строке из двух слов
    // медиана вырождается, и любой пробел выглядел бы «в разы больше».
    let relative = (typical * opts.column_gap_ratio).max(size * opts.column_gap * 0.5);
    let threshold = absolute.min(relative);

    let mut columns = Vec::new();
    let mut current = Vec::new();
    for (index, run) in band.into_iter().enumerate() {
        if index > 0 && gaps[index - 1] > threshold && !current.is_empty() {
            columns.push(std::mem::take(&mut current));
        }
        current.push(run);
    }
    if !current.is_empty() {
        columns.push(current);
    }
    columns
}

/// Расстояние между прямоугольниками по горизонтали; 0, если они пересекаются.
fn horizontal_gap(a: &Rect, b: &Rect) -> f32 {
    if b.left >= a.right {
        b.left - a.right
    } else if a.left >= b.right {
        a.left - b.right
    } else {
        0.0
    }
}

/// Собирает строку: упорядочивает раны слева направо и восстанавливает
/// пробелы.
///
/// Порядок наводится здесь, а не при сортировке: раны с почти одинаковой
/// базовой линией приходят вперемешку, и полагаться на исходный порядок
/// нельзя. Пробелы вставляются после упорядочивания — в PDF слова сплошь и
/// рядом разделены только позиционированием, без глифа пробела.
fn finish_line(mut runs: Vec<TextRun>, opts: &BlockOptions) -> Line {
    runs.sort_by(|a, b| a.bbox.left.total_cmp(&b.bbox.left));

    for index in 1..runs.len() {
        let (left, right) = runs.split_at_mut(index);
        let previous = &left[index - 1];
        let run = &mut right[0];

        let reference = previous.style.size.max(run.style.size);
        let gap = run.bbox.left - previous.bbox.right;
        if gap > opts.space_gap * reference
            && !run.text.starts_with(' ')
            && !previous.text.ends_with(' ')
        {
            run.text.insert(0, ' ');
        }
    }

    let bbox = union_all(runs.iter().map(|r| &r.bbox)).unwrap_or(Rect::ZERO);
    let baseline = runs.first().map(|r| r.baseline()).unwrap_or(0.0);
    Line {
        runs,
        bbox,
        baseline,
    }
}

/// Открытый блок в процессе сборки.
struct Open {
    lines: Vec<Line>,
    /// Левый край, заданный строками начиная со второй. Первая строка не в
    /// счёт: у неё может быть абзацный отступ.
    body_left: Option<f32>,
    leadings: Vec<f32>,
}

impl Open {
    /// Опорный интерлиньяж: медиана уже набранных шагов, а пока их нет —
    /// оценка по кеглю.
    fn reference_leading(&self, size: f32) -> f32 {
        median(&self.leadings).unwrap_or(size * 1.2)
    }
}

/// Шаг 2: строки → абзацы.
pub fn group_lines(lines: Vec<Line>, opts: &BlockOptions) -> Vec<Block> {
    let mut open: Vec<Open> = Vec::new();
    let mut done: Vec<Block> = Vec::new();

    for line in lines {
        let size = line.dominant_size().max(1.0);

        // Ближайший сверху совместимый блок. Именно «ближайший», а не
        // «последний»: в двух колонках строки чередуются.
        let best = open
            .iter()
            .enumerate()
            .filter_map(|(i, blk)| {
                let last = blk.lines.last()?;
                let gap = last.baseline - line.baseline;
                accepts(blk, last, &line, size, gap, opts).then_some((i, gap))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i);

        match best {
            Some(i) => {
                let blk = &mut open[i];
                let last = blk.lines.last().expect("открытый блок непуст");
                blk.leadings.push(last.baseline - line.baseline);
                if blk.lines.len() == 1 {
                    blk.body_left = Some(line.bbox.left);
                }
                blk.lines.push(line);
            }
            None => open.push(Open {
                lines: vec![line],
                body_left: None,
                leadings: Vec::new(),
            }),
        }

        // Блоки, до которых текущая строка уже не дотянулась бы по вертикали,
        // больше не примут ничего — закрываем, чтобы список не рос.
        let cutoff = open
            .last()
            .and_then(|b| b.lines.last())
            .map(|l| l.baseline)
            .unwrap_or(f32::NEG_INFINITY);
        let mut i = 0;
        while i < open.len() {
            let last_baseline = open[i].lines.last().map(|l| l.baseline).unwrap_or(0.0);
            let slack = open[i].reference_leading(size) * opts.para_break;
            if last_baseline - cutoff > slack * 2.0 {
                done.push(seal(open.remove(i)));
            } else {
                i += 1;
            }
        }
    }

    done.extend(open.into_iter().map(seal));
    // Возвращаем в порядке чтения: закрытие блоков идёт вразнобой.
    done.sort_by(|a, b| {
        b.bbox
            .top
            .total_cmp(&a.bbox.top)
            .then_with(|| a.bbox.left.total_cmp(&b.bbox.left))
    });
    done
}

/// Можно ли присоединить `line` к открытому блоку.
fn accepts(blk: &Open, last: &Line, line: &Line, size: f32, gap: f32, opts: &BlockOptions) -> bool {
    // Строка должна лежать строго ниже последней строки блока.
    if gap <= 0.0 {
        return false;
    }
    // Разные колонки: вертикальные метрики совпадают, различает только
    // горизонталь.
    if last.bbox.h_overlap_ratio(&line.bbox) < opts.min_h_overlap {
        return false;
    }
    let last_size = last.dominant_size().max(1.0);
    let ratio = (size / last_size).max(last_size / size);
    if ratio > opts.size_ratio {
        return false;
    }
    // Оформление обязано совпадать. Без этой проверки жирный заголовок
    // сливался с курсивным подзаголовком того же кегля — ровно то, что
    // происходило в оглавлении книги: «1. THE PERCEPTRON» и «Build Your Own
    // Perceptron» попадали в один блок.
    if !same_typeface(last, line) {
        return false;
    }
    let reference = blk.reference_leading(size);
    if gap > opts.para_break * reference || gap < 0.5 * reference {
        return false;
    }
    aligns(blk, last, line, size, opts)
}

/// Одинаковы ли гарнитура и начертание у преобладающих ранов двух строк.
///
/// Сравнивается имя семейства без префикса подмножества, признак полужирного и
/// курсив. Цвет намеренно не учитывается: выделить слово цветом внутри абзаца —
/// обычное дело, и разрывать из-за этого абзац неправильно.
fn same_typeface(a: &Line, b: &Line) -> bool {
    match (a.dominant_style(), b.dominant_style()) {
        (Some(a), Some(b)) => {
            a.clean_family() == b.clean_family()
                && a.is_bold() == b.is_bold()
                && a.italic == b.italic
        }
        // Строка без ранов ничего не утверждает об оформлении.
        _ => true,
    }
}

fn aligns(blk: &Open, last: &Line, line: &Line, size: f32, opts: &BlockOptions) -> bool {
    let tol = opts.align_tol * size;
    let left_ref = blk.body_left.unwrap_or(last.bbox.left);
    if (line.bbox.left - left_ref).abs() <= tol {
        return true;
    }
    // Абзацный отступ: блок пока из одной строки, и она начиналась правее.
    if blk.lines.len() == 1
        && line.bbox.left < last.bbox.left
        && last.bbox.left - line.bbox.left <= opts.max_indent * size
    {
        return true;
    }
    if (line.bbox.center_x() - last.bbox.center_x()).abs() <= tol {
        return true;
    }
    (line.bbox.right - last.bbox.right).abs() <= tol
}

fn seal(open: Open) -> Block {
    let bbox = union_all(open.lines.iter().map(|l| &l.bbox)).unwrap_or(Rect::ZERO);
    let align = detect_align(&open.lines);
    Block {
        lines: open.lines,
        bbox,
        align,
        rotation: 0.0,
        mark: None,
        style: None,
    }
}

/// Восстановление выключки по геометрии строк.
fn detect_align(lines: &[Line]) -> Align {
    if lines.len() < 2 {
        return Align::Left;
    }
    let size = lines
        .iter()
        .map(Line::dominant_size)
        .fold(0.0f32, f32::max)
        .max(1.0);
    let tol = 0.5 * size;

    // Первая строка может иметь абзацный отступ — на длинных блоках
    // исключаем её из оценки краёв.
    let body = if lines.len() >= 3 { &lines[1..] } else { lines };

    let left_ok = spread(body.iter().map(|l| l.bbox.left)) <= tol;
    let center_ok = spread(body.iter().map(|l| l.bbox.center_x())) <= tol;
    let right_edge_ok = spread(body.iter().map(|l| l.bbox.right)) <= tol;

    // Выключка по формату: правый край ровный у всех строк, кроме последней.
    // Нужно минимум две полные строки, иначе «ровный край» из одной строки
    // получается автоматически.
    let justified = body.len() >= 3
        && left_ok
        && spread(body[..body.len() - 1].iter().map(|l| l.bbox.right)) <= tol;

    if justified {
        Align::Justify
    } else if left_ok {
        Align::Left
    } else if center_ok {
        Align::Center
    } else if right_edge_ok {
        Align::Right
    } else {
        Align::Left
    }
}

fn spread(values: impl Iterator<Item = f32>) -> f32 {
    let (min, max) = values.fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| {
        (lo.min(v), hi.max(v))
    });
    if min > max { 0.0 } else { max - min }
}

fn median(values: &[f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f32::total_cmp);
    Some(v[v.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Style;

    /// Ран с базовой линией `y`, левым краем `x` и шириной `w`.
    fn run(text: &str, x: f32, y: f32, w: f32, size: f32) -> TextRun {
        TextRun {
            text: text.into(),
            style: Style {
                size,
                ..Default::default()
            },
            origin: (x, y),
            bbox: Rect::new(x, y - size * 0.22, x + w, y + size * 0.78),
        }
    }

    /// Ран с явной гарнитурой — для проверок, где важен именно шрифт.
    fn run_in(text: &str, x: f32, y: f32, w: f32, size: f32, family: &str) -> TextRun {
        let mut run = run(text, x, y, w, size);
        run.style.family = family.into();
        run
    }

    /// Абзац из `n` строк: левый край `x`, ширина `w`, шаг `leading`.
    fn paragraph(
        prefix: &str,
        x: f32,
        top: f32,
        w: f32,
        size: f32,
        leading: f32,
        n: usize,
    ) -> Vec<TextRun> {
        (0..n)
            .map(|i| {
                run(
                    &format!("{prefix}{i}"),
                    x,
                    top - leading * i as f32,
                    w,
                    size,
                )
            })
            .collect()
    }

    #[test]
    fn runs_on_one_baseline_form_a_single_line() {
        let runs = vec![
            run("Wirtschaft", 100.0, 700.0, 60.0, 11.0),
            run("und Recht", 165.0, 700.0, 50.0, 11.0),
        ];
        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "Wirtschaft und Recht");
    }

    #[test]
    fn wide_gap_inserts_a_space_narrow_gap_does_not() {
        let opts = BlockOptions::default();
        // Зазор 5 pt при кегле 11 — больше 0.22 * 11 = 2.42.
        let spaced = build_lines(
            vec![
                run("aa", 100.0, 700.0, 20.0, 11.0),
                run("bb", 125.0, 700.0, 20.0, 11.0),
            ],
            &opts,
        );
        assert_eq!(spaced[0].text(), "aa bb");

        // Зазор 1 pt — кернинг внутри слова, пробела быть не должно.
        let tight = build_lines(
            vec![
                run("aa", 100.0, 700.0, 20.0, 11.0),
                run("bb", 121.0, 700.0, 20.0, 11.0),
            ],
            &opts,
        );
        assert_eq!(tight[0].text(), "aabb");
    }

    #[test]
    fn close_caption_columns_split_by_relative_gap() {
        // Настоящие числа со страницы 20 книги: подписи под рисунком, кегль
        // 8.5, межсловный зазор 2.6, межколоночный всего 18. Абсолютного
        // порога тут мало — колонки различает только соотношение.
        let runs = vec![
            run("All dials set to zero", 197.0, 109.0, 69.0, 8.45),
            run("Turning up w", 284.0, 109.0, 49.0, 8.45),
            run("results", 335.4, 109.0, 28.0, 8.45),
            run("Turning up w", 381.0, 109.0, 48.0, 8.45),
            run("rotates", 431.6, 109.0, 29.0, 8.45),
        ];

        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(lines.len(), 3, "три колонки подписей: {lines:#?}");
        assert_eq!(lines[0].text(), "All dials set to zero");
        assert_eq!(lines[1].text(), "Turning up w results");
        assert_eq!(lines[2].text(), "Turning up w rotates");
    }

    #[test]
    fn subscript_stays_with_its_line() {
        // «w₂»: подындекс мельче основного кегля и сидит ниже базовой линии.
        // Раньше он становился отдельным блоком кегля 4.9.
        let runs = vec![
            run("Turning up w", 284.0, 109.0, 49.0, 8.45),
            run("2", 333.0, 106.5, 3.0, 4.93),
            run("results", 338.0, 109.0, 28.0, 8.45),
        ];

        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(
            lines.len(),
            1,
            "подындекс обязан остаться в строке: {lines:#?}"
        );
        assert_eq!(lines[0].text(), "Turning up w2 results");
    }

    #[test]
    fn superscript_stays_with_its_line() {
        // Настоящие числа со страницы 25: сноска кеглем 6.4 на базовой линии
        // 449 при основном тексте 11 pt на 446. Надындекс идёт в порядке
        // обхода раньше своей строки — на этом правило и ломалось.
        let runs = vec![
            run("2", 113.0, 449.0, 3.0, 6.41),
            run("dog faces", 73.0, 446.0, 40.0, 11.0),
            run("as shown in Figure 1.16", 116.0, 446.0, 120.0, 11.0),
        ];

        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(
            lines.len(),
            1,
            "сноска обязана остаться в строке: {lines:#?}"
        );
        assert!(
            lines[0].text().starts_with("dog faces2"),
            "получено {:?}",
            lines[0].text()
        );
    }

    #[test]
    fn latex_optical_sizes_keep_the_subscript_in_line() {
        // Строка как на стр. 29 книги: текст MinionPro, переменная
        // LMRoman9-Italic, её индекс — LMRoman7-Regular вдвое мельче и чуть
        // ниже. Раньше индекс отваливался отдельным блоком: гарнитуры
        // сравнивались буквально, а `lmroman9` != `lmroman7`.
        let runs = vec![
            run_in(
                "of the perceptron ",
                108.0,
                165.5,
                310.0,
                9.0,
                "MinionPro-Regular",
            ),
            run_in("w", 421.5, 166.1, 6.5, 9.0, "LMRoman9-Italic"),
            run_in("1", 427.7, 164.2, 3.0, 5.25, "LMRoman7-Regular"),
            run_in("=-1", 430.7, 166.1, 15.0, 9.0, "LMRoman8-Regular"),
        ];
        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(lines.len(), 1, "индекс обязан остаться в своей строке");
        assert_eq!(lines[0].runs.len(), 4);
    }

    #[test]
    fn full_size_run_on_another_baseline_is_not_a_subscript() {
        // Смещение то же, но кегль обычный — это соседняя строка, а не знак.
        let runs = vec![
            run("первая", 60.0, 109.0, 50.0, 8.45),
            run("вторая", 60.0, 104.0, 50.0, 8.45),
        ];
        assert_eq!(build_lines(runs, &BlockOptions::default()).len(), 2);
    }

    #[test]
    fn columns_split_even_when_the_right_one_sits_a_hair_higher() {
        // Случай из оглавления книги: у подписи в правой колонке базовая линия
        // выше на доли пункта, поэтому при сортировке она приходит раньше
        // левой колонки. Знаковый зазор при этом отрицателен, и прежняя
        // проверка разрыва колонок его пропускала.
        let runs = vec![
            run(
                "The misconception that almost stopped AI",
                379.0,
                593.4,
                163.0,
                10.0,
            ),
            run("Supporting Code", 72.0, 593.0, 81.0, 12.0),
        ];

        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(
            lines.len(),
            2,
            "колонки обязаны остаться разными строками: {lines:#?}"
        );
        // И порядок должен быть слева направо, а не в порядке поступления.
        assert_eq!(lines[0].text(), "Supporting Code");
        assert_eq!(lines[1].text(), "The misconception that almost stopped AI");
    }

    #[test]
    fn runs_are_ordered_left_to_right_regardless_of_input_order() {
        // Зазор межсловный (3 пункта), поэтому раны обязаны остаться одной
        // строкой; проверяется именно порядок.
        let runs = vec![
            run("вторая", 123.0, 700.2, 50.0, 11.0),
            run("первая", 60.0, 700.0, 60.0, 11.0),
        ];
        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "первая вторая");
    }

    #[test]
    fn two_columns_do_not_merge_into_one_line() {
        // Одна базовая линия, но между колонками разрыв 60 pt.
        let runs = vec![
            run("левая", 60.0, 700.0, 180.0, 11.0),
            run("правая", 300.0, 700.0, 180.0, 11.0),
        ];
        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(lines.len(), 2, "колонки должны остаться разными строками");
        assert_eq!(lines[0].text(), "левая");
        assert_eq!(lines[1].text(), "правая");
    }

    #[test]
    fn even_leading_keeps_one_block_wide_gap_splits_it() {
        let opts = BlockOptions::default();

        let one = detect_blocks(paragraph("a", 60.0, 700.0, 200.0, 11.0, 13.0, 5), &opts);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].lines.len(), 5);

        // Два абзаца: внутри шаг 13, между ними — 30.
        let mut two = paragraph("a", 60.0, 700.0, 200.0, 11.0, 13.0, 3);
        two.extend(paragraph(
            "b",
            60.0,
            700.0 - 13.0 * 2.0 - 30.0,
            200.0,
            11.0,
            13.0,
            3,
        ));
        let blocks = detect_blocks(two, &opts);
        assert_eq!(
            blocks.len(),
            2,
            "увеличенный отбивкой шаг обязан разорвать блок"
        );
        assert_eq!(blocks[0].lines.len(), 3);
        assert_eq!(blocks[1].lines.len(), 3);
    }

    #[test]
    fn modest_paragraph_spacing_still_splits() {
        // Настоящие базовые линии со страницы 25 книги: внутри абзаца шаг
        // 13–14, отбивка между абзацами всего 19.
        let baselines = [721.0, 708.0, 695.0, 681.0, 662.0, 649.0, 635.0, 622.0];
        let runs: Vec<TextRun> = baselines
            .iter()
            .enumerate()
            .map(|(i, y)| run(&format!("строка {i}"), 73.0, *y, 285.0, 11.0))
            .collect();

        let blocks = detect_blocks(runs, &BlockOptions::default());
        assert_eq!(
            blocks.len(),
            2,
            "отбивка 19 против шага 13 — это два абзаца: {blocks:#?}"
        );
        assert_eq!(blocks[0].lines.len(), 4);
        assert_eq!(blocks[1].lines.len(), 4);
    }

    #[test]
    fn rounding_jitter_in_leading_keeps_one_paragraph() {
        // Тот же абзац, но шаг гуляет округлением: 13, 14, 13, 14.
        let baselines = [721.0, 708.0, 694.0, 681.0, 667.0];
        let runs: Vec<TextRun> = baselines
            .iter()
            .enumerate()
            .map(|(i, y)| run(&format!("строка {i}"), 73.0, *y, 285.0, 11.0))
            .collect();

        assert_eq!(detect_blocks(runs, &BlockOptions::default()).len(), 1);
    }

    #[test]
    fn heading_is_separated_from_body_by_font_size() {
        let opts = BlockOptions::default();
        let mut runs = vec![run("Заголовок", 60.0, 700.0, 200.0, 20.0)];
        runs.extend(paragraph("t", 60.0, 672.0, 200.0, 11.0, 13.0, 3));
        let blocks = detect_blocks(runs, &opts);
        assert_eq!(blocks.len(), 2, "кегль 20 и 11 не могут быть одним абзацем");
        assert_eq!(blocks[0].text(), "Заголовок");
    }

    #[test]
    fn interleaved_columns_are_grouped_per_column() {
        let opts = BlockOptions::default();
        // Строки обеих колонок идут вперемежку по вертикали — именно тот
        // случай, на котором ломается последовательная склейка.
        let mut runs = Vec::new();
        for i in 0..6 {
            let y = 700.0 - 13.0 * i as f32;
            runs.push(run(&format!("L{i}"), 60.0, y, 180.0, 11.0));
            runs.push(run(&format!("R{i}"), 320.0, y, 180.0, 11.0));
        }
        let blocks = detect_blocks(runs, &opts);
        assert_eq!(blocks.len(), 2, "должно получиться ровно две колонки");
        assert!(blocks.iter().all(|b| b.lines.len() == 6));
        assert!(blocks[0].text().starts_with("L0"));
        assert!(blocks[1].text().starts_with("R0"));
    }

    #[test]
    fn first_line_indent_stays_in_the_same_block() {
        let opts = BlockOptions::default();
        let mut runs = vec![run("Первая с отступом", 80.0, 700.0, 180.0, 11.0)];
        runs.extend(paragraph("x", 60.0, 687.0, 200.0, 11.0, 13.0, 3));
        let blocks = detect_blocks(runs, &opts);
        assert_eq!(
            blocks.len(),
            1,
            "абзацный отступ не должен отрывать первую строку"
        );
        assert_eq!(blocks[0].lines.len(), 4);
    }

    #[test]
    fn centred_lines_are_detected_as_centre_aligned() {
        let opts = BlockOptions::default();
        // Строки разной длины с общим центром 300.
        let runs = vec![
            run("длинная строка", 200.0, 700.0, 200.0, 11.0),
            run("короче", 250.0, 687.0, 100.0, 11.0),
            run("ещё короче", 240.0, 674.0, 120.0, 11.0),
        ];
        let blocks = detect_blocks(runs, &opts);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].align, Align::Center);
    }

    #[test]
    fn flush_both_edges_is_detected_as_justified() {
        let opts = BlockOptions::default();
        // Четыре строки одинаковой ширины и короткая последняя.
        let mut runs = paragraph("j", 60.0, 700.0, 200.0, 11.0, 13.0, 4);
        runs.push(run("хвост", 60.0, 700.0 - 13.0 * 4.0, 70.0, 11.0));
        let blocks = detect_blocks(runs, &opts);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].align, Align::Justify);
    }

    /// Ран с явным оформлением.
    fn styled(text: &str, x: f32, y: f32, w: f32, size: f32, style: Style) -> TextRun {
        TextRun {
            text: text.into(),
            style: Style { size, ..style },
            origin: (x, y),
            bbox: Rect::new(x, y - size * 0.22, x + w, y + size * 0.78),
        }
    }

    #[test]
    fn bold_heading_does_not_merge_with_italic_subtitle() {
        // Случай из оглавления настоящей книги: «1. THE PERCEPTRON» жирным и
        // «Build Your Own Perceptron» курсивом того же кегля, вплотную и с
        // одного левого края. По геометрии — один блок, по смыслу — разные.
        let bold = Style {
            weight: 700,
            ..Default::default()
        };
        let italic = Style {
            italic: true,
            ..Default::default()
        };

        let runs = vec![
            styled("1. THE PERCEPTRON", 60.0, 700.0, 200.0, 11.0, bold),
            styled(
                "Build Your Own Perceptron",
                60.0,
                686.0,
                200.0,
                11.0,
                italic.clone(),
            ),
            styled("Supporting Code", 60.0, 672.0, 200.0, 11.0, italic),
        ];

        let blocks = detect_blocks(runs, &BlockOptions::default());
        assert_eq!(
            blocks.len(),
            2,
            "заголовок и подпункты — разные блоки: {blocks:#?}"
        );
        assert_eq!(blocks[0].text(), "1. THE PERCEPTRON");
        assert_eq!(blocks[1].lines.len(), 2);
    }

    #[test]
    fn different_families_stay_apart() {
        let serif = Style {
            family: "Times".into(),
            ..Default::default()
        };
        let sans = Style {
            family: "Arial".into(),
            ..Default::default()
        };
        let runs = vec![
            styled("первая", 60.0, 700.0, 200.0, 11.0, serif),
            styled("вторая", 60.0, 687.0, 200.0, 11.0, sans),
        ];
        assert_eq!(detect_blocks(runs, &BlockOptions::default()).len(), 2);
    }

    #[test]
    fn subset_prefix_does_not_split_a_paragraph() {
        // Один и тот же шрифт может прийти с разными префиксами подмножества.
        // Это деталь упаковки файла, а не смена гарнитуры.
        let a = Style {
            family: "ABCDEF+Georgia".into(),
            ..Default::default()
        };
        let b = Style {
            family: "GHIJKL+Georgia".into(),
            ..Default::default()
        };
        let runs = vec![
            styled("первая строка", 60.0, 700.0, 200.0, 11.0, a),
            styled("вторая строка", 60.0, 687.0, 200.0, 11.0, b),
        ];
        assert_eq!(detect_blocks(runs, &BlockOptions::default()).len(), 1);
    }

    #[test]
    fn partially_overlapping_columns_do_not_merge() {
        // Правая колонка начинается в пределах левой, но перекрытие мало —
        // это соседние колонки, а не продолжение абзаца.
        let opts = BlockOptions::default();
        let runs = vec![
            run("левая колонка", 60.0, 700.0, 240.0, 11.0),
            run("правая колонка", 260.0, 687.0, 240.0, 11.0),
        ];
        let blocks = detect_blocks(runs, &opts);
        assert_eq!(
            blocks.len(),
            2,
            "слабое перекрытие не повод склеивать: {blocks:#?}"
        );
    }

    #[test]
    fn empty_input_yields_no_blocks() {
        assert!(detect_blocks(Vec::new(), &BlockOptions::default()).is_empty());
    }

    /// Номер строки таблицы, легший на базовую линию подписи, не должен
    /// попадать в её текст.
    ///
    /// Так устроены страницы с таблицами токенов: колонка номеров набрана
    /// мелким LMRoman поверх той же полосы, где идёт подпись MinionPro.
    /// Геометрия их не разделяет — раны перекрываются и по горизонтали, — а
    /// правка подписи утаскивала номера в текст и разваливала таблицу.
    #[test]
    fn a_table_number_does_not_join_a_caption_line() {
        let runs = vec![
            run_in("32", 86.0, 100.0, 8.0, 7.0, "LMRoman8-Regular"),
            run_in(
                "some of the 128,256 tokens in Llama's",
                81.0,
                100.0,
                137.0,
                9.0,
                "MinionPro-Regular",
            ),
        ];
        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(
            lines.len(),
            2,
            "номер и подпись обязаны разойтись: {lines:?}"
        );
    }

    /// Полужирный зачин той же гарнитуры остаётся частью строки.
    #[test]
    fn a_bold_lead_in_stays_in_its_line() {
        let runs = vec![
            run_in(
                "Figure 2.4 | Some Tokens.",
                81.0,
                100.0,
                98.0,
                9.0,
                "MinionPro-Bold",
            ),
            run_in("Here are", 180.0, 100.0, 33.0, 9.0, "MinionPro-Regular"),
        ];
        let lines = build_lines(runs, &BlockOptions::default());
        assert_eq!(
            lines.len(),
            1,
            "зачин и продолжение — одна строка: {lines:?}"
        );
    }
}

#[cfg(test)]
mod latex_line_tests {
    use super::*;
    use crate::geom::Rect;
    use crate::model::{Style, TextRun};

    fn r(text: &str, family: &str, size: f32, left: f32, right: f32, bottom: f32) -> TextRun {
        let baseline = bottom + size * 0.22;
        TextRun {
            text: text.into(),
            style: Style {
                size,
                family: family.into(),
                ..Default::default()
            },
            origin: (left, baseline),
            bbox: Rect::new(left, bottom, right, bottom + size * 1.42),
        }
    }

    /// Строка 5 подписи со стр. 29 книги, координаты из probe_runs.
    #[test]
    fn the_caption_line_with_two_subscripts_stays_whole() {
        let runs = vec![
            r(
                "choosing targets of ",
                "MinionPro-Regular",
                9.0,
                108.3,
                178.1,
                133.1,
            ),
            r("y", "LMRoman9-Italic", 9.0, 178.1, 182.8, 133.7),
            r("=1", "LMRoman8-Regular", 9.0, 182.6, 194.8, 133.7),
            r(" and ", "MinionPro-Regular", 9.0, 194.8, 212.4, 133.1),
            r("y", "LMRoman9-Italic", 9.0, 212.4, 217.1, 133.7),
            r("=-1. ", "LMRoman8-Regular", 9.0, 216.9, 238.1, 133.7),
            r(
                "Given the target ",
                "MinionPro-Regular",
                9.0,
                238.1,
                297.8,
                133.1,
            ),
            r("y", "LMRoman9-Italic", 9.0, 297.8, 302.6, 133.7),
            r("=-1 ", "LMRoman8-Regular", 9.0, 302.3, 320.9, 133.7),
            r(
                "for our inputs ",
                "MinionPro-Regular",
                9.0,
                320.9,
                372.5,
                133.1,
            ),
            r("x", "LMRoman9-Italic", 9.0, 372.5, 377.3, 133.7),
            r("1", "LMRoman7-Regular", 5.25, 376.8, 379.8, 131.8),
            r("=-1", "LMRoman8-Regular", 9.0, 379.8, 395.2, 133.7),
            r(", ", "MinionPro-Regular", 9.0, 395.2, 399.3, 133.1),
            r("x", "LMRoman9-Italic", 9.0, 399.3, 404.0, 133.7),
            r("2", "LMRoman7-Regular", 5.25, 403.6, 406.5, 131.8),
            r("=-1", "LMRoman8-Regular", 9.0, 406.5, 421.9, 133.7),
            r(
                ", we can measure our sys",
                "MinionPro-Regular",
                9.0,
                421.9,
                510.3,
                133.1,
            ),
        ];
        let lines = build_lines(runs, &BlockOptions::default());
        for line in &lines {
            eprintln!(
                "СТРОКА y {:.1} x {:.1}..{:.1}: {:?}",
                line.baseline,
                line.bbox.left,
                line.bbox.right,
                line.runs
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(
            lines.len(),
            1,
            "строка с двумя индексами обязана быть одной"
        );
    }
}
