use alloc::{string::String, vec::Vec};

pub const DIR_ENTRY_SIZE: usize = 32;

#[allow(dead_code)]
pub const ATTR_READ_ONLY: u8 = 0x01;
#[allow(dead_code)]
pub const ATTR_HIDDEN: u8 = 0x02;
#[allow(dead_code)]
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LFN: u8 = 0x0F;

/// A directory entry as presented to VFS: one SFN entry, optionally with a preceding LFN name.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub sfn: SfnDirEntry,
    pub long_name: Option<String>,
}

impl DirEntry {
    pub fn name_string(&self) -> String {
        match &self.long_name {
            Some(s) => s.clone(),
            None => self.sfn.name_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SfnDirEntry {
    pub name_raw: [u8; 11],
    pub attr: u8,
    pub first_cluster: u32,
    pub file_size: u32,
    /// Byte offset within the directory file where this 32B entry resides.
    pub entry_offset: usize,
}

impl SfnDirEntry {
    #[allow(dead_code)]
    pub fn is_free(&self) -> bool {
        self.name_raw[0] == 0x00 || self.name_raw[0] == 0xE5
    }

    pub fn is_dir(&self) -> bool {
        (self.attr & ATTR_DIRECTORY) != 0
    }

    #[allow(dead_code)]
    pub fn is_file(&self) -> bool {
        !self.is_dir()
    }

    pub fn is_lfn(&self) -> bool {
        self.attr == ATTR_LFN
    }

    pub fn is_volume_label(&self) -> bool {
        (self.attr & ATTR_VOLUME_ID) != 0
    }

    pub fn name_string(&self) -> String {
        sfn_to_string(&self.name_raw)
    }
}

pub fn sfn_to_string(raw: &[u8; 11]) -> String {
    let name = raw[0..8]
        .iter()
        .copied()
        .take_while(|c| *c != b' ')
        .collect::<Vec<u8>>();
    let ext = raw[8..11]
        .iter()
        .copied()
        .take_while(|c| *c != b' ')
        .collect::<Vec<u8>>();
    if ext.is_empty() {
        String::from_utf8_lossy(&name).into_owned()
    } else {
        let mut s = String::from_utf8_lossy(&name).into_owned();
        s.push('.');
        s.push_str(&String::from_utf8_lossy(&ext));
        s
    }
}

/// Convert an input name to 8.3 SFN (11 bytes, uppercased, space-padded).
///
/// Supported examples:
/// - "FOO" -> "FOO     " + "   "
/// - "FOO.TXT" -> "FOO     " + "TXT"
///
/// Returns None if the name cannot be represented as SFN.
pub fn sfn_from_str(name: &str) -> Option<[u8; 11]> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    let mut parts = name.split('.');
    let base = parts.next().unwrap_or("");
    let ext = parts.next();
    if parts.next().is_some() {
        // more than one dot
        return None;
    }

    let base = base.as_bytes();
    if base.is_empty() || base.len() > 8 {
        return None;
    }
    let ext_bytes = ext.map(|s| s.as_bytes()).unwrap_or(b"");
    if ext_bytes.len() > 3 {
        return None;
    }

    fn valid_sfn_char(c: u8) -> bool {
        matches!(c, b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'$' | b'~' | b'!')
    }

    let mut raw = [b' '; 11];
    for (i, &c) in base.iter().enumerate() {
        let c = c.to_ascii_uppercase();
        if !valid_sfn_char(c) {
            return None;
        }
        raw[i] = c;
    }
    for (i, &c) in ext_bytes.iter().enumerate() {
        let c = c.to_ascii_uppercase();
        if !valid_sfn_char(c) {
            return None;
        }
        raw[8 + i] = c;
    }

    Some(raw)
}

pub fn parse_sfn_dir_entry(raw: &[u8; DIR_ENTRY_SIZE], entry_offset: usize) -> Option<SfnDirEntry> {
    let first = raw[0];
    if first == 0x00 {
        // end of directory
        return None;
    }

    let attr = raw[11];
    let mut name_raw = [0u8; 11];
    name_raw.copy_from_slice(&raw[0..11]);

    let fst_clus_hi = u16::from_le_bytes([raw[20], raw[21]]) as u32;
    let fst_clus_lo = u16::from_le_bytes([raw[26], raw[27]]) as u32;
    let first_cluster = (fst_clus_hi << 16) | fst_clus_lo;

    let file_size = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);

    Some(SfnDirEntry {
        name_raw,
        attr,
        first_cluster,
        file_size,
        entry_offset,
    })
}

// ----------------------
// FAT32 Long File Name (LFN) support
// ----------------------

#[derive(Clone, Debug)]
pub struct LfnPart {
    pub order: u8,
    pub is_last: bool,
    pub checksum: u8,
    /// 13 UTF-16 code units.
    pub name_units: [u16; 13],
}

#[inline]
fn read_u16_le(raw: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([raw[i], raw[i + 1]])
}

#[inline]
fn write_u16_le(raw: &mut [u8], i: usize, v: u16) {
    let b = v.to_le_bytes();
    raw[i] = b[0];
    raw[i + 1] = b[1];
}

/// LFN checksum of the 11-byte SFN name.
///
/// Algorithm per FAT spec: rotate-right by 1 then add each byte.
pub fn lfn_checksum(sfn_raw: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &c in sfn_raw.iter() {
        sum = ((sum & 1) << 7) + (sum >> 1) + c;
    }
    sum
}

/// Parse one raw 32B entry as LFN part.
/// Returns None if it is not a valid LFN entry.
pub fn parse_lfn_part(raw: &[u8; DIR_ENTRY_SIZE]) -> Option<LfnPart> {
    // LFN entry has ATTR_LFN and type==0 and fstClusLO==0.
    if raw[11] != ATTR_LFN {
        return None;
    }
    if raw[12] != 0 {
        return None;
    }
    if raw[26] != 0 || raw[27] != 0 {
        return None;
    }

    let ord = raw[0];
    let is_last = (ord & 0x40) != 0;
    let order = ord & 0x1F;
    if order == 0 {
        return None;
    }

    let checksum = raw[13];

    let mut units = [0u16; 13];
    // name1: 5 UTF-16 units at offset 1..10
    for (i, u) in units.iter_mut().take(5).enumerate() {
        *u = read_u16_le(raw, 1 + i * 2);
    }
    // name2: 6 units at offset 14..25
    for i in 0..6 {
        units[5 + i] = read_u16_le(raw, 14 + i * 2);
    }
    // name3: 2 units at offset 28..31
    for i in 0..2 {
        units[11 + i] = read_u16_le(raw, 28 + i * 2);
    }

    Some(LfnPart {
        order,
        is_last,
        checksum,
        name_units: units,
    })
}

/// Assemble a full LFN string from a set of parts (in any order), verifying checksum and order.
///
/// Returns None if parts are inconsistent or checksum mismatches.
pub fn assemble_lfn(parts: &[LfnPart], sfn_raw: &[u8; 11]) -> Option<String> {
    if parts.is_empty() {
        return None;
    }
    let want_sum = lfn_checksum(sfn_raw);
    let mut last_order: u8 = 0;
    for p in parts.iter() {
        if p.checksum != want_sum {
            return None;
        }
        if p.is_last {
            // In well-formed LFN, exactly one last part exists.
            if last_order != 0 {
                return None;
            }
            last_order = p.order;
        }
    }
    if last_order == 0 {
        return None;
    }
    if parts.len() != last_order as usize {
        return None;
    }

    let mut table: Vec<Option<&LfnPart>> = (0..last_order).map(|_| None).collect();
    for p in parts.iter() {
        if p.order == 0 || p.order > last_order {
            return None;
        }
        let idx = (p.order - 1) as usize;
        if table[idx].is_some() {
            return None;
        }
        table[idx] = Some(p);
    }

    let mut units: Vec<u16> = Vec::new();
    for slot in table.into_iter() {
        let p = slot?;
        units.extend_from_slice(&p.name_units);
    }

    // Strip after terminator 0x0000; ignore 0xFFFF fillers.
    let mut out_units: Vec<u16> = Vec::new();
    for u in units.into_iter() {
        if u == 0x0000 {
            break;
        }
        if u == 0xFFFF {
            continue;
        }
        out_units.push(u);
    }
    Some(String::from_utf16_lossy(&out_units))
}

/// Basic validity check for a FAT LFN (not exhaustive, but rejects common invalid names).
pub fn is_valid_lfn_name(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.len() > 255 {
        return false;
    }
    // FAT forbids trailing spaces/dots in Windows semantics.
    if name.ends_with(' ') || name.ends_with('.') {
        return false;
    }
    // Forbid path separators and a subset of invalid characters.
    for ch in name.chars() {
        if ch == '/' || ch == '\\' {
            return false;
        }
        if ch.is_control() {
            return false;
        }
        // Common forbidden set: " * / : < > ? \\ |
        if matches!(ch, '"' | '*' | ':' | '<' | '>' | '?' | '|') {
            return false;
        }
    }
    true
}

fn sanitize_sfn_char(ch: char) -> u8 {
    if ch.is_ascii_alphanumeric() {
        (ch as u8).to_ascii_uppercase()
    } else {
        // Keep a conservative subset that our SFN parser accepts.
        match ch {
            '_' | '-' | '$' | '~' | '!' => ch as u8,
            _ => b'_',
        }
    }
}

fn digits(mut n: u32) -> usize {
    let mut d = 1usize;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Build an 8.3 SFN alias for a long name using the `~n` scheme.
///
/// This is a simplified (yet compatible) alias generator; caller must handle collision probing.
pub fn sfn_alias_from_lfn(name: &str, n: u32) -> Option<[u8; 11]> {
    if n == 0 {
        return None;
    }
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let (base_part, ext_part) = match name.rsplit_once('.') {
        Some((b, e)) if !b.is_empty() && !e.is_empty() => (b, e),
        _ => (name, ""),
    };

    let mut ext: Vec<u8> = Vec::new();
    for ch in ext_part.chars() {
        if ext.len() >= 3 {
            break;
        }
        ext.push(sanitize_sfn_char(ch));
    }

    let d = digits(n);
    // base prefix length so that PREFIX + '~' + digits <= 8
    let prefix_len = 8usize.saturating_sub(1 + d).min(6);
    let mut prefix: Vec<u8> = Vec::new();
    for ch in base_part.chars() {
        if prefix.len() >= prefix_len {
            break;
        }
        if ch == '.' {
            continue;
        }
        prefix.push(sanitize_sfn_char(ch));
    }
    if prefix.is_empty() {
        prefix.extend_from_slice(b"FILE");
        if prefix.len() > prefix_len {
            prefix.truncate(prefix_len);
        }
    }

    let mut base: Vec<u8> = Vec::new();
    base.extend_from_slice(&prefix);
    base.push(b'~');
    // n in decimal
    let mut tmp = [0u8; 10];
    let mut idx = 0usize;
    let mut m = n;
    while m > 0 {
        tmp[idx] = b'0' + (m % 10) as u8;
        idx += 1;
        m /= 10;
    }
    for i in (0..idx).rev() {
        base.push(tmp[i]);
    }
    if base.len() > 8 {
        base.truncate(8);
    }

    let mut raw = [b' '; 11];
    for (i, &b) in base.iter().enumerate().take(8) {
        raw[i] = b;
    }
    for (i, &b) in ext.iter().enumerate().take(3) {
        raw[8 + i] = b;
    }
    Some(raw)
}

/// Build raw LFN entries (each 32B) for a given long name + its SFN alias.
///
/// Returned vector is in on-disk order: last-part (with 0x40) first, then ... , then order=1.
pub fn build_lfn_entries(name: &str, sfn_raw: &[u8; 11]) -> Option<Vec<[u8; 32]>> {
    if !is_valid_lfn_name(name) {
        return None;
    }

    // Encode UTF-16 with explicit terminator 0x0000.
    let mut units: Vec<u16> = name.encode_utf16().collect();
    units.push(0x0000);
    while units.len() % 13 != 0 {
        units.push(0xFFFF);
    }

    let chunks = units.chunks(13).collect::<Vec<&[u16]>>();
    let n = chunks.len();
    if n == 0 || n > 0x1F {
        // order field stores up to 0x1F parts
        return None;
    }
    let checksum = lfn_checksum(sfn_raw);

    let mut out: Vec<[u8; 32]> = Vec::new();
    // On disk: last part first.
    for (i, chunk) in chunks.into_iter().enumerate().rev() {
        let order = (i + 1) as u8;
        let is_last = order as usize == n;
        let mut raw = [0u8; 32];
        raw[0] = order | if is_last { 0x40 } else { 0 };
        raw[11] = ATTR_LFN;
        raw[12] = 0;
        raw[13] = checksum;
        raw[26] = 0;
        raw[27] = 0;

        // Fill name fields.
        for (j, &u) in chunk.iter().take(5).enumerate() {
            write_u16_le(&mut raw, 1 + j * 2, u);
        }
        for (j, &u) in chunk.iter().skip(5).take(6).enumerate() {
            write_u16_le(&mut raw, 14 + j * 2, u);
        }
        for (j, &u) in chunk.iter().skip(11).take(2).enumerate() {
            write_u16_le(&mut raw, 28 + j * 2, u);
        }

        out.push(raw);
    }

    Some(out)
}

/// Compare two names in a "mostly FAT-like" way: ASCII case-insensitive, otherwise exact.
pub fn name_eq(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

pub fn build_sfn_entry(
    name_raw: &[u8; 11],
    attr: u8,
    first_cluster: u32,
    file_size: u32,
) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0..11].copy_from_slice(name_raw);
    e[11] = attr;

    let hi = ((first_cluster >> 16) as u16).to_le_bytes();
    let lo = ((first_cluster & 0xFFFF) as u16).to_le_bytes();
    e[20] = hi[0];
    e[21] = hi[1];
    e[26] = lo[0];
    e[27] = lo[1];

    e[28..32].copy_from_slice(&file_size.to_le_bytes());
    e
}
