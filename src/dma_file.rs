// Unless explicitly stated otherwise all files in this repository are licensed under the
// MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//
use crate::error::Error;
use crate::parking::Reactor;
use crate::sys;
use crate::sys::{DmaBuffer, SourceType};
use crate::Result;
use std::hash::{Hash, Hasher};
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};

macro_rules! enhance {
    ($expr:expr, $op:expr, $path:expr, $fd:expr) => {{
        match $expr {
            Ok(val) => Ok(val),
            Err(inner) => {
                return Err(Error {
                    inner,
                    op: $op,
                    path: $path.and_then(|x| Some(x.to_path_buf())),
                    fd: $fd,
                })
            }
        }
    }};
    ($expr:expr, $op:expr, $obj:expr) => {{
        enhance!(
            $expr,
            $op,
            $obj.path.as_ref().and_then(|x| Some(x.as_path())),
            Some($obj.as_raw_fd())
        )
    }};
}

macro_rules! path_required {
    ($obj:expr, $op:expr) => {{
        $obj.path.as_ref().ok_or(Error {
            inner: io::Error::new(
                io::ErrorKind::InvalidData,
                "operation requires a valid path",
            ),
            op: $op,
            path: None,
            fd: Some($obj.as_raw_fd()),
        })
    }};
}

macro_rules! bad_buffer {
    ($obj:expr) => {{
        Error {
            inner: io::Error::from_raw_os_error(5),
            op: "processing read buffer",
            path: $obj.path.clone(),
            fd: Some($obj.as_raw_fd()),
        }
    }};
}

fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

fn align_down(v: u64, align: u64) -> u64 {
    v & !(align - 1)
}

#[derive(Debug)]
/// A directory representation where asynchronous operations can be issued
pub struct Directory {
    file: std::fs::File,
    path: Option<PathBuf>,
}

impl AsRawFd for Directory {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Directory {
    /// Try creating a clone of this Directory.
    ///
    /// The new object has a different file descriptor and has to be
    /// closed separately.
    pub fn try_clone(&self) -> Result<Directory> {
        let fd = enhance!(
            sys::duplicate_file(self.file.as_raw_fd()),
            "Cloning directory",
            self
        )?;
        Ok(Directory {
            file: unsafe { std::fs::File::from_raw_fd(fd as _) },
            path: self.path.clone(),
        })
    }

    /// Synchronously open this directory.
    pub fn sync_open<P: AsRef<Path>>(path: P) -> Result<Directory> {
        let path = path.as_ref().to_owned();
        let flags = libc::O_CLOEXEC | libc::O_DIRECTORY;
        let fd = enhance!(
            sys::sync_open(&path, flags, 0o755),
            "Synchronously opening directory",
            Some(&path),
            None
        )?;
        Ok(Directory {
            file: unsafe { std::fs::File::from_raw_fd(fd as _) },
            path: Some(path),
        })
    }

    /// Asynchronously open the directory at path
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Directory> {
        let path = path.as_ref().to_owned();
        let flags = libc::O_DIRECTORY | libc::O_CLOEXEC;
        let source = Reactor::get().open_at(-1, &path, flags, 0o755);
        let fd = enhance!(
            source.collect_rw().await,
            "Opening directory",
            Some(&path),
            None
        )?;
        Ok(Directory {
            file: unsafe { std::fs::File::from_raw_fd(fd as _) },
            path: Some(path),
        })
    }

    /// Similar to create() in the standard library, but returns a DMA file
    pub fn sync_create<P: AsRef<Path>>(path: P) -> Result<Directory> {
        let path = path.as_ref().to_owned();
        enhance!(
            match std::fs::create_dir(&path) {
                Ok(_) => Ok(()),
                Err(x) => {
                    match x.kind() {
                        std::io::ErrorKind::AlreadyExists => Ok(()),
                        _ => Err(x),
                    }
                }
            },
            "Synchronously creating directory",
            Some(&path),
            None
        )?;
        Self::sync_open(&path)
    }

