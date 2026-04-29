#![no_std]
#![no_main]

use user_lib::{brk, get_time};

#[macro_use]
extern crate user_lib;

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    let start_time = get_time();
    let start_brk = brk(0);

    let mut size = 0;
    for _ in 0..100 {
        size += 0x10000;
        brk(size);
    }
    
    let end_time = get_time();
    let end_brk = brk(0);

    // 向brk分配的内存中写入数据
    let ptr = start_brk as *mut u8;
    let len = end_brk - start_brk;
    if len > 0 {
        unsafe {
            for i in 0..len {
                ptr.add(i as usize).write_volatile((i % 256) as u8);
            }
        }
    }
    
    println!("Time taken for 100 brk calls: {} ms", end_time - start_time);
    0
}