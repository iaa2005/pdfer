//! Геометрия рамки выделенного абзаца: маркеры, поворот, привязки.
//!
//! Всё здесь — чистые функции над прямоугольниками в координатах документа.
//! Ни gpui, ни состояния вида: рамку двигают в пунктах страницы, а не в
//! пикселях экрана, поэтому от масштаба просмотра ничего не зависит и всё
//! поведение проверяется тестами без окна.

use gpui::{CursorStyle, Pixels, Point};
use pdfcore::geom::Rect;

/// Уже этого блок не ужимается: в строку перестала бы влезать даже буква.
pub(crate) const MIN_WIDTH: f32 = 12.0;
/// И ниже этого тоже: рамка выродилась бы в линию, за которую не ухватиться.
pub(crate) const MIN_HEIGHT: f32 = 8.0;

/// За что схватились на рамке блока.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Grip {
    /// Контур между маркерами: блок переезжает целиком.
    Move,
    /// Отдельная ручка над рамкой: поворот вокруг центра.
    Rotate,
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl Grip {
    /// Все маркеры изменения размера, по часовой стрелке от левого верхнего.
    pub(crate) const RESIZE: [Grip; 8] = [
        Grip::TopLeft,
        Grip::Top,
        Grip::TopRight,
        Grip::Right,
        Grip::BottomRight,
        Grip::Bottom,
        Grip::BottomLeft,
        Grip::Left,
    ];

    /// Какой курсор показать над этим местом рамки.
    ///
    /// Курсор — единственная подсказка о том, что случится при нажатии.
    /// Набор ограничен тем, что gpui умеет показать на Windows: стрелка,
    /// текстовый, крест, рука-указатель и две двусторонние стрелки. Всё
    /// остальное — открытую ладонь, диагональные и «перемещение» — платформа
    /// молча подменяет обычной стрелкой, из-за чего маркеры и выглядели
    /// «безкурсорными». Поэтому: бока — стрелки вдоль своей оси, углы и
    /// поворот — крест (тянется в обе оси), контур переноса — рука.
    pub(crate) fn cursor(self) -> CursorStyle {
        match self {
            Grip::Move => CursorStyle::PointingHand,
            Grip::Rotate => CursorStyle::Crosshair,
            Grip::Left | Grip::Right => CursorStyle::ResizeLeftRight,
            Grip::Top | Grip::Bottom => CursorStyle::ResizeUpDown,
            Grip::TopLeft | Grip::TopRight | Grip::BottomLeft | Grip::BottomRight => {
                CursorStyle::Crosshair
            }
        }
    }

    fn moves_left(self) -> bool {
        matches!(self, Grip::TopLeft | Grip::Left | Grip::BottomLeft)
    }

    fn moves_right(self) -> bool {
        matches!(self, Grip::TopRight | Grip::Right | Grip::BottomRight)
    }

    fn moves_top(self) -> bool {
        matches!(self, Grip::TopLeft | Grip::Top | Grip::TopRight)
    }

    fn moves_bottom(self) -> bool {
        matches!(self, Grip::BottomLeft | Grip::Bottom | Grip::BottomRight)
    }
}

/// Начатое перетаскивание рамки.
pub(crate) struct FrameDrag {
    pub(crate) grip: Grip,
    /// Экранная точка нажатия. Считается только смещение от неё, поэтому
    /// система координат события роли не играет.
    pub(crate) origin: Point<Pixels>,
    /// Рамка на момент нажатия. Смещение всегда откладывается от неё, а не от
    /// текущей: иначе ошибки округления копились бы за время перетаскивания.
    pub(crate) start: Rect,
    /// Поворот на момент нажатия — по той же причине.
    pub(crate) start_rotation: f32,
}