    /// Returns an iterator to the contents of this directory
    pub fn sync_read_dir(&self) -> Result<std::fs::ReadDir> {
        let path = path_required!(self, "read directory")?;
        enhance!(std::fs::read_dir(path), "Reading a directory", self)
    }

    /// Issues fdatasync into the underlying file.
    pub async fn sync(&self) -> io::Result<()> {
        let source = Reactor::get().fdatasync(self.as_raw_fd());
        source.collect_rw().await?;
        Ok(())
    }

    /// Closes this DMA file.
    pub async fn close(&mut self) -> io::Result<()> {
        let source = Reactor::get().close(self.as_raw_fd());
        source.collect_rw().await?;
        self.file = unsafe { std::fs::File::from_raw_fd(-1) };
        Ok(())
    }
}

#[derive(Debug)]
/// Constructs a file that can issue DMA operations.
/// All access uses Direct I/O, and all operations including
/// open and close are asynchronous.
pub struct DmaFile {
    file: std::fs::File,
    // A file can appear in many paths, through renaming and linking.
    // If we do that, each path should have its own object. This is to
    // facilitate error displaying.
    path: Option<PathBuf>,
    o_direct_alignment: u64,
}

impl DmaFile {
    // FIXME: Don't assume 512, we can read this info from sysfs
    /// align a value up to the minimum alignment needed to access this file
    pub fn align_up(&self, v: u64) -> u64 {
        align_up(v, self.o_direct_alignment)
    }

    /// align a value down to the minimum alignment needed to access this file
    pub fn align_down(&self, v: u64) -> u64 {
        align_down(v, self.o_direct_alignment)
    }
}

impl AsRawFd for DmaFile {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl PartialEq for DmaFile {
    fn eq(&self, other: &Self) -> bool {
        self.as_raw_fd() == other.as_raw_fd()
    }
}

impl Eq for DmaFile {}

impl Hash for DmaFile {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_raw_fd().hash(state);
    }
}

impl Default for DmaFile {
    fn default() -> Self {
        DmaFile {
            file: unsafe { std::fs::File::from_raw_fd(-1) },
            path: None,
            o_direct_alignment: 4096,
        }
    }
}

impl Drop for DmaFile {
    fn drop(&mut self) {
        if self.as_raw_fd() != -1 {
            eprintln!(
                "DmaFile dropped while still active. Should have been async closed ({:?} / fd {})
I will close it and turn a leak bug into a performance bug. Please investigate",
                self.path,
                self.as_raw_fd()
            );
            drop(&self.file);
        }
    }
}

impl DmaFile {
    async fn open_at(
        dir: RawFd,
        path: &Path,
        flags: libc::c_int,
        mode: libc::c_int,
    ) -> io::Result<DmaFile> {
        let source = Reactor::get().open_at(dir, path, flags, mode);
        let fd = source.collect_rw().await?;
        Ok(DmaFile {
            file: unsafe { std::fs::File::from_raw_fd(fd as _) },
            path: Some(path.to_path_buf()),
            o_direct_alignment: 4096,
        })
    }

    /// Allocates a buffer that is suitable for using to write to this file.
    pub fn alloc_dma_buffer(size: usize) -> DmaBuffer {
        Reactor::get().alloc_dma_buffer(size)
    }

    /// Similar to create() in the standard library, but returns a DMA file
    pub async fn create<P: AsRef<Path>>(path: P) -> Result<DmaFile> {
        // FIXME: because we use the poll ring, we really only support xfs and ext4 for this.
        // We should check and maybe do something different in that case.
        let path = path.as_ref().to_owned();

        let flags =
            libc::O_DIRECT | libc::O_CLOEXEC | libc::O_CREAT | libc::O_TRUNC | libc::O_WRONLY;
        let mut f = enhance!(
            DmaFile::open_at(-1 as _, &path, flags, 0o644).await,
            "Creating",
            Some(&path),
            None
        )?;
        f.o_direct_alignment = 4096;
        Ok(f)
    }

