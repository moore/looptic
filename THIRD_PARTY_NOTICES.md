# Third-party notices

## MacroPadSynthPlug drum samples

The 24 WAV files below `samples/kit0_909`, `samples/kit1_tac`, and
`samples/kit2_aku` were copied without modification from the MacroPadSynthPlug
drum-machine example at commit
[`5119f13fe9fa0ca0923efeb2105068b6e2cc3db1`](https://github.com/todbot/macropadsynthplug/tree/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/drumkits).

MacroPadSynthPlug is distributed under
[CC0 1.0 Universal](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/LICENSE).
Its kit-specific provenance notes identify additional source context:

- [`kit0_909/readme.txt`](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/drumkits/kit0_909/readme.txt)
- [`kit1_tac/readme.txt`](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/drumkits/kit1_tac/readme.txt)
- [`kit2_aku/readme.txt`](https://github.com/todbot/macropadsynthplug/blob/5119f13fe9fa0ca0923efeb2105068b6e2cc3db1/circuitpython/drum_machine/drumkits/kit2_aku/readme.txt)

See [`samples/README.md`](samples/README.md) for the corresponding Freesound
pack links, format requirements, ordering, and compatibility-copy details.

## Raspberry Pi pico-extras

LoopTic's PWM audio encoder and PIO program are adapted from these files in
the Raspberry Pi `pico-extras` project:

- `src/rp2_common/pico_audio_pwm/audio_pwm.pio`
- `src/rp2_common/pico_audio_pwm/sample_encoding.cpp`
- `src/rp2_common/pico_audio_pwm/include/pico/audio_pwm/sample_encoding.h`

Upstream project: <https://github.com/raspberrypi/pico-extras>

The adapted portions are distributed under the following BSD 3-Clause
license:

> Copyright (c) 2020 Raspberry Pi (Trading) Ltd.
>
> Redistribution and use in source and binary forms, with or without
> modification, are permitted provided that the following conditions are met:
>
> 1. Redistributions of source code must retain the above copyright notice,
>    this list of conditions and the following disclaimer.
>
> 2. Redistributions in binary form must reproduce the above copyright
>    notice, this list of conditions and the following disclaimer in the
>    documentation and/or other materials provided with the distribution.
>
> 3. Neither the name of the copyright holder nor the names of its contributors
>    may be used to endorse or promote products derived from this software
>    without specific prior written permission.
>
> THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
> AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
> IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
> ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
> LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
> CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
> SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
> INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
> CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
> ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
> POSSIBILITY OF SUCH DAMAGE.
