//! Minimal key/keyring support for the LTP `add_key0x` cases.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::lazy_static;

use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{ProcessControlBlock, ProcessKeyrings};

/// `keyctl(KEYCTL_GET_KEYRING_ID, ...)`
pub const KEYCTL_GET_KEYRING_ID: i32 = 0;
/// `keyctl(KEYCTL_JOIN_SESSION_KEYRING, ...)`
pub const KEYCTL_JOIN_SESSION_KEYRING: i32 = 1;

/// Thread special keyring id.
pub const KEY_SPEC_THREAD_KEYRING: i32 = -1;
/// Process special keyring id.
pub const KEY_SPEC_PROCESS_KEYRING: i32 = -2;
/// Session special keyring id.
pub const KEY_SPEC_SESSION_KEYRING: i32 = -3;
/// User special keyring id.
pub const KEY_SPEC_USER_KEYRING: i32 = -4;
/// User session special keyring id.
pub const KEY_SPEC_USER_SESSION_KEYRING: i32 = -5;

const USER_PAYLOAD_LIMIT: usize = 32_767;
const BIG_KEY_PAYLOAD_LIMIT: usize = (1 << 20) - 1;
const DEFAULT_MAX_KEYS: u32 = 200;
const DEFAULT_MAX_BYTES: usize = 20_000;
const DEFAULT_GC_DELAY: u32 = 1;
const USER_KEY_OVERHEAD: usize = 16;

#[derive(Clone, Debug)]
enum KeyKind {
    Keyring { entries: Vec<i32> },
    User,
    Logon,
    BigKey,
    Opaque,
}

#[derive(Clone, Debug)]
struct Key {
    uid: u32,
    description: String,
    kind: KeyKind,
    quota_bytes: usize,
    quota_counted: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct UserUsage {
    user_keyring: Option<i32>,
    user_session_keyring: Option<i32>,
    used_keys: u32,
    used_bytes: usize,
}

#[derive(Debug)]
struct KeyState {
    next_serial: i32,
    keys: BTreeMap<i32, Key>,
    users: BTreeMap<u32, UserUsage>,
    max_keys: u32,
    max_bytes: usize,
    gc_delay: u32,
}

impl Default for KeyState {
    fn default() -> Self {
        Self {
            next_serial: 1,
            keys: BTreeMap::new(),
            users: BTreeMap::new(),
            max_keys: DEFAULT_MAX_KEYS,
            max_bytes: DEFAULT_MAX_BYTES,
            gc_delay: DEFAULT_GC_DELAY,
        }
    }
}

impl KeyState {
    fn alloc_serial(&mut self) -> i32 {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.saturating_add(1);
        serial
    }

    fn user_usage_mut(&mut self, uid: u32) -> &mut UserUsage {
        self.users.entry(uid).or_default()
    }

    fn key_exists(&self, serial: i32) -> bool {
        self.keys.contains_key(&serial)
    }

    fn key_is_keyring(&self, serial: i32) -> bool {
        self.keys
            .get(&serial)
            .map(|key| matches!(key.kind, KeyKind::Keyring { .. }))
            .unwrap_or(false)
    }

    fn insert_keyring(&mut self, uid: u32, description: String) -> i32 {
        let serial = self.alloc_serial();
        self.keys.insert(
            serial,
            Key {
                uid,
                description,
                kind: KeyKind::Keyring {
                    entries: Vec::new(),
                },
                quota_bytes: 0,
                quota_counted: false,
            },
        );
        serial
    }

    fn insert_payload_key(
        &mut self,
        uid: u32,
        description: String,
        kind: KeyKind,
        quota_bytes: usize,
        quota_counted: bool,
    ) -> i32 {
        let serial = self.alloc_serial();
        self.keys.insert(
            serial,
            Key {
                uid,
                description,
                kind,
                quota_bytes,
                quota_counted,
            },
        );
        serial
    }

    fn link_key(&mut self, dest_keyring: i32, key_serial: i32) -> Result<(), ERRNO> {
        let key = self.keys.get_mut(&dest_keyring).ok_or(ERRNO::EINVAL)?;
        match &mut key.kind {
            KeyKind::Keyring { entries } => {
                entries.push(key_serial);
                Ok(())
            }
            _ => Err(ERRNO::EINVAL),
        }
    }

    fn release_key_recursive(&mut self, serial: i32) {
        let Some(key) = self.keys.remove(&serial) else {
            return;
        };
        if key.quota_counted {
            let usage = self.user_usage_mut(key.uid);
            usage.used_keys = usage.used_keys.saturating_sub(1);
            usage.used_bytes = usage.used_bytes.saturating_sub(key.quota_bytes);
        }
        if let KeyKind::Keyring { entries } = key.kind {
            for entry in entries {
                self.release_key_recursive(entry);
            }
        }
    }

