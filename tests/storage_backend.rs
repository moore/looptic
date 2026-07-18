//! Host-side integration tests for the pinned sequential-storage backend.
//!
//! These tests exercise the actual 511-sector/256-key geometry and inject
//! byte-granular power cuts into a smaller map so recovery can be checked at
//! every mutation point without requiring RP2040 hardware.

use core::future::Future;
use core::ops::Range;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use embedded_storage_async::nor_flash::{
    ErrorType, MultiwriteNorFlash, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};
use looptic::flash_storage::{FLASH_ERASE_BYTES, SONG_MAP_BYTES, SONG_MAP_PAGE_COUNT};
use looptic::{SONG_ENCODED_MAX_LEN, SONG_SLOT_COUNT};
use sequential_storage::cache::Cache;
use sequential_storage::cache::Uncached;
use sequential_storage::cache::key_pointers::ArrayKeyPointers;
use sequential_storage::cache::page_pointers::ArrayPagePointers;
use sequential_storage::cache::page_states::CalculatedPageStates;
use sequential_storage::map::{MapConfig, MapStorage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemoryFlashError {
    OutOfBounds,
    Unaligned,
    WouldSetBit,
    PowerCut,
}

impl NorFlashError for MemoryFlashError {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            Self::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            Self::Unaligned => NorFlashErrorKind::NotAligned,
            Self::WouldSetBit | Self::PowerCut => NorFlashErrorKind::Other,
        }
    }
}

#[derive(Clone)]
struct MemoryFlash<const PAGES: usize, const PAGE_BYTES: usize> {
    bytes: std::vec::Vec<u8>,
    mutations: usize,
    fail_after: Option<usize>,
    power_lost: bool,
}

impl<const PAGES: usize, const PAGE_BYTES: usize> MemoryFlash<PAGES, PAGE_BYTES> {
    fn erased() -> Self {
        Self {
            bytes: std::vec![0xff; PAGES * PAGE_BYTES],
            mutations: 0,
            fail_after: None,
            power_lost: false,
        }
    }

    fn arm_power_cut(&mut self, successful_mutations_before_cut: usize) {
        self.mutations = 0;
        self.fail_after = Some(successful_mutations_before_cut);
        self.power_lost = false;
    }

    fn restore_power(&mut self) {
        self.fail_after = None;
        self.power_lost = false;
        self.mutations = 0;
    }

    fn mutate_byte(&mut self) -> Result<(), MemoryFlashError> {
        if self.power_lost {
            return Err(MemoryFlashError::PowerCut);
        }
        if let Some(remaining) = self.fail_after.as_mut() {
            if *remaining == 0 {
                self.power_lost = true;
                return Err(MemoryFlashError::PowerCut);
            }
            *remaining -= 1;
        }
        self.mutations += 1;
        Ok(())
    }

    fn checked_range(&self, offset: u32, len: usize) -> Result<Range<usize>, MemoryFlashError> {
        let start = usize::try_from(offset).map_err(|_| MemoryFlashError::OutOfBounds)?;
        let end = start
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(MemoryFlashError::OutOfBounds)?;
        Ok(start..end)
    }
}

impl<const PAGES: usize, const PAGE_BYTES: usize> ErrorType for MemoryFlash<PAGES, PAGE_BYTES> {
    type Error = MemoryFlashError;
}

impl<const PAGES: usize, const PAGE_BYTES: usize> ReadNorFlash for MemoryFlash<PAGES, PAGE_BYTES> {
    const READ_SIZE: usize = 1;

    async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        if self.power_lost {
            return Err(MemoryFlashError::PowerCut);
        }
        let range = self.checked_range(offset, bytes.len())?;
        bytes.copy_from_slice(&self.bytes[range]);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.bytes.len()
    }
}

impl<const PAGES: usize, const PAGE_BYTES: usize> NorFlash for MemoryFlash<PAGES, PAGE_BYTES> {
    const WRITE_SIZE: usize = 1;
    const ERASE_SIZE: usize = PAGE_BYTES;

