use super::File;
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::UserBuffer;
use crate::fs::{Stat,StatMode};

/// stdin file for getting chars from console
pub struct Stdin;

/// stdout file for putting chars to console
pub struct Stdout;

impl File for Stdin {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
   fn read(&self, mut user_buf: UserBuffer) -> usize {
      assert_eq!(user_buf.len(), 1);
      let ch = UART.read();
      unsafe {
            user_buf.buffers[0].as_mut_ptr().write_volatile(ch);
      }
      1
   }
    fn write(&self, _user_buf: UserBuffer) -> usize {
        panic!("Cannot write to stdin!");
    }
    fn stat(&self) -> super::Stat {
        super::Stat {
            dev: 0,
            ino: 0,
            mode: super::StatMode::CHAR,
            nlink: 1,
            pad: [0; 7],
        }
    }
}

impl File for Stdout {
    fn readable(&self) -> bool {
        false
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _user_buf: UserBuffer) -> usize {
        panic!("Cannot read from stdout!");
    }
    fn write(&self, buf: UserBuffer) -> usize {
        let mut n = 0usize;
        for slice in buf.buffers.iter() {
            for &ch in slice.iter() {
                UART.write(ch);
                n += 1;
            }
        }
        n
    }
    fn stat(&self) -> super::Stat {
        super::Stat {
            dev: 0,
            ino: 0,
            mode: super::StatMode::CHAR,
            nlink: 1,
            pad: [0; 7],
        }
    }
}
