mod alpha {
    pub fn work() { crate::beta::work(); }
    mod nested { fn work() { super::super::beta::work(); } }
}
mod beta { pub fn work() {} }
mod independent {
    use crate::{alpha as alias, beta::{self, work}};
}
// mod phantom { fn work() { crate::alpha::work(); } }
const EXAMPLE: &str = "mod beta { fn work() { crate::alpha::work(); } }";
const RAW: &str = r#"mod alpha { use super::beta; }"#;
macro_rules! ignored {
    () => { mod beta { use crate::alpha; } };
}
ignored!(mod beta { use crate::alpha; });
