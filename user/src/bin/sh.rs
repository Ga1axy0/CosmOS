#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

const LF: u8 = 0x0au8;
const CR: u8 = 0x0du8;
const DL: u8 = 0x7fu8;
const BS: u8 = 0x08u8;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::console::getchar;
use user_lib::{chdir, close, dup, exec, flush, fork, open, waitpid, OpenFlags};

fn cwd_string() -> String {
    let mut buf = [0u8; 256];
    let ret = user_lib::getcwd(&mut buf);
    if ret == 0 {
        return String::from("?");
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    match core::str::from_utf8(&buf[..len]) {
        Ok(s) => {
            let mut out = String::new();
            out.push_str(s);
            out
        }
        Err(_) => String::from("?"),
    }
}

fn print_prompt() {
    let cwd = cwd_string();
    print!("{} >> ", cwd);
    flush();
}

#[no_mangle]
pub fn main() -> i32 {
    println!("Rust user shell");
    let mut line: String = String::new();
    print_prompt();
    loop {
        let c = getchar();
        match c {
            LF | CR => {
                println!("");
                if !line.is_empty() {
                    let args: Vec<&str> = line.as_str().split_whitespace().collect();
                    if !args.is_empty() {
                        // builtin: cd
                        if args[0] == "cd" {
                            let target = if args.len() >= 2 { args[1] } else { "/" };
                            if chdir(target) < 0 {
                                println!("cd: {}: No such directory", target);
                            }
                            line.clear();
                            print_prompt();
                            continue;
                        }

                        let mut argv: Vec<String> = args
                            .iter()
                            .map(|&arg| {
                                let mut s = String::new();
                                s.push_str(arg);
                                s
                            })
                            .collect();

                        // redirection (tokens must be separated by spaces, e.g., `ls > out`)
                        let mut input: Option<String> = None;
                        let mut output: Option<String> = None;
                        let mut i = 0usize;
                        while i < argv.len() {
                            if argv[i].as_str() == "<" {
                                if i + 1 >= argv.len() {
                                    println!("sh: syntax error near unexpected token `<'");
                                    input = None;
                                    output = None;
                                    argv.clear();
                                    break;
                                }
                                input = Some(argv[i + 1].clone());
                                argv.drain(i..=i + 1);
                                continue;
                            }
                            if argv[i].as_str() == ">" {
                                if i + 1 >= argv.len() {
                                    println!("sh: syntax error near unexpected token `>'");
                                    input = None;
                                    output = None;
                                    argv.clear();
                                    break;
                                }
                                output = Some(argv[i + 1].clone());
                                argv.drain(i..=i + 1);
                                continue;
                            }
                            i += 1;
                        }

                        if !argv.is_empty() {
                            // Convert argv into C strings for exec.
                            argv.iter_mut().for_each(|s| s.push('\0'));
                            let mut args_addr: Vec<*const u8> =
                                argv.iter().map(|arg| arg.as_ptr()).collect();
                            args_addr.push(core::ptr::null());

                            let pid = fork();
                            if pid == 0 {
                                // input redirection
                                if let Some(input) = input {
                                    let input_fd = open(input.as_str(), OpenFlags::RDONLY);
                                    if input_fd == -1 {
                                        println!("Error when opening file {}", input);
                                        return -4;
                                    }
                                    let input_fd = input_fd as usize;
                                    close(0);
                                    assert_eq!(dup(input_fd), 0);
                                    close(input_fd);
                                }
                                // output redirection
                                if let Some(output) = output {
                                    let output_fd =
                                        open(output.as_str(), OpenFlags::CREATE | OpenFlags::WRONLY);
                                    if output_fd == -1 {
                                        println!("Error when opening file {}", output);
                                        return -4;
                                    }
                                    let output_fd = output_fd as usize;
                                    close(1);
                                    assert_eq!(dup(output_fd), 1);
                                    close(output_fd);
                                }
                                // child process
                                if exec(argv[0].as_str(), args_addr.as_slice()) == -1 {
                                    println!("Error when executing!");
                                    return -4;
                                }
                                unreachable!();
                            } else {
                                let mut exit_code: i32 = 0;
                                let exit_pid = waitpid(pid as usize, &mut exit_code);
                                assert_eq!(pid, exit_pid);
                                println!("Shell: Process {} exited with code {}", pid, exit_code);
                            }
                        }
                    }
                    line.clear();
                }
                print_prompt();
            }
            BS | DL => {
                if !line.is_empty() {
                    print!("{}", BS as char);
                    print!(" ");
                    print!("{}", BS as char);
                    flush();
                    line.pop();
                }
            }
            _ => {
                print!("{}", c as char);
                flush();
                line.push(c as char);
            }
        }
    }
}
