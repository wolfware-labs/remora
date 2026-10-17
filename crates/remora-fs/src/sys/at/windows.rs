//! Handle-relative system calls on Windows, through `NtCreateFile` (`sys::nt`) and
//! the by-handle information classes (spec 5.5, 5.9). Every open grants all three
//! share modes and opens a reparse point itself, never its target.

use crate::Error;
use crate::nofollow::{Entry, FileKind, Metadata, Timestamp};
use crate::sys::name::check_name;
use crate::sys::{Dir, nt};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_INFORMATION_CLASS, FILE_NON_DIRECTORY_FILE,
    FILE_RENAME_IGNORE_READONLY_ATTRIBUTE, FILE_RENAME_INFORMATION, FILE_RENAME_POSIX_SEMANTICS,
    FILE_RENAME_REPLACE_IF_EXISTS, FileRenameInformation, FileRenameInformationEx,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION,
    GetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_GENERIC_READ, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS,
    FILE_READ_ATTRIBUTES, FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES,
    FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo, FileDispositionInfoEx,
    FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo, FileStandardInfo,
    GetFileInformationByHandleEx, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

/// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`.
const REPARSE_BUFFER: usize = 16 * 1024;
/// 100 ns intervals between 1601-01-01 and 1970-01-01.
const EPOCH_DIFFERENCE: i64 = 116_444_736_000_000_000;

/// Opens the subdirectory `name` of `parent` relative to the handle, then refuses
/// a reparse point: a symbolic link or junction is `IsSymlink`, anything else that
/// is not a plain directory `NotADirectory`.
pub(crate) fn open_dir(parent: &Dir, name: &OsStr) -> Result<Dir, Error> {
    check_name(name)?;
    match nt::open(
        parent,
        name,
        FILE_GENERIC_READ | FILE_TRAVERSE,
        FILE_DIRECTORY_FILE,
    ) {
        Ok(handle) => match handle_kind(handle.as_handle())? {
            FileKind::Dir => Ok(Dir::from_handle(handle)),
            FileKind::Symlink => Err(Error::IsSymlink),
            _ => Err(Error::NotADirectory),
        },
        // A file symlink has no directory attribute; ask the entry.
        Err(e) if e.kind() == io::ErrorKind::NotADirectory => match stat_at(parent, name)?.kind {
            FileKind::Symlink => Err(Error::IsSymlink),
            _ => Err(Error::NotADirectory),
        },
        Err(e) => Err(e.into()),
    }
}

/// The metadata of `name` itself, from a handle opened for attributes only.
pub(crate) fn stat_at(parent: &Dir, name: &OsStr) -> io::Result<Metadata> {
    check_name(name)?;
    let handle = nt::open(parent, name, FILE_READ_ATTRIBUTES, 0)?;
    metadata(handle.as_handle())
}

/// The metadata of an open file.
pub(crate) fn fstat(file: &File) -> io::Result<Metadata> {
    metadata(file.as_handle())
}

/// The entries of `dir` except `.` and `..`, each with the metadata of its
/// listing record (spec 7.4 rule 8), so a walk needs no open per entry. The
/// listing starts with the restart class, so it always reads from the first
/// entry, and holds the `Dir`'s listing lock, since the position lives in the
/// handle's file object.
pub(crate) fn entries(dir: &Dir) -> io::Result<Vec<Entry>> {
    let _listing_lock = dir.lock_listing();
    let listing = dir.as_handle();
    // The records carry no volume serial: every entry shares the directory's.
    let volume: FILE_ID_INFO = query(listing, FileIdInfo)?;
    // u64 elements: the records are 8-byte aligned.
    let mut buf = vec![0u64; 8 * 1024];
    let buf_bytes = (buf.len() * 8) as u32;
    let mut class = FileIdExtdDirectoryRestartInfo;
    let mut out = Vec::new();
    loop {
        // SAFETY: `buf` holds `buf_bytes` writable bytes, aligned for the records.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                listing.as_raw_handle(),
                class,
                buf.as_mut_ptr().cast(),
                buf_bytes,
            )
        };
        if ok == 0 {
            // SAFETY: reads the calling thread's last error.
            if unsafe { GetLastError() } == ERROR_NO_MORE_FILES {
                return Ok(out);
            }
            return Err(io::Error::last_os_error());
        }
        class = FileIdExtdDirectoryInfo;
        let base = buf.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            // SAFETY: the call filled the buffer with records chained by
            // NextEntryOffset, each 8-byte aligned and inside the buffer; fields are
            // read through the raw pointer, without a reference to the record.
            let (next, meta, name) = unsafe {
                let record = base.add(offset).cast::<FILE_ID_EXTD_DIR_INFO>();
                let name_ptr = base
                    .add(offset + std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName))
                    .cast::<u16>();
                let len = (*record).FileNameLength as usize / 2;
                let meta = Metadata {
                    kind: kind((*record).FileAttributes, (*record).ReparsePointTag),
                    mode: (*record).FileAttributes,
                    size: u64::try_from((*record).EndOfFile).unwrap_or(0),
                    modified: from_filetime((*record).LastWriteTime),
                    changed: from_filetime((*record).ChangeTime),
                    dev: volume.VolumeSerialNumber,
                    ino: u128::from_le_bytes((*record).FileId.Identifier),
                };
                (
                    (*record).NextEntryOffset as usize,
                    meta,
                    std::slice::from_raw_parts(name_ptr, len).to_vec(),
                )
            };
            if name != [u16::from(b'.')] && name != [u16::from(b'.'), u16::from(b'.')] {
                let os_name = OsString::from_wide(&name);
                let mut meta = meta;
                // A mount-point tag is a junction or a volume mount point; only
                // the reparse data tells them apart.
                if meta.kind == FileKind::Symlink
                    && let Ok(child) = nt::open(dir, &os_name, FILE_READ_ATTRIBUTES, 0)
                    && let Ok(kind) = handle_kind(child.as_handle())
                {
                    meta.kind = kind;
                }
                out.push(Entry::new(os_name, Some(meta.kind), Some(meta)));
            }
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
}

