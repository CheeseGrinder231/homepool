mod alpha {
    use crate::{beta::{self, work}, independent};
    mod nested {
        fn work() { super::super::beta::work(); }
    }
}
mod beta {
    use super::gamma::work;
    pub fn work() {}
}
mod gamma {
    pub fn work() { crate::alpha::work(); }
}
mod independent {}
