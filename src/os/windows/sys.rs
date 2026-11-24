// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The calls into Windows itself that the standard library has no safe form of: who this
//! user is, who may open a file, a folder or a pipe, who serves a pipe, where a file's data
//! lies on the disk (and that it may leave holes), moving a file without replacing another,
//! and which handles a new program is given.
//!
//! This is the one module in cahoots with unchecked code. Each piece of it says why it is
//! sound, and the rest of the program sees only the safe functions here. It goes, with
//! `windows-sys`, if the standard library ever offers all of this itself.

use std::{
    ffi::{OsStr, c_void},
    fs, io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
    sync::OnceLock,
};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_MORE_DATA, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation,
    },
    Security::{
        ACL,
        Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
            SetSecurityInfo,
        },
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser, UNPROTECTED_DACL_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_TYPE_PIPE, FileStandardInfo, GetFileInformationByHandleEx, GetFileType,
        GetVolumeInformationByHandleW, MoveFileExW, QueryDosDeviceW, READ_CONTROL, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
    },
    System::{
        IO::DeviceIoControl,
        Ioctl::{FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES, FSCTL_SET_SPARSE},
        Pipes::GetNamedPipeServerProcessId,
        Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

/// The administrators of this computer and the system itself, as Windows writes them. They
/// can open anything here anyway, so a folder or pipe of theirs is as safe as one's own.
const ADMINISTRATORS: &str = "S-1-5-32-544";
const SYSTEM: &str = "S-1-5-18";
/// Set on a disk that keeps who may open what (NTFS, ReFS), and not on FAT or exFAT.
const FILE_PERSISTENT_ACLS: u32 = 0x8;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}

/// A path for the calls here that take one. Windows refuses a path longer than 260 characters
/// unless it is written `\\?\C:\...` (or `\\?\UNC\server\share\...`), as the standard library
/// writes them for its own calls; a restore of a long name into a deep folder needs that. That
/// form is taken exactly as it is, so the path is made whole first, which also settles `.`,
/// `..` and `/`.
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::path::{Component, Prefix};
    let path = std::path::absolute(path)?;
    let mut out: Vec<u16> = match path.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(_) => r"\\?\".encode_utf16().chain(path.as_os_str().encode_wide()).collect(),
            // \\server\share\... becomes \\?\UNC\server\share\...
            Prefix::UNC(..) => r"\\?\UNC".encode_utf16().chain(path.as_os_str().encode_wide().skip(1)).collect(),
            _ => path.as_os_str().encode_wide().collect(),
        },
        _ => path.as_os_str().encode_wide().collect(),
    };
    out.push(0);
    Ok(out)
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn answer(code: u32) -> io::Result<()> {
    if code == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(code as i32)) }
}

/// Memory Windows handed out with LocalAlloc, given back when this is dropped.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from Windows, which made it with LocalAlloc, and
            // nothing uses it after this.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Text Windows wrote into memory of its own, which is given back here.
///
/// SAFETY: the caller passes a string ending in a zero that Windows made with LocalAlloc,
/// and does not use it afterwards.
unsafe fn take_text(p: *mut u16) -> String {
    let _given_back = Local(p.cast());
    // SAFETY: the string ends in a zero (the caller's promise), so every place read up to
    // it is part of it.
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    // SAFETY: those `len` places were all just read, and Windows does not change them.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

/// A user or group in the form Windows writes it (`S-1-5-21-…`).
///
/// SAFETY: `sid` points at a valid SID for as long as the call runs.
unsafe fn sid_text(sid: PSID) -> io::Result<String> {
    let mut text = null_mut();
    // SAFETY: `sid` is valid (the caller's promise); Windows writes a new string's address
    // into `text`.
    check(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    // SAFETY: the call succeeded, so `text` is a string Windows made with LocalAlloc.
    Ok(unsafe { take_text(text) })
}

/// The user a process's token stands for.
fn token_user(process: HANDLE) -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: `process` is open for the call (both callers hold it); Windows writes a new
    // handle into `token`.
    check(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) })?;
    // SAFETY: the call succeeded, so `token` is a new handle that only this code holds.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    // Room for any TOKEN_USER (a SID is at most 68 bytes), aligned as it needs.
    let mut room = [0u64; 32];
    let mut len = 0;
    // SAFETY: `room` is writable for the size given and aligned for a TOKEN_USER.
    check(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            room.as_mut_ptr().cast(),
            size_of_val(&room) as u32,
            &mut len,
        )
    })?;
    // SAFETY: on success `room` starts with a TOKEN_USER, and the SID it points at is in
    // `room` too, which outlives the call to sid_text.
    unsafe { sid_text((*room.as_ptr().cast::<TOKEN_USER>()).User.Sid) }
}