    /// Similar to open() in the standard library, but returns a DMA file
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<DmaFile> {
        let path = path.as_ref().to_owned();

        let flags = libc::O_DIRECT | libc::O_CLOEXEC | libc::O_RDONLY;
        let mut f = enhance!(
            DmaFile::open_at(-1 as _, &path, flags, 0o644).await,
            "Opening",
            Some(&path),
            None
        )?;
        f.o_direct_alignment = 512;
        Ok(f)
    }

    /// Writes the buffer in buf to a specific position in the file.
    ///
    /// It is expected that the buffer is properly aligned for Direct I/O.
    /// In most platforms that means 512 bytes.
    pub async fn write_dma(&self, buf: &DmaBuffer, pos: u64) -> Result<usize> {
        let source = Reactor::get().write_dma(self.as_raw_fd(), buf, pos);
        enhance!(source.collect_rw().await, "Writing", self)
    }

    /// Reads into buffer in buf from a specific position in the file.
    ///
    /// It is expected that the buffer is properly aligned for Direct I/O.
    /// In most platforms that means 512 bytes.
    pub async fn read_dma_aligned(&self, pos: u64, size: usize) -> Result<DmaBuffer> {
        let mut source = Reactor::get().read_dma(self.as_raw_fd(), pos, size);
        enhance!(source.collect_rw().await, "Reading", self)?;
        let stype = source.as_mut().extract_source_type();
        match stype {
            SourceType::DmaRead(buffer) => buffer.ok_or(bad_buffer!(self)),
            _ => Err(bad_buffer!(self)),
        }
    }

    /// Reads into buffer in buf from a specific position in the file.
    ///
    /// It is not necessary to respect the O_DIRECT alignment of the file, and this
    /// API will internally convert the positions and sizes to match, at a cost.
    ///
    /// If you can guarantee proper alignment, prefer read_dma_aligned instead
    pub async fn read_dma(&self, pos: u64, size: usize) -> Result<DmaBuffer> {
        let eff_pos = self.align_down(pos);
        let b = (pos - eff_pos) as usize;

        let eff_size = self.align_up((size + b) as u64) as usize;
        let mut source = Reactor::get().read_dma(self.as_raw_fd(), eff_pos, eff_size);

        enhance!(source.collect_rw().await, "Reading", self)?;
        let stype = source.as_mut().extract_source_type();
        match stype {
            SourceType::DmaRead(buffer) => buffer
                .and_then(|mut buffer| {
                    buffer.trim_front(b);
                    buffer.trim_to_size(size);
                    Some(buffer)
                })
                .ok_or(bad_buffer!(self)),
            _ => Err(bad_buffer!(self)),
        }
    }

    /// Issues fdatasync into the underlying file.
    pub async fn fdatasync(&self) -> Result<()> {
        let source = Reactor::get().fdatasync(self.as_raw_fd());
        enhance!(source.collect_rw().await, "Syncing", self)?;
        Ok(())
    }

    /// pre-allocates space in the filesystem to hold a file at least as big as the size argument
    pub async fn pre_allocate(&self, size: u64) -> Result<()> {
        let flags = libc::FALLOC_FL_ZERO_RANGE;
        let source = Reactor::get().fallocate(self.as_raw_fd(), 0, size, flags);
        enhance!(source.collect_rw().await, "Pre-allocate space", self)?;
        Ok(())
    }

    /// Allocating blocks at the filesystem level turns asynchronous writes into threaded
    /// synchronous writes, as we need to first find the blocks to host the file.
    ///
    /// If the extent is larger, that means many blocks are allocated at a time. For instance,
    /// if the extent size is 1MB, that means that only 1 out of 4 256kB writes will be turned
    /// synchronous. Combined with diligent use of fallocate we can greatly minimize context
    /// switches.
    ///
    /// It is important not to set the extent size too big. Writes can fail otherwise if the
    /// extent can't be allocated
    pub async fn hint_extent_size(&self, size: usize) -> nix::Result<i32> {
        sys::fs_hint_extentsize(self.as_raw_fd(), size)
    }

