pub struct Point {
    x: i32,
}

impl Point {
    pub fn new(x: i32) -> Self {
        Point { x }
    }

    pub fn norm(&self) -> i32 {
        self.x.abs()
    }
}

pub fn origin() -> Point {
    Point::new(0)
}