/// The user this program runs as.
pub fn user() -> io::Result<&'static str> {
    static USER: OnceLock<String> = OnceLock::new();
    if let Some(me) = USER.get() {
        return Ok(me);
    }
    // SAFETY: no arguments; the answer stands for this process and needs no closing.
    let me = token_user(unsafe { GetCurrentProcess() })?;
    Ok(USER.get_or_init(|| me))
}

/// Whether an owner is this user, or one that can open everything here anyway. A folder
/// made by this user as administrator belongs to the administrators.
fn trusted(owner: &str) -> io::Result<bool> {
    Ok(owner == user()? || owner == ADMINISTRATORS || owner == SYSTEM)
}

fn mine(owner: io::Result<String>) -> bool {
    owner.is_ok_and(|o| user().is_ok_and(|me| o == me))
}

/// A security descriptor Windows built from its text form, given back when dropped.
struct Descriptor(Local);

impl Descriptor {
    fn new(text: &str) -> io::Result<Descriptor> {
        let text = wide(OsStr::new(text));
        let mut sd = null_mut();
        // SAFETY: `text` ends in a zero and outlives the call; Windows writes a new
        // descriptor's address into `sd`.
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
        })?;
        Ok(Descriptor(Local(sd)))
    }

    fn ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0.0
    }

    /// Its list of who may do what, in the form Windows writes it (`D:P(A;;FA;;;SY)…`).
    fn dacl_text(&self) -> io::Result<String> {
        let mut text = null_mut();
        // SAFETY: the descriptor is valid while `self` lives; Windows writes a new string's
        // address into `text`.
        check(unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                self.ptr(),
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                null_mut(),
            )
        })?;
        // SAFETY: the call succeeded, so `text` is a string Windows made with LocalAlloc.
        Ok(unsafe { take_text(text) })
    }
}

/// Who a private file or folder is kept for besides this user and the system itself.
#[derive(Clone, Copy, PartialEq)]
pub enum Keep {
    /// Nobody else: the home, and what a restore keeps private.
    Mine,
    /// This computer's administrators too, for a store. They are the same group on every
    /// computer, so whoever moves a drive to a new computer (as another account there) can
    /// always get back into it as administrator, and an administrator can open it anyway.
    Store,
}

/// Only this user, and the system itself (and, for a store, the administrators), may open it,
/// and nothing is taken from the folder it is in. For a folder, all that is made in it later
/// starts out the same. `owner` names this user its owner too: a new file or folder needs no
/// owner named, being its maker's, and a share may refuse one it does not know.
fn private_text(folder: bool, keep: Keep, owner: bool) -> io::Result<String> {
    let me = user()?;
    let passed_on = if folder { "OICI" } else { "" };
    let owner = if owner { format!("O:{me}") } else { String::new() };
    let admins = if keep == Keep::Store { format!("(A;{passed_on};FA;;;BA)") } else { String::new() };
    Ok(format!("{owner}D:P(A;{passed_on};FA;;;{me})(A;{passed_on};FA;;;SY){admins}"))
}

