use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use libc;
use log::debug;
use log::error;
use log::warn;

use crate::client::NodeClient;
use crate::generated::{ErrorCode, FileKind, Timestamp, UserContext};
use crate::storage::metadata_storage::MAX_NAME_LENGTH;
use crate::utils::check_access;
use bytes::Bytes;
use fuse::{
    Filesystem, ReplyAttr, ReplyBmap, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyLock, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request,
};
use libc::ENOSYS;
use std::collections::HashMap;
use std::os::raw::c_int;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const READ_AHEAD_CACHE_TTL_MS: u64 = 1;
// Fuse splits reads larger than 128kb into multiple smaller reads
const FUSE_MAX_READ_SIZE: u32 = 128 * 1024;
// TODO: should dynamically size this, based on prediction of what client process will read
// TODO: should also track wasted read aheads
const SPECULATIVE_READ_SIZE: u32 = 8 * FUSE_MAX_READ_SIZE;

struct FileHandleAttributes {
    read: bool,
    write: bool,
}

struct CachedRead {
    data: Bytes,
    file_offset: u64,
    process_id: u32,
    read_at: Instant,
}

pub struct FleetFUSE {
    client: NodeClient,
    next_file_handle: AtomicU64,
    file_handles: Mutex<HashMap<u64, FileHandleAttributes>>,
    read_ahead_cache: Mutex<HashMap<u64, CachedRead>>,
}

impl FleetFUSE {
    pub fn new(server_ip_port: SocketAddr) -> FleetFUSE {
        FleetFUSE {
            client: NodeClient::new(server_ip_port),
            next_file_handle: AtomicU64::new(1),
            file_handles: Mutex::new(HashMap::new()),
            read_ahead_cache: Mutex::new(HashMap::new()),
        }
    }

    fn allocate_file_handle(&self, read: bool, write: bool) -> u64 {
        let handle = self.next_file_handle.fetch_add(1, Ordering::SeqCst);
        let mut handles = self
            .file_handles
            .lock()
            .expect("file_handles lock is poisoned");
        handles.insert(handle, FileHandleAttributes { read, write });

        handle
    }

    fn deallocate_file_handle(&self, handle: u64) {
        let mut handles = self
            .file_handles
            .lock()
            .expect("file_handles lock is poisoned");
        handles.remove(&handle);
        let mut read_cache = self
            .read_ahead_cache
            .lock()
            .expect("read_ahead_cache lock is poisoned");
        read_cache.remove(&handle);
    }

    fn check_read(&self, handle: u64) -> bool {
        let handles = self
            .file_handles
            .lock()
            .expect("file_handles lock is poisoned");
        if let Some(value) = handles.get(&handle).map(|x| x.read) {
            return value;
        } else {
            error!("Undefined file handle: {}", handle);
            return false;
        }
    }

    fn check_write(&self, handle: u64) -> bool {
        let handles = self
            .file_handles
            .lock()
            .expect("file_handles lock is poisoned");
        if let Some(value) = handles.get(&handle).map(|x| x.write) {
            return value;
        } else {
            error!("Undefined file handle: {}", handle);
            return false;
        }
    }
}

fn into_fuse_error(error: ErrorCode) -> c_int {
    match error {
        ErrorCode::DoesNotExist => libc::ENOENT,
        ErrorCode::InodeDoesNotExist => libc::EBADFD,
        ErrorCode::Uncategorized => libc::EIO,
        ErrorCode::Corrupted => libc::EIO,
        ErrorCode::RaftFailure => libc::EIO,
        ErrorCode::BadResponse => libc::EIO,
        ErrorCode::BadRequest => libc::EIO,
        ErrorCode::FileTooLarge => libc::EFBIG,
        ErrorCode::AccessDenied => libc::EACCES,
        ErrorCode::OperationNotPermitted => libc::EPERM,
        ErrorCode::NameTooLong => libc::ENAMETOOLONG,
        ErrorCode::NotEmpty => libc::ENOTEMPTY,
        ErrorCode::MissingXattrKey => libc::ENODATA,
        ErrorCode::AlreadyExists => libc::EEXIST,
        ErrorCode::DefaultValueNotAnError => unreachable!(),
    }
}

