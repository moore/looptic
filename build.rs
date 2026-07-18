//! Install the RP2040 memory map and linker scripts for firmware binaries.

use std::env;
use std::fs;
use std::path::PathBuf;

fn parse_number(value: &str) -> u32 {
    let value = value.trim().replace('_', "");
    if let Some(hex) = value.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).expect("valid hexadecimal memory.x value")
    } else if let Some(kib) = value.strip_suffix('K') {
        kib.parse::<u32>().expect("valid KiB memory.x value") * 1024
    } else if let Some(mib) = value.strip_suffix('M') {
        mib.parse::<u32>().expect("valid MiB memory.x value") * 1024 * 1024
    } else {
        value.parse().expect("valid decimal memory.x value")
    }
}

fn memory_region(memory: &str, name: &str) -> (u32, u32) {
    let prefix = format!("{name} :");
    let line = memory
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("memory.x must define {name}"));
    let (_, fields) = line.split_once(':').expect("memory region separator");
    let mut origin = None;
    let mut length = None;
    for field in fields.split(',') {
        let (key, value) = field
            .split_once('=')
            .expect("memory region field must contain '='");
        match key.trim() {
            "ORIGIN" => origin = Some(parse_number(value)),
            "LENGTH" => length = Some(parse_number(value)),
            _ => {}
        }
    }
    (
        origin.expect("memory region ORIGIN"),
        length.expect("memory region LENGTH"),
    )
}

fn main() {
    println!("cargo:rerun-if-changed=memory.x");

    // `memory.x` is the single source for physical addresses. Parse and check
    // it even for host builds, then generate the constants consumed by the
    // flash driver so a linker/storage boundary cannot silently diverge.
    let memory = include_str!("memory.x");
    let (boot2_origin, boot2_bytes) = memory_region(memory, "BOOT2");
    let (firmware_origin, firmware_bytes) = memory_region(memory, "FLASH");
    let (storage_origin, storage_bytes) = memory_region(memory, "SONG_STORAGE");

    assert_eq!(boot2_origin, 0x1000_0000, "unexpected XIP base");
    assert_eq!(boot2_bytes, 0x100, "unexpected boot2 size");
    assert_eq!(firmware_origin, 0x1000_0100, "unexpected firmware origin");
    assert_eq!(firmware_bytes, 0x005f_ff00, "unexpected firmware size");
    assert_eq!(storage_origin, 0x1060_0000, "unexpected storage origin");
    assert_eq!(storage_bytes, 0x0020_0000, "unexpected storage size");
    assert_eq!(boot2_origin + boot2_bytes, firmware_origin);
    assert_eq!(firmware_origin + firmware_bytes, storage_origin);
    assert_eq!(storage_origin + storage_bytes, 0x1080_0000);

    let total_flash_bytes = storage_origin + storage_bytes - boot2_origin;
    let storage_offset = storage_origin - boot2_origin;
    let generated_layout = format!(
        "// Generated from memory.x by build.rs; do not edit.\n\
         pub const FLASH_XIP_BASE: u32 = {boot2_origin:#010x};\n\
         pub const TOTAL_FLASH_BYTES: u32 = {total_flash_bytes:#010x};\n\
         pub const FIRMWARE_END_OFFSET: u32 = {storage_offset:#010x};\n\
         pub const SONG_STORAGE_OFFSET: u32 = {storage_offset:#010x};\n\
         pub const SONG_STORAGE_BYTES: u32 = {storage_bytes:#010x};\n"
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    fs::write(out_dir.join("flash_layout.rs"), generated_layout)
        .expect("write generated flash layout");

    // Host-side library tests do not need an embedded linker script.
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("arm") {
        return;
    }

    fs::write(out_dir.join("memory.x"), include_bytes!("memory.x"))
        .expect("copy memory.x into OUT_DIR");

    println!("cargo:rustc-link-search={}", out_dir.display());
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
}