/// What Windows keeps about who owns, or who may open, what `h` has open: as much as
/// `what` asks for.
fn security(h: &impl AsRawHandle, what: u32) -> io::Result<Descriptor> {
    let mut sd = null_mut();
    // SAFETY: the handle is open for the call; Windows writes a new descriptor's address
    // into `sd` and leaves the other pointers alone, being given none.
    answer(unsafe {
        GetSecurityInfo(
            h.as_raw_handle(),
            SE_FILE_OBJECT,
            what,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        )
    })?;
    Ok(Descriptor(Local(sd)))
}

/// This user as Windows writes them in a list of who may do what: by id, or by the short
/// name it has for a few accounts (`LA`, the administrator a computer starts out with, which
/// is who a GitHub runner works as).
#[cfg(test)]
pub fn user_as_listed() -> io::Result<String> {
    let text = Descriptor::new(&format!("D:(A;;FA;;;{})", user()?))?.dacl_text()?;
    Ok(text.trim_end_matches(')').rsplit(';').next().unwrap_or_default().to_string())
}

/// Who owns what `h` has open.
pub fn owner(h: &impl AsRawHandle) -> io::Result<String> {
    let sd = security(h, OWNER_SECURITY_INFORMATION)?;
    let (mut sid, mut defaulted) = (null_mut(), 0);
    // SAFETY: the descriptor is valid while `sd` lives; Windows points `sid` into it.
    check(unsafe { GetSecurityDescriptorOwner(sd.ptr(), &mut sid, &mut defaulted) })?;
    if sid.is_null() {
        return Err(io::Error::other("it has no owner"));
    }
    // SAFETY: `sid` points into `sd`, which lives until after the call.
    unsafe { sid_text(sid) }
}

/// Who may do what with what `h` has open, as Windows writes it.
pub fn dacl(h: &impl AsRawHandle) -> io::Result<String> {
    security(h, DACL_SECURITY_INFORMATION)?.dacl_text()
}

/// Whether the disk `h` is on keeps who may open what at all.
fn keeps_permissions(h: &impl AsRawHandle) -> io::Result<bool> {
    let mut flags = 0;
    // SAFETY: the handle is open for the call; Windows writes only into `flags`, being
    // given no other room.
    check(unsafe {
        GetVolumeInformationByHandleW(
            h.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    })?;
    Ok(flags & FILE_PERSISTENT_ACLS != 0)
}

/// Whether what `f` has open belongs to this user (or to one that can open everything here
/// anyway). On a disk that keeps no owners, nothing belongs to anyone, and it does.
pub fn owned_by_me(f: &fs::File) -> io::Result<bool> {
    Ok(!keeps_permissions(f)? || trusted(&owner(f)?)?)
}

/// Whether what `f` has open belongs to this user and nobody else, as every file this
/// program makes private in its home does (`create_private` names its owner). On a disk that
/// keeps no owners, nothing belongs to anyone, and it does.
pub fn made_by_me(f: &fs::File) -> bool {
    keeps_permissions(f).is_ok_and(|keeps| !keeps || mine(owner(f)))
}

/// Whether `f` is already open to whom `keep` says only, as `private_text` writes it.
fn is_private(f: &fs::File, folder: bool, keep: Keep) -> io::Result<bool> {
    // Windows writes the same list back with marks of its own (`AI`, for one), so both are
    // read the way Windows writes them and only the protection and the entries compared.
    let entries = |text: &str| {
        let rest = text.strip_prefix("D:")?;
        let at = rest.find('(')?;
        Some((rest[..at].contains('P'), rest[at..].to_string()))
    };
    let want = entries(&Descriptor::new(&private_text(folder, keep, false)?)?.dacl_text()?);
    Ok(want.is_some() && entries(&dacl(f)?) == want)
}

/// Makes what `f` has open (with at least READ_CONTROL) this user's alone, and the
/// system's (and the administrators', for a store), and for a folder, all that is already in
/// it too. `again` opens the very same thing once more with the rights asked for: Windows
/// cannot add rights to a folder's handle, so the caller opens it by name and makes sure it
/// is the same one. It is refused when someone else owns it: whoever owns a thing can always
/// open it to themselves again. A store another account owns (one made on another computer)
/// is taken over where this user may (as administrator, or once given full control of it),
/// and refused otherwise. A disk that keeps no permissions (FAT, exFAT) is left as it is,
/// there being nothing to set.
pub fn make_private(
    f: &fs::File,
    folder: bool,
    keep: Keep,
    again: impl Fn(u32) -> io::Result<fs::File>,
) -> io::Result<()> {
    if !keeps_permissions(f)? {
        return Ok(());
    }
    let (owner, me) = (owner(f)?, user()?);
    let theirs = !trusted(&owner)?;
    let refused = || {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("it belongs to another account ({owner}), which could open it again at any time"),
        )
    };
    if theirs && keep != Keep::Store {
        return Err(refused());
    }
    if !theirs && is_private(f, folder, keep)? {
        return Ok(());
    }
    let sd = Descriptor::new(&private_text(folder, keep, true)?)?;
    let (sid, dacl) = parts(&sd)?;
    let set = |owner_too: bool| {
        let mut access = READ_CONTROL | WRITE_DAC;
        let mut what = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        if owner_too {
            access |= WRITE_OWNER;
            what |= OWNER_SECURITY_INFORMATION;
        }
        if folder {
            // So Windows can go through what is already in it.
            access |= FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
        }
        let h = again(access)?;
        // SAFETY: `h` is open with the rights asked for; `sid` and `dacl` point into `sd`,
        // which outlives the call, and Windows only reads them.
        answer(unsafe { SetSecurityInfo(h.as_raw_handle(), SE_FILE_OBJECT, what, sid, null_mut(), dacl, null()) })
    };
    match set(owner != me) {
        // Another account's store that this user may not take over: left to the caller.
        Err(_) if theirs => Err(refused()),
        // The administrators' own, to an administrator who is not one just now: it stays
        // theirs, and closed to everyone else all the same.
        Err(e) if owner != me && e.kind() == io::ErrorKind::PermissionDenied => set(false),
        done => done,
    }
}

