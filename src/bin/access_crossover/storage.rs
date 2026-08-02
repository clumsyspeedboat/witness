use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;

use witness::access_compiler::{ClosureMode, ReadSession, SerializedPage};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StorageTier {
    Memory,
    MmapHot,
    BufferedHot,
    BufferedCold,
}

impl StorageTier {
    pub const ALL: [Self; 4] = [
        Self::Memory,
        Self::MmapHot,
        Self::BufferedHot,
        Self::BufferedCold,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Memory => "memory_warm",
            Self::MmapHot => "mmap_hot",
            Self::BufferedHot => "buffered_hot",
            Self::BufferedCold => "buffered_drop_cache",
        }
    }

    pub fn is_cold(self) -> bool {
        matches!(self, Self::BufferedCold)
    }
}

pub struct StorageBundle {
    file: File,
    mapping: MappedFile,
    pub offsets: Vec<u64>,
    pub lengths: Vec<usize>,
}

impl StorageBundle {
    pub fn build(
        path: impl AsRef<Path>,
        pages: &[&[u8]],
        alignment: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if pages.is_empty() || !alignment.is_power_of_two() {
            return Err("bundle pages and alignment are invalid".into());
        }
        let mut writer = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path.as_ref())?;
        let mut offsets = Vec::with_capacity(pages.len());
        let mut cursor = 0_u64;
        for page in pages {
            cursor = align_up(cursor, alignment as u64)?;
            writer.seek(SeekFrom::Start(cursor))?;
            writer.write_all(page)?;
            offsets.push(cursor);
            cursor = cursor
                .checked_add(page.len() as u64)
                .ok_or("bundle length overflow")?;
        }
        writer.set_len(cursor)?;
        writer.sync_all()?;
        drop(writer);
        let file = File::open(path)?;
        let mapping = MappedFile::new(&file)?;
        Ok(Self {
            file,
            mapping,
            offsets,
            lengths: pages.iter().map(|page| page.len()).collect(),
        })
    }

    pub fn session<'a>(
        &'a self,
        page: &'a SerializedPage,
        tier: StorageTier,
        page_index: usize,
        mode: ClosureMode,
    ) -> Result<ReadSession<'a>, String> {
        let offset = *self
            .offsets
            .get(page_index)
            .ok_or("bundle page index is absent")?;
        match tier {
            StorageTier::Memory => Ok(ReadSession::new(page, mode)),
            StorageTier::MmapHot => {
                ReadSession::from_bytes(page, mode, self.mapping.as_slice(), offset as usize)
            }
            StorageTier::BufferedHot | StorageTier::BufferedCold => {
                Ok(ReadSession::from_file(page, mode, &self.file, offset))
            }
        }
    }

    pub fn evict_page(&self, page_index: usize) -> Result<(), String> {
        let offset = self.offsets[page_index];
        let length = self.lengths[page_index] as i64;
        advise_drop_cache(&self.file, offset as i64, length)?;
        self.mapping.evict(offset as usize, length as usize)
    }

    pub fn evict_all(&self) -> Result<(), String> {
        advise_drop_cache(&self.file, 0, 0)?;
        self.mapping.evict(0, self.mapping.len)
    }

    pub fn file_len(&self) -> usize {
        self.mapping.len
    }

    #[allow(dead_code)] // Used by real_access_study, which shares this storage module.
    pub fn file(&self) -> &File {
        &self.file
    }
}

struct MappedFile {
    pointer: NonNull<libc::c_void>,
    len: usize,
}

impl MappedFile {
    fn new(file: &File) -> Result<Self, Box<dyn std::error::Error>> {
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Err("cannot map an empty bundle".into());
        }
        // SAFETY: the file remains open for the mapping lifetime and the mapping is read-only.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            pointer: NonNull::new(pointer).ok_or("mmap returned a null pointer")?,
            len,
        })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `pointer` names a live read-only mapping of exactly `len` bytes.
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr().cast(), self.len) }
    }

    fn evict(&self, offset: usize, length: usize) -> Result<(), String> {
        let page_size = 4096;
        let start = offset / page_size * page_size;
        let end = offset
            .checked_add(length)
            .ok_or("mmap eviction range overflow")?
            .div_ceil(page_size)
            .saturating_mul(page_size)
            .min(self.len);
        // SAFETY: the advised range is page-aligned and lies inside this mapping.
        let status = unsafe {
            libc::madvise(
                self.pointer.as_ptr().cast::<u8>().add(start).cast(),
                end - start,
                libc::MADV_DONTNEED,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(format!(
                "madvise(DONTNEED) failed: {}",
                std::io::Error::last_os_error()
            ))
        }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // SAFETY: this pair exactly matches the successful mmap in `new`.
        unsafe {
            libc::munmap(self.pointer.as_ptr(), self.len);
        }
    }
}

fn align_up(value: u64, alignment: u64) -> Result<u64, String> {
    value
        .checked_add(alignment - 1)
        .map(|value| value / alignment * alignment)
        .ok_or("bundle alignment overflow".into())
}

fn advise_drop_cache(file: &File, offset: i64, length: i64) -> Result<(), String> {
    // SAFETY: posix_fadvise only consumes the valid descriptor and scalar range.
    let status =
        unsafe { libc::posix_fadvise(file.as_raw_fd(), offset, length, libc::POSIX_FADV_DONTNEED) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("posix_fadvise(DONTNEED) failed with {status}"))
    }
}