/// Куда переезжает рамка при смещении курсора.
///
/// `dx` и `dy` — смещение в пунктах документа, `dy` считается вниз по экрану.
/// В PDF ось ординат смотрит вверх, поэтому знак у него переворачивается.
pub(crate) fn dragged(start: Rect, grip: Grip, dx: f32, dy: f32) -> Rect {
    let mut frame = start;
    if grip == Grip::Move {
        frame.left += dx;
        frame.right += dx;
        frame.top -= dy;
        frame.bottom -= dy;
        return frame;
    }

    if grip.moves_left() {
        frame.left = (start.left + dx).min(start.right - MIN_WIDTH);
    }
    if grip.moves_right() {
        frame.right = (start.right + dx).max(start.left + MIN_WIDTH);
    }
    if grip.moves_top() {
        frame.top = (start.top - dy).max(start.bottom + MIN_HEIGHT);
    }
    if grip.moves_bottom() {
        frame.bottom = (start.bottom - dy).min(start.top - MIN_HEIGHT);
    }
    frame
}

/// Новый угол поворота по положению курсора относительно центра рамки.
///
/// Считается не абсолютный угол, а его приращение от точки нажатия: иначе
/// рамка прыгала бы на курсор в тот миг, когда за ручку только взялись.
pub(crate) fn rotated(
    start_rotation: f32,
    centre: Point<Pixels>,
    origin: Point<Pixels>,
    now: Point<Pixels>,
) -> f32 {
    // Экранная ось ординат смотрит вниз, а в PDF — вверх, поэтому её знак
    // переворачивается: иначе рамка крутилась бы навстречу руке.
    let angle = |value: Point<Pixels>| {
        let dx = f32::from(value.x - centre.x);
        let dy = f32::from(centre.y - value.y);
        if dx == 0.0 && dy == 0.0 {
            None
        } else {
            Some(dy.atan2(dx).to_degrees())
        }
    };
    let (Some(from), Some(to)) = (angle(origin), angle(now)) else {
        return start_rotation;
    };
    normalise(start_rotation + to - from)
}

/// Приводит угол к промежутку от минус ста восьмидесяти до ста восьмидесяти.
pub(crate) fn normalise(degrees: f32) -> f32 {
    let wrapped = degrees % 360.0;
    if wrapped > 180.0 {
        wrapped - 360.0
    } else if wrapped <= -180.0 {
        wrapped + 360.0
    } else {
        wrapped
    }
}

/// Линия привязки, которую видно, пока рамка стоит вровень с чем-то ещё.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Guide {
    /// Вертикальная линия: абсцисса в пунктах страницы.
    Vertical(f32),
    /// Горизонтальная линия: ордината в пунктах страницы.
    Horizontal(f32),
}

/// Подтягивает рамку к краям и центрам соседних блоков.
///
/// Возвращает поправленную рамку и линии, которые надо показать. Подтягивается
/// только то, что пользователь и так двигает: при переносе — вся рамка, при
/// растягивании — лишь тот край, за который тянут. Иначе блок при изменении
/// ширины ещё и уезжал бы в сторону.
///
/// Допуск задаётся в пунктах документа, поэтому на любом масштабе притяжение
/// ощущается одинаково: считать его в пикселях экрана значило бы, что при
/// сильном увеличении привязка почти перестаёт срабатывать.
pub(crate) fn snap(
    frame: Rect,
    grip: Grip,
    targets: &[Rect],
    tolerance: f32,
) -> (Rect, Vec<Guide>) {
    if grip == Grip::Rotate || targets.is_empty() || tolerance <= 0.0 {
        return (frame, Vec::new());
    }

    let mut guides = Vec::new();
    let mut snapped = frame;

    let vertical: Vec<f32> = targets
        .iter()
        .flat_map(|r| [r.left, r.center_x(), r.right])
        .collect();
    let horizontal: Vec<f32> = targets
        .iter()
        .flat_map(|r| [r.top, (r.top + r.bottom) * 0.5, r.bottom])
        .collect();

    let own_x: &[f32] = match grip {
        Grip::Move => &[frame.left, frame.center_x(), frame.right],
        _ if grip.moves_left() => &[frame.left],
        _ if grip.moves_right() => &[frame.right],
        _ => &[],
    };
    if let Some((delta, guide)) = nearest(own_x, &vertical, tolerance) {
        if grip == Grip::Move {
            snapped.left += delta;
            snapped.right += delta;
        } else if grip.moves_left() {
            snapped.left = (snapped.left + delta).min(snapped.right - MIN_WIDTH);
        } else {
            snapped.right = (snapped.right + delta).max(snapped.left + MIN_WIDTH);
        }
        guides.push(Guide::Vertical(guide));
    }

    let centre_y = (frame.top + frame.bottom) * 0.5;
    let own_y: &[f32] = match grip {
        Grip::Move => &[frame.top, centre_y, frame.bottom],
        _ if grip.moves_top() => &[frame.top],
        _ if grip.moves_bottom() => &[frame.bottom],
        _ => &[],
    };
    if let Some((delta, guide)) = nearest(own_y, &horizontal, tolerance) {
        if grip == Grip::Move {
            snapped.top += delta;
            snapped.bottom += delta;
        } else if grip.moves_top() {
            snapped.top = (snapped.top + delta).max(snapped.bottom + MIN_HEIGHT);
        } else {
            snapped.bottom = (snapped.bottom + delta).min(snapped.top - MIN_HEIGHT);
        }
        guides.push(Guide::Horizontal(guide));
    }

    (snapped, guides)
}

