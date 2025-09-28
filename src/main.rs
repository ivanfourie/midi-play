use anyhow::{Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fluidlite::{Settings, Synth};
use midly::{MetaMessage, Smf, TrackEventKind};
use std::{
    fs, sync::{Arc, Mutex}, thread, time::{Duration, Instant}
};

/// CLI options:
/// - midi: path to a Standard MIDI file
/// - soundfont: path to a GM .sf2 SoundFont
#[derive(Parser, Debug)]
struct Opt {
    /// Path to .mid file
    midi: String,
    /// Path to GM SoundFont (.sf2)
    soundfont: String,
}

fn main() -> Result<()> {
    let opt = Opt::parse();

    println!("Playing MIDI file: {}", opt.midi);
    println!("Using SoundFont: {}", opt.soundfont);

    // 1) Read and parse the MIDI file into an in-memory SMF structure.
    let bytes = fs::read(&opt.midi).with_context(|| "reading MIDI file")?;
    let smf = Smf::parse(&bytes).with_context(|| "parsing MIDI")?;

    // 2) Timing setup.
    // PPQ = pulses (ticks) per quarter note. We need this to convert MIDI delta ticks to time.
    let ppq = match smf.header.timing {
        midly::Timing::Metrical(t) => t.as_int() as f64,
        _ => 480.0, // fallback if file uses SMPTE timing
    };
    println!("PPQ (ticks per quarter note): {}", ppq);

    // Default tempo if the file does not set one: 120 BPM = 500_000 microseconds per quarter note.
    let mut default_us_per_qn: f64 = 500_000.0;
    // Scan tracks for the first Tempo meta event to seed the initial tempo.
    'scan: for tr in &smf.tracks {
        for ev in tr {
            if let TrackEventKind::Meta(midly::MetaMessage::Tempo(tp)) = ev.kind {
                default_us_per_qn = tp.as_int() as f64;
                break 'scan;
            }
        }
    }
    println!("Initial tempo: {} µs per quarter note (~{:.1} BPM)", 
         default_us_per_qn, 60_000_000.0 / default_us_per_qn);

    // 3) Build a single timeline of timestamped events.
    // We convert each track’s delta ticks to absolute time in microseconds, then merge.
    #[derive(Clone, Copy)]
    /// Represents a MIDI message extracted from the timeline.
    ///
    /// Each variant corresponds to a MIDI event type.
    /// Fields follow the MIDI message structure:
    /// - First parameter is usually the channel (0–15)
    /// - Subsequent parameters depend on the event type
    enum Msg {
        /// Note On: Start playing a note.
        /// - channel: 0–15
        /// - key: MIDI note number (0–127)
        /// - velocity: 0–127
        NoteOn(u8, u8, u8),

        /// Note Off: Stop playing a note.
        /// - channel: 0–15
        /// - key: MIDI note number (0–127)
        /// - velocity: release velocity (0–127, often unused)
        NoteOff(u8, u8, u8),

        /// Program Change: Change the program (also known as instrument) for a channel.
        /// - channel: 0–15
        /// - program: instrument/patch number (0–127)
        Program(u8, u8),

        /// Control: Modify the value of a MIDI controller.
        /// - channel: 0–15
        /// - controller: controller number (0–127)
        /// - value: controller value (0–127)
        Control(u8, u8, u8),

        /// Pitch Bend: Set the pitch bend value for the entire channel.
        /// - channel: 0–15
        /// - bend value: 14-bit signed value, 0–16383
        ///   - center (no bend) = 8192
        ///   - <8192 = bend down, >8192 = bend up
        PitchBend(u8, u16),

        /// Aftertouch (Polyphonic): Modify the velocity of a note after it has been played.
        /// - channel: 0–15
        /// - key: MIDI note number (0–127)
        /// - velocity: 0–127, The velocity of the key
        AfterTouch(u8, u8, u8),

        /// ChannelAftertouch: Change the note velocity of a whole channel at once, without starting new notes.
        /// - channel: 0–15
        /// - pressure: 0–127
        ChannelAftertouch(u8, u8),

        /// Tempo change: (microseconds per quarter note)
        /// - value is in µs per quarter note (not BPM)
        /// - To convert to BPM: bpm = 60_000_000 / value
        #[allow(dead_code)]
        Tempo(f64),
    }
    
    #[derive(Clone, Copy)]
    struct Timed {
        t_us: u64, // absolute time in microseconds since start
        msg: Msg,
    }

    let mut timeline: Vec<Timed> = Vec::new();

    // Walk every track and accumulate absolute tick count.
    // Convert ticks to time using the current tempo, which can change mid track.
    for tr in &smf.tracks {
        let mut abs_ticks: u64 = 0;
        let mut us_per_qn = default_us_per_qn;

        for ev in tr {
            abs_ticks += ev.delta.as_int() as u64;

            // ticks -> seconds -> microseconds, using the current tempo
            let t_sec = (abs_ticks as f64) / ppq * (us_per_qn / 1_000_000.0);
            let t_us = (t_sec * 1_000_000.0) as u64;

            match ev.kind {
                // Metadata
                TrackEventKind::Meta(m) => {
                    match m {
                        // Tempo changes affect future events in this track.
                        MetaMessage::Tempo(tp) => {
                            us_per_qn = tp.as_int() as f64;
                            timeline.push(Timed { t_us, msg: Msg::Tempo(us_per_qn) });
                            println!("Tempo change at {} µs: {:.1} BPM", t_us, 60_000_000.0 / us_per_qn);
                        }
                        MetaMessage::TimeSignature(numer, denom, _, _) => {
                            println!("Time signature: {}/{}", numer, 1 << denom);
                        }
                        MetaMessage::KeySignature(key, scale) => {
                            println!("Key signature: {:?} ({})", key, if !scale { "major" } else { "minor" });
                        }
                        MetaMessage::TrackName(name) => {
                            if let Ok(s) = std::str::from_utf8(name) {
                                println!("Track name: {}", s);
                            }
                        }
                        _ => {}
                    }
                }
                // MIDI messages
                TrackEventKind::Midi { channel, message } => {
                    let ch = u8::from(channel);
                    use midly::MidiMessage::*;
                    match message {
                        NoteOn { key, vel } if vel.as_int() == 0 => {
                            // normalize to NoteOff to avoid any synth-specific ambiguity
                            timeline.push(Timed { t_us, msg: Msg::NoteOff(ch, key.as_int(), 0) });
                        }
                        NoteOn { key, vel } => {
                            timeline.push(Timed { t_us, msg: Msg::NoteOn(ch, key.as_int(), vel.as_int()) });
                        }
                        NoteOff { key, vel } => {
                            timeline.push(Timed { t_us, msg: Msg::NoteOff(ch, key.as_int(), vel.as_int()) });
                        }
                        ProgramChange { program } => {
                            timeline.push(Timed { t_us, msg: Msg::Program(ch, program.as_int()) });
                        }
                        Controller { controller, value } => {
                            timeline.push(Timed { t_us, msg: Msg::Control(ch, controller.as_int(), value.as_int()) });
                        }
                        PitchBend { bend } => {
                            let raw = bend.0.as_int(); 
                            timeline.push(Timed { t_us, msg: Msg::PitchBend(ch, raw) });
                        }
                        Aftertouch { key, vel } => {
                            timeline.push(Timed { t_us, msg: Msg::AfterTouch(ch, key.as_int(), vel.as_int()) }); 
                        }
                        ChannelAftertouch { vel } => {
                            timeline.push(Timed { t_us, msg: Msg::ChannelAftertouch(ch, vel.as_int()) });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Merge and order events from all tracks by absolute time.
    timeline.sort_by_key(|e| e.t_us);
    let last_t_us = timeline.last().map(|e| e.t_us).unwrap_or(0);

    println!("Total events parsed: {}", timeline.len());
    println!("Estimated track length: {}", format_duration(last_t_us));

    // 4) Create a FluidLite synth, load the SoundFont, and share it across threads.
    let settings = Settings::new()?;

    let fl = Synth::new(settings)?;
    fl.sfload(&opt.soundfont, true).context("loading soundfont")?;

    let id = fl.sfload(&opt.soundfont, true).context("loading soundfont")?;
    println!("Loaded SoundFont: {} (id={})", opt.soundfont, id);
    
    // Master gain
    fl.set_gain(0.7);

    // Reverb
    fl.set_reverb_on(true);
    fl.set_reverb_params(0.7, 0.2, 0.9, 0.5); // roomsize, damp, width, level

    // Chorus
    fl.set_chorus_on(true);
    fl.set_chorus_params(3, 1.2, 0.25, 8.0, Default::default()); // the default should be Sine
    
    let synth = Arc::new(Mutex::new(fl));
    
    // 5) Set up audio output with CPAL and let FluidLite fill the audio buffers.
    let host = cpal::default_host();
    let dev = host.default_output_device().context("no default output device")?;
    let cfg = dev.default_output_config().context("default_output_config")?;

    // Tell FluidLite the audio device sample rate so it renders at the correct rate.
    let sample_rate = cfg.sample_rate().0 as f32;
    {
        let s = synth.lock().unwrap();
        s.set_sample_rate(sample_rate);

        // clean start
        for ch in 0..16u32 {
            let _ = s.pitch_bend(ch, 8192); // center
            let _ = s.cc(ch, 121, 0);       // Reset All Controllers
            let _ = s.cc(ch, 120, 0);       // All Sound Off (optional)
        }
    }
    let fmt = cfg.sample_format();
    let stream_cfg = cfg.config();

    println!("Sample rate set to {}", sample_rate);

    // 6) Start a simple "conductor" thread.
    // It schedules MIDI events in wall-clock time and sends them to the synth.
    // The CPAL audio callback runs in parallel and pulls audio from the synth.
    let synth_for_midi = synth.clone();
    let timeline_for_midi = timeline.clone();
    thread::spawn(move || {
        let start = Instant::now();
        let mut i = 0usize;

        while i < timeline_for_midi.len() {
            let now_us = start.elapsed().as_micros() as u64;

            // Dispatch all events that are due at this moment
            while i < timeline_for_midi.len() && timeline_for_midi[i].t_us <= now_us {
                let s = synth_for_midi.lock().unwrap();
                match timeline_for_midi[i].msg {
                    Msg::NoteOn(ch, key, vel) => {
                        let _ = s.note_on(ch as u32, key as u32, vel as u32);
                    }
                    Msg::NoteOff(ch, key, _vel) => {
                        let _ = s.note_off(ch as u32, key as u32);
                    }
                    Msg::Program(ch, prog) => {
                        let _ = s.program_change(ch as u32, prog as u32);
                    }
                    Msg::Control(ch, cc, val) => {
                        let _ = s.cc(ch as u32, cc as u32, val as u32);
                    }
                    Msg::PitchBend(ch, bend) => {
                        if bend > 16383 {
                            eprintln!("Dropping out-of-range raw bend {}", bend);
                        } else {
                            let _ = s.pitch_bend(ch as u32, bend as u32);
                        }
                    }
                    Msg::AfterTouch(ch, key, vel) => {
                        let _ = s.key_pressure(ch as u32, key as u32, vel as u32);
                    }
                    Msg::ChannelAftertouch(ch, vel) => {
                        let _ = s.channel_pressure(ch as u32, vel as u32);
                    }
                    Msg::Tempo(_) => {
                        // Timeline already has absolute times, so no rescale is needed here.
                    }
                }
                i += 1;
            }

            // Short sleep to avoid busy waiting. This is a simple scheduler.
            thread::sleep(Duration::from_millis(1));
        }

        // After the last event, let tails ring out
        thread::sleep(Duration::from_secs(2));
    });

    // 7) Build the CPAL output stream. We support f32 or i16, call the matching Synth::write.
    let err_fn = |e| eprintln!("stream error: {e}");
    let stream = match fmt {
        cpal::SampleFormat::I16 => {
            dev.build_output_stream(
                &stream_cfg,
                {
                    let synth = synth.clone();
                    move |out: &mut [i16], _| {
                        if let Err(e) = synth.lock().unwrap().write(out) {
                            eprintln!("fluid write i16: {e}");
                        }
                    }
                },
                err_fn,
                None,
            )?
        }
        _ => {
            // Default to f32. This is the common format on macOS.
            dev.build_output_stream(
                &stream_cfg,
                {
                    let synth = synth.clone();
                    move |out: &mut [f32], _| {
                        if let Err(e) = synth.lock().unwrap().write(out) {
                            eprintln!("fluid write f32: {e}");
                        }
                    }
                },
                err_fn,
                None,
            )?
        }
    };

    // Start audio
    stream.play()?;

    // Keep main alive until the song finishes plus a short tail
    let secs = (last_t_us as f64) / 1_000_000.0 + 3.0;
    thread::sleep(Duration::from_secs_f64(secs));
    Ok(())
}

fn format_duration(us: u64) -> String {
    let total_secs = us / 1_000_000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}