pub use tenon_sdk::{Error, Result};

pub const ROLE: &str = "worker";

pub fn run(_args: &[String]) -> i32 {
    println!("tenon {ROLE}: not implemented in P3.0");
    2
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_stub_exits_two() {
        assert_eq!(super::run(&[]), 2);
    }
}
