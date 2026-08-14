//! Pure LoongArch scalar load/store decoding used by ALE emulation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegisterFile {
    General,
    Float,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Load,
    Store,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedAccess {
    pub(crate) register: usize,
    pub(crate) size: usize,
    pub(crate) sign_extend: bool,
    pub(crate) register_file: RegisterFile,
    pub(crate) operation: Operation,
}

impl DecodedAccess {
    const fn integer_load(register: usize, size: usize, sign_extend: bool) -> Self {
        Self {
            register,
            size,
            sign_extend,
            register_file: RegisterFile::General,
            operation: Operation::Load,
        }
    }

    const fn integer_store(register: usize, size: usize) -> Self {
        Self {
            register,
            size,
            sign_extend: false,
            register_file: RegisterFile::General,
            operation: Operation::Store,
        }
    }

    const fn float_load(register: usize, size: usize) -> Self {
        Self {
            register,
            size,
            // Linux's bytewise unaligned_read sign-extends FLD.S as well.
            sign_extend: true,
            register_file: RegisterFile::Float,
            operation: Operation::Load,
        }
    }

    const fn float_store(register: usize, size: usize) -> Self {
        Self {
            register,
            size,
            sign_extend: false,
            register_file: RegisterFile::Float,
            operation: Operation::Store,
        }
    }
}

/// Decode exactly the scalar instruction families handled by Linux
/// LoongArch's `emulate_load_store_insn`.
pub(crate) fn decode(instruction: u32) -> Option<DecodedAccess> {
    let rd = (instruction & 0x1f) as usize;

    // reg2i12: opcode is bits [31:22]. Byte accesses cannot be unaligned and
    // therefore deliberately do not appear in this whitelist.
    let decoded = match instruction >> 22 {
        0xa1 => Some(DecodedAccess::integer_load(rd, 2, true)), // ld.h
        0xa9 => Some(DecodedAccess::integer_load(rd, 2, false)), // ld.hu
        0xa5 => Some(DecodedAccess::integer_store(rd, 2)),      // st.h
        0xa2 => Some(DecodedAccess::integer_load(rd, 4, true)), // ld.w
        0xaa => Some(DecodedAccess::integer_load(rd, 4, false)), // ld.wu
        0xa6 => Some(DecodedAccess::integer_store(rd, 4)),      // st.w
        0xa3 => Some(DecodedAccess::integer_load(rd, 8, true)), // ld.d
        0xa7 => Some(DecodedAccess::integer_store(rd, 8)),      // st.d
        0xac => Some(DecodedAccess::float_load(rd, 4)),         // fld.s
        0xad => Some(DecodedAccess::float_store(rd, 4)),        // fst.s
        0xae => Some(DecodedAccess::float_load(rd, 8)),         // fld.d
        0xaf => Some(DecodedAccess::float_store(rd, 8)),        // fst.d
        _ => None,
    };
    if decoded.is_some() {
        return decoded;
    }

    // reg2i14: opcode is bits [31:24]. LL/SC occupy 0x20..=0x23 and must not
    // be converted into non-atomic loads/stores.
    let decoded = match instruction >> 24 {
        0x24 => Some(DecodedAccess::integer_load(rd, 4, true)), // ldptr.w
        0x25 => Some(DecodedAccess::integer_store(rd, 4)),      // stptr.w
        0x26 => Some(DecodedAccess::integer_load(rd, 8, true)), // ldptr.d
        0x27 => Some(DecodedAccess::integer_store(rd, 8)),      // stptr.d
        _ => None,
    };
    if decoded.is_some() {
        return decoded;
    }

    // reg3 indexed forms: opcode is bits [31:15]. Atomic and bounds-checking
    // opcodes sharing this format remain unsupported, matching Linux.
    match instruction >> 15 {
        0x7008 => Some(DecodedAccess::integer_load(rd, 2, true)), // ldx.h
        0x7048 => Some(DecodedAccess::integer_load(rd, 2, false)), // ldx.hu
        0x7028 => Some(DecodedAccess::integer_store(rd, 2)),      // stx.h
        0x7010 => Some(DecodedAccess::integer_load(rd, 4, true)), // ldx.w
        0x7050 => Some(DecodedAccess::integer_load(rd, 4, false)), // ldx.wu
        0x7030 => Some(DecodedAccess::integer_store(rd, 4)),      // stx.w
        0x7018 => Some(DecodedAccess::integer_load(rd, 8, true)), // ldx.d
        0x7038 => Some(DecodedAccess::integer_store(rd, 8)),      // stx.d
        0x7060 => Some(DecodedAccess::float_load(rd, 4)),         // fldx.s
        0x7070 => Some(DecodedAccess::float_store(rd, 4)),        // fstx.s
        0x7068 => Some(DecodedAccess::float_load(rd, 8)),         // fldx.d
        0x7078 => Some(DecodedAccess::float_store(rd, 8)),        // fstx.d
        _ => None,
    }
}

