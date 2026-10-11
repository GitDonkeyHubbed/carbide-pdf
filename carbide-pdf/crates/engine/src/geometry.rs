//! Page geometry. Everything user-facing is expressed in *display space*: origin at the bottom-left
//! of the page as a viewer shows it (crop box applied, `/Rotate` applied), units are points.
//! The engine converts to PDF user space only when it writes content.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    /// Builds a normalized rect from two opposite corners.
    pub fn from_corners(a: (f32, f32), b: (f32, f32)) -> Self {
        Self::new(a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
    }

    pub fn max_x(&self) -> f32 {
        self.x + self.width
    }

    pub fn max_y(&self) -> f32 {
        self.y + self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// Shrinks the rect by `amount` on every side (grows it when negative).
    pub fn inset(&self, amount: f32) -> Rect {
        Rect::new(self.x + amount, self.y + amount, self.width - 2.0 * amount, self.height - 2.0 * amount)
    }
}

/// The visible box of a page plus its rotation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    /// Visible box in user space: x0, y0 (lower-left) and unrotated width/height.
    pub x0: f32,
    pub y0: f32,
    pub box_width: f32,
    pub box_height: f32,
    /// Clockwise rotation in degrees, one of 0, 90, 180, 270.
    pub rotation: i32,
}

pub fn normalize_rotation(degrees: i64) -> i32 {
    let snapped = ((degrees as f64 / 90.0).round() as i64) * 90;
    (((snapped % 360) + 360) % 360) as i32
}

impl PageGeometry {
    pub fn new(x0: f32, y0: f32, box_width: f32, box_height: f32, rotation: i32) -> Self {
        Self { x0, y0, box_width, box_height, rotation: normalize_rotation(rotation as i64) }
    }

    /// Displayed size (width, height): swapped for 90/270 degree rotations.
    pub fn display_size(&self) -> (f32, f32) {
        if self.rotation % 180 == 0 {
            (self.box_width, self.box_height)
        } else {
            (self.box_height, self.box_width)
        }
    }

    pub fn display_rect(&self) -> Rect {
        let (w, h) = self.display_size();
        Rect::new(0.0, 0.0, w, h)
    }

    /// PDF matrix `[a b c d e f]` mapping display space to user space.
    pub fn display_to_user(&self) -> [f32; 6] {
        let (bx, by, bw, bh) = (self.x0, self.y0, self.box_width, self.box_height);
        match self.rotation {
            90 => [0.0, 1.0, -1.0, 0.0, bx + bw, by],
            180 => [-1.0, 0.0, 0.0, -1.0, bx + bw, by + bh],
            270 => [0.0, -1.0, 1.0, 0.0, bx, by + bh],
            _ => [1.0, 0.0, 0.0, 1.0, bx, by],
        }
    }

    pub fn map_point(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.display_to_user();
        (a * x + c * y + e, b * x + d * y + f)
    }

    /// Inverse of [`Self::map_point`]: user space to display space.
    pub fn user_to_display(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.display_to_user();
        let det = a * d - b * c;
        let (dx, dy) = (x - e, y - f);
        ((d * dx - c * dy) / det, (-b * dx + a * dy) / det)
    }

    /// Bounding rect, in user space, of a display-space rect.
    pub fn map_rect(&self, rect: Rect) -> Rect {
        let corners = [
            self.map_point(rect.x, rect.y),
            self.map_point(rect.max_x(), rect.y),
            self.map_point(rect.x, rect.max_y()),
            self.map_point(rect.max_x(), rect.max_y()),
        ];
        let min_x = corners.iter().map(|c| c.0).fold(f32::INFINITY, f32::min);
        let max_x = corners.iter().map(|c| c.0).fold(f32::NEG_INFINITY, f32::max);
        let min_y = corners.iter().map(|c| c.1).fold(f32::INFINITY, f32::min);
        let max_y = corners.iter().map(|c| c.1).fold(f32::NEG_INFINITY, f32::max);
        Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
    }
}

pub(crate) fn matrix_string(m: [f32; 6]) -> String {
    format!("{} {} {} {} {} {} cm", fmt(m[0]), fmt(m[1]), fmt(m[2]), fmt(m[3]), fmt(m[4]), fmt(m[5]))
}

/// Compact, locale-independent number formatting for content streams.
pub(crate) fn fmt(value: f32) -> String {
    if value == value.trunc() && value.abs() < 1.0e9 {
        format!("{}", value as i64)
    } else {
        let s = format!("{:.4}", value);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_left_moves_clockwise() {
        // Unrotated bottom-left corner of a 100x200 box at the origin.
        let g = PageGeometry::new(0.0, 0.0, 100.0, 200.0, 90);
        assert_eq!(g.display_size(), (200.0, 100.0));
        // Display top-left (0, 100) is the user-space bottom-left (0, 0).
        assert_eq!(g.map_point(0.0, 100.0), (0.0, 0.0));
        // Display bottom-left (0, 0) is the user-space bottom-right (100, 0).
        assert_eq!(g.map_point(0.0, 0.0), (100.0, 0.0));
    }

    #[test]
    fn half_turn_and_three_quarter_turn() {
        let g = PageGeometry::new(10.0, 20.0, 100.0, 200.0, 180);
        assert_eq!(g.map_point(0.0, 0.0), (110.0, 220.0));
        let g = PageGeometry::new(0.0, 0.0, 100.0, 200.0, 270);
        // Display bottom-left maps to the user-space top-left of the unrotated box.
        assert_eq!(g.map_point(0.0, 0.0), (0.0, 200.0));
        assert_eq!(g.map_point(200.0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn user_to_display_inverts_map_point() {
        for rotation in [0, 90, 180, 270] {
            let g = PageGeometry::new(10.0, 20.0, 100.0, 200.0, rotation);
            let (ux, uy) = g.map_point(33.0, 44.0);
            let (dx, dy) = g.user_to_display(ux, uy);
            assert!((dx - 33.0).abs() < 1e-3 && (dy - 44.0).abs() < 1e-3, "rotation {rotation}");
        }
    }

    #[test]
    fn rotation_is_normalized() {
        assert_eq!(normalize_rotation(-90), 270);
        assert_eq!(normalize_rotation(450), 90);
        assert_eq!(normalize_rotation(0), 0);
    }

    #[test]
    fn number_formatting() {
        assert_eq!(fmt(12.0), "12");
        assert_eq!(fmt(0.5), "0.5");
        assert_eq!(fmt(-1.25), "-1.25");
    }
}
