//! Compile-time sample catalog used by the firmware and host tests.

use crate::{SAMPLE_COUNT, SampleCatalog, WavError, WavPcm16};

macro_rules! sample_catalog {
    ($(($name:literal, $path:literal)),+ $(,)?) => {
        pub static SAMPLE_NAMES: [&str; SAMPLE_COUNT] = [$($name),+];
        pub const SAMPLE_PATHS: [&str; SAMPLE_COUNT] = [$($path),+];
        pub const SAMPLE_BYTES: [&[u8]; SAMPLE_COUNT] = [
            $(include_bytes!(concat!("../samples/", $path))),+
        ];

        pub fn parse_catalog() -> Result<SampleCatalog<'static>, WavError> {
            let samples = [
                $(WavPcm16::parse(include_bytes!(concat!("../samples/", $path)))?),+
            ];
            Ok(SampleCatalog::new(samples, &SAMPLE_NAMES))
        }
    };
}

sample_catalog![
    ("909 Kick", "kit0_909/00_909kick4.wav"),
    ("909 Snare", "kit0_909/01_909snare2.wav"),
    ("909 Hat Closed", "kit0_909/02_909hatclosed2a.wav"),
    ("909 Hat Open", "kit0_909/03_909hatopen5.wav"),
    ("909 Clap", "kit0_909/04_909clap1.wav"),
    ("909 Tom", "kit0_909/05_909tommed.wav"),
    ("909 Blip", "kit0_909/06_909blip.wav"),
    ("909 Cymbal", "kit0_909/07_909cym2.wav"),
    ("Tac Kick", "kit1_tac/00tictac_kick.wav"),
    ("Tac Snare", "kit1_tac/01tictac_snare.wav"),
    ("Tac Hat Closed", "kit1_tac/02tictac_hatc2.wav"),
    ("Tac Hat Open", "kit1_tac/03tictac_hato3.wav"),
    ("Tac Snare Roll", "kit1_tac/04tictac_snareroll.wav"),
    ("Tac Tom", "kit1_tac/05tictac_tomlight.wav"),
    ("Tac Ride Bell", "kit1_tac/06tictac_ridebell.wav"),
    ("Tac Cymbal", "kit1_tac/07tictac_cymbal1.wav"),
    ("AKU Kick", "kit2_aku/00_kick02.wav"),
    ("AKU Snare", "kit2_aku/01_sd02.wav"),
    ("AKU Hat 1", "kit2_aku/02_ho02.wav"),
    ("AKU Hat 2", "kit2_aku/03_ho02.wav"),
    ("AKU Clq", "kit2_aku/04_clq02.wav"),
    ("AKU Pcq 06", "kit2_aku/05_pcq06.wav"),
    ("AKU Pcq 10", "kit2_aku/06_pcq10.wav"),
    ("AKU Cymbal", "kit2_aku/07_cyq01.wav"),
];
