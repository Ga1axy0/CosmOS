//! LoongArch user-mode unaligned load/store emulation.
//!
//! The instruction whitelist and signal split mirror Linux LoongArch's
//! `emulate_load_store_insn`: ordinary integer and scalar-FP loads/stores are
//! emulated, while LL/SC, atomic and bounds-checking instructions are not.

use crate::hal::traits::{UnalignedAccessKind, UnalignedEmulationOutcome};
use crate::mm::{PageFaultAccess, USER_SPACE_END};
use crate::syscall::{
    read_bytes_from_user, translated_byte_buffer_with_access, write_bytes_to_user,
};
use crate::task::current_trap_cx;

#[path = "unaligned_decode.rs"]
mod decode;

use decode::{assemble_load_value, decode, Operation, RegisterFile};

const INSTRUCTION_SIZE: usize = 4;

/// Emulate the current user ALE and advance ERA after a successful access.
pub(crate) fn emulate_current_user_ale(fault_addr: usize) -> UnalignedEmulationOutcome {
    let pc = current_trap_cx().user_pc();

    // Linux treats an alignment exception attributed to instruction fetch as
    // BUS_ADRALN rather than attempting data-access emulation.
    if fault_addr == pc {
        return UnalignedEmulationOutcome::Unsupported {
            instruction: None,
            reason: "unaligned instruction fetch",
        };
    }

    let instruction = match fetch_user_instruction(pc) {
        Ok(instruction) => instruction,
        Err(()) => {
            return UnalignedEmulationOutcome::Fault {
                instruction: None,
                access: UnalignedAccessKind::Execute,
                reason: "cannot fetch faulting instruction",
            };
        }
    };
    let Some(decoded) = decode(instruction) else {
        return UnalignedEmulationOutcome::Unsupported {
            instruction: Some(instruction),
            reason: "unsupported unaligned instruction",
        };
    };

    // Match Linux's access_ok() classification: a non-user or overflowing
    // range is SIGBUS here; a fault while touching an otherwise valid user
    // range is SIGSEGV.
    if !valid_user_range(fault_addr, decoded.size) {
        return UnalignedEmulationOutcome::Unsupported {
            instruction: Some(instruction),
            reason: "unaligned address outside user range",
        };
    }

    match decoded.operation {
        Operation::Load => {
            let bytes = match read_bytes_from_user(fault_addr as *const u8, decoded.size) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return UnalignedEmulationOutcome::Fault {
                        instruction: Some(instruction),
                        access: access_kind(decoded.operation),
                        reason: "unaligned user load fault",
                    };
                }
            };
            let value = assemble_load_value(&bytes, decoded.sign_extend);
            let cx = current_trap_cx();
            match decoded.register_file {
                RegisterFile::General => cx.set_reg(decoded.register, value as usize),
                RegisterFile::Float => cx.arch.f[decoded.register] = value,
            }
            cx.advance_user_pc(INSTRUCTION_SIZE);
        }
        Operation::Store => {
            let value = {
                let cx = current_trap_cx();
                match decoded.register_file {
                    RegisterFile::General => cx.reg(decoded.register) as u64,
                    RegisterFile::Float => cx.arch.f[decoded.register],
                }
            };
            let bytes = value.to_le_bytes();
            if write_bytes_to_user(fault_addr as *mut u8, &bytes[..decoded.size]).is_err() {
                return UnalignedEmulationOutcome::Fault {
                    instruction: Some(instruction),
                    access: access_kind(decoded.operation),
                    reason: "unaligned user store fault",
                };
            }
            current_trap_cx().advance_user_pc(INSTRUCTION_SIZE);
        }
    }

    UnalignedEmulationOutcome::Handled {
        instruction: Some(instruction),
        access: access_kind(decoded.operation),
        size: decoded.size,
    }
}

const fn access_kind(operation: Operation) -> UnalignedAccessKind {
    match operation {
        Operation::Load => UnalignedAccessKind::Read,
        Operation::Store => UnalignedAccessKind::Write,
    }
}

fn fetch_user_instruction(pc: usize) -> Result<u32, ()> {
    let buffers = translated_byte_buffer_with_access(
        pc as *const u8,
        INSTRUCTION_SIZE,
        PageFaultAccess::Exec,
    )
    .map_err(|_| ())?;
    let mut instruction = [0u8; INSTRUCTION_SIZE];
    let mut copied = 0usize;
    for buffer in buffers {
        let len = buffer.len().min(INSTRUCTION_SIZE - copied);
        instruction[copied..copied + len].copy_from_slice(&buffer[..len]);
        copied += len;
        if copied == INSTRUCTION_SIZE {
            break;
        }
    }
    if copied != INSTRUCTION_SIZE {
        return Err(());
    }
    Ok(u32::from_le_bytes(instruction))
}

#[inline]
fn valid_user_range(start: usize, size: usize) -> bool {
    start < USER_SPACE_END
        && start
            .checked_add(size)
            .is_some_and(|end| end <= USER_SPACE_END)
}
