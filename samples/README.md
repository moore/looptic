# Drum sample provenance

The three kit directories contain 24 WAV files copied without modification
from the MacroPadSynthPlug drum-machine example at commit
[`5119f13fe9fa0ca0923efeb2105068b6e2cc3db1`](https://github.com/todbot/macropadsynthplug/tree/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/drumkits).
The commit is pinned so the imported bytes and their provenance notes can be
reproduced even if the upstream project changes.

## Kits and source notes

| Directory | Upstream provenance note | Source context recorded upstream |
| --- | --- | --- |
| `kit0_909` | [`readme.txt`](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/drumkits/kit0_909/readme.txt) | Some sounds came from the [GRD-music 909 drums pack](https://freesound.org/people/GRD-music-/packs/23395/); the note also mentions the Amen break. |
| `kit1_tac` | [`readme.txt`](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/drumkits/kit1_tac/readme.txt) | Samples came mostly from the [TicTacShutUp pack](https://freesound.org/people/TicTacShutUp/packs/17/). |
| `kit2_aku` | [`readme.txt`](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/drumkits/kit2_aku/readme.txt) | Samples came from the [AKUSTIKA pack](https://freesound.org/people/AKUSTIKA/packs/28423/). |

The pinned MacroPadSynthPlug repository distributes its work under
[CC0 1.0 Universal](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/LICENSE).
The upstream per-kit notes above are retained as additional source context even
though CC0 does not require attribution.

## Format and ordering

Every imported file is a little-endian RIFF/WAVE containing mono, signed
16-bit PCM at 22,050 Hz. The upstream conversion guidance is equivalent to:

```console
sox input-audio -b 16 -c 1 -r 22050 output.wav
```

Each kit contains exactly eight WAV files. Files are selected in ASCII
lexicographic filename order; their `00` through `07` prefixes preserve the
[original drum-machine slot order](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/code.py#L208-L221):

| Prefix | Intended slot |
| ---: | --- |
| `00` | Kick |
| `01` | Snare |
| `02` | Closed hi-hat |
| `03` | Open hi-hat |
| `04` | Clap or kit-specific variation |
| `05` | Tom or percussion |
| `06` | Ride or kit-specific variation |
| `07` | Crash/cymbal |

Filenames and bytes are preserved verbatim, including kit-specific naming and
sound choices. In particular, the two `kit2_aku` hi-hat filenames are similar
but contain different audio.

The flat `00_kick02.wav` and `02_ho02.wav` files predate this full-kit import
and remain in place for firmware compatibility. They are byte-identical to
`kit2_aku/00_kick02.wav` and `kit2_aku/02_ho02.wav`, respectively, and are not
part of the 24-file imported-kit count.
