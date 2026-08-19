//! Кэш растров страниц.
//!
//! Ключевая деталь для больших документов: масштаб *квантуется*. При плавном
//! зуме каждый кадр даёт новое значение scale, и без квантования кэш
//! промахивался бы всегда, а рендер шёл бы непрерывно. Бакеты растут
//! геометрически — по четыре на удвоение, то есть шаг около 19%. Промежуточные
//! значения показываются растяжением ближайшего бакета: разница незаметна,
//! зато прокрутка и зум не упираются в рендер.
//!
//! Вытеснение считается по байтам, а не по числу записей: разворот A3 при
//! 400% занимает столько же, сколько три сотни миниатюр.
//!
//! Кэш обобщён по типу значения. Ядро складывает в него [`Bitmap`], а UI —
//! уже загруженные текстуры, чтобы одни и те же пиксели не лежали в памяти
//! дважды. Размер записи передаётся явно при вставке: так тип значения может
//! быть чужим (например, `Arc<RenderImage>` из gpui), не требуя реализации
//! локального трейта.

use std::sync::Arc;

use lru::LruCache;
use rustc_hash::FxBuildHasher;

use crate::geom::Rotation;

/// Растр в формате BGRA8 — ровно то, что отдаёт `FPDFBitmap_BGRA`, и ровно
/// то, что ожидает `gpui::RenderImage`. Перекладки каналов нигде нет.
#[derive(Debug)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Bitmap {
    pub fn new(width: u32, height: u32) -> Bitmap {
        Bitmap {
            width,
            height,
            pixels: vec![0; (width as usize * height as usize) * 4],
        }
    }

    pub fn byte_size(&self) -> usize {
        self.pixels.len()
    }
}

/// Сколько бакетов приходится на удвоение масштаба.
const BUCKETS_PER_OCTAVE: f32 = 4.0;

/// Квантованный масштаб. Хранится целым, чтобы участвовать в ключе хэша.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ZoomBucket(pub i16);

impl ZoomBucket {
    /// Ближайший бакет для запрошенного масштаба.
    pub fn from_scale(scale: f32) -> ZoomBucket {
        let clamped = scale.clamp(0.02, 64.0);
        ZoomBucket((clamped.log2() * BUCKETS_PER_OCTAVE).round() as i16)
    }

    /// Фактический масштаб, с которым нужно рендерить этот бакет.
    pub fn scale(self) -> f32 {
        (self.0 as f32 / BUCKETS_PER_OCTAVE).exp2()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub page: u32,
    pub zoom: ZoomBucket,
    pub rotation: Rotation,
}

/// LRU-кэш с бюджетом в байтах.
pub struct TileCache<V = Arc<Bitmap>> {
    lru: LruCache<TileKey, (V, usize), FxBuildHasher>,
    bytes: usize,
    budget: usize,
}

impl<V: Clone> TileCache<V> {
    /// `budget` — верхняя граница занятой памяти в байтах.
    pub fn new(budget: usize) -> TileCache<V> {
        TileCache {
            lru: LruCache::unbounded_with_hasher(FxBuildHasher),
            bytes: 0,
            budget,
        }
    }

    pub fn get(&mut self, key: &TileKey) -> Option<V> {
        self.lru.get(key).map(|(value, _)| value.clone())
    }

    /// Проверка без обновления давности — для решения «а надо ли рендерить»,
    /// которое не должно влиять на порядок вытеснения.
    pub fn contains(&self, key: &TileKey) -> bool {
        self.lru.contains(key)
    }

    /// Кладёт запись в кэш и возвращает всё, что при этом вытеснено.
    ///
    /// Возврат вытесненного — не удобство, а необходимость: у значения может
    /// быть ресурс за пределами оперативной памяти (текстура в атласе GPU),
    /// который освобождается не через `Drop`, а явным вызовом. Молча ронять
    /// такие значения означает течь до конца сеанса.
    #[must_use = "вытесненные значения нужно освободить явно"]
    pub fn insert(&mut self, key: TileKey, value: V, bytes: usize) -> Vec<V> {
        let mut evicted = Vec::new();
        if let Some((old, old_bytes)) = self.lru.put(key, (value, bytes)) {
            self.bytes = self.bytes.saturating_sub(old_bytes);
            evicted.push(old);
        }
        self.bytes += bytes;
        self.evict(&mut evicted);
        evicted
    }

