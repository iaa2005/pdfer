//! Геометрия в координатах PDF: начало в левом нижнем углу, ось Y направлена
//! вверх, единица — типографский пункт (1/72 дюйма).
//!
//! Экранная система координат (Y вниз) начинается только на границе UI —
//! внутри ядра пересчётов нет, иначе знаки путаются на каждом шаге.

/// Прямоугольник в пунктах. Инвариант: `left <= right`, `bottom <= top`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        left: 0.0,
        bottom: 0.0,
        right: 0.0,
        top: 0.0,
    };

    /// Нормализует углы, поэтому принимает их в любом порядке — pdfium
    /// возвращает bbox с перевёрнутыми краями для повёрнутого текста.
    pub fn new(left: f32, bottom: f32, right: f32, top: f32) -> Self {
        Self {
            left: left.min(right),
            bottom: bottom.min(top),
            right: left.max(right),
            top: bottom.max(top),
        }
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn height(&self) -> f32 {
        self.top - self.bottom
    }

    pub fn center_x(&self) -> f32 {
        (self.left + self.right) * 0.5
    }

    pub fn center_y(&self) -> f32 {
        (self.bottom + self.top) * 0.5
    }

    pub fn is_empty(&self) -> bool {
        self.width() <= 0.0 || self.height() <= 0.0
    }

    pub fn union(&self, other: &Rect) -> Rect {
        Rect {
            left: self.left.min(other.left),
            bottom: self.bottom.min(other.bottom),
            right: self.right.max(other.right),
            top: self.top.max(other.top),
        }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.bottom && y <= self.top
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.bottom < other.top
            && other.bottom < self.top
    }

    /// Раздвигает границы на `d` во все стороны (для хит-тестов с допуском).
    pub fn inflate(&self, d: f32) -> Rect {
        Rect {
            left: self.left - d,
            bottom: self.bottom - d,
            right: self.right + d,
            top: self.top + d,
        }
    }

    /// Доля горизонтального перекрытия относительно более узкого из двух
    /// прямоугольников: 1.0 — один целиком накрыт другим, 0.0 — не пересекаются.
    ///
    /// Это главный признак принадлежности строк одной колонке: в двухколоночной
    /// вёрстке строки соседних колонок имеют схожий интерлиньяж и кегль, и
    /// отличить их можно только по горизонтали.
    pub fn h_overlap_ratio(&self, other: &Rect) -> f32 {
        let overlap = self.right.min(other.right) - self.left.max(other.left);
        if overlap <= 0.0 {
            return 0.0;
        }
        let narrower = self.width().min(other.width());
        if narrower <= 0.0 {
            0.0
        } else {
            (overlap / narrower).min(1.0)
        }
    }
}

/// Объединение всех прямоугольников последовательности. `None` для пустой.
pub fn union_all<'a>(mut rects: impl Iterator<Item = &'a Rect>) -> Option<Rect> {
    let first = *rects.next()?;
    Some(rects.fold(first, |acc, r| acc.union(r)))
}

/// Размер страницы в пунктах, как он записан в MediaBox.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

impl PageSize {
    /// Размер с учётом флага поворота страницы: при 90°/270° стороны меняются.
    pub fn rotated(&self, rotation: Rotation) -> PageSize {
        match rotation {
            Rotation::None | Rotation::Half => *self,
            Rotation::Quarter | Rotation::ThreeQuarters => PageSize {
                width: self.height,
                height: self.width,
            },
        }
    }
}

/// Поворот страницы, кратный 90° — то, что хранится в ключе `/Rotate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Rotation {
    #[default]
    None,
    Quarter,
    Half,
    ThreeQuarters,
}

impl Rotation {
    pub fn from_degrees(deg: i32) -> Rotation {
        match deg.rem_euclid(360) / 90 {
            1 => Rotation::Quarter,
            2 => Rotation::Half,
            3 => Rotation::ThreeQuarters,
            _ => Rotation::None,
        }
    }

    pub fn to_degrees(self) -> i32 {
        match self {
            Rotation::None => 0,
            Rotation::Quarter => 90,
            Rotation::Half => 180,
            Rotation::ThreeQuarters => 270,
        }
    }

    pub fn rotate_right(self) -> Rotation {
        Rotation::from_degrees(self.to_degrees() + 90)
    }

    pub fn rotate_left(self) -> Rotation {
        Rotation::from_degrees(self.to_degrees() - 90)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_normalises_swapped_corners() {
        let r = Rect::new(10.0, 20.0, 0.0, 5.0);
        assert_eq!(
            r,
            Rect {
                left: 0.0,
                bottom: 5.0,
                right: 10.0,
                top: 20.0
            }
        );
    }

    #[test]
    fn h_overlap_is_relative_to_narrower_rect() {
        let wide = Rect::new(0.0, 0.0, 100.0, 10.0);
        let narrow = Rect::new(40.0, 0.0, 60.0, 10.0);
        // Узкий целиком внутри широкого — перекрытие полное.
        assert_eq!(wide.h_overlap_ratio(&narrow), 1.0);

        let left_col = Rect::new(0.0, 0.0, 40.0, 10.0);
        let right_col = Rect::new(60.0, 0.0, 100.0, 10.0);
        assert_eq!(left_col.h_overlap_ratio(&right_col), 0.0);
    }

    #[test]
    fn rotation_swaps_page_sides_on_quarter_turns() {
        let a4 = PageSize {
            width: 595.0,
            height: 842.0,
        };
        assert_eq!(a4.rotated(Rotation::Half), a4);
        assert_eq!(
            a4.rotated(Rotation::Quarter),
            PageSize {
                width: 842.0,
                height: 595.0
            }
        );
    }

    #[test]
    fn rotation_wraps_around() {
        assert_eq!(Rotation::ThreeQuarters.rotate_right(), Rotation::None);
        assert_eq!(Rotation::None.rotate_left(), Rotation::ThreeQuarters);
    }
}
