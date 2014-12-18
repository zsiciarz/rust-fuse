//!
//! A session runs a filesystem implementation while it is being mounted
//! to a specific mount point. A session begins by mounting the filesystem
//! and ends by unmounting it. While the filesystem is mounted, the session
//! loop receives, dispatches and replies to kernel requests for filesystem
//! operations under its mount point.
//!

use std::task::TaskBuilder;
use libc::{EAGAIN, EINTR, ENODEV, ENOENT};
use channel;
use channel::Channel;
use Filesystem;
use request::{request, dispatch};

/// The max size of write requests from the kernel. The absolute minimum is 4k,
/// FUSE recommends at least 128k, max 16M. The FUSE default is 16M on OS X
/// and 128k on other systems.
pub const MAX_WRITE_SIZE: uint = 16*1024*1024;

/// Size of the buffer for reading a request from the kernel. Since the kernel may send
/// up to MAX_WRITE_SIZE bytes in a write request, we use that value plus some extra space.
const BUFFER_SIZE: uint = MAX_WRITE_SIZE + 4096;

/// The session data structure
pub struct Session<FS> {
    /// Filesystem operation implementations
    pub filesystem: FS,
    /// Path of the mounted filesystem
    pub mountpoint: Path,
    /// Communication channel to the kernel driver
    ch: Channel,
    /// FUSE protocol major version
    pub proto_major: uint,
    /// FUSE protocol minor version
    pub proto_minor: uint,
    /// True if the filesystem is initialized (init operation done)
    pub initialized: bool,
    /// True if the filesystem was destroyed (destroy operation done)
    pub destroyed: bool,
}

impl<FS: Filesystem+Send> Session<FS> {
    /// Create a new session by mounting the given filesystem to the given mountpoint
    pub fn new (filesystem: FS, mountpoint: &Path, options: &[&[u8]]) -> Session<FS> {
        info!("Mounting {}", mountpoint.display());
        let ch = match Channel::new(mountpoint, options) {
            Ok(ch) => ch,
            Err(err) => panic!("Unable to mount filesystem. Error {}", err),
        };
        Session {
            filesystem: filesystem,
            mountpoint: mountpoint.clone(),
            ch: ch,
            proto_major: 0,
            proto_minor: 0,
            initialized: false,
            destroyed: false,
        }
    }

    /// Run the session loop that receives kernel requests and dispatches them to method
    /// calls into the filesystem. This read-dispatch-loop is non-concurrent to prevent
    /// having multiple buffers (which take up much memory), but the filesystem methods
    /// may run concurrent by spawning tasks.
    /// Make sure to run this on a new single threaded scheduler since native I/O in the
    /// session loop can block.
    pub fn run (&mut self) {
        // Buffer for receiving requests from the kernel. Only one is allocated and
        // it is reused immediately after dispatching to conserve memory and allocations.
        let mut buffer = Vec::from_elem(BUFFER_SIZE, 0u8);
        loop {
            // Read the next request from the given channel to kernel driver
            // The kernel driver makes sure that we get exactly one request per read
            match self.ch.receive(buffer.as_mut_slice()) {
                Err(ENOENT) => continue,                // Operation interrupted. Accordingly to FUSE, this is safe to retry
                Err(EINTR) => continue,                 // Interrupted system call, retry
                Err(EAGAIN) => continue,                // Explicitly try again
                Err(ENODEV) => break,                   // Filesystem was unmounted, quit the loop
                Err(err) => panic!("Lost connection to FUSE device. Error {}", err),
                Ok(len) => match request(self.ch.sender(), buffer.slice_to(len)) {
                    None => break,                      // Illegal request, quit the loop
                    Some(req) => dispatch(&req, self),
                },
            }
        }
    }

    /// Run the session loop in a background task
    pub fn spawn (self) -> BackgroundSession {
        BackgroundSession::new(self)
    }
}

#[unsafe_destructor]
impl<FS: Filesystem+Send> Drop for Session<FS> {
    fn drop (&mut self) {
        info!("Unmounted {}", self.mountpoint.display());
        // The actual unmounting takes place because self.ch is dropped here
    }
}

/// The background session data structure
pub struct BackgroundSession {
    /// Path of the mounted filesystem
    pub mountpoint: Path,
}

impl BackgroundSession {
    /// Create a new background session for the given session by running its
    /// session loop in a background task. If the returned handle is dropped,
    /// the filesystem is unmounted and the given session ends.
    pub fn new<FS: Filesystem+Send> (se: Session<FS>) -> BackgroundSession {
        let mountpoint = se.mountpoint.clone();
        // The background task is started using a a new native thread
        // since native I/O in the session loop can block
        let task = TaskBuilder::new().named(format!("FUSE {}", mountpoint.display()));
        task.spawn(move || {
            let mut se = se;
            se.run();
        });
        BackgroundSession { mountpoint: mountpoint }
    }
}

impl Drop for BackgroundSession {
    fn drop (&mut self) {
        info!("Unmounting {}", self.mountpoint.display());
        // Unmounting the filesystem will eventually end the session loop,
        // drop the session and hence end the background task.
        channel::unmount(&self.mountpoint);
    }
}