    /// Сбрасывает все растры страницы. Вызывается после правки её содержимого:
    /// геометрия не изменилась, но пиксели устарели.
    #[must_use = "вытесненные значения нужно освободить явно"]
    pub fn invalidate_page(&mut self, page: u32) -> Vec<V> {
        let stale: Vec<TileKey> = self
            .lru
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| k.page == page)
            .collect();
        let mut evicted = Vec::with_capacity(stale.len());
        for key in stale {
            if let Some((value, bytes)) = self.lru.pop(&key) {
                self.bytes = self.bytes.saturating_sub(bytes);
                evicted.push(value);
            }
        }
        evicted
    }

    /// Полный сброс — после операций, сдвигающих нумерацию страниц
    /// (удаление, вставка, перестановка).
    #[must_use = "вытесненные значения нужно освободить явно"]
    pub fn clear(&mut self) -> Vec<V> {
        let evicted = self.lru.iter().map(|(_, (v, _))| v.clone()).collect();
        self.lru.clear();
        self.bytes = 0;
        evicted
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.lru.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lru.is_empty()
    }

    fn evict(&mut self, evicted: &mut Vec<V>) {
        // Последнюю запись не выселяем никогда: если один растр сам по себе
        // больше бюджета, пустой кэш обрёк бы приложение на вечный рендер.
        while self.bytes > self.budget && self.lru.len() > 1 {
            match self.lru.pop_lru() {
                Some((_, (value, bytes))) => {
                    self.bytes = self.bytes.saturating_sub(bytes);
                    evicted.push(value);
                }
                None => break,
            }
        }
    }
}

impl<V> std::fmt::Debug for TileCache<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileCache")
            .field("tiles", &self.lru.len())
            .field("bytes", &self.bytes)
            .field("budget", &self.budget)
            .finish()
    }
}

/// Высота миниатюры в пикселях.
pub const THUMBNAIL_HEIGHT_PX: f32 = 190.0;

/// Бюджет кэша растров.
///
/// Считать его «половиной от желаемого расхода памяти» — верно: у каждого
/// показанного тайла есть вторая копия в атласе спрайтов GPU. Отсюда и
/// умеренное значение.
pub const DEFAULT_TILE_BUDGET: usize = 256 * 1024 * 1024;

