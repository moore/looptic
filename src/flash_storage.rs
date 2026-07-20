//! Physical flash layout and the low-level song-map backend.
//!
//! The RP2040 flash API addresses bytes from the start of the flash device,
//! while linker addresses use the XIP window starting at [`FLASH_XIP_BASE`].
//! Keep that distinction explicit: sequential-storage receives offsets, never
//! XIP pointers.

use core::ops::Range;

// These constants are generated from the linker memory map. Keeping the
// driver and linker on one source of truth prevents storage writes from ever
// drifting into an expanded firmware region.
include!(concat!(env!("OUT_DIR"), "/flash_layout.rs"));

/// XIP address of the first persistent byte.
pub const SONG_STORAGE_XIP_START: u32 = FLASH_XIP_BASE + SONG_STORAGE_OFFSET;
/// XIP address one byte past the persistent partition.
pub const SONG_STORAGE_XIP_END: u32 = SONG_STORAGE_XIP_START + SONG_STORAGE_BYTES;

/// RP2040/W25Q64 erase-sector size used by Embassy.
pub const FLASH_ERASE_BYTES: u32 = 4096;
/// RP2040 flash program-page size.
pub const FLASH_PROGRAM_BYTES: usize = 256;
/// Number of pre-named song slots.
pub const SONG_SLOT_COUNT: usize = crate::SONG_SLOT_COUNT;

/// The raw superblock occupies the first erase sector in the partition.
pub const SUPERBLOCK_BYTES: u32 = FLASH_ERASE_BYTES;
/// Number of bytes inspected before storage is classified or initialized.
pub const SUPERBLOCK_SECTOR_BYTES: usize = SUPERBLOCK_BYTES as usize;
/// Offset of the sequential-storage map in the physical flash device.
pub const SONG_MAP_OFFSET: u32 = SONG_STORAGE_OFFSET + SUPERBLOCK_BYTES;
/// Bytes available to sequential-storage after the superblock sector.
pub const SONG_MAP_BYTES: u32 = SONG_STORAGE_BYTES - SUPERBLOCK_BYTES;
/// Number of erase pages managed by sequential-storage.
pub const SONG_MAP_PAGE_COUNT: usize = (SONG_MAP_BYTES / FLASH_ERASE_BYTES) as usize;

/// Complete physical range reserved from firmware linking.
pub const SONG_STORAGE_RANGE: Range<u32> =
    SONG_STORAGE_OFFSET..SONG_STORAGE_OFFSET + SONG_STORAGE_BYTES;
/// Physical range passed to the song map after applying its partition view.
pub const SONG_MAP_RANGE: Range<u32> = SONG_MAP_OFFSET..SONG_MAP_OFFSET + SONG_MAP_BYTES;
/// Erase the metadata sector first so any interrupted initialization reboots
/// as Blank, then erase every remaining byte before committing metadata.
pub const INITIALIZE_METADATA_ERASE_RANGE: Range<u32> =
    SONG_STORAGE_OFFSET..SONG_STORAGE_OFFSET + SUPERBLOCK_BYTES;
pub const INITIALIZE_MAP_ERASE_RANGE: Range<u32> =
    SONG_MAP_OFFSET..SONG_STORAGE_OFFSET + SONG_STORAGE_BYTES;

const _: () = {
    assert!(SONG_STORAGE_OFFSET + SONG_STORAGE_BYTES == TOTAL_FLASH_BYTES);
    assert!(SONG_STORAGE_OFFSET.is_multiple_of(FLASH_ERASE_BYTES));
    assert!(SONG_STORAGE_BYTES.is_multiple_of(FLASH_ERASE_BYTES));
    assert!(SONG_MAP_OFFSET.is_multiple_of(FLASH_ERASE_BYTES));
    assert!(SONG_MAP_BYTES.is_multiple_of(FLASH_ERASE_BYTES));
    assert!(SONG_MAP_PAGE_COUNT == 511);
    assert!(INITIALIZE_METADATA_ERASE_RANGE.start == SONG_STORAGE_RANGE.start);
    assert!(INITIALIZE_METADATA_ERASE_RANGE.end == INITIALIZE_MAP_ERASE_RANGE.start);
    assert!(INITIALIZE_MAP_ERASE_RANGE.end == SONG_STORAGE_RANGE.end);
    assert!(SONG_STORAGE_XIP_START == 0x1060_0000);
    assert!(SONG_STORAGE_XIP_END == 0x1080_0000);
};

