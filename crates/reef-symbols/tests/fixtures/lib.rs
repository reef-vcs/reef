use std::fmt;

pub const LIMIT: usize = 8;
static NAME: &str = "lib";

pub struct Foo<T> {
    value: T,
}

pub enum Mode {
    Fast,
    Slow,
}

pub trait Run {
    fn run(&self) -> usize;
    fn name(&self) -> &str {
        "run"
    }
}

type Pair = (u8, u8);

impl<T> Foo<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }

    pub fn bar(&self) -> usize {
        helper(LIMIT)
    }
}

impl<T> fmt::Display for Foo<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "foo")
    }
}

mod inner {
    pub fn helper(n: usize) -> usize {
        n.max(1)
    }
}

macro_rules! twice {
    ($e:expr) => {
        $e + $e
    };
}

fn helper(n: usize) -> usize {
    inner::helper(twice!(n))
}