/// The parts of a security descriptor that `SetSecurityInfo` and the like take: its owner
/// and its list of who may do what, both pointing into `sd`.
fn parts(sd: &Descriptor) -> io::Result<(PSID, *mut ACL)> {
    let (mut sid, mut dacl, mut present, mut defaulted) = (null_mut(), null_mut(), 0, 0);
    // SAFETY: the descriptor is valid while `sd` lives; Windows points `sid` and `dacl`
    // into it.
    check(unsafe { GetSecurityDescriptorOwner(sd.ptr(), &mut sid, &mut defaulted) })?;
    // SAFETY: as above.
    check(unsafe { GetSecurityDescriptorDacl(sd.ptr(), &mut present, &mut dacl, &mut defaulted) })?;
    Ok((sid, dacl))
}

/// A new file only this user and the system can open, with `access`; fails if the name is
/// taken, by a link too. It is private from the moment it exists, not made so after. With
/// `owner`, this user is named its owner even when running as administrator (whose files
/// would otherwise be the administrators'), as the home's files must be (see `made_by_me`).
pub fn create_private(path: &Path, access: u32, owner: bool) -> io::Result<fs::File> {
    let sd = Descriptor::new(&private_text(false, Keep::Mine, owner)?)?;
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.ptr(),
        bInheritHandle: 0,
    };
    let name = wide_path(path)?;
    // SAFETY: `name` ends in a zero, and it, `attrs` and the descriptor `attrs` points at
    // all outlive the call; a failure returns INVALID_HANDLE_VALUE.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attrs,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new handle that only this code holds.
    Ok(unsafe { fs::File::from_raw_handle(h) })
}

/// What a file being restored is opened with: to write it, read what it is, and later give
/// it the permissions of the folder it is in.
pub const RESTORING: u32 = GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC;

/// Makes a new folder only this user and the system can open, as can all that is made in
/// it; fails if the name is taken. Private from the moment it exists, not made so after.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let sd = Descriptor::new(&private_text(true, Keep::Mine, false)?)?;
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.ptr(),
        bInheritHandle: 0,
    };
    let name = wide_path(path)?;
    // SAFETY: `name` ends in a zero, and it, `attrs` and the descriptor `attrs` points at
    // all outlive the call.
    check(unsafe { CreateDirectoryW(name.as_ptr(), &attrs) })
}