const SUPERBLOCK_MAGIC: [u8; 8] = *b"LOOPTIC\0";
/// Version of the fixed raw superblock header prefix.
pub const SUPERBLOCK_HEADER_VERSION: u16 = 1;
const SUPERBLOCK_HEADER_BYTES: u16 = 32;
/// LoopTic's physical/backend storage layout version.
pub const STORAGE_LAYOUT_VERSION: u32 = 1;
/// Major version whose sequential-storage on-flash layout this firmware uses.
pub const SEQUENTIAL_STORAGE_FORMAT_VERSION: u16 = 8;
const SUPERBLOCK_CRC_OFFSET: usize = 28;
const SUPERBLOCK_COMMIT_OFFSET: usize = 32;
// A single programmed bit is the final initialization commit. Keeping every
// other bit high makes an interrupted first write distinguishable from a
// committed partition without another erase sector.
const SUPERBLOCK_COMMIT_MARKER: u8 = 0xfe;

/// Decoded, CRC-verified metadata from the raw storage superblock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SuperblockInfo {
    /// Format of the superblock header itself.
    pub header_version: u16,
    /// LoopTic partition/backend layout version.
    pub storage_layout_version: u32,
    /// sequential-storage on-flash format family.
    pub sequential_storage_version: u16,
    /// Song record format in use when this partition was first initialized.
    ///
    /// Individual records remain self-versioned; this field is diagnostic and
    /// does not prevent a future firmware from supporting mixed record versions.
    pub initial_song_format_version: u16,
    /// Tail-partition size recorded at initialization.
    pub partition_bytes: u32,
    /// Map offset relative to the start of the tail partition.
    pub map_relative_offset: u32,
}

impl SuperblockInfo {
    fn is_supported(self) -> bool {
        self.header_version == SUPERBLOCK_HEADER_VERSION
            && self.storage_layout_version == STORAGE_LAYOUT_VERSION
            && self.sequential_storage_version == SEQUENTIAL_STORAGE_FORMAT_VERSION
            && self.partition_bytes == SONG_STORAGE_BYTES
            && self.map_relative_offset == SUPERBLOCK_BYTES
    }
}

/// Why a metadata page with LoopTic's current magic was not trustworthy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuperblockCorruption {
    /// The sector is not erased and does not begin with LoopTic's magic.
    UnknownMagic,
    /// The immutable header length is invalid for this header version.
    InvalidHeaderLength,
    /// The CRC does not match the fixed header prefix.
    CrcMismatch,
    /// Bytes reserved by the current header format were unexpectedly written.
    ReservedBytesWritten,
    /// The commit byte is neither erased nor the one-bit committed value.
    InvalidCommitMarker,
    /// An erased commit byte accompanied metadata other than an exact partial
    /// image of the descriptor this firmware is prepared to initialize.
    UntrustedIncompleteDescriptor,
}

/// Result of inspecting storage metadata without allowing map repair or writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuperblockStatus {
    /// The whole reserved metadata sector is erased and may be initialized.
    Blank,
    /// Initialization did not reach its one-bit commit. The journal cannot
    /// have been opened yet, so an explicit first Save may safely retry it.
    Incomplete,
    /// Metadata and backend layout are supported by this firmware.
    Ready(SuperblockInfo),
    /// Metadata is valid, but its layout/backend version is not supported.
    Unsupported(SuperblockInfo),
    /// Metadata is neither blank nor safe to interpret.
    Corrupt(SuperblockCorruption),
}

