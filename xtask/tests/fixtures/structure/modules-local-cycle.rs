pub fn demo() {
    mod local {
        pub mod left {
            pub fn f() {
                let _ = super::right::f as fn();
            }
        }
        pub mod right {
            pub fn f() {
                let _ = super::left::f as fn();
            }
        }
    }
    local::left::f();
}