/// Gives what `f` has open (with WRITE_DAC) the permissions the folder it is in passes on,
/// as a file made there in the usual way gets, in place of a list of its own.
pub fn inherit(f: &fs::File) -> io::Result<()> {
    // An empty list, no longer protected: what remains is what the folder passes on.
    set_dacl(f, "D:", false)?;
    // A folder that passes nothing on would leave it open to nobody. It stays this user's
    // then, as a file made there in the usual way is its maker's.
    let now = dacl(f)?;
    if !now.contains('(') && !now.contains("NO_ACCESS_CONTROL") {
        set_dacl(f, &private_text(false, Keep::Mine, false)?, true)?;
    }
    Ok(())
}

/// Gives what `f` has open (with WRITE_DAC) the list `text` says: with `protected`, that
/// and nothing from its folder; without, that and what its folder passes on.
fn set_dacl(f: &fs::File, text: &str, protected: bool) -> io::Result<()> {
    let sd = Descriptor::new(text)?;
    let (_, dacl) = parts(&sd)?;
    let how = if protected { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
    // SAFETY: the handle is open for the call with WRITE_DAC; `dacl` points into `sd`, which
    // outlives the call, and Windows only reads it.
    answer(unsafe {
        SetSecurityInfo(
            f.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | how,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    })
}

/// Moves `from` to `to`, never replacing what is there: a name that is taken, by any of
/// the names Windows knows it by (`PROGRA~1` as much as `Program Files`), fails.
pub fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (wide_path(from)?, wide_path(to)?);
    // SAFETY: both names end in a zero and outlive the call, which only reads them.
    check(unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) })
}

/// Whether drive `letter` is a disk of this computer's own, found without opening anything
/// on it: by the name Windows keeps for it, so a drive that stands for a share (`net use`)
/// or for a folder (`subst`) is not one, and nothing is asked of the network.
pub fn local_drive(letter: u8) -> bool {
    if !letter.is_ascii_alphabetic() {
        return false;
    }
    let name = wide(OsStr::new(&format!("{}:", letter as char)));
    let mut room = [0u16; 1024];
    // SAFETY: `name` ends in a zero and outlives the call; Windows writes at most the length
    // given into `room`, and returns how much it wrote (0 on failure).
    let n = unsafe { QueryDosDeviceW(name.as_ptr(), room.as_mut_ptr(), room.len() as u32) } as usize;
    // What it writes is a list of names, each ending in a zero: the first is the one in use.
    let first = room[..n.min(room.len())].split(|&c| c == 0).next().unwrap_or_default();
    String::from_utf16_lossy(first).starts_with(r"\Device\HarddiskVolume")
}

/// Makes an instance of a named pipe that only this user and the system can open.
pub fn create_pipe(options: &ServerOptions, name: &str) -> io::Result<NamedPipeServer> {
    let me = user()?;
    let sd = Descriptor::new(&format!("O:{me}D:P(A;;FA;;;{me})(A;;FA;;;SY)"))?;
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: `attrs` is a SECURITY_ATTRIBUTES, as tokio asks, and it and the descriptor it
    // points at outlive the call, which only reads them.
    unsafe { options.create_with_security_attributes_raw(name, (&raw mut attrs).cast()) }
}