/// The target of the symbolic link or junction `name`, from its reparse data: the
/// print name, or the substitute name without its `\??\` prefix when the print
/// name is empty. `InvalidData` for another reparse tag, a volume mount point, a
/// plain entry, or a target longer than `max` bytes (in WTF-8).
pub(crate) fn read_link_at(parent: &Dir, name: &OsStr, max: usize) -> io::Result<Vec<u8>> {
    check_name(name)?;
    let handle = nt::open(parent, name, FILE_READ_ATTRIBUTES, 0)?;
    let buf = reparse_data(handle.as_handle())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("not a link: {e}")))?;
    if is_volume_mount(&buf) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a volume mount point is not a link",
        ));
    }
    let target = parse_link(&buf)?;
    let bytes = OsString::from_wide(&target).into_encoded_bytes();
    if bytes.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("symlink target longer than {max} bytes"),
        ));
    }
    Ok(bytes)
}

/// Opens the file `name` for reading relative to `parent`. A symbolic link, a
/// link to a directory included, is `IsSymlink`; a directory or another reparse
/// point `Changed`.
pub(crate) fn open_read(parent: &Dir, name: &OsStr) -> Result<File, Error> {
    check_name(name)?;
    let handle = match nt::open(parent, name, FILE_GENERIC_READ, FILE_NON_DIRECTORY_FILE) {
        Ok(handle) => handle,
        // A link to a directory carries the directory attribute; ask the entry.
        Err(e) if e.kind() == io::ErrorKind::IsADirectory => {
            return Err(match stat_at(parent, name)?.kind {
                FileKind::Symlink => Error::IsSymlink,
                _ => Error::Changed,
            });
        }
        Err(e) => return Err(e.into()),
    };
    match handle_kind(handle.as_handle())? {
        FileKind::File => Ok(File::from(handle)),
        FileKind::Symlink => Err(Error::IsSymlink),
        _ => Err(Error::Changed),
    }
}

