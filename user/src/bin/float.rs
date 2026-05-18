#![no_std]
#![no_main]

use user_lib::yield_;
use core::hint::black_box;

#[macro_use]
extern crate user_lib;

#[inline(never)]
fn make_input() -> f64 {
    black_box(3.0)
}

#[inline(never)]
fn ret_f64(x: f64) -> f64 {
    black_box(x)
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {

    println!("Test floating point calculation in user space:");
    let mut a = 1.0f64;
    for _ in 0..100 {
        a = a * 1.1;
        yield_();
    }
    println!("a = {}", a);

    println!("Test floating point return value:");
    let x = make_input();
    let y = ret_f64(x);
    println!("x = {}", x);
    println!("y = {}", y);
    0
}