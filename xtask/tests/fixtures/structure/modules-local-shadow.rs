pub mod a {
    pub fn f() {}
}

pub fn demo() {
    mod a {
        pub fn f() {
            crate::b::f();
            super::b::f();
        }
    }
    a::f();
}

pub mod b {
    pub fn f() {
        crate::a::f();
    }
}