/// Nothing to undo on Windows: the open in [`open_read`] does not block.
pub(crate) fn finish_read(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Deletes `name` itself, a reparse point included, with POSIX semantics (the name
/// goes away at once) and ignoring the read-only attribute (spec 5.9 rule 3). On a
/// filesystem without POSIX deletes it clears read-only and uses the classic
/// disposition. As on Unix, `is_dir` must match the entry: `true` for a real
/// directory, `false` for a file, a symlink or a junction (a link is removed
/// itself).
pub(crate) fn unlink_at(parent: &Dir, name: &OsStr, is_dir: bool) -> Result<(), Error> {
    check_name(name)?;
    let handle = nt::open(
        parent,
        name,
        DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
        0,
    )
    .map_err(in_use_or)?;
    let real_dir = handle_kind(handle.as_handle())? == FileKind::Dir;
    if is_dir != real_dir {
        // The I/O errors Unix reports for the same mismatch.
        let kind = if is_dir {
            io::ErrorKind::NotADirectory
        } else {
            io::ErrorKind::IsADirectory
        };
        return Err(io::Error::from(kind).into());
    }
    let posix = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    match set_info(handle.as_handle(), FileDispositionInfoEx, &posix) {
        Ok(()) => Ok(()),
        Err(e) if is_unsupported(&e) => {
            clear_read_only_on(handle.as_handle()).map_err(in_use_or)?;
            let classic = FILE_DISPOSITION_INFO { DeleteFile: true };
            set_info(handle.as_handle(), FileDispositionInfo, &classic).map_err(in_use_or)
        }
        Err(e) => Err(in_use_or(e)),
    }
}

/// Renames `from` in `from_dir` to `to` in `to_dir`, replacing an existing file,
/// a read-only one included (spec 5.9 rule 2).
pub(crate) fn rename_at(
    from_dir: &Dir,
    from: &OsStr,
    to_dir: &Dir,
    to: &OsStr,
) -> Result<(), Error> {
    rename(from_dir, from, to_dir, to, true, false)
}

/// The rename without replacing: an existing `to` fails with `AlreadyExists`
/// and both entries stay as they were.
pub(crate) fn rename_no_replace(
    from_dir: &Dir,
    from: &OsStr,
    to_dir: &Dir,
    to: &OsStr,
) -> Result<(), Error> {
    rename(from_dir, from, to_dir, to, false, false)
}

/// Hard links are not a fallback on Windows: [`rename_no_replace`] has its own
/// fallback and never reports `Unsupported`, so this is never reached there.
pub(crate) fn link_at(
    _from_dir: &Dir,
    _from: &OsStr,
    _to_dir: &Dir,
    _to: &OsStr,
) -> Result<(), Error> {
    Err(io::Error::from(io::ErrorKind::Unsupported).into())
}

/// The mode is ignored on Windows: access comes from the security descriptor set
/// at creation.
pub(crate) fn set_mode(_file: &File, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// No call on Windows: NTFS and ReFS journal directory entries, so a directory is
/// not flushed (spec 5.4).
pub(crate) fn sync_dir(_dir: &Dir) -> io::Result<()> {
    Ok(())
}

/// `FileRenameInformationEx` on a handle to `from` itself, with `RootDirectory`
/// set to `to_dir` and POSIX semantics. On a filesystem without POSIX renames, or
/// when `force_fallback` is set for a test, the classic `FileRenameInformation`
/// on the same handles.
fn rename(
    from_dir: &Dir,
    from: &OsStr,
    to_dir: &Dir,
    to: &OsStr,
    replace: bool,
    force_fallback: bool,
) -> Result<(), Error> {
    check_name(from)?;
    check_name(to)?;
    let handle = nt::open(from_dir, from, DELETE | FILE_READ_ATTRIBUTES, 0).map_err(in_use_or)?;
    if !force_fallback {
        let mut flags = FILE_RENAME_POSIX_SEMANTICS;
        if replace {
            flags |= FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_IGNORE_READONLY_ATTRIBUTE;
        }
        match rename_by_handle(
            handle.as_handle(),
            to_dir,
            to,
            flags,
            FileRenameInformationEx,
        ) {
            Ok(()) => return Ok(()),
            Err(e) if is_unsupported(&e) => {}
            Err(e) => return Err(in_use_or(e)),
        }
    }
    // The classic class has no POSIX semantics and cannot ignore a read-only
    // target, so clear the attribute first. It keeps RootDirectory, so the rename
    // stays relative to the parent handle.
    let cleared = if replace {
        clear_read_only(to_dir, to)?
    } else {
        None
    };
    // In the classic structure the first byte is ReplaceIfExists.
    let replace_flag = u32::from(replace);
    let renamed = rename_by_handle(
        handle.as_handle(),
        to_dir,
        to,
        replace_flag,
        FileRenameInformation,
    )
    .map_err(in_use_or);
    if renamed.is_err()
        && let Some(id) = cleared
    {
        // A failed rename leaves the target as it was, read-only included, unless
        // the name now holds another file. The rename's error is the one reported.
        let _ = nt::open(to_dir, to, FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, 0).and_then(
            |target| {
                if file_id(target.as_handle())? == id {
                    set_read_only_on(target.as_handle(), true)?;
                }
                Ok(())
            },
        );
    }
    renamed
}

/// Clears the read-only attribute of `name` in `dir`. Returns the identity of the
/// file if the attribute was set, `None` otherwise; a missing name is not an error.
fn clear_read_only(dir: &Dir, name: &OsStr) -> Result<Option<(u64, u128)>, Error> {
    match nt::open(dir, name, FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, 0) {
        Ok(handle) => {
            let id = file_id(handle.as_handle()).map_err(in_use_or)?;
            let cleared = clear_read_only_on(handle.as_handle()).map_err(in_use_or)?;
            Ok(cleared.then_some(id))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(in_use_or(e)),
    }
}

/// The volume serial number and the 128-bit file id of an open file.
fn file_id(handle: BorrowedHandle<'_>) -> io::Result<(u64, u128)> {
    let id: FILE_ID_INFO = query(handle, FileIdInfo)?;
    Ok((
        id.VolumeSerialNumber,
        u128::from_le_bytes(id.FileId.Identifier),
    ))
}

fn clear_read_only_on(handle: BorrowedHandle<'_>) -> io::Result<bool> {
    set_read_only_on(handle, false)
}

/// Sets or clears the read-only attribute. Returns whether it changed.
fn set_read_only_on(handle: BorrowedHandle<'_>, read_only: bool) -> io::Result<bool> {
    let basic: FILE_BASIC_INFO = query(handle, FileBasicInfo)?;
    if (basic.FileAttributes & FILE_ATTRIBUTE_READONLY != 0) == read_only {
        return Ok(false);
    }
    let attributes = if read_only {
        (basic.FileAttributes | FILE_ATTRIBUTE_READONLY) & !FILE_ATTRIBUTE_NORMAL
    } else {
        basic.FileAttributes & !FILE_ATTRIBUTE_READONLY
    };
    // Zero leaves the attributes unchanged, so "none left" is FILE_ATTRIBUTE_NORMAL.
    let clear = FILE_BASIC_INFO {
        FileAttributes: if attributes == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attributes
        },
        ..FILE_BASIC_INFO::default()
    };
    set_info(handle, FileBasicInfo, &clear)?;
    Ok(true)
}

/// `NtSetInformationFile` with a `FILE_RENAME_INFORMATION` for `class`
/// (`FileRenameInformationEx` takes flags, `FileRenameInformation` a
/// ReplaceIfExists byte in the same place).
fn rename_by_handle(
    handle: BorrowedHandle<'_>,
    to_dir: &Dir,
    to: &OsStr,
    flags: u32,
    class: FILE_INFORMATION_CLASS,
) -> io::Result<()> {
    let name: Vec<u16> = to.encode_wide().collect();
    let header = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let total = header + (name.len() + 1) * 2;
    // u64 elements: FILE_RENAME_INFORMATION holds a handle and needs pointer
    // alignment.
    let mut buf = vec![0u64; total.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let name_bytes = u32::try_from(name.len() * 2).map_err(io::Error::other)?;
    // SAFETY: `buf` is zeroed, aligned and `total` bytes long, which covers the
    // header and the name plus a NUL; fields are written through the raw pointer.
    unsafe {
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = to_dir.as_handle().as_raw_handle();
        (*info).FileNameLength = name_bytes;
        let dest = buf.as_mut_ptr().cast::<u8>().add(header).cast::<u16>();
        std::ptr::copy_nonoverlapping(name.as_ptr(), dest, name.len());
    }
    let size = u32::try_from(total).map_err(io::Error::other)?;
    let mut status_block = IO_STATUS_BLOCK::default();
    // The native call: the Win32 SetFileInformationByHandle refuses a rename with
    // a RootDirectory handle as an invalid parameter.
    // SAFETY: `buf` holds a FILE_RENAME_INFORMATION of `size` bytes for this
    // class, and `status_block` is a live local.
    let status = unsafe {
        NtSetInformationFile(
            handle.as_raw_handle(),
            &mut status_block,
            buf.as_ptr().cast(),
            size,
            class,
        )
    };
    if status < 0 {
        return Err(nt::status_error(status));
    }
    Ok(())
}

/// Sets the access and modification times of `name` itself, through a handle
/// to the reparse point when it is one (spec 5.5 rule 7).
pub(crate) fn set_times_at(
    parent: &Dir,
    name: &OsStr,
    accessed: Timestamp,
    modified: Timestamp,
) -> io::Result<()> {
    check_name(name)?;
    let handle = nt::open(parent, name, FILE_WRITE_ATTRIBUTES, 0)?;
    // Zero means "leave unchanged" for the creation and change times and the
    // attributes.
    let info = FILE_BASIC_INFO {
        LastAccessTime: to_filetime(accessed),
        LastWriteTime: to_filetime(modified),
        ..FILE_BASIC_INFO::default()
    };
    set_info(handle.as_handle(), FileBasicInfo, &info)
}

fn metadata(handle: BorrowedHandle<'_>) -> io::Result<Metadata> {
    let id: FILE_ID_INFO = query(handle, FileIdInfo)?;
    let basic: FILE_BASIC_INFO = query(handle, FileBasicInfo)?;
    let standard: FILE_STANDARD_INFO = query(handle, FileStandardInfo)?;
    Ok(Metadata {
        kind: handle_kind(handle)?,
        mode: basic.FileAttributes,
        size: u64::try_from(standard.EndOfFile).unwrap_or(0),
        modified: from_filetime(basic.LastWriteTime),
        changed: from_filetime(basic.ChangeTime),
        dev: id.VolumeSerialNumber,
        ino: u128::from_le_bytes(id.FileId.Identifier),
    })
}

/// The kind of the entry an open handle refers to. A mount-point tag is a
/// junction, which counts as a symlink, unless its reparse data names a volume
/// (`\??\Volume{...}`): a volume mount point is another filesystem, reported as
/// `ReparsePoint`.
fn handle_kind(handle: BorrowedHandle<'_>) -> io::Result<FileKind> {
    let tag: FILE_ATTRIBUTE_TAG_INFO = query(handle, FileAttributeTagInfo)?;
    let kind = kind(tag.FileAttributes, tag.ReparseTag);
    if kind == FileKind::Symlink
        && tag.ReparseTag == IO_REPARSE_TAG_MOUNT_POINT
        && is_volume_mount(&reparse_data(handle)?)
    {
        return Ok(FileKind::ReparsePoint(IO_REPARSE_TAG_MOUNT_POINT));
    }
    Ok(kind)
}

/// The reparse data of the reparse point an open handle refers to.
fn reparse_data(handle: BorrowedHandle<'_>) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; REPARSE_BUFFER];
    let mut returned = 0u32;
    // SAFETY: `buf` holds REPARSE_BUFFER writable bytes; no input buffer.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buf.as_mut_ptr().cast(),
            REPARSE_BUFFER as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(returned as usize);
    Ok(buf)
}

/// Whether reparse data is a mount point that names a volume rather than a
/// directory. A symbolic link to a volume path is still a link.
fn is_volume_mount(buf: &[u8]) -> bool {
    let tag = buf.first_chunk::<4>().map(|b| u32::from_le_bytes(*b));
    let prefix: Vec<u16> = r"\??\Volume{".encode_utf16().collect();
    tag == Some(IO_REPARSE_TAG_MOUNT_POINT)
        && substitute_name(buf).is_ok_and(|name| name.starts_with(&prefix))
}

fn kind(attributes: u32, tag: u32) -> FileKind {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        if is_link_tag(tag) {
            FileKind::Symlink
        } else {
            FileKind::ReparsePoint(tag)
        }
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        FileKind::Dir
    } else {
        FileKind::File
    }
}

/// Symbolic links and junctions are both recorded as symlinks (spec 5.5 rule 2).
fn is_link_tag(tag: u32) -> bool {
    tag == IO_REPARSE_TAG_SYMLINK || tag == IO_REPARSE_TAG_MOUNT_POINT
}

/// Decodes the substitute and print names of `REPARSE_DATA_BUFFER` for a
/// symbolic link or a mount point.
fn link_names(buf: &[u8]) -> io::Result<(Vec<u16>, Vec<u16>)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "malformed reparse data");
    let u16_at = |at: usize| -> io::Result<usize> {
        let bytes = buf.get(at..at + 2).ok_or_else(invalid)?;
        Ok(usize::from(u16::from_le_bytes([bytes[0], bytes[1]])))
    };
    let tag_bytes = buf.get(0..4).ok_or_else(invalid)?;
    let tag = u32::from_le_bytes([tag_bytes[0], tag_bytes[1], tag_bytes[2], tag_bytes[3]]);
    // Symbolic links carry a 4-byte Flags field before the path buffer.
    let path_start = match tag {
        IO_REPARSE_TAG_SYMLINK => 20,
        IO_REPARSE_TAG_MOUNT_POINT => 16,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("reparse tag {other:#x} is not a link"),
            ));
        }
    };
    let name = |offset: usize, len: usize| -> io::Result<Vec<u16>> {
        if !offset.is_multiple_of(2) || !len.is_multiple_of(2) {
            return Err(invalid());
        }
        let start = path_start + offset;
        let bytes = buf.get(start..start + len).ok_or_else(invalid)?;
        Ok(bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect())
    };
    Ok((
        name(u16_at(8)?, u16_at(10)?)?,
        name(u16_at(12)?, u16_at(14)?)?,
    ))
}