    fn ensure_user_special_keyring(&mut self, uid: u32, session: bool) -> i32 {
        let existing = self.users.get(&uid).and_then(|usage| {
            if session {
                usage.user_session_keyring
            } else {
                usage.user_keyring
            }
        });
        if let Some(serial) = existing {
            return serial;
        }
        let description = if session {
            format!("_uid_ses.{uid}")
        } else {
            format!("_uid.{uid}")
        };
        let serial = self.insert_keyring(uid, description);
        let usage = self.user_usage_mut(uid);
        if session {
            usage.user_session_keyring = Some(serial);
        } else {
            usage.user_keyring = Some(serial);
        }
        serial
    }

    fn ensure_process_special_keyring(
        &mut self,
        uid: u32,
        keyrings: &mut ProcessKeyrings,
        special_id: i32,
    ) -> Result<i32, ERRNO> {
        let slot = match special_id {
            KEY_SPEC_THREAD_KEYRING => &mut keyrings.thread,
            KEY_SPEC_PROCESS_KEYRING => &mut keyrings.process,
            KEY_SPEC_SESSION_KEYRING => &mut keyrings.session,
            _ => return Err(ERRNO::EINVAL),
        };
        if let Some(serial) = *slot {
            return Ok(serial);
        }
        let description = match special_id {
            KEY_SPEC_THREAD_KEYRING => "[thread-keyring]".to_string(),
            KEY_SPEC_PROCESS_KEYRING => "[process-keyring]".to_string(),
            KEY_SPEC_SESSION_KEYRING => "[session-keyring]".to_string(),
            _ => return Err(ERRNO::EINVAL),
        };
        let serial = self.insert_keyring(uid, description);
        *slot = Some(serial);
        Ok(serial)
    }

    fn resolve_destination_keyring(
        &mut self,
        uid: u32,
        keyrings: &mut ProcessKeyrings,
        ringid: i32,
        create: bool,
    ) -> Result<i32, ERRNO> {
        if ringid > 0 {
            if self.key_is_keyring(ringid) {
                return Ok(ringid);
            }
            return Err(ERRNO::EINVAL);
        }
        match ringid {
            KEY_SPEC_THREAD_KEYRING | KEY_SPEC_PROCESS_KEYRING | KEY_SPEC_SESSION_KEYRING => {
                if create {
                    self.ensure_process_special_keyring(uid, keyrings, ringid)
                } else {
                    let slot = match ringid {
                        KEY_SPEC_THREAD_KEYRING => keyrings.thread,
                        KEY_SPEC_PROCESS_KEYRING => keyrings.process,
                        KEY_SPEC_SESSION_KEYRING => keyrings.session,
                        _ => None,
                    };
                    slot.ok_or(ERRNO::ENOKEY)
                }
            }
            KEY_SPEC_USER_KEYRING => Ok(self.ensure_user_special_keyring(uid, false)),
            KEY_SPEC_USER_SESSION_KEYRING => Ok(self.ensure_user_special_keyring(uid, true)),
            _ => Err(ERRNO::EINVAL),
        }
    }