/// Build the uncommitted first program page for a new song partition.
///
/// This function does not erase anything. Callers must first observe
/// [`SuperblockStatus::Blank`] or [`SuperblockStatus::Incomplete`] and should
/// write the page only during an explicit, audio-quiesced Save operation.
pub fn encode_superblock(initial_song_format_version: u16) -> [u8; FLASH_PROGRAM_BYTES] {
    let mut page = [0xff; FLASH_PROGRAM_BYTES];
    page[..8].copy_from_slice(&SUPERBLOCK_MAGIC);
    page[8..10].copy_from_slice(&SUPERBLOCK_HEADER_VERSION.to_le_bytes());
    page[10..12].copy_from_slice(&SUPERBLOCK_HEADER_BYTES.to_le_bytes());
    page[12..16].copy_from_slice(&STORAGE_LAYOUT_VERSION.to_le_bytes());
    page[16..18].copy_from_slice(&SEQUENTIAL_STORAGE_FORMAT_VERSION.to_le_bytes());
    page[18..20].copy_from_slice(&initial_song_format_version.to_le_bytes());
    page[20..24].copy_from_slice(&SONG_STORAGE_BYTES.to_le_bytes());
    page[24..28].copy_from_slice(&SUPERBLOCK_BYTES.to_le_bytes());
    let crc = crc32(&page[..SUPERBLOCK_CRC_OFFSET]);
    page[SUPERBLOCK_CRC_OFFSET..SUPERBLOCK_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
    page
}

/// Produce the second, multiwrite-safe page image that commits initialization.
#[cfg(any(target_arch = "arm", test))]
fn commit_superblock(mut page: [u8; FLASH_PROGRAM_BYTES]) -> [u8; FLASH_PROGRAM_BYTES] {
    page[SUPERBLOCK_COMMIT_OFFSET] = SUPERBLOCK_COMMIT_MARKER;
    page
}

/// Inspect the complete reserved metadata sector.
///
/// `Incomplete` is intentionally narrow: every programmed bit must be
/// compatible with the exact uncommitted descriptor requested by this
/// firmware, and every reserved byte must remain erased. Any broader rule
/// could mistake foreign metadata for a retryable initialization and erase it.
pub fn inspect_superblock(
    sector: &[u8; SUPERBLOCK_SECTOR_BYTES],
    expected_initial_song_format_version: u16,
) -> SuperblockStatus {
    if sector.iter().all(|byte| *byte == 0xff) {
        return SuperblockStatus::Blank;
    }

    let page = &sector[..FLASH_PROGRAM_BYTES];
    let matches_uncommitted_descriptor = |expected: &[u8; FLASH_PROGRAM_BYTES]| {
        page[SUPERBLOCK_COMMIT_OFFSET] == 0xff
            && sector[SUPERBLOCK_COMMIT_OFFSET + 1..]
                .iter()
                .all(|byte| *byte == 0xff)
            && page[..SUPERBLOCK_COMMIT_OFFSET]
                .iter()
                .zip(&expected[..SUPERBLOCK_COMMIT_OFFSET])
                .all(|(&observed, &target)| observed & target == target)
    };
    let expected = encode_superblock(expected_initial_song_format_version);
    let is_exact_partial_descriptor = matches_uncommitted_descriptor(&expected)
        || (expected_initial_song_format_version == crate::SONG_FORMAT_VERSION
            && (matches_uncommitted_descriptor(&encode_superblock(crate::SONG_FORMAT_V2))
                || matches_uncommitted_descriptor(&encode_superblock(crate::SONG_FORMAT_V3))));
    if is_exact_partial_descriptor {
        return SuperblockStatus::Incomplete;
    }

    if page[..8] != SUPERBLOCK_MAGIC {
        return SuperblockStatus::Corrupt(SuperblockCorruption::UnknownMagic);
    }

    let info = SuperblockInfo {
        header_version: u16::from_le_bytes([page[8], page[9]]),
        storage_layout_version: u32::from_le_bytes([page[12], page[13], page[14], page[15]]),
        sequential_storage_version: u16::from_le_bytes([page[16], page[17]]),
        initial_song_format_version: u16::from_le_bytes([page[18], page[19]]),
        partition_bytes: u32::from_le_bytes([page[20], page[21], page[22], page[23]]),
        map_relative_offset: u32::from_le_bytes([page[24], page[25], page[26], page[27]]),
    };
    let stored_crc = u32::from_le_bytes([
        page[SUPERBLOCK_CRC_OFFSET],
        page[SUPERBLOCK_CRC_OFFSET + 1],
        page[SUPERBLOCK_CRC_OFFSET + 2],
        page[SUPERBLOCK_CRC_OFFSET + 3],
    ]);
    if stored_crc != crc32(&page[..SUPERBLOCK_CRC_OFFSET]) {
        return SuperblockStatus::Corrupt(SuperblockCorruption::CrcMismatch);
    }

    // A future header must preserve the v1 prefix and CRC if it wants an old
    // firmware to report Unsupported instead of Corrupt.
    if info.header_version != SUPERBLOCK_HEADER_VERSION {
        return SuperblockStatus::Unsupported(info);
    }
    let header_bytes = u16::from_le_bytes([page[10], page[11]]);
    if header_bytes != SUPERBLOCK_HEADER_BYTES {
        return SuperblockStatus::Corrupt(SuperblockCorruption::InvalidHeaderLength);
    }
    if sector[SUPERBLOCK_COMMIT_OFFSET + 1..]
        .iter()
        .any(|byte| *byte != 0xff)
    {
        return SuperblockStatus::Corrupt(SuperblockCorruption::ReservedBytesWritten);
    }

    if !info.is_supported() {
        SuperblockStatus::Unsupported(info)
    } else {
        match page[SUPERBLOCK_COMMIT_OFFSET] {
            SUPERBLOCK_COMMIT_MARKER => SuperblockStatus::Ready(info),
            0xff => SuperblockStatus::Corrupt(SuperblockCorruption::UntrustedIncompleteDescriptor),
            _ => SuperblockStatus::Corrupt(SuperblockCorruption::InvalidCommitMarker),
        }
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(target_arch = "arm")]
mod rp2040 {
    use core::cell::RefCell;

    use embassy_embedded_hal::adapter::BlockingAsync;
    use embassy_embedded_hal::flash::partition::{BlockingPartition, Error as PartitionError};
    use embassy_rp::Peri;
    use embassy_rp::flash::{Blocking, Error as FlashError, Flash};
    use embassy_rp::peripherals::FLASH;
    use embassy_sync::blocking_mutex::Mutex;
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;
    use embassy_time::{Duration, Timer};
    use sequential_storage::Error as SequentialError;
    use sequential_storage::cache::Cache;
    use sequential_storage::cache::key_pointers::ArrayKeyPointers;
    use sequential_storage::cache::page_pointers::ArrayPagePointers;
    use sequential_storage::cache::page_states::CalculatedPageStates;
    use sequential_storage::map::{MapConfig, MapStorage};
    use static_cell::StaticCell;

    use super::{
        INITIALIZE_MAP_ERASE_RANGE, INITIALIZE_METADATA_ERASE_RANGE, SONG_MAP_BYTES,
        SONG_MAP_OFFSET, SONG_MAP_PAGE_COUNT, SONG_SLOT_COUNT, SONG_STORAGE_OFFSET,
        SUPERBLOCK_SECTOR_BYTES, SuperblockCorruption, SuperblockInfo, SuperblockStatus,
        TOTAL_FLASH_BYTES, commit_superblock, encode_superblock, inspect_superblock,
    };

    type RawFlash = Flash<'static, FLASH, Blocking, { TOTAL_FLASH_BYTES as usize }>;
    type FlashLock = Mutex<NoopRawMutex, RefCell<RawFlash>>;
    type PhysicalPartition = BlockingPartition<'static, NoopRawMutex, RawFlash>;
    type AsyncPartition = BlockingAsync<PhysicalPartition>;
    type SongCache = Cache<
        CalculatedPageStates,
        ArrayPagePointers<SONG_MAP_PAGE_COUNT>,
        ArrayKeyPointers<u8, SONG_SLOT_COUNT>,
        u8,
    >;
    type RawSongMap = MapStorage<u8, AsyncPartition, SongCache>;

    static RAW_FLASH: StaticCell<FlashLock> = StaticCell::new();
    static SONG_MAP: StaticCell<SongMap> = StaticCell::new();

    /// Error returned by sequential-storage over the restricted song partition.
    pub type SongMapError = SequentialError<PartitionError<FlashError>>;

    /// A probed blank device that has not yet written its superblock.
    pub struct BlankSongStorage {
        flash: RawFlash,
        force_reformat: bool,
    }

    /// A valid but unsupported partition retained solely for an explicit,
    /// user-confirmed destructive reformat.
    pub struct UnsupportedSongStorage {
        info: SuperblockInfo,
        storage: BlankSongStorage,
    }

    /// Outcome of the read-only metadata probe performed before audio starts.
    pub enum StorageProbe {
        /// Fresh/erased storage. Initialize only as part of an explicit Save.
        Blank(BlankSongStorage),
        /// A power cut interrupted the one-time superblock write before its
        /// commit bit. Explicit first Save can safely retry initialization.
        Incomplete(BlankSongStorage),
        /// The superblock is supported and the map is safe to use.
        Ready(&'static mut SongMap),
        /// Valid metadata describes an unsupported backend layout.
        Unsupported(UnsupportedSongStorage),
        /// Metadata is damaged or is not a LoopTic superblock.
        Corrupt(SuperblockCorruption),
    }

    /// Failure while initializing a previously blank partition.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum InitializeError {
        /// Low-level flash read/program failure.
        Flash(FlashError),
        /// The sector changed after probing and is no longer blank.
        NoLongerBlank(SuperblockStatus),
        /// The programmed page did not verify as the current layout.
        VerificationFailed(SuperblockStatus),
    }

    /// A sequential, wear-levelled key/value map restricted to slots 0..=255.
    pub struct SongMap {
        map: RawSongMap,
    }

    impl SongMap {
        /// Load a raw, self-versioned song record into `buffer`.
        pub async fn load<'a>(
            &mut self,
            slot: u8,
            buffer: &'a mut [u8],
        ) -> Result<Option<&'a [u8]>, SongMapError> {
            self.map.fetch_item::<&'a [u8]>(buffer, &slot).await
        }

        /// Store a raw, self-versioned song record.
        ///
        /// `work_buffer` must be large enough for the one-byte key plus the
        /// record, rounded up to flash word alignment.
        pub async fn save(
            &mut self,
            slot: u8,
            work_buffer: &mut [u8],
            record: &[u8],
        ) -> Result<(), SongMapError> {
            self.map.store_item(work_buffer, &slot, &record).await
        }

        /// Remove all physical entries for a slot.
        pub async fn delete(
            &mut self,
            slot: u8,
            work_buffer: &mut [u8],
        ) -> Result<(), SongMapError> {
            self.map.remove_item(work_buffer, &slot).await
        }

        /// Scan the map once and return a 256-bit occupied-slot bitmap.
        pub async fn scan_occupancy(
            &mut self,
            work_buffer: &mut [u8],
        ) -> Result<[u32; SONG_SLOT_COUNT / 32], SongMapError> {
            let mut words = [0_u32; SONG_SLOT_COUNT / 32];
            let mut items = self.map.fetch_all_items(work_buffer).await?;
            while let Some((slot, _record)) = items.next::<&[u8]>(work_buffer).await? {
                let slot = usize::from(slot);
                words[slot / 32] |= 1 << (slot % 32);
            }
            Ok(words)
        }
    }

    impl BlankSongStorage {
        /// Write and verify the immutable superblock, then open the empty map.
        ///
        /// Initialization invalidates metadata first, then erases the complete
        /// map before writing a new descriptor. Thus a power cut at any point
        /// before commit reboots as Blank or retryable Incomplete; a stale map
        /// can never be opened under freshly initialized metadata.
        pub async fn initialize(
            self,
            initial_song_format_version: u16,
        ) -> Result<&'static mut SongMap, InitializeError> {
            self.initialize_with_progress(initial_song_format_version, |_| {})
                .await
        }

        async fn initialize_with_progress(
            mut self,
            initial_song_format_version: u16,
            mut report_progress: impl FnMut(u8),
        ) -> Result<&'static mut SongMap, InitializeError> {
            let mut sector = [0_u8; SUPERBLOCK_SECTOR_BYTES];
            self.flash
                .blocking_read(SONG_STORAGE_OFFSET, &mut sector)
                .map_err(InitializeError::Flash)?;
            let before = inspect_superblock(&sector, initial_song_format_version);
            match before {
                SuperblockStatus::Blank | SuperblockStatus::Incomplete => {}
                SuperblockStatus::Unsupported(_) if self.force_reformat => {}
                _ => return Err(InitializeError::NoLongerBlank(before)),
            }

            // A factory-erased partition is by far the common first-Save
            // case. Erasing all 511 map sectors can hold the RP2040 in the ROM
            // flash routine for tens of seconds, making the device appear to
            // have crashed. Reads do not take XIP offline, so cheaply verify
            // the map and avoid rewriting already-erased flash. An
            // interrupted initialization must still erase unconditionally:
            // it may have left an internally consistent-looking fragment of
            // an older journal behind.
            let map_is_erased = if before == SuperblockStatus::Blank && !self.force_reformat {
                let mut erased = true;
                let mut offset = INITIALIZE_MAP_ERASE_RANGE.start;
                while offset < INITIALIZE_MAP_ERASE_RANGE.end {
                    self.flash
                        .blocking_read(offset, &mut sector)
                        .map_err(InitializeError::Flash)?;
                    if sector.iter().any(|byte| *byte != 0xff) {
                        erased = false;
                        break;
                    }
                    offset += SUPERBLOCK_SECTOR_BYTES as u32;
                    // Reads are quick, but scanning the full 2 MiB map should
                    // still let the display and LED tasks report that the
                    // explicit Save is alive.
                    Timer::after(Duration::from_millis(1)).await;
                }
                erased
            } else {
                false
            };

            // Erase metadata first. If power fails during the much longer map
            // erase, the next boot cannot mistake a partly erased old journal
            // for a committed current one and will repeat the full erase.
            self.flash
                .blocking_erase(
                    INITIALIZE_METADATA_ERASE_RANGE.start,
                    INITIALIZE_METADATA_ERASE_RANGE.end,
                )
                .map_err(InitializeError::Flash)?;
            report_progress(0);
            if !map_is_erased {
                let mut offset = INITIALIZE_MAP_ERASE_RANGE.start;
                while offset < INITIALIZE_MAP_ERASE_RANGE.end {
                    self.flash
                        .blocking_erase(offset, offset + SUPERBLOCK_SECTOR_BYTES as u32)
                        .map_err(InitializeError::Flash)?;
                    offset += SUPERBLOCK_SECTOR_BYTES as u32;
                    let completed = (offset - INITIALIZE_MAP_ERASE_RANGE.start)
                        / SUPERBLOCK_SECTOR_BYTES as u32;
                    report_progress(((completed * 100) / SONG_MAP_PAGE_COUNT as u32).min(100) as u8);
                    // The ROM routine disables interrupts and XIP only for one
                    // sector. Yield between sectors so the low executor can
                    // refresh Busy UI/LED feedback during a long recovery.
                    Timer::after(Duration::from_millis(1)).await;
                }
            }
            self.flash
                .blocking_read(SONG_STORAGE_OFFSET, &mut sector)
                .map_err(InitializeError::Flash)?;
            let erased = inspect_superblock(&sector, initial_song_format_version);
            if erased != SuperblockStatus::Blank {
                return Err(InitializeError::VerificationFailed(erased));
            }

            let descriptor = encode_superblock(initial_song_format_version);
            self.flash
                .blocking_write(SONG_STORAGE_OFFSET, &descriptor)
                .map_err(InitializeError::Flash)?;
            self.flash
                .blocking_read(SONG_STORAGE_OFFSET, &mut sector)
                .map_err(InitializeError::Flash)?;
            let descriptor_status = inspect_superblock(&sector, initial_song_format_version);
            if descriptor_status != SuperblockStatus::Incomplete {
                return Err(InitializeError::VerificationFailed(descriptor_status));
            }

            let committed = commit_superblock(descriptor);
            self.flash
                .blocking_write(SONG_STORAGE_OFFSET, &committed)
                .map_err(InitializeError::Flash)?;
            self.flash
                .blocking_read(SONG_STORAGE_OFFSET, &mut sector)
                .map_err(InitializeError::Flash)?;
            let after = inspect_superblock(&sector, initial_song_format_version);
            if !matches!(after, SuperblockStatus::Ready(_)) {
                return Err(InitializeError::VerificationFailed(after));
            }
            report_progress(100);
            Ok(open_map(self.flash))
        }
    }

    impl UnsupportedSongStorage {
        pub const fn info(&self) -> SuperblockInfo {
            self.info
        }

        /// Destroy the unsupported partition only after UI confirmation.
        pub async fn reformat(
            self,
            initial_song_format_version: u16,
            report_progress: impl FnMut(u8),
        ) -> Result<&'static mut SongMap, InitializeError> {
            self.storage
                .initialize_with_progress(initial_song_format_version, report_progress)
                .await
        }
    }

    /// Inspect metadata without opening sequential-storage or permitting its
    /// auto-repair logic to touch an incompatible partition.
    pub fn probe(flash: Peri<'static, FLASH>) -> Result<StorageProbe, FlashError> {
        let mut flash = Flash::new_blocking(flash);
        let mut sector = [0_u8; SUPERBLOCK_SECTOR_BYTES];
        flash.blocking_read(SONG_STORAGE_OFFSET, &mut sector)?;
        Ok(
            match inspect_superblock(&sector, crate::SONG_FORMAT_VERSION) {
                SuperblockStatus::Blank => StorageProbe::Blank(BlankSongStorage {
                    flash,
                    force_reformat: false,
                }),
                SuperblockStatus::Incomplete => StorageProbe::Incomplete(BlankSongStorage {
                    flash,
                    force_reformat: false,
                }),
                SuperblockStatus::Ready(_) => StorageProbe::Ready(open_map(flash)),
                SuperblockStatus::Unsupported(info) => {
                    StorageProbe::Unsupported(UnsupportedSongStorage {
                        info,
                        storage: BlankSongStorage {
                            flash,
                            force_reformat: true,
                        },
                    })
                }
                SuperblockStatus::Corrupt(reason) => StorageProbe::Corrupt(reason),
            },
        )
    }

    fn open_map(flash: RawFlash) -> &'static mut SongMap {
        let flash = RAW_FLASH.init(Mutex::new(RefCell::new(flash)));
        let physical = BlockingPartition::new(flash, SONG_MAP_OFFSET, SONG_MAP_BYTES);
        let storage = BlockingAsync::new(physical);
        let cache = Cache::new(
            CalculatedPageStates::new(SONG_MAP_PAGE_COUNT),
            ArrayPagePointers::new(),
            ArrayKeyPointers::new(),
        );
        let config = const { MapConfig::new(0..SONG_MAP_BYTES) };
        SONG_MAP.init(SongMap {
            map: MapStorage::new(storage, config, cache),
        })
    }
}

