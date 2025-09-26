# MIDI-PLAY

A tiny MIDI player in Rust that uses:

* `midly` to parse Standard MIDI Files
* `fluidlite` to render GM SoundFont instruments to audio
* `cpal` to send audio to your default output device

This is a teaching example and a clean starting point for integrating MIDI playback into other programs.

## Why this structure

There are two loops running in parallel:

1. A conductor loop on a thread that schedules MIDI events by wall clock and sends them to the synth.
2. An audio callback on the CPAL thread that asks the synth to render the next chunk of PCM samples into the output buffer.

This separation keeps audio low latency and lets event scheduling remain simple.

## How timing works

MIDI files store delta times in ticks. The file header gives you pulses per quarter note (PPQ). Tempo Meta events give you microseconds per quarter note. The conversion is:

```rust
absolute_ticks += event.delta
seconds = absolute_ticks / PPQ * (us_per_quarter / 1_000_000)
microseconds = seconds * 1_000_000
```

We compute an absolute microsecond timestamp for every event across all tracks, merge, and sort. Tempo changes only affect conversion for later events on that track. Since all events are converted to absolute time, the conductor does not need to rescale when a tempo event is encountered.

## Audio path

* `Synth::sfload` loads a `.sf2` SoundFont and resets presets.
* `Synth::set_sample_rate` must match the output device sample rate.
* The CPAL callback calls `Synth::write(out)` where `out` is either `&mut [f32]` or `&mut [i16]`. FluidLite fills the buffer with the current mix.
* The conductor writes NoteOn, NoteOff, ProgramChange, and Control Change messages into the synth. The synth updates its internal state and the next `write` produces sound accordingly.

## Threading model

* `Synth` is wrapped in `Arc<Mutex<_>>` so both the conductor thread and the CPAL callback can access it safely.
* The conductor holds the lock only while sending short MIDI commands, then releases it.
* The audio callback locks only to invoke write. Keep work inside the lock very short.

## Building

```bash
# macOS tip: ensure clang is present for bindgen used by fluidlite
xcode-select --install

# If bindgen cannot find libclang:
brew install llvm
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"

cargo run --release -- path/to/song.mid path/to/YourGM.sf2
```

## Choosing a SoundFont

Any General MIDI .sf2 will work. Popular choices:

* FluidR3 GM
* Arachno SoundFont
* GeneralUser GS

## Extending

Good next steps:

* Sustain pedal: handle CC 64 in the conductor and forward to `synth.cc`.
* Pitch bend: forward `PitchBend { bend }` to `synth.pitch_bend`.
* Per-track channel mapping: MIDI files often assume channel programs. Preserve per-channel instruments and volumes.
* Looping: detect end-of-timeline and restart by resetting state with `system_reset` and re-scheduling.
* Accurate scheduling: replace the simple sleep(1 ms) with a small ring buffer of events and a tighter tick, or use a high precision timer.
* Volume and gain: expose a master gain and music on/off switch. 
* Error handling: synth calls return status. Log or handle failed program changes and unknown controllers.