pub(crate) fn assemble_load_value(bytes: &[u8], sign_extend: bool) -> u64 {
    debug_assert!(matches!(bytes.len(), 2 | 4 | 8));
    let mut value = 0u64;
    for (index, byte) in bytes.iter().enumerate() {
        value |= (*byte as u64) << (index * 8);
    }
    if sign_extend && bytes.len() < 8 {
        let shift = 64 - bytes.len() * 8;
        ((value << shift) as i64 >> shift) as u64
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg2i12(opcode: u32, rd: u32) -> u32 {
        (opcode << 22) | rd
    }

    fn reg2i14(opcode: u32, rd: u32) -> u32 {
        (opcode << 24) | rd
    }

    fn reg3(opcode: u32, rd: u32) -> u32 {
        (opcode << 15) | rd
    }

    #[test]
    fn decodes_every_linux_whitelisted_opcode() {
        let cases = [
            reg2i12(0xa1, 1),
            reg2i12(0xa9, 2),
            reg2i12(0xa5, 3),
            reg2i12(0xa2, 4),
            reg2i12(0xaa, 5),
            reg2i12(0xa6, 6),
            reg2i12(0xa3, 7),
            reg2i12(0xa7, 8),
            reg2i12(0xac, 9),
            reg2i12(0xad, 10),
            reg2i12(0xae, 11),
            reg2i12(0xaf, 12),
            reg2i14(0x24, 13),
            reg2i14(0x25, 14),
            reg2i14(0x26, 15),
            reg2i14(0x27, 16),
            reg3(0x7008, 17),
            reg3(0x7048, 18),
            reg3(0x7028, 19),
            reg3(0x7010, 20),
            reg3(0x7050, 21),
            reg3(0x7030, 22),
            reg3(0x7018, 23),
            reg3(0x7038, 24),
            reg3(0x7060, 25),
            reg3(0x7070, 26),
            reg3(0x7068, 27),
            reg3(0x7078, 28),
        ];
        for instruction in cases {
            assert!(decode(instruction).is_some(), "{instruction:#010x}");
        }
    }

    #[test]
    fn rejects_byte_llsc_atomic_and_unknown_instructions() {
        for instruction in [
            reg2i12(0xa0, 1), // ld.b
            reg2i12(0xa4, 1), // st.b
            reg2i14(0x20, 1), // ll.w
            reg2i14(0x23, 1), // sc.d
            reg3(0x70c2, 1),  // amadd.w
            0,
        ] {
            assert_eq!(decode(instruction), None, "{instruction:#010x}");
        }
    }

    #[test]
    fn load_assembly_is_little_endian_and_sign_correct() {
        assert_eq!(assemble_load_value(&[0x34, 0x12], true), 0x1234);
        assert_eq!(assemble_load_value(&[0x00, 0x80], true), u64::MAX - 0x7fff);
        assert_eq!(assemble_load_value(&[0x00, 0x80], false), 0x8000);
        assert_eq!(
            assemble_load_value(&[0, 0, 0, 0x80], true),
            0xffff_ffff_8000_0000
        );
        assert_eq!(
            assemble_load_value(&[1, 2, 3, 4, 5, 6, 7, 8], true),
            0x0807_0605_0403_0201
        );
    }

    #[test]
    fn destination_register_is_common_low_field() {
        for instruction in [reg2i12(0xa2, 31), reg2i14(0x24, 30), reg3(0x7010, 29)] {
            assert_eq!(
                decode(instruction).unwrap().register,
                (instruction & 0x1f) as usize
            );
        }
    }
}
