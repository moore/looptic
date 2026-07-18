MEMORY {
    /* Embassy supplies the RP2040 second-stage bootloader in this region. */
    BOOT2 : ORIGIN = 0x10000000, LENGTH = 0x100

    /*
     * Adafruit MacroPad RP2040: 8 MiB external W25Q64 flash.
     *
     * Firmware is deliberately limited to the first 6 MiB. The final 2 MiB
     * is a persistent song partition and must never contain a loadable ELF
     * section. UF2/normal sector flashing can therefore update firmware
     * without touching saved songs.
     */
    FLASH : ORIGIN = 0x10000100, LENGTH = 0x005FFF00
    SONG_STORAGE : ORIGIN = 0x10600000, LENGTH = 0x00200000

    /* All six RP2040 SRAM banks, including the two 4 KiB scratch banks. */
    RAM : ORIGIN = 0x20000000, LENGTH = 264K
}

/* Exported for map-file inspection and future low-level storage diagnostics. */
__song_storage_start = ORIGIN(SONG_STORAGE);
__song_storage_end = ORIGIN(SONG_STORAGE) + LENGTH(SONG_STORAGE);

ASSERT(ORIGIN(BOOT2) + LENGTH(BOOT2) == ORIGIN(FLASH),
       "BOOT2 and firmware FLASH must be contiguous");
ASSERT(ORIGIN(FLASH) + LENGTH(FLASH) == ORIGIN(SONG_STORAGE),
       "firmware FLASH must end at the song-storage boundary");
ASSERT(ORIGIN(FLASH) == 0x10000100 && LENGTH(FLASH) == 0x005FFF00,
       "firmware partition must remain exactly 6 MiB including boot2");
ASSERT(ORIGIN(SONG_STORAGE) == 0x10600000 && LENGTH(SONG_STORAGE) == 0x00200000,
       "song-storage partition must remain the final 2 MiB");
ASSERT(ORIGIN(SONG_STORAGE) + LENGTH(SONG_STORAGE) == 0x10800000,
       "song storage must end at the end of the 8 MiB device");