fn as_file_kind(mode: u32) -> FileKind {
    if mode & libc::S_IFREG != 0 {
        return FileKind::File;
    } else if mode & libc::S_IFLNK != 0 {
        return FileKind::Symlink;
    } else if mode & libc::S_IFDIR != 0 {
        return FileKind::Directory;
    } else {
        unimplemented!();
    }
}

impl Filesystem for FleetFUSE {
    fn init(&mut self, _req: &Request) -> Result<(), c_int> {
        Ok(())
    }

    fn destroy(&mut self, _req: &Request) {}

    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        // TODO: avoid this double lookup
        match self
            .client
            .lookup(parent, name, UserContext::new(req.uid(), req.gid()))
        {
            Ok(inode) => match self.client.getattr(inode) {
                Ok(attr) => reply.entry(&Duration::new(0, 0), &attr, 0),
                Err(error_code) => reply.error(into_fuse_error(error_code)),
            },
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn forget(&mut self, _req: &Request, _ino: u64, _nlookup: u64) {}

    fn getattr(&mut self, _req: &Request, inode: u64, reply: ReplyAttr) {
        debug!("getattr() called with {:?}", inode);
        match self.client.getattr(inode) {
            Ok(attr) => reply.attr(&Duration::new(0, 0), &attr),
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn setattr(
        &mut self,
        req: &Request,
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<SystemTime>,
        atime_now: bool,
        mtime: Option<SystemTime>,
        mtime_now: bool,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if let Some(mode) = mode {
            debug!("chmod() called with {:?}, {:o}", inode, mode);
            if let Err(error_code) =
                self.client
                    .chmod(inode, mode, UserContext::new(req.uid(), req.gid()))
            {
                reply.error(into_fuse_error(error_code));
                return;
            }
        }

        if uid.is_some() || gid.is_some() {
            debug!("chown() called with {:?} {:?} {:?}", inode, uid, gid);
            if let Some(gid) = gid {
                // Non-root users can only change gid to a group they're in
                if req.uid() != 0 && !get_groups(req.pid()).contains(&gid) {
                    reply.error(libc::EPERM);
                    return;
                }
            }
            if let Err(error_code) =
                self.client
                    .chown(inode, uid, gid, UserContext::new(req.uid(), req.gid()))
            {
                reply.error(into_fuse_error(error_code));
                return;
            }
        }

        if let Some(size) = size {
            debug!("truncate() called with {:?}", inode);
            if let Some(handle) = fh {
                // If the file handle is available, check access locally.
                // This is important as it preserves the semantic that a file handle opened
                // with W_OK will never fail to truncate, even if the file has been subsequently
                // chmod'ed
                if self.check_write(handle) {
                    if let Err(error_code) =
                        self.client.truncate(inode, size, UserContext::new(0, 0))
                    {
                        reply.error(into_fuse_error(error_code));
                        return;
                    }
                } else {
                    reply.error(libc::EACCES);
                    return;
                }
            } else if let Err(error_code) =
                self.client
                    .truncate(inode, size, UserContext::new(req.uid(), req.gid()))
            {
                reply.error(into_fuse_error(error_code));
                return;
            }
        }

        if atime.is_some() || mtime.is_some() {
            debug!(
                "utimens() called with {:?}, {:?}, {:?}",
                inode, atime, mtime
            );
            let atimestamp = if atime_now {
                Some(Timestamp::new(0, libc::UTIME_NOW as i32))
            } else {
                atime
                    .map(|x| x.duration_since(UNIX_EPOCH).unwrap())
                    .map(|x| Timestamp::new(x.as_secs() as i64, x.subsec_nanos() as i32))
            };
            let mtimestamp = if mtime_now {
                Some(Timestamp::new(0, libc::UTIME_NOW as i32))
            } else {
                mtime
                    .map(|x| x.duration_since(UNIX_EPOCH).unwrap())
                    .map(|x| Timestamp::new(x.as_secs() as i64, x.subsec_nanos() as i32))
            };
            if let Err(error_code) = self.client.utimens(
                inode,
                atimestamp,
                mtimestamp,
                UserContext::new(req.uid(), req.gid()),
            ) {
                reply.error(into_fuse_error(error_code));
                return;
            }
        }

        match self.client.getattr(inode) {
            Ok(attr) => reply.attr(&Duration::new(0, 0), &attr),
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn readlink(&mut self, req: &Request, inode: u64, reply: ReplyData) {
        debug!("readlink() called on {:?}", inode);
        match self
            .client
            .readlink(inode, UserContext::new(req.uid(), req.gid()))
        {
            Ok(data) => reply.data(&data),
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn mknod(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        if (mode & (libc::S_IFREG | libc::S_IFLNK)) == 0 {
            // TODO
            warn!("mknod() implementation is incomplete. Only supports regular files and symlinks. Got {:o}", mode);
            reply.error(libc::ENOSYS);
        } else {
            match self.client.create(
                parent,
                name,
                req.uid(),
                req.gid(),
                mode as u16,
                as_file_kind(mode),
            ) {
                Ok(attr) => reply.entry(&Duration::new(0, 0), &attr, 0),
                Err(error_code) => reply.error(into_fuse_error(error_code)),
            }
        }
    }

    fn mkdir(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, reply: ReplyEntry) {
        debug!("mkdir() called with {:?} {:?} {:o}", parent, name, mode);
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        match self
            .client
            .mkdir(parent, name, req.uid(), req.gid(), mode as u16)
        {
            Ok(attr) => reply.entry(&Duration::new(0, 0), &attr, 0),
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn unlink(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        debug!("unlink() called with {:?} {:?}", parent, name);
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        if let Err(error_code) =
            self.client
                .unlink(parent, name, UserContext::new(req.uid(), req.gid()))
        {
            reply.error(into_fuse_error(error_code));
        } else {
            reply.ok();
        }
    }

    fn rmdir(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        debug!("rmdir() called with {:?} {:?}", parent, name);
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        if let Err(error_code) =
            self.client
                .rmdir(parent, name, UserContext::new(req.uid(), req.gid()))
        {
            reply.error(into_fuse_error(error_code));
        } else {
            reply.ok();
        }
    }

    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        link: &Path,
        reply: ReplyEntry,
    ) {
        debug!("symlink() called with {:?} {:?} {:?}", parent, name, link);
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        let link = if let Some(value) = link.to_str() {
            value
        } else {
            error!("Link is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };

        match self
            .client
            .create(parent, name, req.uid(), req.gid(), 0o755, FileKind::Symlink)
        {
            Ok(attrs) => {
                if let Err(error_code) =
                    self.client
                        .truncate(attrs.ino, 0, UserContext::new(req.uid(), req.gid()))
                {
                    reply.error(into_fuse_error(error_code));
                    return;
                }
                if let Err(error_code) = self.client.write(
                    attrs.ino,
                    &Vec::from(link.to_string()),
                    0,
                    UserContext::new(req.uid(), req.gid()),
                ) {
                    reply.error(into_fuse_error(error_code));
                    return;
                }

                reply.entry(&Duration::new(0, 0), &attrs, 0);
            }
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn rename(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        reply: ReplyEmpty,
    ) {
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        let new_name = if let Some(value) = new_name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        if let Err(error_code) = self.client.rename(
            parent,
            name,
            new_parent,
            new_name,
            UserContext::new(req.uid(), req.gid()),
        ) {
            reply.error(into_fuse_error(error_code));
        } else {
            reply.ok();
        }
    }

    fn link(
        &mut self,
        req: &Request,
        inode: u64,
        new_parent: u64,
        new_name: &OsStr,
        reply: ReplyEntry,
    ) {
        debug!(
            "link() called for {}, {}, {:?}",
            inode, new_parent, new_name
        );
        let new_name = if let Some(value) = new_name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        match self.client.hardlink(
            inode,
            new_parent,
            new_name,
            UserContext::new(req.uid(), req.gid()),
        ) {
            Ok(attr) => reply.entry(&Duration::new(0, 0), &attr, 0),
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn open(&mut self, req: &Request, inode: u64, flags: u32, reply: ReplyOpen) {
        debug!("open() called for {:?}", inode);
        let (access_mask, read, write) = match flags as i32 & libc::O_ACCMODE {
            libc::O_RDONLY => {
                // Behavior is undefined, but most filesystems return EACCES
                if flags as i32 & libc::O_TRUNC != 0 {
                    reply.error(libc::EACCES);
                    return;
                }
                (libc::R_OK, true, false)
            }
            libc::O_WRONLY => (libc::W_OK, false, true),
            libc::O_RDWR => (libc::R_OK | libc::W_OK, true, true),
            // Exactly one access mode flag must be specified
            _ => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        match self.client.getattr(inode) {
            Ok(attr) => {
                if check_access(
                    attr.uid,
                    attr.gid,
                    attr.perm,
                    req.uid(),
                    req.gid(),
                    access_mask as u32,
                ) {
                    reply.opened(self.allocate_file_handle(read, write), 0);
                    return;
                } else {
                    reply.error(libc::EACCES);
                    return;
                }
            }
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn read(
        &mut self,
        req: &Request,
        inode: u64,
        fh: u64,
        offset: i64,
        size: u32,
        reply: ReplyData,
    ) {
        debug!("read() called on {:?}", inode);
        assert!(offset >= 0);
        if !self.check_read(fh) {
            reply.error(libc::EACCES);
            return;
        }

        {
            let mut read_cache = self
                .read_ahead_cache
                .lock()
                .expect("read_ahead_cache lock is poisoned");
            if let Some(cached) = read_cache.get_mut(&fh) {
                if cached.file_offset == offset as u64
                    && req.pid() == cached.process_id
                    && size <= cached.data.len() as u32
                    && cached.read_at.elapsed() < Duration::from_millis(READ_AHEAD_CACHE_TTL_MS)
                {
                    reply.data(&cached.data[0..size as usize]);
                    cached.data.advance(size as usize);
                    cached.file_offset += u64::from(size);
                    // TODO: should drop cached reads that have been fully consumed
                    return;
                }
            }
        }

        if size >= FUSE_MAX_READ_SIZE {
            match self.client.read_to_vec(
                inode,
                offset as u64,
                SPECULATIVE_READ_SIZE,
                UserContext::new(req.uid(), req.gid()),
            ) {
                Ok(data) => {
                    let mut read_cache = self
                        .read_ahead_cache
                        .lock()
                        .expect("read_ahead_cache lock is poisoned");
                    reply.data(&data[0..size as usize]);
                    if data.len() > size as usize {
                        let mut data_bytes = Bytes::from(data);
                        data_bytes.advance(size as usize);
                        let cached = CachedRead {
                            data: data_bytes,
                            file_offset: (offset + i64::from(size)) as u64,
                            process_id: req.pid(),
                            read_at: Instant::now(),
                        };

                        read_cache.insert(fh, cached);
                    }
                }
                Err(error_code) => reply.error(into_fuse_error(error_code)),
            }
        } else {
            self.client.read(
                inode,
                offset as u64,
                size,
                UserContext::new(req.uid(), req.gid()),
                move |result| match result {
                    Ok(data) => reply.data(data),
                    Err(error_code) => reply.error(into_fuse_error(error_code)),
                },
            );
        }
    }

    fn write(
        &mut self,
        req: &Request,
        inode: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _flags: u32,
        reply: ReplyWrite,
    ) {
        debug!("write() called with {:?}", inode);
        assert!(offset >= 0);
        if !self.check_write(fh) {
            reply.error(libc::EACCES);
            return;
        }
        match self.client.write(
            inode,
            &data,
            offset as u64,
            UserContext::new(req.uid(), req.gid()),
        ) {
            Ok(written) => reply.written(written),
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn flush(&mut self, _req: &Request, inode: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        debug!("flush() called on {:?}", inode);
        reply.error(ENOSYS);
    }

    fn release(
        &mut self,
        _req: &Request,
        inode: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        debug!("release() called on {:?} {}", inode, fh);
        self.deallocate_file_handle(fh);
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request, inode: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        debug!("fsync() called with {:?}", inode);
        if let Err(error_code) = self.client.fsync(inode) {
            reply.error(into_fuse_error(error_code));
        } else {
            reply.ok();
        }
    }

    fn opendir(&mut self, req: &Request, inode: u64, flags: u32, reply: ReplyOpen) {
        debug!("opendir() called on {:?}", inode);
        let (access_mask, read, write) = match flags as i32 & libc::O_ACCMODE {
            libc::O_RDONLY => {
                // Behavior is undefined, but most filesystems return EACCES
                if flags as i32 & libc::O_TRUNC != 0 {
                    reply.error(libc::EACCES);
                    return;
                }
                (libc::R_OK, true, false)
            }
            libc::O_WRONLY => (libc::W_OK, false, true),
            libc::O_RDWR => (libc::R_OK | libc::W_OK, true, true),
            // Exactly one access mode flag must be specified
            _ => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        match self.client.getattr(inode) {
            Ok(attr) => {
                if check_access(
                    attr.uid,
                    attr.gid,
                    attr.perm,
                    req.uid(),
                    req.gid(),
                    access_mask as u32,
                ) {
                    reply.opened(self.allocate_file_handle(read, write), 0);
                    return;
                } else {
                    reply.error(libc::EACCES);
                    return;
                }
            }
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    // TODO: send offset to server and do pagination
    fn readdir(
        &mut self,
        _req: &Request,
        inode: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!("readdir() called with {:?}", inode);
        assert!(offset >= 0);
        match self.client.readdir(inode) {
            Ok(entries) => {
                for (index, entry) in entries.iter().skip(offset as usize).enumerate() {
                    let (inode, name, file_type) = entry;

                    let buffer_full: bool =
                        reply.add(*inode, offset + index as i64 + 1, *file_type, name);

                    if buffer_full {
                        break;
                    }
                }

                reply.ok();
            }
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn releasedir(&mut self, _req: &Request, inode: u64, fh: u64, _flags: u32, reply: ReplyEmpty) {
        debug!("releasedir() called on {:?} {}", inode, fh);
        self.deallocate_file_handle(fh);
        reply.ok();
    }

    fn fsyncdir(
        &mut self,
        _req: &Request,
        inode: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        debug!("fsyncdir() called with {:?}", inode);
        if let Err(error_code) = self.client.fsync(inode) {
            reply.error(into_fuse_error(error_code));
        } else {
            reply.ok();
        }
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        warn!("statfs() implementation is a stub");
        // TODO: real implementation of this
        reply.statfs(10, 10, 10, 1, 10, 4096, MAX_NAME_LENGTH, 4096);
    }

    fn setxattr(
        &mut self,
        _req: &Request,
        inode: u64,
        name: &OsStr,
        value: &[u8],
        _flags: u32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        debug!("setxattr() called with {:?} {:?} {:?}", inode, name, value);
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Key is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        if let Err(error_code) = self.client.setxattr(inode, name, value) {
            reply.error(into_fuse_error(error_code));
        } else {
            reply.ok();
        }
    }

    fn getxattr(&mut self, _req: &Request, inode: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        debug!("getxattr() called with {:?} {:?}", inode, name);
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Key is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        match self.client.getxattr(inode, name) {
            Ok(data) => {
                if size == 0 {
                    reply.size(data.len() as u32);
                } else if data.len() <= size as usize {
                    reply.data(&data);
                } else {
                    reply.error(libc::ERANGE);
                }
            }
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn listxattr(&mut self, _req: &Request, inode: u64, size: u32, reply: ReplyXattr) {
        debug!("listxattr() called with {:?}", inode);
        match self.client.listxattr(inode).map(|xattrs| {
            let mut bytes = vec![];
            // Convert to concatenated null-terminated strings
            for attr in xattrs {
                bytes.extend(attr.as_bytes());
                bytes.push(0);
            }
            bytes
        }) {
            Ok(data) => {
                if size == 0 {
                    reply.size(data.len() as u32);
                } else if data.len() <= size as usize {
                    reply.data(&data);
                } else {
                    reply.error(libc::ERANGE);
                }
            }
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn removexattr(&mut self, _req: &Request, inode: u64, name: &OsStr, reply: ReplyEmpty) {
        debug!("removexattr() called with {:?} {:?}", inode, name);
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Key is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        if let Err(error_code) = self.client.removexattr(inode, name) {
            reply.error(into_fuse_error(error_code));
        } else {
            reply.ok();
        }
    }

    fn access(&mut self, req: &Request, inode: u64, mask: u32, reply: ReplyEmpty) {
        debug!("access() called with {:?} {:?}", inode, mask);
        match self.client.getattr(inode) {
            Ok(attr) => {
                if check_access(attr.uid, attr.gid, attr.perm, req.uid(), req.gid(), mask) {
                    reply.ok();
                } else {
                    reply.error(libc::EACCES);
                }
            }
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        flags: u32,
        reply: ReplyCreate,
    ) {
        debug!("create() called with {:?} {:?}", parent, name);
        let name = if let Some(value) = name.to_str() {
            value
        } else {
            error!("Path component is not UTF-8");
            reply.error(libc::EINVAL);
            return;
        };
        let (read, write) = match flags as i32 & libc::O_ACCMODE {
            libc::O_RDONLY => (true, false),
            libc::O_WRONLY => (false, true),
            libc::O_RDWR => (true, true),
            // Exactly one access mode flag must be specified
            _ => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        match self.client.create(
            parent,
            name,
            req.uid(),
            req.gid(),
            mode as u16,
            as_file_kind(mode),
        ) {
            Ok(attr) => {
                // TODO: implement flags
                reply.created(
                    &Duration::new(0, 0),
                    &attr,
                    0,
                    self.allocate_file_handle(read, write),
                    0,
                )
            }
            Err(error_code) => reply.error(into_fuse_error(error_code)),
        }
    }

    fn getlk(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _typ: u32,
        _pid: u32,
        reply: ReplyLock,
    ) {
        reply.error(ENOSYS);
    }

    fn setlk(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _typ: u32,
        _pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        reply.error(ENOSYS);
    }

    fn bmap(&mut self, _req: &Request, _ino: u64, _blocksize: u32, _idx: u64, reply: ReplyBmap) {
        reply.error(ENOSYS);
    }
}

fn get_groups(pid: u32) -> Vec<u32> {
    let path = format!("/proc/{}/task/{}/status", pid, pid);
    let file = File::open(path).unwrap();
    for line in BufReader::new(file).lines() {
        let line = line.unwrap();
        if line.starts_with("Groups:") {
            return line["Groups: ".len()..]
                .split(' ')
                .filter(|x| !x.trim().is_empty())
                .map(|x| x.parse::<u32>().unwrap())
                .collect();
        }
    }

    vec![]
}
