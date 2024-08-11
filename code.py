"""
Loop-tick synth
"""
from rainbowio import colorwheel
from adafruit_macropad import MacroPad
import audiocore, audiomixer, audiopwmio
import time, os
import board, digitalio


use_macrosynthplug = True  # False to use built-in speaker of MacroPad RP2040
num_pads = 12  # If we decide to use some of the keys for other things this can be adjusted
waves = [None] * num_pads
pads_lit = [0] * num_pads  # list of drum keys that are being played
pads_mute = [0] * num_pads # which pads are muted


# Load wave objects upfront in attempt to reduce play latency
def load_samples():
    for (i, fname) in [(0, "00_kick02.wav"), (1, "02_ho02.wav")]:
        waves[i] = audiocore.WaveFile(open("samples/" + fname,"rb"))  #

# play a drum sample, either by sequencer or pressing pads
def play_drum(num, pressed):
    pads_lit[num] = pressed
    voice = mixer.voice[num]   # get mixer voice
    if pressed and not pads_mute[num]:
        voice.play(waves[num],loop=False)
    else: # released
        pass   # not doing this for samples


# macropadsynthplug midi!
#midi_uart = busio.UART(rx=board.SCL, tx=None, baudrate=31250, timeout=0.001)
#midi_uart_in = smolmidi.MidiIn(midi_uart) # can't do smolmidi because it wants port.readinto(buf,len)
#midi_usb_in = smolmidi.MidiIn(usb_midi.ports[0]) # can't do smolmidi because it wants port.readinto(buf,len)


macropad = MacroPad()


if use_macrosynthplug:
    audio = audiopwmio.PWMAudioOut(board.SDA) # macropadsynthplug!
else:
    audio = audiopwmio.PWMAudioOut(board.SPEAKER) # built-in tiny spkr
    speaker_en = digitalio.DigitalInOut(board.SPEAKER_ENABLE)
    speaker_en.switch_to_output(value=True)

mixer = audiomixer.Mixer(voice_count=num_pads, sample_rate=22050, channel_count=1,
                         bits_per_sample=16, samples_signed=True, buffer_size=4096)
audio.play(mixer) # attach mixer to audio playback

load_samples()

text_lines = macropad.display_text(title="LoopTic")
text_lines.show()

class Tick:
    active = False # this should control if tick is played or not
    beat = 0
    next = 0 # this should start at 0

tones = [196, 220, 246, 262, 294, 330, 349, 392, 440, 494, 523, 587]
beats = [Tick() for i in range(num_pads)]

last_encoder = 0
current_beat = None
beat_offset = 0
tone_off = 0

while True:
    key_event = macropad.keys.events.get()
    t = int(time.monotonic_ns() / (10**6))

    for i in range(len(beats)):
        key = beats[i]
        if key.next <= t:
            key.next += 1000 - key.beat

            if key.active:
                macropad.pixels[i] = colorwheel(
                    int(255 / 12) * i
                )

                drum = 0
                if i >= 6:
                    drum = 1 

                play_drum(drum, True)

                if False: #if tone_off < t:
                    macropad.start_tone(tones[i])
                    tone_off = t + 100
                    text_lines[0].text = "Tone off {}".format(tone_off)


    encoder_value = macropad.encoder

    if encoder_value != last_encoder:
        last_encoder = encoder_value
        text_lines[1].text = "Encoder: {}".format(encoder_value)
        if current_beat is not None:
            delta = encoder_value - beat_offset
            current_beat.beat += delta
            current_beat.next += delta

            if current_beat.beat > 0:
                current_beat.active = True
            else:
                current_beat.active = False
                current_beat.beat = 0


            beat_offset = encoder_value
            text_lines[3].text = "Beat {}".format(current_beat.beat)


    if key_event:
        if key_event.pressed:
            beat_offset = encoder_value
            key_number = key_event.key_number
            current_beat = beats[key_number]
            text_lines[2].text = "Time {}".format(t)
            text_lines[3].text = "Beat {}".format(current_beat.beat)

        else:
            current_beat = None

    text_lines.show()

    if t >= tone_off:
        macropad.stop_tone()
        macropad.pixels.fill((0, 0, 0))
