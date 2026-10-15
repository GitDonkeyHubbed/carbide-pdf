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
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Builds a normalized rect from two opposite corners.
    pub fn from_corners(a: (f32, f32), b: (f32, f32)) -> Self {
        Self::new(
            a.0.min(b.0),
            a.1.min(b.1),
            (a.0 - b.0).abs(),
            (a.1 - b.1).abs(),
        )
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

    /// True when every coordinate is a finite number, and so is the size of the area once it is
    /// [normalized](Rect::normalized): a corner too far away to be a number makes it neither.
    pub fn is_finite(&self) -> bool {
        let finite = |rect: &Rect| {
            [rect.x, rect.y, rect.width, rect.height]
                .iter()
                .all(|v| v.is_finite())
        };
        finite(self) && finite(&self.normalized())
    }

    /// The same area with a non-negative width and height: a rect given with a negative width
    /// (dragged or typed from right to left) starts at its other corner.
    pub fn normalized(&self) -> Rect {
        Rect::from_corners(
            (self.x, self.y),
            (self.x + self.width, self.y + self.height),
        )
    }

    /// Shrinks the rect by `amount` on every side (grows it when negative).
    pub fn inset(&self, amount: f32) -> Rect {
        Rect::new(
            self.x + amount,
            self.y + amount,
            self.width - 2.0 * amount,
            self.height - 2.0 * amount,
        )
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

/// `degrees` as one of 0, 90, 180 or 270, the way viewers read a page's `/Rotate`: whole quarter
/// turns, rounded toward zero (45 is 0 and -135 is 270), for any value without overflowing.
pub fn normalize_rotation(degrees: i64) -> i32 {
    ((degrees / 90).rem_euclid(4) * 90) as i32
}

/// A page's `/Rotate` value read the way PDFium reads it, normalized as [`normalize_rotation`]
/// does. PDFium reads the digits of an integer into 32 bits: a positive one that does not fit in
/// 32 unsigned bits is 0 and one past the signed range wraps around, a negative one below the
/// signed range is 0. A real is taken toward zero, saturating. Anything else counts as 0.
pub fn rotation_value(value: &lopdf::Object) -> i32 {
    let degrees = match *value {
        lopdf::Object::Integer(v) if v >= 0 => u32::try_from(v).map_or(0, |v| v as i32),
        lopdf::Object::Integer(v) => i32::try_from(v).unwrap_or(0),
        lopdf::Object::Real(v) => v as i32,
        _ => 0,
    };
    normalize_rotation(degrees as i64)
}

impl PageGeometry {
    pub fn new(x0: f32, y0: f32, box_width: f32, box_height: f32, rotation: i32) -> Self {
        Self {
            x0,
            y0,
            box_width,
            box_height,
            rotation: normalize_rotation(rotation as i64),
        }
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
        let max_x = corners
            .iter()
            .map(|c| c.0)
            .fold(f32::NEG_INFINITY, f32::max);
        let min_y = corners.iter().map(|c| c.1).fold(f32::INFINITY, f32::min);
        let max_y = corners
            .iter()
            .map(|c| c.1)
            .fold(f32::NEG_INFINITY, f32::max);
        Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
    }
}

pub(crate) fn matrix_string(m: [f32; 6]) -> String {
    format!(
        "{} {} {} {} {} {} cm",
        fmt(m[0]),
        fmt(m[1]),
        fmt(m[2]),
        fmt(m[3]),
        fmt(m[4]),
        fmt(m[5])
    )
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
            assert!(
                (dx - 33.0).abs() < 1e-3 && (dy - 44.0).abs() < 1e-3,
                "rotation {rotation}"
            );
        }
    }

    #[test]
    fn rotation_is_normalized() {
        assert_eq!(normalize_rotation(-90), 270);
        assert_eq!(normalize_rotation(450), 90);
        assert_eq!(normalize_rotation(0), 0);
        assert_eq!(normalize_rotation(i64::MAX), 0);
        assert_eq!(normalize_rotation(i64::MIN), 0);
    }

    #[test]
    fn rotate_values_are_read_like_viewers_read_them() {
        use lopdf::Object::{Integer, Real};
        let cases = [
            (Integer(90), 90),
            (Real(90.0), 90),
            (Real(270.9), 270),
            (Integer(45), 0),
            (Integer(135), 90),
            (Integer(-90), 270),
            (Integer(-135), 270),
            (Integer(i64::MAX), 0),
            (Integer(3_000_000_000), 270),
            (Integer(-3_000_000_000), 0),
            (Integer(-2_147_483_648), 270),
            (Real(1.0e30), 90),
            (Real(f32::NAN), 0),
            (lopdf::Object::Name(b"Ninety".to_vec()), 0),
        ];
        for (value, expected) in cases {
            assert_eq!(rotation_value(&value), expected, "{value:?}");
        }
    }

    #[test]
    fn rects_given_from_the_other_corner_are_normalized() {
        assert_eq!(
            Rect::new(500.0, 700.0, -200.0, -50.0).normalized(),
            Rect::new(300.0, 650.0, 200.0, 50.0)
        );
        assert_eq!(
            Rect::new(1.0, 2.0, 3.0, 4.0).normalized(),
            Rect::new(1.0, 2.0, 3.0, 4.0)
        );
        assert!(!Rect::new(f32::NAN, 0.0, 1.0, 1.0).is_finite());
    }

    #[test]
    fn number_formatting() {
        assert_eq!(fmt(12.0), "12");
        assert_eq!(fmt(0.5), "0.5");
        assert_eq!(fmt(-1.25), "-1.25");
    }
}
