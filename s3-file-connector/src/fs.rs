use async_trait::async_trait;
use std::collections::{HashMap};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, UNIX_EPOCH, SystemTime};
use tracing::{error, trace};

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};
use s3_client::{S3Client, StreamingGetObject};

// FIXME Use newtype here? Will add a bunch of .into()s...
type Inode = u64;

const ROOT_INODE: Inode = 1u64;
const DIR_PERMISSIONS: u16 = 0o755;
const FILE_PERMISSIONS: u16 = 0o644;
const UID: u32 = 501;
const GID: u32 = 20;

const FILE_INODE: u64 = 2;

const TTL_ZERO: Duration = Duration::from_secs(0);

const ROOT_DIR_ATTR: FileAttr = FileAttr {
    ino: ROOT_INODE,
    size: 0,
    blocks: 0,
    atime: UNIX_EPOCH, // 1970-01-01 00:00:00
    mtime: UNIX_EPOCH,
    ctime: UNIX_EPOCH,
    crtime: UNIX_EPOCH,
    kind: FileType::Directory,
    perm: DIR_PERMISSIONS,
    nlink: 2,
    uid: UID,
    gid: GID,
    rdev: 0,
    flags: 0,
    blksize: 512,
};

#[derive(Clone, Debug)]
struct InodeInfo {
    name: String,
    parent: Inode,
    mtime: SystemTime,
    kind: FileType,
    size: u64,
}

impl InodeInfo {
    fn new(name: String, parent: Inode, mtime: SystemTime, kind: FileType, size: u64) -> Self {
        Self {
            name,
            parent,
            mtime,
            kind,
            size,
        }
    }
}

const BLOCK_SIZE: u64 = 4096;

#[derive(Clone, Debug)]
struct DirHandle {
    children: Vec<Inode>,
}

pub struct S3Filesystem {
    client: Arc<S3Client>,
    bucket: String,
    key: String,
    size: usize,
    inflight_reads: RwLock<HashMap<u64, Mutex<StreamingGetObject>>>,
    next_handle: AtomicU64,
    next_inode: AtomicU64,
    inode_info: RwLock<HashMap<Inode, InodeInfo>>,
    dir_handles: RwLock<HashMap<u64, DirHandle>>,
    dir_entries: RwLock<HashMap<Inode, Arc<RwLock<HashMap<String, Inode>>>>>,
}

impl S3Filesystem {
    pub fn new(client: S3Client, bucket: &str, key: &str, size: usize) -> Self {
        let mut inode_info = HashMap::new();
        inode_info.insert(ROOT_INODE, InodeInfo::new(
            "".into(),
            ROOT_INODE,
            UNIX_EPOCH,
            FileType::Directory,
            1u64, // FIXME
        ));

        let mut root_entries = HashMap::new();
        root_entries.insert(".".into(), ROOT_INODE);
        root_entries.insert("..".into(), ROOT_INODE);

        let mut dir_entries = HashMap::new();
        dir_entries.insert(ROOT_INODE, Arc::new(RwLock::new(root_entries)));

        Self {
            client: Arc::new(client),
            bucket: bucket.to_string(),
            key: key.to_string(),
            size,
            inflight_reads: Default::default(),
            next_handle: AtomicU64::new(1),
            next_inode: AtomicU64::new(ROOT_INODE + 1), // next Inode to allocate
            inode_info: RwLock::new(inode_info),
            dir_handles: RwLock::new(HashMap::new()),
            dir_entries: RwLock::new(dir_entries),
        }
    }

    fn path_from_root(&self, mut ino: Inode) -> Option<String> {
        if ino == ROOT_INODE {
            Some("".into())
        } else {
            let inode_info = self.inode_info.read().unwrap();
            let mut path = vec!["".into()]; // because we want the path to end in a /
            while ino != ROOT_INODE {
                // FIXME Check that only the first one can return None?
                let info = inode_info.get(&ino)?;
                path.push(info.name.clone());
                ino = info.parent;
            }
            drop(inode_info);
            path.reverse();
            Some(path.join("/"))
        }
    }

    fn next_inode(&self) -> u64 {
        // FIXME
        self.next_inode.fetch_add(1, Ordering::SeqCst)
    }

    fn next_handle(&self) -> u64 {
        // FIXME
        self.next_handle.fetch_add(1, Ordering::SeqCst)
    }
}

fn make_attr(ino: Inode, inode_info: &InodeInfo) -> FileAttr {
    let (perm, nlink, blksize) = match inode_info.kind {
        FileType::RegularFile => (FILE_PERMISSIONS, 1, BLOCK_SIZE as u32),
        FileType::Directory   => (DIR_PERMISSIONS, 2, 512),
        _ => unreachable!(),
    };
    FileAttr {
        ino,
        size: inode_info.size,
        blocks: inode_info.size / BLOCK_SIZE,
        atime: UNIX_EPOCH,
        mtime: inode_info.mtime,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: inode_info.kind,
        perm,
        nlink,
        uid: UID,
        gid: GID,
        rdev: 0,
        flags: 0,
        blksize,
    }
}

#[async_trait]
impl Filesystem for S3Filesystem {
    async fn init(&self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        let _ = config.set_max_readahead(0);
        Ok(())
    }

