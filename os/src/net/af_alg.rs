//! Minimal AF_ALG socket support for LTP crypto regression tests.
//!
//! This models the userspace ABI shape (algorithm socket -> request socket)
//! and the errno semantics exercised by the current AF_ALG LTP cases. It is
//! intentionally not a full crypto subsystem implementation.

use alloc::{sync::Arc, vec::Vec};
use core::any::Any;

use crate::fs::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::poll::{POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;

pub(crate) const AF_ALG: i32 = 38;
pub(crate) const SOL_ALG: i32 = 279;
pub(crate) const SOCK_SEQPACKET: i32 = 5;

pub(crate) const ALG_SET_KEY: i32 = 1;
pub(crate) const ALG_SET_IV: i32 = 2;
pub(crate) const ALG_SET_OP: i32 = 3;
pub(crate) const ALG_SET_AEAD_ASSOCLEN: i32 = 4;

pub(crate) const ALG_OP_DECRYPT: u32 = 0;
pub(crate) const ALG_OP_ENCRYPT: u32 = 1;

#[derive(Clone, Debug)]
enum AlgBinding {
    Hash(HashBinding),
    Skcipher(SkcipherBinding),
    Aead(AeadBinding),
}

#[derive(Clone, Debug)]
enum HashBinding {
    Plain,
    Hmac,
    Vmac,
}

#[derive(Clone, Debug)]
enum SkcipherBinding {
    Salsa20,
    CbcAesGeneric,
}

#[derive(Clone, Debug)]
enum AeadBinding {
    Rfc7539,
    AuthEnc,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AlgSendMsgParams {
    pub(crate) op: Option<u32>,
    pub(crate) iv_len: usize,
    pub(crate) assoclen: Option<u32>,
}

#[derive(Clone, Debug, Default)]
enum ReadPlan {
    #[default]
    Idle,
    Eof,
    Zeros(usize),
    Invalid,
}

#[derive(Default)]
struct AlgSocketState {
    binding: Option<AlgBinding>,
    key: Vec<u8>,
}

#[derive(Default)]
struct AlgRequestState {
    read_plan: ReadPlan,
    bytes_written: usize,
}

pub(crate) struct AlgSocketFile {
    state: SpinNoIrqLock<AlgSocketState>,
}

pub(crate) struct AlgRequestFile {
    binding: AlgBinding,
    key: Vec<u8>,
    state: SpinNoIrqLock<AlgRequestState>,
}

fn supported_plain_hash(name: &str) -> bool {
    matches!(
        name,
        "md5"
            | "md5-generic"
            | "sha1"
            | "sha1-generic"
            | "sha224"
            | "sha224-generic"
            | "sha256"
            | "sha256-generic"
            | "sha3-256"
            | "sha3-256-generic"
            | "sha3-512"
            | "sha3-512-generic"
            | "sm3"
            | "sm3-generic"
    )
}

fn supported_vmac_cipher(name: &str) -> bool {
    matches!(name, "aes" | "sm4" | "sm4-generic")
}

fn parse_hash_binding(name: &str) -> Option<HashBinding> {
    if supported_plain_hash(name) {
        return Some(HashBinding::Plain);
    }

    if let Some(inner) = name
        .strip_prefix("hmac(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return supported_plain_hash(inner).then_some(HashBinding::Hmac);
    }

    if name.starts_with("hmac(hmac(") && name.ends_with("))") {
        return None;
    }

    if let Some(inner) = name
        .strip_prefix("vmac64(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return supported_vmac_cipher(inner).then_some(HashBinding::Vmac);
    }

    if let Some(inner) = name
        .strip_prefix("vmac(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return supported_vmac_cipher(inner).then_some(HashBinding::Vmac);
    }

    None
}

fn parse_binding(algtype: &str, algname: &str) -> Option<AlgBinding> {
    match algtype {
        "hash" => parse_hash_binding(algname).map(AlgBinding::Hash),
        "skcipher" => match algname {
            "salsa20" => Some(AlgBinding::Skcipher(SkcipherBinding::Salsa20)),
            "cbc(aes-generic)" => Some(AlgBinding::Skcipher(SkcipherBinding::CbcAesGeneric)),
            _ => None,
        },
        "aead" => match algname {
            "rfc7539(chacha20,poly1305)" => Some(AlgBinding::Aead(AeadBinding::Rfc7539)),
            "authenc(hmac(sha256),cbc(aes))" => Some(AlgBinding::Aead(AeadBinding::AuthEnc)),
            _ => None,
        },
        _ => None,
    }
}

fn copy_zeros_to_user(mut buf: UserBuffer, len: usize) -> usize {
    let mut remaining = len;
    let mut written = 0usize;
    for slice in buf.buffers.iter_mut() {
        if remaining == 0 {
            break;
        }
        let copy_len = remaining.min(slice.len());
        slice[..copy_len].fill(0);
        written += copy_len;
        remaining -= copy_len;
    }
    written
}

impl AlgSocketFile {
    fn source_id(&self) -> usize {
        self as *const Self as usize
    }

    pub(crate) fn bind(&self, algtype: &str, algname: &str) -> Result<(), ERRNO> {
        let binding = parse_binding(algtype, algname).ok_or(ERRNO::ENOENT)?;
        let mut state = self.state.lock();
        state.binding = Some(binding);
        state.key.clear();
        Ok(())
    }

    pub(crate) fn set_key(&self, optname: i32, key: &[u8]) -> Result<(), ERRNO> {
        if optname != ALG_SET_KEY {
            return Err(ERRNO::EOPNOTSUPP);
        }
        let mut state = self.state.lock();
        let binding = state.binding.as_ref().ok_or(ERRNO::EINVAL)?;
        match binding {
            AlgBinding::Aead(AeadBinding::AuthEnc) => Err(ERRNO::EINVAL),
            _ => {
                if key.is_empty() {
                    return Err(ERRNO::EINVAL);
                }
                state.key.clear();
                state.key.extend_from_slice(key);
                Ok(())
            }
        }
    }

    pub(crate) fn accept(&self) -> Result<Arc<AlgRequestFile>, ERRNO> {
        let state = self.state.lock();
        let binding = state.binding.clone().ok_or(ERRNO::EINVAL)?;
        Ok(Arc::new(AlgRequestFile {
            binding,
            key: state.key.clone(),
            state: SpinNoIrqLock::new(AlgRequestState::default()),
        }))
    }
}

impl AlgRequestFile {
    fn source_id(&self) -> usize {
        self as *const Self as usize
    }

    fn set_plan_for_write(&self, len: usize) {
        let mut state = self.state.lock();
        state.bytes_written = state.bytes_written.saturating_add(len);
        state.read_plan = match self.binding {
            AlgBinding::Skcipher(SkcipherBinding::CbcAesGeneric) if len % 16 != 0 => {
                ReadPlan::Invalid
            }
            _ => ReadPlan::Eof,
        };
    }

    pub(crate) fn sendmsg(&self, len: usize, params: AlgSendMsgParams) -> Result<usize, ERRNO> {
        let mut state = self.state.lock();
        state.bytes_written = state.bytes_written.saturating_add(len);
        state.read_plan = match self.binding {
            AlgBinding::Skcipher(SkcipherBinding::Salsa20) => {
                if params.op != Some(ALG_OP_ENCRYPT) || params.iv_len != 8 {
                    return Err(ERRNO::EINVAL);
                }
                ReadPlan::Eof
            }
            AlgBinding::Skcipher(SkcipherBinding::CbcAesGeneric) => {
                if len % 16 != 0 {
                    ReadPlan::Invalid
                } else {
                    ReadPlan::Zeros(len)
                }
            }
            AlgBinding::Aead(_) => {
                let _ = params.assoclen;
                ReadPlan::Zeros(len)
            }
            AlgBinding::Hash(_) => ReadPlan::Eof,
        };
        Ok(len)
    }
}

impl File for AlgSocketFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        true
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0u16;
        if (events & POLLOUT) != 0 {
            ready |= POLLOUT;
        }
        ready
    }

    fn poll_source_id(&self) -> usize {
        self.source_id()
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self.source_id() as u64,
            mode: StatMode::SOCK,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

impl File for AlgRequestFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at_result(&self, _offset: usize, buf: UserBuffer) -> Result<usize, ERRNO> {
        let plan = self.state.lock().read_plan.clone();
        match plan {
            ReadPlan::Idle | ReadPlan::Eof => Ok(0),
            ReadPlan::Zeros(len) => Ok(copy_zeros_to_user(buf, len)),
            ReadPlan::Invalid => Err(ERRNO::EINVAL),
        }
    }

    fn write_at_result(&self, _offset: usize, buf: UserBuffer) -> Result<usize, ERRNO> {
        let len = buf.len();
        let _ = self.key.len();
        self.set_plan_for_write(len);
        Ok(len)
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0u16;
        if (events & POLLIN) != 0 {
            ready |= POLLIN;
        }
        if (events & POLLOUT) != 0 {
            ready |= POLLOUT;
        }
        ready
    }

    fn poll_source_id(&self) -> usize {
        self.source_id()
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self.source_id() as u64,
            mode: StatMode::SOCK,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

pub(crate) fn create_alg_socket_file() -> Arc<AlgSocketFile> {
    Arc::new(AlgSocketFile {
        state: SpinNoIrqLock::new(AlgSocketState::default()),
    })
}