/// Бюджет кэша миниатюр. Отдельный, чтобы крупные растры основного вида не
/// вытесняли дешёвые миниатюры, которые нужны постоянно.
pub const DEFAULT_THUMBNAIL_BUDGET: usize = 64 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    fn key(page: u32, scale: f32) -> TileKey {
        TileKey {
            page,
            zoom: ZoomBucket::from_scale(scale),
            rotation: Rotation::None,
        }
    }

    fn tile(bytes: usize) -> Arc<Bitmap> {
        Arc::new(Bitmap {
            width: 1,
            height: (bytes / 4) as u32,
            pixels: vec![0; bytes],
        })
    }

    fn insert(cache: &mut TileCache, page: u32, scale: f32, bytes: usize) -> Vec<Arc<Bitmap>> {
        cache.insert(key(page, scale), tile(bytes), bytes)
    }

    #[test]
    fn nearby_scales_share_one_bucket() {
        // Дрожание масштаба при жесте зума не должно менять ключ.
        assert_eq!(ZoomBucket::from_scale(1.00), ZoomBucket::from_scale(1.02));
        assert_eq!(ZoomBucket::from_scale(1.00).scale(), 1.0);
        // Удвоение масштаба — ровно четыре бакета.
        assert_eq!(
            ZoomBucket::from_scale(2.0).0 - ZoomBucket::from_scale(1.0).0,
            4
        );
    }

    #[test]
    fn bucket_scale_round_trips() {
        for &s in &[0.25f32, 0.5, 1.0, 2.0, 4.0, 8.0] {
            let b = ZoomBucket::from_scale(s);
            assert!((b.scale() - s).abs() < 1e-4, "бакет {b:?} для масштаба {s}");
        }
    }

    #[test]
    fn eviction_respects_byte_budget() {
        let mut cache = TileCache::new(1000);
        for page in 0..5 {
            let _ = insert(&mut cache, page, 1.0, 400);
        }
        assert!(cache.bytes() <= 1000, "бюджет превышен: {}", cache.bytes());
        assert!(!cache.contains(&key(0, 1.0)));
        assert!(cache.contains(&key(4, 1.0)));
    }

    #[test]
    fn eviction_hands_back_every_displaced_value() {
        // Вызывающая сторона обязана получить вытесненное: у значения может
        // быть ресурс на GPU, который не освобождается через Drop.
        let mut cache = TileCache::new(1000);
        assert!(insert(&mut cache, 0, 1.0, 400).is_empty());
        assert!(insert(&mut cache, 1, 1.0, 400).is_empty());
        let evicted = insert(&mut cache, 2, 1.0, 400);
        assert_eq!(
            evicted.len(),
            1,
            "переполнение бюджета обязано вернуть вытесненное"
        );
        assert_eq!(evicted[0].byte_size(), 400);

        // Замена по тому же ключу тоже отдаёт старое значение.
        let replaced = insert(&mut cache, 2, 1.0, 400);
        assert_eq!(replaced.len(), 1);

        let remaining = cache.len();
        assert_eq!(
            cache.clear().len(),
            remaining,
            "сброс обязан отдать всё содержимое"
        );
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn oversized_tile_is_kept_rather_than_evicted_to_nothing() {
        let mut cache = TileCache::new(100);
        insert(&mut cache, 0, 1.0, 4000);
        assert_eq!(
            cache.len(),
            1,
            "единственный растр нельзя выселять, иначе рендер зациклится"
        );
    }

    #[test]
    fn touching_a_tile_protects_it_from_eviction() {
        let mut cache = TileCache::new(1000);
        insert(&mut cache, 0, 1.0, 400);
        insert(&mut cache, 1, 1.0, 400);
        assert!(cache.get(&key(0, 1.0)).is_some()); // страница 0 снова свежая
        insert(&mut cache, 2, 1.0, 400);
        assert!(cache.contains(&key(0, 1.0)));
        assert!(!cache.contains(&key(1, 1.0)));
    }

    #[test]
    fn invalidating_a_page_drops_all_its_zoom_levels() {
        let mut cache = TileCache::new(1_000_000);
        insert(&mut cache, 3, 0.5, 400);
        insert(&mut cache, 3, 2.0, 400);
        insert(&mut cache, 4, 1.0, 400);

        assert_eq!(cache.invalidate_page(3).len(), 2);
        assert!(!cache.contains(&key(3, 0.5)));
        assert!(!cache.contains(&key(3, 2.0)));
        assert!(cache.contains(&key(4, 1.0)));
        assert_eq!(
            cache.bytes(),
            400,
            "байты должны списываться вместе с записями"
        );
    }

    #[test]
    fn replacing_a_key_does_not_double_count_bytes() {
        let mut cache = TileCache::new(1_000_000);
        insert(&mut cache, 0, 1.0, 400);
        insert(&mut cache, 0, 1.0, 800);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), 800);
    }

    #[test]
    fn cache_holds_foreign_value_types() {
        // UI хранит в этом же кэше текстуры, а не растры.
        let mut cache: TileCache<Arc<String>> = TileCache::new(1000);
        let _ = cache.insert(key(0, 1.0), Arc::new("текстура".to_owned()), 600);
        let _ = cache.insert(key(1, 1.0), Arc::new("ещё одна".to_owned()), 600);
        assert_eq!(cache.len(), 1, "бюджет действует и для чужих типов");
        assert!(cache.contains(&key(1, 1.0)));
    }
}
