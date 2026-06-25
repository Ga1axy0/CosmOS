//! Key management syscalls used by LTP `add_key0x`.

use crate::keys;
use crate::mm::{translated_str, PageFaultAccess};
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::translated_byte_buffer_with_access;
use crate::syscall_body;
use crate::task::current_process;

/// `add_key(2)`
pub fn sys_add_key(
    type_ptr: *const u8,
    desc_ptr: *const u8,
    payload_ptr: *const u8,
    plen: usize,
    ringid: i32,
) -> isize {
    syscall_body!({
        let token = crate::task::current_user_token();
        let key_type = translated_str(token, type_ptr).or_errno(ERRNO::EFAULT)?;
        let description = translated_str(token, desc_ptr).or_errno(ERRNO::EFAULT)?;

        if !keys::key_type_supported(key_type.as_str()) {
            return Err(ERRNO::ENODEV);
        }

        match key_type.as_str() {
            "keyring" if plen != 0 => return Err(ERRNO::EINVAL),
            "user" | "logon" if plen > 32_767 => return Err(ERRNO::EINVAL),
            "big_key" if plen > ((1 << 20) - 1) => return Err(ERRNO::EINVAL),
            _ => {}
        }

        if plen > 0 {
            if payload_ptr.is_null() {
                return Err(ERRNO::EFAULT);
            }
            let _ = translated_byte_buffer_with_access(payload_ptr, plen, PageFaultAccess::Read)?;
        }

        let serial = keys::add_key(
            &current_process(),
            key_type.as_str(),
            description.as_str(),
            plen,
            ringid,
        )?;
        Ok(serial as isize)
    })
}

/// `keyctl(2)`
pub fn sys_keyctl(cmd: i32, arg2: usize, arg3: usize, _arg4: usize, _arg5: usize) -> isize {
    syscall_body!({
        let process = current_process();
        match cmd {
            keys::KEYCTL_GET_KEYRING_ID => {
                let keyring_id = arg2 as i32;
                let create = arg3 != 0;
                Ok(keys::get_keyring_id(&process, keyring_id, create)? as isize)
            }
            keys::KEYCTL_JOIN_SESSION_KEYRING => {
                let name = if arg2 == 0 {
                    None
                } else {
                    let token = crate::task::current_user_token();
                    Some(translated_str(token, arg2 as *const u8).or_errno(ERRNO::EFAULT)?)
                };
                Ok(keys::join_session_keyring(&process, name.as_deref())? as isize)
            }
            _ => Err(ERRNO::EINVAL),
        }
    })
}