    fn validate_type_and_plen(&self, key_type: &str, plen: usize) -> Result<KeyKind, ERRNO> {
        match key_type {
            "keyring" => {
                if plen != 0 {
                    return Err(ERRNO::EINVAL);
                }
                Ok(KeyKind::Keyring {
                    entries: Vec::new(),
                })
            }
            "user" => {
                if plen > USER_PAYLOAD_LIMIT {
                    return Err(ERRNO::EINVAL);
                }
                Ok(KeyKind::User)
            }
            "logon" => {
                if plen > USER_PAYLOAD_LIMIT {
                    return Err(ERRNO::EINVAL);
                }
                Ok(KeyKind::Logon)
            }
            "big_key" => {
                if plen > BIG_KEY_PAYLOAD_LIMIT {
                    return Err(ERRNO::EINVAL);
                }
                Ok(KeyKind::BigKey)
            }
            "asymmetric" | "cifs.idmap" | "cifs.spnego" | "pkcs7_test" | "rxrpc" => {
                Ok(KeyKind::Opaque)
            }
            "rxrpc_s" => {
                if plen != 8 {
                    return Err(ERRNO::EINVAL);
                }
                Ok(KeyKind::Opaque)
            }
            _ => Err(ERRNO::ENODEV),
        }
    }
}

lazy_static! {
    static ref KEY_STATE: SpinNoIrqLock<KeyState> = SpinNoIrqLock::new(KeyState::default());
}

fn key_quota_bytes(description: &str, payload_len: usize) -> usize {
    USER_KEY_OVERHEAD + description.len() + 1 + payload_len
}

/// Returns whether a key type is supported by this minimal implementation.
pub fn key_type_supported(key_type: &str) -> bool {
    matches!(
        key_type,
        "keyring"
            | "user"
            | "logon"
            | "big_key"
            | "asymmetric"
            | "cifs.idmap"
            | "cifs.spnego"
            | "pkcs7_test"
            | "rxrpc"
            | "rxrpc_s"
    )
}

/// Add a key to the requested keyring.
pub fn add_key(
    process: &Arc<ProcessControlBlock>,
    key_type: &str,
    description: &str,
    payload_len: usize,
    ringid: i32,
) -> Result<i32, ERRNO> {
    let mut inner = process.inner_exclusive_access();
    let uid = inner.cred.uid;
    let privileged = inner.cred.euid == 0;

    let mut state = KEY_STATE.lock();
    let kind = state.validate_type_and_plen(key_type, payload_len)?;
    let quota_counted = !privileged && !matches!(kind, KeyKind::Keyring { .. });
    let quota_bytes = if quota_counted {
        key_quota_bytes(description, payload_len)
    } else {
        0
    };

    if quota_counted {
        let max_keys = state.max_keys;
        let max_bytes = state.max_bytes;
        let usage = state.user_usage_mut(uid);
        if usage.used_keys.saturating_add(1) > max_keys {
            return Err(ERRNO::EDQUOT);
        }
        if usage.used_bytes.saturating_add(quota_bytes) > max_bytes {
            return Err(ERRNO::EDQUOT);
        }
    }

    let dest_keyring = state.resolve_destination_keyring(uid, &mut inner.keyrings, ringid, true)?;
    let serial = match kind {
        KeyKind::Keyring { .. } => state.insert_keyring(uid, description.to_string()),
        other => state.insert_payload_key(
            uid,
            description.to_string(),
            other,
            quota_bytes,
            quota_counted,
        ),
    };
    state.link_key(dest_keyring, serial)?;

    if quota_counted {
        let usage = state.user_usage_mut(uid);
        usage.used_keys = usage.used_keys.saturating_add(1);
        usage.used_bytes = usage.used_bytes.saturating_add(quota_bytes);
    }

    Ok(serial)
}

/// Drop the per-thread keyring attached to a process context.
pub fn release_process_thread_keyring(keyrings: ProcessKeyrings) {
    if let Some(serial) = keyrings.thread {
        KEY_STATE.lock().release_key_recursive(serial);
    }
}

/// `keyctl(KEYCTL_GET_KEYRING_ID, ...)`
pub fn get_keyring_id(
    process: &Arc<ProcessControlBlock>,
    id: i32,
    create: bool,
) -> Result<i32, ERRNO> {
    let mut inner = process.inner_exclusive_access();
    let uid = inner.cred.uid;
    let mut state = KEY_STATE.lock();
    state.resolve_destination_keyring(uid, &mut inner.keyrings, id, create)
}

/// `keyctl(KEYCTL_JOIN_SESSION_KEYRING, ...)`
pub fn join_session_keyring(
    process: &Arc<ProcessControlBlock>,
    name: Option<&str>,
) -> Result<i32, ERRNO> {
    let mut inner = process.inner_exclusive_access();
    let uid = inner.cred.uid;
    let description = name.unwrap_or("[joined-session-keyring]").to_string();
    let mut state = KEY_STATE.lock();
    let serial = state.insert_keyring(uid, description);
    inner.keyrings.session = Some(serial);
    Ok(serial)
}

/// Render `/proc/key-users`.
pub fn render_key_users() -> String {
    let state = KEY_STATE.lock();
    let mut out = String::new();
    for (uid, usage) in state.users.iter() {
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "{uid:>5}: {:>5} {}/{} {}/{} {}/{}\n",
                1,
                usage.used_keys,
                usage.used_keys,
                usage.used_keys,
                state.max_keys,
                usage.used_bytes,
                state.max_bytes,
            ),
        );
    }
    out
}

/// Read `/proc/sys/kernel/keys/gc_delay`.
pub fn gc_delay() -> u32 {
    KEY_STATE.lock().gc_delay
}

/// Update `/proc/sys/kernel/keys/gc_delay`.
pub fn set_gc_delay(value: u32) {
    KEY_STATE.lock().gc_delay = value;
}

/// Read `/proc/sys/kernel/keys/maxkeys`.
pub fn max_keys() -> u32 {
    KEY_STATE.lock().max_keys
}

/// Update `/proc/sys/kernel/keys/maxkeys`.
pub fn set_max_keys(value: u32) {
    KEY_STATE.lock().max_keys = value;
}

/// Read `/proc/sys/kernel/keys/maxbytes`.
pub fn max_bytes() -> usize {
    KEY_STATE.lock().max_bytes
}

/// Update `/proc/sys/kernel/keys/maxbytes`.
pub fn set_max_bytes(value: usize) {
    KEY_STATE.lock().max_bytes = value;
}

/// Return whether a positive serial exists.
pub fn key_serial_exists(serial: i32) -> bool {
    KEY_STATE.lock().key_exists(serial)
}
