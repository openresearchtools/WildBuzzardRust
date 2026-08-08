use std::ops::{Add, AddAssign, Sub, SubAssign};

/// Gecko-compatible scale for deterministic CSS geometry: 60 app units per CSS px.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Au(i32);

impl Au {
    pub const PER_CSS_PX: i32 = 60;
    pub const ZERO: Self = Self(0);

    pub const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }

    pub const fn from_px(px: i32) -> Self {
        Self(px.saturating_mul(Self::PER_CSS_PX))
    }

    pub fn scale(self, numerator: i32, denominator: i32) -> Self {
        assert_ne!(
            denominator, 0,
            "app-unit scale denominator must be non-zero"
        );
        let value = i64::from(self.0) * i64::from(numerator) / i64::from(denominator);
        Self(clamp_i64_to_i32(value))
    }

    pub const fn max(self, other: Self) -> Self {
        if self.0 >= other.0 { self } else { other }
    }

    pub const fn min(self, other: Self) -> Self {
        if self.0 <= other.0 { self } else { other }
    }

    pub const fn non_negative(self) -> Self {
        self.max(Self::ZERO)
    }
}

impl Add for Au {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl AddAssign for Au {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sub for Au {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl SubAssign for Au {
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

const fn clamp_i64_to_i32(value: i64) -> i32 {
    if value > i32::MAX as i64 {
        i32::MAX
    } else if value < i32::MIN as i64 {
        i32::MIN
    } else {
        value as i32
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Point {
    pub x: Au,
    pub y: Au,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Size {
    pub width: Au,
    pub height: Au,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const fn new(x: Au, y: Au, width: Au, height: Au) -> Self {
        Self {
            origin: Point { x, y },
            size: Size { width, height },
        }
    }

    pub fn right(self) -> Au {
        self.origin.x + self.size.width
    }

    pub fn bottom(self) -> Au {
        self.origin.y + self.size.height
    }

    pub fn union(self, other: Self) -> Self {
        let left = self.origin.x.min(other.origin.x);
        let top = self.origin.y.min(other.origin.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self::new(left, top, right - left, bottom - top)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Edges {
    pub top: Au,
    pub right: Au,
    pub bottom: Au,
    pub left: Au,
}

impl Edges {
    pub const fn all(value: Au) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }

    pub fn horizontal(self) -> Au {
        self.left + self.right
    }

    pub fn vertical(self) -> Au {
        self.top + self.bottom
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    pub size: Size,
}

impl Viewport {
    pub const fn new(width: Au, height: Au) -> Self {
        Self {
            size: Size { width, height },
        }
    }

    pub const fn from_css_pixels(width: i32, height: i32) -> Self {
        Self::new(Au::from_px(width), Au::from_px(height))
    }
}