/// Whether `pipe` is a pipe this user made and a program of this user serves. Its owner is
/// the proof: nobody can make a pipe that belongs to someone else, and the daemon names
/// itself the owner of its own. The server's user is asked too, where Windows lets this
/// user ask it.
pub fn served_by_me(pipe: &impl AsRawHandle) -> bool {
    let h = pipe.as_raw_handle();
    // SAFETY: the handle is open for the call, which only asks what kind it is.
    if unsafe { GetFileType(h) } != FILE_TYPE_PIPE {
        return false;
    }
    if !mine(owner(pipe)) {
        return false;
    }
    let mut pid = 0;
    // SAFETY: the handle is open for the call; Windows writes only into `pid`.
    if unsafe { GetNamedPipeServerProcessId(h, &mut pid) } == 0 {
        return false;
    }
    // SAFETY: no pointers; a failure returns null.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: a new handle that only this code holds.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    match token_user(process.as_raw_handle()) {
        Ok(user) => mine(Ok(user)),
        // A daemon started as administrator keeps its token from this user when this user
        // is not; the pipe's owner, checked above, already says it is ours.
        Err(e) => e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32),
    }
}

/// How much of a file the disk really holds.
pub fn allocated(f: &fs::File) -> io::Result<u64> {
    let mut info = FILE_STANDARD_INFO::default();
    // SAFETY: the handle is open for the call, and `info` is writable for the size given.
    check(unsafe {
        GetFileInformationByHandleEx(
            f.as_raw_handle(),
            FileStandardInfo,
            (&raw mut info).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    })?;
    Ok(info.AllocationSize.max(0) as u64)
}

/// The first stretch of the file between `from` and `to` that the disk holds data for, as
/// (start, end); None when there is only empty space there.
pub fn first_data(f: &fs::File, from: u64, to: u64) -> io::Result<Option<(u64, u64)>> {
    if from >= to {
        return Ok(None);
    }
    let ask = FILE_ALLOCATED_RANGE_BUFFER { FileOffset: from as i64, Length: (to - from) as i64 };
    let mut got = [FILE_ALLOCATED_RANGE_BUFFER::default()];
    let mut len = 0;
    // SAFETY: the handle is open for the call, and was opened for plain (not overlapped)
    // use, so the call finishes before it returns; `ask` and `got` are readable and
    // writable for the sizes given.
    let done = unsafe {
        DeviceIoControl(
            f.as_raw_handle(),
            FSCTL_QUERY_ALLOCATED_RANGES,
            (&raw const ask).cast(),
            size_of_val(&ask) as u32,
            got.as_mut_ptr().cast(),
            size_of_val(&got) as u32,
            &mut len,
            null_mut(),
        )
    };
    // More stretches than the room for one is not a failure: the first is what is wanted.
    if done == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
            return Err(e);
        }
    }
    if (len as usize) < size_of::<FILE_ALLOCATED_RANGE_BUFFER>() {
        return Ok(None);
    }
    let (start, n) = (got[0].FileOffset.max(0) as u64, got[0].Length.max(0) as u64);
    Ok(Some((start.max(from), start.saturating_add(n).min(to))).filter(|(s, e)| s < e))
}

/// Marks a file as one the disk may leave holes in: what is skipped over when writing it,
/// or left at its end, then takes no room on the disk.
pub fn set_sparse(f: &fs::File) -> io::Result<()> {
    let mut len = 0;
    // SAFETY: the handle is open for the call, for plain use; the call is given no buffers.
    check(unsafe {
        DeviceIoControl(f.as_raw_handle(), FSCTL_SET_SPARSE, null(), 0, null_mut(), 0, &mut len, null_mut())
    })
}

/// Keeps what this program starts from being handed this program's own input and output.
/// Windows passes every handle marked for it to a new program, and a shell marks the pipe
/// it gives us: the daemon started by `cahoots status | findstr x` would hold that pipe
/// open for as long as it runs, and `findstr` would wait for it for ever.
///
/// The standard library has no stable way to start a program without passing handles on.
/// When it has one (`inherit_handles`, https://github.com/rust-lang/rust/issues/146407),
/// use that and take this function out.
pub fn keep_stdio() {
    let own = [io::stdin().as_raw_handle(), io::stdout().as_raw_handle(), io::stderr().as_raw_handle()];
    for handle in own.into_iter().filter(|h| !h.is_null()) {
        // SAFETY: the handle is one of this process's own standard handles, which stay open
        // for as long as it runs. The call clears one flag on it and touches no memory; on
        // a handle Windows does not know it fails and changes nothing.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    }
}
