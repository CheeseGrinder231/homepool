pub fn demo() {
    {
        mod a {
            pub mod left {
                pub fn f() {
                    super::right::f();
                }
            }
            pub mod right {
                pub fn f() {}
            }
        }
        a::left::f();
    }
    {
        mod a {
            pub mod left {
                pub fn f() {}
            }
            pub mod right {
                pub fn f() {
                    super::left::f();
                }
            }
        }
        a::right::f();
    }
}