    async fn lookup(&self, _req: &Request<'_>, parent: Inode, name: &OsStr, reply: ReplyEntry) {
        trace!("fs:lookup with parent {:?} name {:?}", parent, name);

        let dir_entries = {
            let dir_entries = self.dir_entries.read().unwrap();
            if let Some(entries) = dir_entries.get(&parent) {
                Arc::clone(entries)
            } else {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let ino = {
            let dir_entries = dir_entries.read().unwrap();
            match dir_entries.get(name.to_str().unwrap()) {
                Some(ino) => *ino,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let inode_info = self.inode_info.read().unwrap();
        let info = inode_info.get(&ino).unwrap();
        reply.entry(&TTL_ZERO, &make_attr(ino, info), 0);
    }

    async fn getattr(&self, _req: &Request<'_>, ino: Inode, reply: ReplyAttr) {
        trace!("fs:getattr with ino {:?}", ino);

        if ino == ROOT_INODE {
            reply.attr(&TTL_ZERO, &ROOT_DIR_ATTR);
            return;
        }

        match self.inode_info.read().unwrap().get(&ino) {
            Some(inode_info) => reply.attr(&TTL_ZERO, &make_attr(ino, &inode_info)),
            None => reply.error(libc::ENOENT)
        }
    }

    async fn open(&self, _req: &Request<'_>, _ino: Inode, _flags: i32, reply: ReplyOpen) {
        trace!("fs:open with ino {:?} flags {:?}", _ino, _flags);

        let fh = self.next_handle.fetch_add(1, Ordering::SeqCst);
        reply.opened(fh, 0);
    }

    async fn read(
        &self,
        _req: &Request<'_>,
        ino: Inode,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        trace!("fs:read with ino {:?} fh {:?} offset {:?} size {:?}", ino, fh, offset, size);

        if ino == FILE_INODE {
            let mut inflight_reads = self.inflight_reads.read().unwrap();
            if !inflight_reads.contains_key(&fh) {
                drop(inflight_reads);
                let mut inflight_reads_mut = self.inflight_reads.write().unwrap();
                println!("{} {} {}", &self.bucket, &self.key, self.size as u64);
                let request =
                    StreamingGetObject::new(Arc::clone(&self.client), &self.bucket, &self.key, self.size as u64);
                inflight_reads_mut.insert(fh, Mutex::new(request));
                drop(inflight_reads_mut);
                inflight_reads = self.inflight_reads.read().unwrap();
            }
            let mut inflight_read = inflight_reads.get(&fh).unwrap().lock().unwrap();
            let body = inflight_read.read(offset as u64, size as usize);
            reply.data(&body);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    async fn opendir(&self, _req: &Request<'_>, parent: Inode, _flags: i32, reply: ReplyOpen) {
        trace!("fs:opendir with parent {:?} flags {:?}", parent, _flags);

        let prefix = match self.path_from_root(parent) {
            Some(path) => path,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let children = match self.client.list_objects_v2(&self.bucket, &prefix, "/", None).await {
            Ok(result) => result.objects,
            Err(err) => {
                error!(?err, "ListObjectsV2 failed");
                reply.error(libc::EIO);
                return;
            }
        };

        // FIXME
        //   For now we're going to issue a LIST on every opendir to keep it simple and not
        //   try and cache directory entries. This means children will get allocated fresh
        //   inode numbers on each opendir.
        let mut new_map = HashMap::new();
        let mut inode_info = self.inode_info.write().unwrap();
        let mut new_inodes = Vec::new();

        for child in children {
            let (name, kind) = if !child.key.is_empty() { // an object
                (child.key.into_string().unwrap(), FileType::RegularFile)
            } else {
                // unwrap is okay because S3 keys are UTF-8
                let mut str = child.prefix.into_string().unwrap();
                assert_eq!(str.pop(), Some('/'));
                (str, FileType::Directory)
            };

            debug_assert!(name.starts_with(&prefix));

            let name = name[prefix.len()..].to_string();

            // FIXME Fix S3Client's list_objects_v2 to also return object mtime
            // FIXME and return that here
            let mtime = UNIX_EPOCH;
            let info = InodeInfo::new(name.clone(), parent, mtime, kind, child.size);
            let ino = self.next_inode();
            inode_info.insert(ino, info);
            new_inodes.push(ino);

            new_map.insert(name, ino);
        }
        drop(inode_info);

        let mut dir_entries = self.dir_entries.write().unwrap();
        let _old_map = dir_entries.insert(parent, Arc::new(RwLock::new(new_map)));
        drop(dir_entries);

        // FIXME We could garbage collect old inodes from the inode table as below
        //  but that would break any concurrent filesystem calls that were accessing the previous inode
        /*
        if let Some(old_map) = old_map {
            let mut inode_info = self.inode_info.write().unwrap();
            for (_, ino) in old_map.write().unwrap().drain() {
                if ino != ROOT_INODE { // Because / has entries for . and ..
                    assert!(inode_info.remove(&ino).is_some());
                }
            }
        }
        */

        // Allocate a handle
        let fh = self.next_handle();
        let handle = DirHandle {
            children: new_inodes,
        };

        let mut dir_handles = self.dir_handles.write().unwrap();
        dir_handles.insert(fh, handle);
        reply.opened(fh, 0);
    }

    async fn readdir(&self, _req: &Request<'_>, ino: Inode, fh: u64, offset: i64, mut reply: ReplyDirectory) {
        trace!("fs:readdir with ino {:?} fh {:?} offset {:?}", ino, fh, offset);

        let dir_handles = self.dir_handles.read().unwrap();
        let handle = match dir_handles.get(&fh) {
            Some(handle) => handle.clone(),
            None => {
                reply.error(libc::EBADF);
                return;
            }
        };

        if (offset as usize) >= handle.children.len() {
            reply.ok();
            return;
        }

        let inode_info = self.inode_info.read().unwrap();
        for (i, ino) in handle.children.iter().enumerate().skip(offset as usize) {
            // i + 1 means the index of the next entry
            let inode_info = inode_info.get(ino).unwrap();
            let _ = reply.add(*ino, (i+3) as i64, inode_info.kind, inode_info.name.clone());
        }

        reply.ok();
    }
}