/// Ближайшая пара «своя линия — чужая линия» в пределах допуска. Возвращает
/// поправку и то место, где надо провести линию привязки.
fn nearest(own: &[f32], targets: &[f32], tolerance: f32) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    for &mine in own {
        for &target in targets {
            let delta = target - mine;
            if delta.abs() <= tolerance
                && best.is_none_or(|(current, _)| delta.abs() < current.abs())
            {
                best = Some((delta, target));
            }
        }
    }
    best
}

/// Режим рамочного выделения — как в Компас-3D и чертёжных пакетах.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BandMode {
    /// Слева направо: берётся только то, что накрыто рамкой целиком.
    Window,
    /// Справа налево: берётся всё, что рамка хотя бы задела.
    Crossing,
}

impl BandMode {
    /// Режим определяется направлением протяжки по горизонтали, как принято
    /// в чертёжных пакетах: слева направо — «окно», справа налево — «секущая».
    pub(crate) fn from_drag(start_x: f32, end_x: f32) -> BandMode {
        if end_x >= start_x {
            BandMode::Window
        } else {
            BandMode::Crossing
        }
    }
}

/// Отбирает блоки рамкой: «окно» берёт целиком накрытые, «секущая» — задетые.
///
/// Возвращает номера блоков в исходном порядке — по ним вызывающая сторона
/// достаёт сами блоки, не завися от их типа.
pub(crate) fn blocks_in_band(band: Rect, mode: BandMode, blocks: &[Rect]) -> Vec<usize> {
    blocks
        .iter()
        .enumerate()
        .filter(|(_, bbox)| match mode {
            BandMode::Window => {
                bbox.left >= band.left
                    && bbox.right <= band.right
                    && bbox.bottom >= band.bottom
                    && bbox.top <= band.top
            }
            BandMode::Crossing => bbox.intersects(&band),
        })
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]

mod tests {
    use super::*;
    use gpui::{point, px};

    fn frame() -> Rect {
        Rect::new(100.0, 700.0, 300.0, 760.0)
    }

    #[test]
    fn dragging_the_outline_moves_the_whole_block() {
        // Вниз по экрану — вниз по странице, то есть в сторону меньших
        // ординат: в PDF ось смотрит вверх.
        let moved = dragged(frame(), Grip::Move, 20.0, 30.0);
        assert_eq!((moved.left, moved.right), (120.0, 320.0));
        assert_eq!((moved.top, moved.bottom), (730.0, 670.0));
        assert_eq!(moved.width(), frame().width(), "перенос не меняет размер");
        assert_eq!(moved.height(), frame().height());
    }

    #[test]
    fn side_markers_move_only_their_own_edge() {
        let right = dragged(frame(), Grip::Right, 50.0, 40.0);
        assert_eq!(right.right, 350.0);
        assert_eq!((right.left, right.top, right.bottom), (100.0, 760.0, 700.0));

        let bottom = dragged(frame(), Grip::Bottom, 50.0, 40.0);
        assert_eq!(bottom.bottom, 660.0);
        assert_eq!(
            (bottom.left, bottom.right, bottom.top),
            (100.0, 300.0, 760.0)
        );
    }