#[cfg(target_arch = "arm")]
pub use rp2040::{
    BlankSongStorage, InitializeError, SongMap, SongMapError, StorageProbe, UnsupportedSongStorage,
    probe,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn sector_with_page(page: [u8; FLASH_PROGRAM_BYTES]) -> [u8; SUPERBLOCK_SECTOR_BYTES] {
        let mut sector = [0xff; SUPERBLOCK_SECTOR_BYTES];
        sector[..FLASH_PROGRAM_BYTES].copy_from_slice(&page);
        sector
    }

    fn inspect_page(
        page: [u8; FLASH_PROGRAM_BYTES],
        expected_song_format: u16,
    ) -> SuperblockStatus {
        inspect_superblock(&sector_with_page(page), expected_song_format)
    }

    #[test]
    fn partition_geometry_is_exact_and_sector_aligned() {
        assert_eq!(SONG_STORAGE_RANGE, 0x0060_0000..0x0080_0000);
        assert_eq!(SONG_MAP_RANGE, 0x0060_1000..0x0080_0000);
        assert_eq!(SONG_MAP_PAGE_COUNT, 511);
        assert_eq!(INITIALIZE_METADATA_ERASE_RANGE, 0x0060_0000..0x0060_1000);
        assert_eq!(INITIALIZE_MAP_ERASE_RANGE, 0x0060_1000..0x0080_0000);
    }

    #[test]
    fn erased_superblock_is_blank() {
        assert_eq!(
            inspect_superblock(&[0xff; SUPERBLOCK_SECTOR_BYTES], 1),
            SuperblockStatus::Blank
        );
    }

    #[test]
    fn encoded_superblock_round_trips_schema_information() {
        let page = commit_superblock(encode_superblock(7));
        let SuperblockStatus::Ready(info) = inspect_page(page, 7) else {
            panic!("current superblock should be ready");
        };
        assert_eq!(info.initial_song_format_version, 7);
        assert_eq!(info.storage_layout_version, STORAGE_LAYOUT_VERSION);
        assert_eq!(info.partition_bytes, SONG_STORAGE_BYTES);
        assert_eq!(info.map_relative_offset, SUPERBLOCK_BYTES);
    }

    #[test]
    fn crc32_matches_the_standard_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn crc_failure_is_corrupt_not_blank_or_unsupported() {
        let mut page = commit_superblock(encode_superblock(1));
        page[20] ^= 0x40;
        assert_eq!(
            inspect_page(page, 1),
            SuperblockStatus::Corrupt(SuperblockCorruption::CrcMismatch)
        );
    }

    #[test]
    fn valid_unknown_layout_is_reported_as_unsupported() {
        let mut page = commit_superblock(encode_superblock(1));
        let unknown_version = STORAGE_LAYOUT_VERSION + 1;
        page[12..16].copy_from_slice(&unknown_version.to_le_bytes());
        let crc = crc32(&page[..SUPERBLOCK_CRC_OFFSET]);
        page[SUPERBLOCK_CRC_OFFSET..SUPERBLOCK_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            inspect_page(page, 1),
            SuperblockStatus::Unsupported(SuperblockInfo {
                storage_layout_version,
                ..
            }) if storage_layout_version == unknown_version
        ));
    }

    #[test]
    fn unknown_nonblank_magic_is_corrupt() {
        let mut page = [0xff; FLASH_PROGRAM_BYTES];
        page[0] = 0;
        assert_eq!(
            inspect_page(page, 1),
            SuperblockStatus::Corrupt(SuperblockCorruption::UnknownMagic)
        );
    }

    #[test]
    fn writes_anywhere_after_current_header_are_detected() {
        let page = commit_superblock(encode_superblock(1));
        let mut sector = sector_with_page(page);
        sector[1000] = 0;
        assert_eq!(
            inspect_superblock(&sector, 1),
            SuperblockStatus::Corrupt(SuperblockCorruption::ReservedBytesWritten)
        );
    }

    #[test]
    fn arbitrary_reserved_write_is_not_retryable_incomplete_storage() {
        let mut sector = [0xff; SUPERBLOCK_SECTOR_BYTES];
        sector[100] = 0;
        assert!(matches!(
            inspect_superblock(&sector, 1),
            SuperblockStatus::Corrupt(_)
        ));

        let descriptor = encode_superblock(1);
        sector[..11].copy_from_slice(&descriptor[..11]);
        assert!(matches!(
            inspect_superblock(&sector, 1),
            SuperblockStatus::Corrupt(_)
        ));
    }

    #[test]
    fn uncommitted_or_partially_programmed_current_header_is_retryable() {
        let descriptor = encode_superblock(1);
        assert_eq!(inspect_page(descriptor, 1), SuperblockStatus::Incomplete);
        let committed = commit_superblock(descriptor);
        assert_eq!(descriptor[SUPERBLOCK_COMMIT_OFFSET], 0xff);
        assert_eq!(committed[SUPERBLOCK_COMMIT_OFFSET], 0xfe);
        assert_eq!(
            descriptor
                .iter()
                .zip(committed)
                .map(|(&before, after)| (before ^ after).count_ones())
                .sum::<u32>(),
            1
        );

        for prefix_len in 1..=SUPERBLOCK_COMMIT_OFFSET {
            let mut partial = [0xff; FLASH_PROGRAM_BYTES];
            partial[..prefix_len].copy_from_slice(&descriptor[..prefix_len]);
            assert_eq!(
                inspect_page(partial, 1),
                SuperblockStatus::Incomplete,
                "partial prefix length {prefix_len}"
            );
        }
    }

    #[test]
    fn crc_bad_uncommitted_metadata_is_corrupt_not_retryable() {
        let mut page = encode_superblock(1);
        page[8] = 0;
        assert_eq!(
            inspect_page(page, 1),
            SuperblockStatus::Corrupt(SuperblockCorruption::CrcMismatch)
        );
    }

    #[test]
    fn only_the_expected_song_format_descriptor_is_retryable() {
        assert_eq!(
            inspect_page(encode_superblock(2), 1),
            SuperblockStatus::Corrupt(SuperblockCorruption::UntrustedIncompleteDescriptor)
        );
    }

    #[test]
    fn interrupted_v2_and_v3_initialization_is_retryable_after_the_v4_upgrade() {
        for legacy_version in [crate::SONG_FORMAT_V2, crate::SONG_FORMAT_V3] {
            let descriptor = encode_superblock(legacy_version);
            assert_eq!(
                inspect_page(descriptor, crate::SONG_FORMAT_VERSION),
                SuperblockStatus::Incomplete
            );

            for prefix_len in 1..=SUPERBLOCK_COMMIT_OFFSET {
                let mut partial = [0xff; FLASH_PROGRAM_BYTES];
                partial[..prefix_len].copy_from_slice(&descriptor[..prefix_len]);
                assert_eq!(
                    inspect_page(partial, crate::SONG_FORMAT_VERSION),
                    SuperblockStatus::Incomplete,
                    "v{legacy_version} partial prefix length {prefix_len}"
                );
            }
        }
    }

    #[test]
    fn malformed_commit_marker_is_corrupt() {
        let mut page = encode_superblock(1);
        page[SUPERBLOCK_COMMIT_OFFSET] = 0xfc;
        assert_eq!(
            inspect_page(page, 1),
            SuperblockStatus::Corrupt(SuperblockCorruption::InvalidCommitMarker)
        );
    }
}