    /// Truncates a file to the specified size.
    ///
    /// Warning: synchronous operation, will block the reactor
    pub async fn truncate(&self, size: u64) -> Result<()> {
        enhance!(
            sys::truncate_file(self.as_raw_fd(), size),
            "Truncating",
            self
        )
    }

    /// rename an existing file.
    ///
    /// Warning: synchronous operation, will block the reactor
    pub async fn rename<P: AsRef<Path>>(&mut self, new_path: P) -> Result<()> {
        let new_path = new_path.as_ref().to_owned();
        let old_path = path_required!(self, "rename")?;

        enhance!(sys::rename_file(&old_path, &new_path), "Renaming", self)?;
        self.path = Some(new_path);
        Ok(())
    }

    /// remove an existing file given its name
    ///
    /// Warning: synchronous operation, will block the reactor
    pub async fn remove<P: AsRef<Path>>(path: P) -> Result<()> {
        enhance!(
            sys::remove_file(path.as_ref()),
            "Removing",
            Some(path.as_ref()),
            None
        )
    }

    // Retrieve file metadata, backed by the statx(2) syscall
    async fn statx(&self) -> Result<libc::statx> {
        let path = path_required!(self, "stat")?;

        let mut source = Reactor::get().statx(self.as_raw_fd(), path);
        enhance!(source.collect_rw().await, "getting file metadata", self)?;
        let stype = source.as_mut().extract_source_type();
        let stat_buf = match stype {
            SourceType::Statx(_, buf) => buf,
            _ => panic!("Source type is wrong for describe operation"),
        };
        Ok(stat_buf.into_inner())
    }

    /// Returns the size of a file, in bytes
    pub async fn file_size(&self) -> Result<u64> {
        let st = self.statx().await?;
        Ok(st.stx_size)
    }

