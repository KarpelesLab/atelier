//! The root image atelier fetches when the sandbox is on and the user named
//! no root filesystem of their own: the official Alpine image, by the
//! digest nixvm pins ([`ALPINE`]), pulled from its registry by nixvm
//! (`image::pull`: HTTPS in Rust, every blob checked against its digest)
//! and kept in a store under the user's data directory.
//!
//! The store holds what nixvm caches there — the registry's blobs and the
//! flattened root, a squashfs file a guest mounts without unpacking — and
//! two things of atelier's: `current`, naming the image that was fetched
//! ([`super::image_root`]), and `layers/<root>`, the packages installed over
//! it. That layer is written by one kind of command only, the user's
//! `atelier sandbox run --rw-root`; every other guest reads it, under a
//! layer of its own in memory. So what an agent's command installs is gone
//! with its guest, as with a root directory, and nothing one project's
//! commands do to the root is there for another's.
//!
//! Fetching is network access nobody typed a command for, so it says what
//! it fetches and from where, on stderr, before it does; and it happens
//! only here, from [`ensure`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nixvm::Arch;
use nixvm::fs::blockdev::SharedDisks;
use nixvm::fs::{
    Attrs, CgroupFs, DirEntry, FsToolMount, MountFs, NodeKind, Overlay, PageStore, Passthrough,
    ProcFs, SetTime,
};
use nixvm::image::{ALPINE, PullOptions, pull_image};
use nixvm::limits::Quota;

/// The image's root (a squashfs file) and the directory of the packages
/// installed over it, fetching the image into `store` first if it is not
/// there — or is another one than this build pins.
pub fn ensure(store: &Path) -> Result<(PathBuf, PathBuf), String> {
    let there = |store: &Path| {
        super::image_root(store)
            .filter(|(reference, _, _)| reference == ALPINE)
            .map(|(_, root, layer)| (root, layer))
    };
    if let Some(found) = there(store) {
        return Ok(found);
    }
    let arch = Arch::host_native()
        .ok_or("there is no root image for this machine's architecture: provide a root filesystem (ATELIER_SANDBOX_ROOT, docs/sandbox.md)")?;
    eprintln!(
        "atelier: sandbox: no root filesystem is configured: fetching the image {ALPINE} from \
         its registry, once, into {}",
        store.display()
    );
    let failed = |why: String| {
        format!(
            "could not fetch the sandbox's root image ({ALPINE}): {why}. It is downloaded once \
             and that needs the network; without it, unpack a root filesystem yourself and \
             name it with ATELIER_SANDBOX_ROOT (docs/sandbox.md)"
        )
    };
    std::fs::create_dir_all(store).map_err(|e| failed(format!("{}: {e}", store.display())))?;
    let pulled = pull_image(ALPINE, arch, store, PullOptions::default())
        .map_err(|e| failed(e.to_string()))?;
    let file = pulled
        .path
        .strip_prefix(store)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| failed(format!("unexpected root {}", pulled.path.display())))?;
    // Written whole, then given its name: a reader sees all of it or none.
    let current = store.join(super::IMAGE_CURRENT);
    let draft = store.join(format!("{}.{}", super::IMAGE_CURRENT, std::process::id()));
    std::fs::write(&draft, format!("{ALPINE}\n{file}\n"))
        .and_then(|()| std::fs::rename(&draft, &current))
        .map_err(|e| failed(format!("{}: {e}", current.display())))?;
    let size = std::fs::metadata(&pulled.path).map_or(0, |m| m.len());
    eprintln!(
        "atelier: sandbox: fetched: {} ({:.1} MiB)",
        pulled.path.display(),
        size as f64 / (1 << 20) as f64
    );
    there(store).ok_or_else(|| failed("the image is not where it was put".to_string()))
}

/// The image's root, to mount read-only from its file.
pub fn open(file: &Path) -> Result<FsToolMount, String> {
    FsToolMount::open_squashfs(file).map_err(|e| format!("{}: {e}", file.display()))
}

/// `image` with the packages in `layer` over it, read-only: what a guest
/// that is not installing anything has under its own writable layer. The
/// layer is read as nixvm's persistent overlay writes it (files at their
/// paths, `.wh.` markers for what was removed), without its lock.
pub fn with_layer(image: Box<FsToolMount>, layer: &Path) -> Box<dyn MountFs> {
    if layer.is_dir() {
        let packages = RootOwned::over(Box::new(Passthrough::read_only(layer)), layer);
        Box::new(Overlay::new(image, Box::new(packages)))
    } else {
        image
    }
}

/// `image` with `layer` over it as the writable layer that stays, for the
/// one command that installs into it (`--rw-root`). `EBUSY` while another
/// does.
pub fn with_layer_kept(image: Box<FsToolMount>, layer: &Path) -> io::Result<Box<dyn MountFs>> {
    let kept = Overlay::persistent(image, layer)?;
    Ok(Box::new(RootOwned::over(Box::new(kept), layer)))
}

/// A filesystem kept in a directory of the user's, as the guest should see
/// it: owned by root.
///
/// The layer's files are made by nixvm running as the user, so on the host
/// they are the user's, and the user cannot give them to root. Left alone,
/// the guest would see its packages owned by a uid it does not know, and
/// `apk add` would exit with an error for every file it could not `chown`
/// — after installing them all. So here the user's files read as root's,
/// and a change of owner the host refuses is taken as done. What is lost
/// is ownership by anyone else: a package's files meant for a service
/// account are root's too. The guest runs everything as root, so nothing
/// in it is refused for that.
#[derive(Debug)]
struct RootOwned {
    inner: Box<dyn MountFs>,
    /// The user the directory belongs to on the host.
    owner: Option<(u32, u32)>,
}