/// The target a link's reparse data names: the print name, or the substitute
/// name without its `\??\` prefix when the print name is empty.
fn parse_link(buf: &[u8]) -> io::Result<Vec<u16>> {
    let (substitute, print) = link_names(buf)?;
    if !print.is_empty() {
        return Ok(print);
    }
    let prefix: Vec<u16> = r"\??\".encode_utf16().collect();
    Ok(substitute
        .strip_prefix(prefix.as_slice())
        .map_or(substitute.clone(), <[u16]>::to_vec))
}

fn substitute_name(buf: &[u8]) -> io::Result<Vec<u16>> {
    Ok(link_names(buf)?.0)
}

/// Reads one information class into a value of the type that class fills.
fn query<T: Default>(
    handle: BorrowedHandle<'_>,
    class: FILE_INFO_BY_HANDLE_CLASS,
) -> io::Result<T> {
    let mut out = T::default();
    let size = u32::try_from(size_of::<T>()).map_err(io::Error::other)?;
    // SAFETY: `out` is a writable, aligned T; every caller pairs T with the class
    // that fills exactly a T (FILE_ID_INFO with FileIdInfo, and so on).
    let ok = unsafe {
        GetFileInformationByHandleEx(handle.as_raw_handle(), class, (&raw mut out).cast(), size)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(out)
}

/// Writes one information class from a value of the type that class takes.
fn set_info<T>(
    handle: BorrowedHandle<'_>,
    class: FILE_INFO_BY_HANDLE_CLASS,
    value: &T,
) -> io::Result<()> {
    let size = u32::try_from(size_of::<T>()).map_err(io::Error::other)?;
    // SAFETY: `value` is a readable T; every caller pairs T with the class that
    // takes exactly a T (FILE_BASIC_INFO with FileBasicInfo, and so on).
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle(),
            class,
            std::ptr::from_ref(value).cast(),
            size,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn is_unsupported(e: &io::Error) -> bool {
    let code = e.raw_os_error();
    code == Some(ERROR_INVALID_PARAMETER as i32) || code == Some(ERROR_NOT_SUPPORTED as i32)
}

/// A sharing violation means another process holds the entry without sharing
/// delete access (spec 10.12 `in_use`).
fn in_use_or(e: io::Error) -> Error {
    if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) {
        Error::InUse
    } else {
        Error::Io(e)
    }
}

fn from_filetime(t: i64) -> Timestamp {
    let since_epoch = t - EPOCH_DIFFERENCE;
    Timestamp {
        secs: since_epoch.div_euclid(10_000_000),
        nanos: u32::try_from(since_epoch.rem_euclid(10_000_000) * 100).unwrap_or(0),
    }
}

fn to_filetime(t: Timestamp) -> i64 {
    t.secs * 10_000_000 + i64::from(t.nanos / 100) + EPOCH_DIFFERENCE
}

/// Builders for reparse points the tests need, without running `mklink`
/// (spec 3.5 forbids OS utilities).
#[cfg(test)]
pub(crate) mod test_support {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

    /// A tag outside Microsoft's range, used for a reparse point that is no link.
    pub(crate) const TEST_TAG: u32 = 0x0000_7e57;

    /// Turns the new directory `link` into a junction to the directory `target`.
    pub(crate) fn junction(link: &Path, target: &Path) -> io::Result<()> {
        std::fs::create_dir(link)?;
        let target = std::fs::canonicalize(target)?;
        let plain: Vec<u16> = target.as_os_str().encode_wide().collect();
        // canonicalize gives a verbatim `\\?\` path; junctions store `\??\` plus the
        // plain path as the substitute name and the plain path as the print name.
        let plain = plain
            .strip_prefix(&[92, 92, 63, 92][..])
            .unwrap_or(&plain)
            .to_vec();
        let mut substitute: Vec<u16> = r"\??\".encode_utf16().collect();
        substitute.extend_from_slice(&plain);
        let sub_bytes = substitute.len() * 2;
        let print_bytes = plain.len() * 2;
        let mut data = Vec::new();
        for v in [0, sub_bytes, sub_bytes + 2, print_bytes] {
            data.extend_from_slice(&(v as u16).to_le_bytes());
        }
        for c in substitute
            .iter()
            .chain([&0])
            .chain(plain.iter())
            .chain([&0])
        {
            data.extend_from_slice(&c.to_le_bytes());
        }
        set_reparse(link, IO_REPARSE_TAG_MOUNT_POINT, &[], &data, true)
    }

    /// Mounts the volume holding `on` at the new directory `at`; the mount goes
    /// away when the guard drops, so a failing test leaves no volume mounted.
    pub(crate) fn mount_volume(at: &Path, on: &Path) -> io::Result<VolumeMount> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, SetVolumeMountPointW,
        };
        std::fs::create_dir(at)?;
        let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
        let mut root = vec![0u16; 1024];
        // SAFETY: `on` is NUL-terminated and `root` holds 1024 writable characters.
        if unsafe { GetVolumePathNameW(wide(on).as_ptr(), root.as_mut_ptr(), 1024) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut volume = vec![0u16; 64];
        // SAFETY: `root` is NUL-terminated and `volume` holds 64 writable characters.
        if unsafe { GetVolumeNameForVolumeMountPointW(root.as_ptr(), volume.as_mut_ptr(), 64) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // The mount point is a directory path with a trailing backslash.
        let mut point: Vec<u16> = at.as_os_str().encode_wide().collect();
        point.extend_from_slice(&[92, 0]);
        // SAFETY: both strings are NUL-terminated and outlive the call.
        if unsafe { SetVolumeMountPointW(point.as_ptr(), volume.as_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(VolumeMount(point))
    }

    pub(crate) struct VolumeMount(Vec<u16>);

    impl Drop for VolumeMount {
        fn drop(&mut self) {
            // SAFETY: the stored mount point is NUL-terminated.
            unsafe {
                windows_sys::Win32::Storage::FileSystem::DeleteVolumeMountPointW(self.0.as_ptr())
            };
        }
    }

    /// Puts a reparse point with [`TEST_TAG`] on the existing file `path`.
    pub(crate) fn other_tag(path: &Path) -> io::Result<()> {
        // Tags outside Microsoft's range carry a GUID before their data.
        let guid = [0x5au8; 16];
        set_reparse(path, TEST_TAG, &guid, b"remora test", false)
    }

    fn set_reparse(path: &Path, tag: u32, guid: &[u8], data: &[u8], dir: bool) -> io::Result<()> {
        let mut flags = FILE_FLAG_OPEN_REPARSE_POINT;
        if dir {
            flags |= FILE_FLAG_BACKUP_SEMANTICS;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(flags)
            .open(path)?;
        let mut buf = Vec::new();
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&(data.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(guid);
        buf.extend_from_slice(data);
        let mut returned = 0;
        // SAFETY: `buf` is a readable reparse buffer of `buf.len()` bytes and the
        // handle is open for the call.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_SET_REPARSE_POINT,
                buf.as_ptr().cast(),
                buf.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TEST_TAG, junction, mount_volume, other_tag};
    use super::*;
    use crate::nofollow;
    use crate::sys::test_dir::TestDir;
    use std::io::{Read, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;

    fn name(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    /// Creates a symbolic link, or reports that this account may not (no
    /// `SeCreateSymbolicLinkPrivilege` and no Developer Mode) so the test can skip.
    fn try_symlink(target: &Path, link: &Path, dir: bool) -> bool {
        let made = if dir {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        match made {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD as i32) => {
                eprintln!("skipping: this account cannot create symbolic links");
                false
            }
            Err(e) => panic!("symlink: {e}"),
        }
    }

    /// A root with `file`, `sub/inner` and `junction` to an outside directory.
    fn setup() -> (TestDir, TestDir, Dir) {
        let outside = TestDir::new();
        std::fs::write(outside.path().join("sentinel"), b"outside").unwrap();
        let tmp = TestDir::new();
        std::fs::write(tmp.path().join("file"), b"hello").unwrap();
        tmp.mkdir_all("sub");
        std::fs::write(tmp.path().join("sub").join("inner"), b"in").unwrap();
        junction(&tmp.path().join("junction"), outside.path()).unwrap();
        let root = Dir::open(tmp.path()).unwrap();
        (outside, tmp, root)
    }

    #[test]
    fn junction_not_followed() {
        let (outside, _tmp, root) = setup();
        assert!(matches!(
            open_dir(&root, name("junction")),
            Err(Error::IsSymlink)
        ));
        assert_eq!(
            stat_at(&root, name("junction")).unwrap().kind,
            FileKind::Symlink
        );
        let target = read_link_at(&root, name("junction"), 4096).unwrap();
        let expected = std::fs::canonicalize(outside.path()).unwrap();
        let expected = expected.as_os_str().to_string_lossy();
        assert_eq!(
            String::from_utf8(target).unwrap(),
            expected.trim_start_matches(r"\\?\")
        );
    }

    #[test]
    fn dir_symlink_not_followed() {
        let (outside, tmp, root) = setup();
        if !try_symlink(outside.path(), &tmp.path().join("dirlink"), true) {
            return;
        }
        assert!(matches!(
            open_dir(&root, name("dirlink")),
            Err(Error::IsSymlink)
        ));
        let meta = stat_at(&root, name("dirlink")).unwrap();
        assert_eq!(meta.kind, FileKind::Symlink);
        assert_ne!(
            meta.mode & FILE_ATTRIBUTE_DIRECTORY,
            0,
            "a link to a directory"
        );
    }

    #[test]
    fn file_symlink_reported() {
        let (_outside, tmp, root) = setup();
        if !try_symlink(Path::new("file"), &tmp.path().join("filelink"), false) {
            return;
        }
        let meta = stat_at(&root, name("filelink")).unwrap();
        assert_eq!(meta.kind, FileKind::Symlink);
        assert_eq!(
            read_link_at(&root, name("filelink"), 4096).unwrap(),
            b"file"
        );
        assert!(matches!(
            open_dir(&root, name("filelink")),
            Err(Error::IsSymlink)
        ));
        assert!(matches!(
            open_read(&root, name("filelink")),
            Err(Error::IsSymlink)
        ));
    }

    #[test]
    fn other_reparse_tag_reported() {
        let (_outside, tmp, root) = setup();
        std::fs::write(tmp.path().join("placeholder"), b"").unwrap();
        other_tag(&tmp.path().join("placeholder")).unwrap();
        let meta = stat_at(&root, name("placeholder")).unwrap();
        assert_eq!(meta.kind, FileKind::ReparsePoint(TEST_TAG));
        assert!(read_link_at(&root, name("placeholder"), 4096).is_err());
        unlink_at(&root, name("placeholder"), false).unwrap();
    }

    #[test]
    fn enumeration_returns_file_ids() {
        let (_outside, tmp, root) = setup();
        let listed = entries(&root).unwrap();
        assert_eq!(listed.len(), std::fs::read_dir(tmp.path()).unwrap().count());
        for entry in &listed {
            let entry_name = std::str::from_utf8(&entry.name).unwrap();
            let meta = stat_at(&root, name(entry_name)).unwrap();
            assert_eq!(entry.kind_hint, Some(meta.kind), "{entry_name}");
            // Identity, type and size; the listing's times can lag a later query.
            let listed = entry.metadata.unwrap();
            assert_eq!(
                (listed.kind, listed.size, listed.dev, listed.ino),
                (meta.kind, meta.size, meta.dev, meta.ino),
                "{entry_name}"
            );
            assert_ne!(meta.ino, 0);
        }
        // A second listing of the same handle sees everything again.
        assert_eq!(entries(&root).unwrap().len(), listed.len());
    }

    #[test]
    fn concurrent_write_rename_delete_succeed() {
        let (_outside, tmp, root) = setup();
        let expected = stat_at(&root, name("file")).unwrap();
        let mut held = nofollow::open_file_read(&root, name("file"), &expected).unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join("file"))
            .unwrap();
        writer.write_all(b" world").unwrap();
        drop(writer);
        std::fs::rename(tmp.path().join("file"), tmp.path().join("renamed")).unwrap();
        std::fs::remove_file(tmp.path().join("renamed")).unwrap();
        let mut text = String::new();
        held.read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn identity_change_detected() {
        let (_outside, tmp, root) = setup();
        let expected = stat_at(&root, name("file")).unwrap();
        std::fs::rename(
            tmp.path().join("sub").join("inner"),
            tmp.path().join("file"),
        )
        .unwrap();
        assert!(matches!(
            nofollow::open_file_read(&root, name("file"), &expected),
            Err(Error::Changed)
        ));
    }

    #[test]
    fn reopen_parent_refuses_junction() {
        let (_outside, _tmp, root) = setup();
        match nofollow::reopen_parent(&root, b"junction/sentinel") {
            Err(Error::UnsafeParent(prefix)) => assert_eq!(prefix, b"junction"),
            other => panic!("{other:?}"),
        }
        let (dir, last) = nofollow::reopen_parent(&root, b"sub/inner").unwrap();
        assert_eq!(stat_at(&dir, last).unwrap().kind, FileKind::File);
    }

    fn set_read_only(p: &Path) {
        let mut perms = std::fs::metadata(p).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(p, perms).unwrap();
    }

    #[test]
    fn rename_replaces_a_plain_target() {
        let (_outside, tmp, root) = setup();
        std::fs::write(tmp.path().join("target"), b"old").unwrap();
        rename_at(&root, name("file"), &root, name("target")).unwrap();
        assert_eq!(std::fs::read(tmp.path().join("target")).unwrap(), b"hello");
    }

    #[test]
    fn clear_read_only_clears_the_attribute() {
        let (_outside, tmp, root) = setup();
        set_read_only(&tmp.path().join("file"));
        assert!(clear_read_only(&root, name("file")).unwrap().is_some());
        assert!(
            !std::fs::metadata(tmp.path().join("file"))
                .unwrap()
                .permissions()
                .readonly()
        );
        assert!(clear_read_only(&root, name("file")).unwrap().is_none());
        assert!(clear_read_only(&root, name("missing")).unwrap().is_none());
    }

    #[test]
    fn delete_read_only() {
        let (_outside, tmp, root) = setup();
        set_read_only(&tmp.path().join("file"));
        unlink_at(&root, name("file"), false).unwrap();
        assert!(!tmp.path().join("file").exists());
    }

    #[test]
    fn replace_read_only() {
        let (_outside, tmp, root) = setup();
        std::fs::write(tmp.path().join("target"), b"old").unwrap();
        set_read_only(&tmp.path().join("target"));
        rename_at(&root, name("file"), &root, name("target")).unwrap();
        assert_eq!(std::fs::read(tmp.path().join("target")).unwrap(), b"hello");
    }

    #[test]
    fn rename_fallback_on_invalid_parameter() {
        let (_outside, tmp, root) = setup();
        let sub = open_dir(&root, name("sub")).unwrap();
        rename(&root, name("file"), &sub, name("moved"), true, true).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("sub").join("moved")).unwrap(),
            b"hello"
        );
        // The fallback replaces too, a read-only target included.
        set_read_only(&tmp.path().join("sub").join("inner"));
        rename(&sub, name("moved"), &sub, name("inner"), true, true).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("sub").join("inner")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn failed_fallback_rename_keeps_target_read_only() {
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let (_outside, tmp, root) = setup();
        let target = tmp.path().join("target");
        std::fs::write(&target, b"old").unwrap();
        set_read_only(&target);
        // Open without delete sharing, so the classic rename cannot replace it.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&target)
            .unwrap();
        assert!(rename(&root, name("file"), &root, name("target"), true, true).is_err());
        assert!(std::fs::metadata(&target).unwrap().permissions().readonly());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read(tmp.path().join("file")).unwrap(), b"hello");
    }

    #[test]
    fn rename_without_replace_keeps_both_files() {
        let (_outside, tmp, root) = setup();
        std::fs::write(tmp.path().join("other"), b"other").unwrap();
        for force_fallback in [false, true] {
            let result = rename(
                &root,
                name("file"),
                &root,
                name("other"),
                false,
                force_fallback,
            );
            assert!(
                matches!(&result, Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists),
                "fallback {force_fallback}: {result:?}"
            );
            assert_eq!(std::fs::read(tmp.path().join("file")).unwrap(), b"hello");
            assert_eq!(std::fs::read(tmp.path().join("other")).unwrap(), b"other");
        }
        rename_no_replace(&root, name("file"), &root, name("fresh")).unwrap();
        assert_eq!(std::fs::read(tmp.path().join("fresh")).unwrap(), b"hello");
    }

    #[test]
    fn sharing_violation_is_in_use() {
        let (_outside, tmp, root) = setup();
        let _locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(tmp.path().join("file"))
            .unwrap();
        assert!(matches!(
            unlink_at(&root, name("file"), false),
            Err(Error::InUse)
        ));
        assert!(matches!(
            rename_at(&root, name("file"), &root, name("elsewhere")),
            Err(Error::InUse)
        ));
    }

    #[test]
    fn set_times_on_junction_leaves_target() {
        let (outside, tmp, root) = setup();
        let outside_dir = Dir::open(outside.path().parent().unwrap()).unwrap();
        let outside_name = outside.path().file_name().unwrap();
        let before = stat_at(&outside_dir, outside_name).unwrap().modified;
        let t = Timestamp {
            secs: 1_234_567_890,
            nanos: 500,
        };
        set_times_at(&root, name("junction"), t, t).unwrap();
        assert_eq!(stat_at(&root, name("junction")).unwrap().modified, t);
        assert_eq!(
            stat_at(&outside_dir, outside_name).unwrap().modified,
            before
        );
        drop(tmp);
    }

    #[test]
    fn remove_tree_does_not_follow_junction() {
        let (outside, tmp, root) = setup();
        tmp.mkdir_all("doomed/deeper");
        std::fs::write(tmp.path().join("doomed").join("deeper").join("f"), b"x").unwrap();
        junction(&tmp.path().join("doomed").join("escape"), outside.path()).unwrap();
        nofollow::remove_tree(&root, name("doomed")).unwrap();
        assert!(!tmp.path().join("doomed").exists());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"outside"
        );
    }

    #[test]
    fn parse_link_rejects_malformed_data() {
        let mut symlink = Vec::new();
        symlink.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        symlink.extend_from_slice(&[0; 4]);
        // Substitute name at 0, 2 bytes; print name at 2, 3 bytes (odd).
        for v in [0u16, 2, 2, 3] {
            symlink.extend_from_slice(&v.to_le_bytes());
        }
        symlink.extend_from_slice(&[0; 4]);
        symlink.extend_from_slice(&[b'a', 0, b'b', 0, b'c']);
        assert!(parse_link(&symlink).is_err());
        assert!(parse_link(&symlink[..10]).is_err());
        assert!(parse_link(&[]).is_err());
    }

    #[test]
    fn only_a_mount_point_tag_names_a_volume_mount() {
        let data = |tag: u32| {
            let target: Vec<u8> = r"\??\Volume{0}\"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            let len = u16::try_from(target.len()).unwrap();
            let mut buf = Vec::new();
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&[0; 4]);
            // Substitute name first, empty print name after it.
            for v in [0, len, len, 0] {
                buf.extend_from_slice(&v.to_le_bytes());
            }
            if tag == IO_REPARSE_TAG_SYMLINK {
                buf.extend_from_slice(&[0; 4]);
            }
            buf.extend_from_slice(&target);
            buf
        };
        assert!(is_volume_mount(&data(IO_REPARSE_TAG_MOUNT_POINT)));
        let symlink = data(IO_REPARSE_TAG_SYMLINK);
        assert!(!is_volume_mount(&symlink));
        assert!(parse_link(&symlink).is_ok());
    }

    #[test]
    fn unpaired_surrogate_names_are_listed_and_removed() {
        let (_outside, tmp, root) = setup();
        let odd = OsString::from_wide(&[u16::from(b'x'), 0xD800]);
        tmp.mkdir_all("doomed");
        let doomed = tmp.path().join("doomed");
        std::fs::create_dir(doomed.join(&odd)).unwrap();
        std::fs::write(doomed.join(&odd).join(&odd), b"x").unwrap();
        let dir = open_dir(&root, name("doomed")).unwrap();
        let listed = entries(&dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].os_name(), odd.as_os_str());
        // The key form refuses it, the entry's own name opens it.
        assert!(crate::sys::name_from_bytes(&listed[0].name).is_err());
        assert_eq!(
            stat_at(&dir, listed[0].os_name()).unwrap().kind,
            FileKind::Dir
        );
        drop(dir);
        nofollow::remove_tree(&root, name("doomed")).unwrap();
        assert!(!doomed.exists());
    }

    #[test]
    fn unlink_at_checks_is_dir() {
        let (outside, tmp, root) = setup();
        assert!(matches!(
            unlink_at(&root, name("file"), true),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotADirectory
        ));
        assert!(matches!(
            unlink_at(&root, name("sub"), false),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::IsADirectory
        ));
        // A junction is removed as a link, and its target stays.
        assert!(unlink_at(&root, name("junction"), true).is_err());
        unlink_at(&root, name("junction"), false).unwrap();
        assert!(outside.path().join("sentinel").exists());
        unlink_at(&root, name("file"), false).unwrap();
        assert!(tmp.path().join("sub").exists());
    }

    #[test]
    fn volume_mount_point_is_not_a_link() {
        let (_outside, tmp, root) = setup();
        let at = tmp.path().join("volume");
        let mount = mount_volume(&at, tmp.path()).unwrap();
        let tag = FileKind::ReparsePoint(IO_REPARSE_TAG_MOUNT_POINT);
        assert_eq!(stat_at(&root, name("volume")).unwrap().kind, tag);
        let listed = entries(&root).unwrap();
        let entry = listed.iter().find(|e| e.os_name() == "volume").unwrap();
        assert_eq!(entry.kind_hint, Some(tag));
        assert!(matches!(
            open_dir(&root, name("volume")),
            Err(Error::NotADirectory)
        ));
        assert!(read_link_at(&root, name("volume"), 4096).is_err());
        // The junction next to it is still a link.
        assert_eq!(
            stat_at(&root, name("junction")).unwrap().kind,
            FileKind::Symlink
        );
        drop(mount);
    }

    #[test]
    fn filetime_round_trip() {
        for t in [
            Timestamp { secs: 0, nanos: 0 },
            Timestamp {
                secs: 1_700_000_000,
                nanos: 123_456_700,
            },
            Timestamp {
                secs: -1,
                nanos: 999_999_900,
            },
        ] {
            assert_eq!(from_filetime(to_filetime(t)), t);
        }
    }
}
