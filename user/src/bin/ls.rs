#![no_std]
#![no_main]

use user_lib::{OpenFlags, STDOUT, close, getdents64, open, println, write};

const BUF_SIZE: usize = 6 * 1024;
const DT_DIR: u8 = 4;

#[no_mangle]
pub fn main(_argc: usize, _argv: &[&str]) -> i32 {
    let mut buf = [0u8; BUF_SIZE];
    let fd = open(".", OpenFlags::RDONLY);
    if fd < 0 {
        return 1;
    }

    loop {
        let nread = getdents64(fd as usize, &mut buf);
        if nread < 0 {
            close(fd as usize);
            return 1;
        }
        if nread == 0 {
            break; // 目录读取完毕
        }

        let mut pos = 0usize;
        let nread = nread as usize;
        while pos + 19 <= nread {
            // d_reclen: u16 @ +16
            let reclen = u16::from_le_bytes([buf[pos + 16], buf[pos + 17]]) as usize;
            if reclen == 0 || pos + reclen > nread {
                break;
            }

            // d_type: u8 @ +18
            let dtype = buf[pos + 18];

            // d_name: starts @ +19, NUL-terminated within this record
            let name_field = &buf[pos + 19..pos + reclen];
            let name_len = name_field.iter().position(|&b| b == 0).unwrap_or(name_field.len());
            if name_len > 0 {
                if let Ok(name) = core::str::from_utf8(&name_field[..name_len]) {
                    if dtype == DT_DIR {
                        let _ = write(STDOUT, b"\x1b[34m");
                        let _ = write(STDOUT, name.as_bytes());
                        let _ = write(STDOUT, b"\x1b[0m\t");
                    } else {
                        let _ = write(STDOUT, name.as_bytes());
                        let _ = write(STDOUT, b"\t");
                    }
                }
            }

            pos += reclen;
        }
    }

    close(fd as usize);
    println!("");
    0
}