impl RootOwned {
    fn over(inner: Box<dyn MountFs>, dir: &Path) -> Self {
        use std::os::unix::fs::MetadataExt;
        let owner = std::fs::metadata(dir).ok().map(|m| (m.uid(), m.gid()));
        Self { inner, owner }
    }
}

impl MountFs for RootOwned {
    fn stat(&mut self, rel: &str) -> Option<Attrs> {
        let mut attrs = self.inner.stat(rel)?;
        if let Some((uid, gid)) = self.owner {
            if attrs.uid == uid {
                attrs.uid = 0;
            }
            if attrs.gid == gid {
                attrs.gid = 0;
            }
        }
        Some(attrs)
    }
    fn set_owner(&mut self, rel: &str, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
        match self.inner.set_owner(rel, uid, gid) {
            // EPERM: the host's answer to a user giving a file away.
            Err(e) if e.raw_os_error() == Some(1) => Ok(()),
            other => other,
        }
    }
    fn read_at(&mut self, rel: &str, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_at(rel, off, buf)
    }
    fn readdir(&mut self, rel: &str) -> io::Result<Vec<DirEntry>> {
        self.inner.readdir(rel)
    }
    fn attach_pages(&mut self, store: &Arc<dyn PageStore>) {
        self.inner.attach_pages(store);
    }
    fn page_id(&mut self, rel: &str) -> Option<u64> {
        self.inner.page_id(rel)
    }
    fn attach_quota(&mut self, quota: &Arc<Quota>) {
        self.inner.attach_quota(quota);
    }
    fn attach_disk_quota(&mut self, quota: &Arc<Quota>) {
        self.inner.attach_disk_quota(quota);
    }
    fn attach_disks(&mut self, disks: &SharedDisks) {
        self.inner.attach_disks(disks);
    }
    fn own_dev(&self) -> Option<u64> {
        self.inner.own_dev()
    }
    fn data_dev(&mut self, rel: &str) -> Option<u64> {
        self.inner.data_dev(rel)
    }
    fn fs_type(&self) -> &'static str {
        self.inner.fs_type()
    }
    fn read_only(&self) -> bool {
        self.inner.read_only()
    }
    fn check_write(&mut self, rel: &str) -> io::Result<()> {
        self.inner.check_write(rel)
    }
    fn write_at(&mut self, rel: &str, off: u64, buf: &[u8]) -> io::Result<usize> {
        self.inner.write_at(rel, off, buf)
    }
    fn create(&mut self, rel: &str, mode: u32) -> io::Result<()> {
        self.inner.create(rel, mode)
    }
    fn mkdir(&mut self, rel: &str, mode: u32) -> io::Result<()> {
        self.inner.mkdir(rel, mode)
    }
    fn mknod(&mut self, rel: &str, mode: u32) -> io::Result<()> {
        self.inner.mknod(rel, mode)
    }
    fn unlink(&mut self, rel: &str) -> io::Result<()> {
        self.inner.unlink(rel)
    }
    fn rmdir(&mut self, rel: &str) -> io::Result<()> {
        self.inner.rmdir(rel)
    }
    fn truncate(&mut self, rel: &str, len: u64) -> io::Result<()> {
        self.inner.truncate(rel, len)
    }
    fn set_times(&mut self, rel: &str, atime: SetTime, mtime: SetTime) -> io::Result<()> {
        self.inner.set_times(rel, atime, mtime)
    }
    fn set_mode(&mut self, rel: &str, mode: u32) -> io::Result<()> {
        self.inner.set_mode(rel, mode)
    }
    fn symlink(&mut self, target: &str, linkpath: &str) -> io::Result<()> {
        self.inner.symlink(target, linkpath)
    }
    fn readlink(&mut self, rel: &str) -> io::Result<String> {
        self.inner.readlink(rel)
    }
    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn link(&mut self, old_rel: &str, new_rel: &str) -> io::Result<()> {
        self.inner.link(old_rel, new_rel)
    }
    fn getxattr(&mut self, rel: &str, name: &str) -> io::Result<Vec<u8>> {
        self.inner.getxattr(rel, name)
    }
    fn setxattr(&mut self, rel: &str, name: &str, value: &[u8]) -> io::Result<()> {
        self.inner.setxattr(rel, name, value)
    }
    fn listxattr(&mut self, rel: &str) -> io::Result<Vec<String>> {
        self.inner.listxattr(rel)
    }
    fn removexattr(&mut self, rel: &str, name: &str) -> io::Result<()> {
        self.inner.removexattr(rel, name)
    }
    fn as_procfs_mut(&mut self) -> Option<&mut ProcFs> {
        None
    }
    fn as_cgroupfs_mut(&mut self) -> Option<&mut CgroupFs> {
        None
    }
}

/// Whether the fetched image in `store`, with its packages, has something
/// that is not a directory at `path`. `false` if it has not been fetched.
pub fn has(store: &Path, path: &str) -> bool {
    let Some((_, root, layer)) = super::image_root(store) else {
        return false;
    };
    let Ok(image) = open(&root) else {
        return false;
    };
    with_layer(Box::new(image), &layer)
        .stat(path)
        .is_some_and(|node| node.kind != NodeKind::Dir)
}