    #[test]
    fn corner_markers_move_two_edges_at_once() {
        let corner = dragged(frame(), Grip::TopLeft, 30.0, -20.0);
        assert_eq!(corner.left, 130.0);
        assert_eq!(corner.top, 780.0);
        assert_eq!(
            (corner.right, corner.bottom),
            (300.0, 700.0),
            "дальние края на месте"
        );
    }

    #[test]
    fn a_block_cannot_be_dragged_smaller_than_a_letter() {
        for grip in [Grip::Left, Grip::TopLeft, Grip::BottomLeft] {
            assert_eq!(dragged(frame(), grip, 1000.0, 0.0).width(), MIN_WIDTH);
        }
        for grip in [Grip::Right, Grip::TopRight, Grip::BottomRight] {
            assert_eq!(dragged(frame(), grip, -1000.0, 0.0).width(), MIN_WIDTH);
        }
        assert_eq!(
            dragged(frame(), Grip::Top, 0.0, 1000.0).height(),
            MIN_HEIGHT
        );
        assert_eq!(
            dragged(frame(), Grip::Bottom, 0.0, -1000.0).height(),
            MIN_HEIGHT
        );
    }

    #[test]
    fn dragging_is_measured_from_where_it_started() {
        // Возврат курсора в исходную точку возвращает и рамку.
        assert_eq!(dragged(frame(), Grip::Move, 0.0, 0.0), frame());
        assert_eq!(dragged(frame(), Grip::BottomRight, 0.0, 0.0), frame());
    }

    #[test]
    fn rotation_follows_the_hand_counterclockwise() {
        let centre = point(px(100.0), px(100.0));
        // Справа от центра вверх по экрану — против часовой стрелки.
        let angle = rotated(
            0.0,
            centre,
            point(px(200.0), px(100.0)),
            point(px(100.0), px(0.0)),
        );
        assert!((angle - 90.0).abs() < 0.01, "получено {angle}");
    }

    #[test]
    fn rotation_starts_from_the_current_angle_without_jumping() {
        let centre = point(px(100.0), px(100.0));
        let grabbed = point(px(140.0), px(160.0));
        // Пока курсор не сдвинулся, угол обязан остаться прежним.
        assert_eq!(rotated(30.0, centre, grabbed, grabbed), 30.0);
    }

    #[test]
    fn angles_stay_within_half_a_turn() {
        assert_eq!(normalise(370.0), 10.0);
        assert_eq!(normalise(-190.0), 170.0);
        assert_eq!(normalise(180.0), 180.0);
        assert_eq!(normalise(-180.0), 180.0);
    }

    #[test]
    fn without_neighbours_nothing_snaps() {
        let (result, guides) = snap(frame(), Grip::Move, &[], 6.0);
        assert_eq!(result, frame());
        assert!(guides.is_empty());
    }

    #[test]
    fn a_moved_block_lines_up_with_a_neighbour() {
        // Сосед стоит левым краем на 104 — почти вровень, но не совсем.
        let neighbour = Rect::new(104.0, 500.0, 260.0, 560.0);
        let (result, guides) = snap(frame(), Grip::Move, &[neighbour], 6.0);
        assert_eq!(result.left, 104.0, "левый край обязан подтянуться");
        assert_eq!(result.width(), frame().width(), "привязка не меняет размер");
        assert!(guides.contains(&Guide::Vertical(104.0)));
    }

    #[test]
    fn a_neighbour_too_far_away_does_not_pull() {
        // Ни один край и ни один центр соседа не подходит к рамке ближе
        // допуска — притягивать не к чему.
        let neighbour = Rect::new(140.0, 500.0, 190.0, 560.0);
        let (result, guides) = snap(frame(), Grip::Move, &[neighbour], 6.0);
        assert_eq!(result, frame());
        assert!(guides.is_empty());
    }

