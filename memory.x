MEMORY {
    /* Embassy supplies the RP2040 second-stage bootloader in this region. */
    BOOT2 : ORIGIN = 0x10000000, LENGTH = 0x100

    /* Adafruit MacroPad RP2040: 8 MiB external W25Q64 flash. */
    FLASH : ORIGIN = 0x10000100, LENGTH = 8192K - 0x100

    /* All six RP2040 SRAM banks, including the two 4 KiB scratch banks. */
    RAM : ORIGIN = 0x20000000, LENGTH = 264K
}