    /// Closes this DMA file.
    pub async fn close(&mut self) -> Result<()> {
        let source = Reactor::get().close(self.as_raw_fd());
        enhance!(source.collect_rw().await, "Closing", self)?;
        self.file = unsafe { std::fs::File::from_raw_fd(-1) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{LocalExecutor, DmaFile, DmaBuffer};
    use std::path::PathBuf;

    fn make_test_directory(test_name: &str) -> PathBuf {
        // Scipio currently only supports NVMe-backed volumes formatted with XFS or EXT4.
        // We therefore can't use the system-provided tmpfs for our test so instead we let the user
        // decide what directory scipio should use to perform integration tests.
        // For more information regarding this limitation, see the README
        let p = std::env::var("SCIPIO_TESTROOTDIR").expect("SCIPIO_TESTDIR is not part of the ENV");

        let mut dir = PathBuf::from(p);
        std::assert!(dir.exists());

        dir.push(test_name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_create_close() {
        let ex = LocalExecutor::new(None).unwrap();
        let mut path = make_test_directory("io_file_create_close");
        path.push("testfile");

        ex.run(async {
            let mut new_file = DmaFile::create(&path).await.expect("failed to create file");
            new_file.close().await.expect("failed to close newly created file");
        });

        std::assert!(path.exists());
    }

    #[test]
    fn file_open() {
        let ex = LocalExecutor::new(None).unwrap();
        let mut path = make_test_directory("io_file_create_close");
        path.push("testfile");

        ex.run(async {
            let mut new_file = DmaFile::create(&path).await.expect("failed to create file");
            new_file.close().await.expect("failed to close newly created file");

            let mut file = DmaFile::open(&path).await.expect("failed to open file");
            file.close().await.expect("failed to close newly created file");
        });

        std::assert!(path.exists());
    }

    #[test]
    fn file_rename() {
        let ex = LocalExecutor::new(None).unwrap();
        let root_path = make_test_directory("io_file_rename");

        ex.run(async {
            let mut new_file = DmaFile::create(root_path.join("testfile")).await.expect("failed to create file");

            new_file.rename(root_path.join("testfile2")).await.expect("failed to rename file");

            std::assert!(!root_path.join("testfile").exists());
            std::assert!(root_path.join("testfile2").exists());

            new_file.close().await.expect("failed to close newly created file");
        });

    }

    #[test]
    fn file_allocate() {
        let ex = LocalExecutor::new(None).unwrap();
        let root_path = make_test_directory("io_file_allocate");

        ex.run(async {
            let mut new_file = DmaFile::create(root_path.join("testfile")).await.expect("failed to create file");

            new_file.pre_allocate(4096).await.expect("fallocate failed");
            std::assert_eq!(new_file.file_size().await.unwrap(), 4096, "file doesn't have expected size");
            let metadata = std::fs::metadata(root_path.join("testfile")).unwrap();
            std::assert_eq!(metadata.len(), 4096);

            // should be noop
            new_file.pre_allocate(2048).await.expect("fallocate failed");

            std::assert_eq!(new_file.file_size().await.unwrap(), 4096, "file doesn't have expected size");
            let metadata = std::fs::metadata(root_path.join("testfile")).unwrap();
            std::assert_eq!(metadata.len(), 4096);

            new_file.close().await.expect("failed to close newly created file");
        });
    }

    #[test]
    fn file_allocate_zero() {
        let ex = LocalExecutor::new(None).unwrap();
        let root_path = make_test_directory("io_file_allocate_zero");

        ex.run(async {
            let mut new_file = DmaFile::create(root_path.join("testfile")).await.expect("failed to create file");
            new_file.pre_allocate(0).await.expect_err("fallocate should fail with len == 0");

            new_file.close().await.expect("failed to close file");
        });
    }

    #[test]
    fn file_align_up_down() {
        let ex = LocalExecutor::new(None).unwrap();
        let root_path = make_test_directory("io_file_align_up_down");

        ex.run(async {
            let mut new_file = DmaFile::create(root_path.join("testfile")).await.expect("failed to create file");

            // assume 4KiB alignment
            assert_eq!(new_file.align_up(0), 0);
            assert_eq!(new_file.align_down(0), 0);
            assert_eq!(new_file.align_up(1), 4096);
            assert_eq!(new_file.align_down(1), 0);
            assert_eq!(new_file.align_up(4096), 4096);
            assert_eq!(new_file.align_down(4096), 4096);

            new_file.close().await.expect("failed to close file");
        });
    }

    #[test]
    fn file_simple_readwrite() {
        let ex = LocalExecutor::new(None).unwrap();
        let root_path = make_test_directory("io_file_simple_readwrite");

        ex.run(async {
            let mut new_file = DmaFile::create(root_path.join("testfile")).await.expect("failed to create file");

            let buf = DmaBuffer::new(4096).expect("failed to allocate dma buffer");
            for i in buf.as_mut_bytes() {
                *i = 42
            }
            new_file.write_dma(&buf, 0).await.expect("failed to write");

            let read_buf = new_file.read_dma(0, 500).await.expect("failed to read");
            std::assert_eq!(read_buf.len(), 500);
            for i in 0..read_buf.len() {
                std::assert_eq!(read_buf.as_bytes()[i], buf.as_bytes()[i]);
            }

            let read_buf = new_file.read_dma_aligned(0, 4096).await.expect("failed to read");
            std::assert_eq!(read_buf.len(), 4906);
            for i in 0..read_buf.len() {
                std::assert_eq!(read_buf.as_bytes()[i], buf.as_bytes()[i]);
            }

            new_file.close().await.expect("failed to close file");
        });
    }

    #[test]
    fn file_invalid_read() {
        let ex = LocalExecutor::new(None).unwrap();
        let root_path = make_test_directory("io_file_simple_invalid_read");

        ex.run(async {
            let mut new_file = DmaFile::create(root_path.join("testfile")).await.expect("read should fail");
            new_file.read_dma(0, 512).await.expect_err("failed to read");

            new_file.close().await.expect("failed to close file");
        });
    }
}