    #[test]
    fn resizing_snaps_only_the_edge_being_dragged() {
        // Правый край рамки — 300, сосед стоит на 303.
        let neighbour = Rect::new(303.0, 500.0, 400.0, 560.0);
        let (result, guides) = snap(frame(), Grip::Right, &[neighbour], 6.0);
        assert_eq!(result.right, 303.0);
        assert_eq!(result.left, 100.0, "противоположный край стоит на месте");
        assert_eq!(guides, vec![Guide::Vertical(303.0)]);
    }

    #[test]
    fn centres_snap_to_centres() {
        // Центр рамки по горизонтали — 200. Сосед центрирован на 202.
        let neighbour = Rect::new(102.0, 500.0, 302.0, 560.0);
        let (result, _) = snap(frame(), Grip::Move, &[neighbour], 6.0);
        assert_eq!(result.center_x(), 202.0);
    }

    #[test]
    fn the_nearest_line_wins() {
        // Два соседа в пределах допуска: побеждает тот, что ближе.
        let far = Rect::new(105.0, 500.0, 260.0, 560.0);
        let near = Rect::new(101.0, 400.0, 260.0, 460.0);
        let (result, _) = snap(frame(), Grip::Move, &[far, near], 6.0);
        assert_eq!(result.left, 101.0);
    }

    #[test]
    fn rotation_is_never_snapped() {
        let neighbour = Rect::new(101.0, 500.0, 260.0, 560.0);
        let (result, guides) = snap(frame(), Grip::Rotate, &[neighbour], 6.0);
        assert_eq!(result, frame());
        assert!(guides.is_empty());
    }

    #[test]
    fn a_left_to_right_band_takes_only_fully_covered_blocks() {
        // «Окно»: блок, торчащий из рамки хоть на волос, не берётся.
        let inside = Rect::new(110.0, 710.0, 190.0, 750.0);
        let sticking_out = Rect::new(150.0, 690.0, 260.0, 730.0);
        let far = Rect::new(400.0, 400.0, 500.0, 440.0);
        let band = Rect::new(100.0, 700.0, 300.0, 760.0);

        let picked = blocks_in_band(band, BandMode::Window, &[inside, sticking_out, far]);
        assert_eq!(picked, vec![0], "взят только целиком накрытый блок");
    }

    #[test]
    fn a_right_to_left_band_takes_touched_blocks_too() {
        let inside = Rect::new(110.0, 710.0, 190.0, 750.0);
        let sticking_out = Rect::new(150.0, 690.0, 260.0, 730.0);
        let far = Rect::new(400.0, 400.0, 500.0, 440.0);
        let band = Rect::new(100.0, 700.0, 300.0, 760.0);

        let picked = blocks_in_band(band, BandMode::Crossing, &[inside, sticking_out, far]);
        assert_eq!(picked, vec![0, 1], "секущая берёт и задетый блок");
    }

    #[test]
    fn drag_direction_picks_the_mode() {
        assert_eq!(BandMode::from_drag(10.0, 200.0), BandMode::Window);
        assert_eq!(BandMode::from_drag(200.0, 10.0), BandMode::Crossing);
    }

    #[test]
    fn cursors_stay_within_what_windows_actually_shows() {
        // На Windows gpui показывает лишь малый набор курсоров; всё вне его
        // молча падает в стрелку — и маркер выглядит «безкурсорным». Тест
        // держит выбор внутри работающего набора.
        let allowed = [
            CursorStyle::PointingHand,
            CursorStyle::Crosshair,
            CursorStyle::ResizeLeftRight,
            CursorStyle::ResizeUpDown,
            CursorStyle::IBeam,
        ];
        for grip in Grip::RESIZE.iter().chain([Grip::Move, Grip::Rotate].iter()) {
            assert!(
                allowed.contains(&grip.cursor()),
                "{grip:?} вне живого набора"
            );
        }
        // Оси различимы между собой и не путаются с переносом.
        assert_ne!(Grip::Left.cursor(), Grip::Top.cursor());
        assert_ne!(Grip::Move.cursor(), Grip::TopLeft.cursor());
        assert_eq!(Grip::RESIZE.len(), 8);
    }
}
