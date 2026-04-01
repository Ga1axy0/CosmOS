#![no_std]
#![no_main]

use user_lib::yield_;

#[macro_use]
extern crate user_lib;

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    let mut a = 1.0f64;
    for _ in 0..100 {
        a = a * 1.1;
        yield_();
    }
    println!("a = {}", a);
    0
}