    async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        let range = self.checked_range(offset, bytes.len())?;
        for (address, &new) in range.zip(bytes) {
            self.mutate_byte()?;
            let old = self.bytes[address];
            if old & new != new {
                return Err(MemoryFlashError::WouldSetBit);
            }
            self.bytes[address] = old & new;
        }
        Ok(())
    }

    async fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        let from = usize::try_from(from).map_err(|_| MemoryFlashError::OutOfBounds)?;
        let to = usize::try_from(to).map_err(|_| MemoryFlashError::OutOfBounds)?;
        if from > to || from % PAGE_BYTES != 0 || to % PAGE_BYTES != 0 {
            return Err(MemoryFlashError::Unaligned);
        }
        let range = self.checked_range(from as u32, to - from)?;
        for address in range {
            self.mutate_byte()?;
            self.bytes[address] = 0xff;
        }
        Ok(())
    }
}

impl<const PAGES: usize, const PAGE_BYTES: usize> MultiwriteNorFlash
    for MemoryFlash<PAGES, PAGE_BYTES>
{
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

type FullFlash = MemoryFlash<SONG_MAP_PAGE_COUNT, { FLASH_ERASE_BYTES as usize }>;
type FullCache = Cache<
    CalculatedPageStates,
    ArrayPagePointers<SONG_MAP_PAGE_COUNT>,
    ArrayKeyPointers<u8, SONG_SLOT_COUNT>,
    u8,
>;
type FullMap = MapStorage<u8, FullFlash, FullCache>;

fn full_map(flash: FullFlash) -> FullMap {
    let cache = Cache::new(
        CalculatedPageStates::new(SONG_MAP_PAGE_COUNT),
        ArrayPagePointers::new(),
        ArrayKeyPointers::new(),
    );
    MapStorage::new(flash, const { MapConfig::new(0..SONG_MAP_BYTES) }, cache)
}

#[test]
fn actual_partition_holds_all_256_slots_then_updates_deletes_and_reopens() {
    let mut map = full_map(FullFlash::erased());
    let mut work = [0_u8; 4096];
    let mut record = [0_u8; SONG_ENCODED_MAX_LEN - 128];

    for slot in 0_u16..SONG_SLOT_COUNT as u16 {
        record.fill(slot as u8);
        let value: &[u8] = &record;
        block_on(map.store_item(&mut work, &(slot as u8), &value)).unwrap();
    }

    record.fill(0xa5);
    let replacement: &[u8] = &record;
    block_on(map.store_item(&mut work, &17, &replacement)).unwrap();
    block_on(map.remove_item(&mut work, &93)).unwrap();

    let (flash, _) = map.destroy();
    let mut reopened = full_map(flash);
    for slot in 0_u16..SONG_SLOT_COUNT as u16 {
        let loaded = block_on(reopened.fetch_item::<&[u8]>(&mut work, &(slot as u8))).unwrap();
        if slot == 93 {
            assert!(loaded.is_none());
        } else {
            let loaded = loaded.expect("occupied slot survives reopen");
            let expected = if slot == 17 { 0xa5 } else { slot as u8 };
            assert_eq!(loaded.len(), record.len());
            assert!(loaded.iter().all(|byte| *byte == expected));
        }
    }
}

const SMALL_PAGES: usize = 8;
const SMALL_PAGE_BYTES: usize = 256;
const SMALL_BYTES: u32 = (SMALL_PAGES * SMALL_PAGE_BYTES) as u32;
type SmallFlash = MemoryFlash<SMALL_PAGES, SMALL_PAGE_BYTES>;
type SmallCache = Cache<Uncached, Uncached, Uncached, u8>;
type SmallMap = MapStorage<u8, SmallFlash, SmallCache>;

fn small_map(flash: SmallFlash) -> SmallMap {
    MapStorage::new(
        flash,
        const { MapConfig::new(0..SMALL_BYTES) },
        Cache::new_uncached(),
    )
}

fn stored_value(flash: SmallFlash, key: u8) -> (SmallFlash, Option<std::vec::Vec<u8>>) {
    let mut map = small_map(flash);
    let mut work = [0_u8; SMALL_PAGE_BYTES];
    let value = block_on(map.fetch_item::<&[u8]>(&mut work, &key))
        .unwrap()
        .map(<[u8]>::to_vec);
    let (flash, _) = map.destroy();
    (flash, value)
}

fn save_small(mut flash: SmallFlash, key: u8, value: &[u8]) -> SmallFlash {
    flash.restore_power();
    let mut map = small_map(flash);
    let mut work = [0_u8; SMALL_PAGE_BYTES];
    block_on(map.store_item(&mut work, &key, &value)).unwrap();
    map.destroy().0
}

fn delete_small(mut flash: SmallFlash, key: u8) -> SmallFlash {
    flash.restore_power();
    let mut map = small_map(flash);
    let mut work = [0_u8; SMALL_PAGE_BYTES];
    block_on(map.remove_item(&mut work, &key)).unwrap();
    map.destroy().0
}

fn mutation_count_for_save(base: &SmallFlash, key: u8, value: &[u8]) -> usize {
    let mut completed = save_small(base.clone(), key, value);
    let count = completed.mutations;
    completed.restore_power();
    count
}

fn mutation_count_for_delete(base: &SmallFlash, key: u8) -> usize {
    let mut completed = delete_small(base.clone(), key);
    let count = completed.mutations;
    completed.restore_power();
    count
}

#[test]
fn power_cuts_never_expose_torn_first_save_overwrite_copy_or_delete() {
    let old = [0x11_u8; 64];
    let new = [0x22_u8; 64];

    let erased = SmallFlash::erased();
    let first_count = mutation_count_for_save(&erased, 1, &old);
    for cut in 0..first_count {
        let mut flash = erased.clone();
        flash.arm_power_cut(cut);
        let mut map = small_map(flash);
        let mut work = [0_u8; SMALL_PAGE_BYTES];
        let value: &[u8] = &old;
        assert!(block_on(map.store_item(&mut work, &1, &value)).is_err());
        let mut flash = map.destroy().0;
        flash.restore_power();
        let (_, recovered) = stored_value(flash, 1);
        assert!(recovered.as_deref().is_none_or(|bytes| bytes == old));
    }

    let with_old = save_small(erased.clone(), 1, &old);
    let overwrite_count = mutation_count_for_save(&with_old, 1, &new);
    for cut in 0..overwrite_count {
        let mut flash = with_old.clone();
        flash.arm_power_cut(cut);
        let mut map = small_map(flash);
        let mut work = [0_u8; SMALL_PAGE_BYTES];
        let value: &[u8] = &new;
        assert!(block_on(map.store_item(&mut work, &1, &value)).is_err());
        let mut flash = map.destroy().0;
        flash.restore_power();
        let (_, recovered) = stored_value(flash, 1);
        assert!(matches!(recovered.as_deref(), Some(bytes) if bytes == old || bytes == new));
    }

    // Copy is a source read followed by an ordinary destination save. A cut
    // may leave destination absent or complete, but must never damage source.
    let copy_count = mutation_count_for_save(&with_old, 2, &old);
    for cut in 0..copy_count {
        let mut flash = with_old.clone();
        flash.arm_power_cut(cut);
        let mut map = small_map(flash);
        let mut work = [0_u8; SMALL_PAGE_BYTES];
        let value: &[u8] = &old;
        assert!(block_on(map.store_item(&mut work, &2, &value)).is_err());
        let mut flash = map.destroy().0;
        flash.restore_power();
        let (flash, source) = stored_value(flash, 1);
        let (_, destination) = stored_value(flash, 2);
        assert_eq!(source.as_deref(), Some(old.as_slice()));
        assert!(destination.as_deref().is_none_or(|bytes| bytes == old));
    }

    let delete_count = mutation_count_for_delete(&with_old, 1);
    for cut in 0..delete_count {
        let mut flash = with_old.clone();
        flash.arm_power_cut(cut);
        let mut map = small_map(flash);
        let mut work = [0_u8; SMALL_PAGE_BYTES];
        assert!(block_on(map.remove_item(&mut work, &1)).is_err());
        let mut flash = map.destroy().0;
        flash.restore_power();
        let (_, recovered) = stored_value(flash, 1);
        assert!(recovered.as_deref().is_none_or(|bytes| bytes == old));
    }
}
