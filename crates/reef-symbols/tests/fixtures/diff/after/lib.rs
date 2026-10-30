pub struct Point {
    x: i32,
}

impl Point {
    pub fn new(x: i32) -> Self {
        Point { x }
    }

    pub fn norm(&self) -> i32 {
        self.x.abs().max(0)
    }

    pub fn twice(&self) -> i32 {
        self.x * 2
    }
}
