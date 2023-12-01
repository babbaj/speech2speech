mod audio;

use std::ffi::{c_int, c_uchar};
use input::{Event, Libinput, LibinputInterface};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::{fs::OpenOptionsExt, io::OwnedFd};
use std::path::Path;
use std::thread;
use std::ops::{DerefMut};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;
use clap::{Arg};
use input::event::keyboard::{KeyboardEventTrait, KeyState};
use nix::poll::{poll, PollFlags, PollFd};

use evdev::Key;

extern crate libc;
use libc::{c_short, O_RDONLY, O_RDWR, O_WRONLY};

use futures_util::{StreamExt};
use once_cell::sync::{Lazy};
use tokio::runtime::Runtime;

use reqwest::multipart;
use serde_json::json;

use lame_sys::*;
use minimp3_sys::*;

use audio::*;

struct Interface;

// pasted example code
impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_RDONLY != 0) | (flags & O_RDWR != 0))
            .write((flags & O_WRONLY != 0) | (flags & O_RDWR != 0))
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

enum State {
    Idle(Option<Arc<Vec<u8>>>),
    Recording(pipewire::channel::Sender<Terminate>),
    Generating,
    Playing
}

fn encode_mp3(pcm: &mut [i16]) -> Vec<u8> {
    // worst case estimate according to lame.h
    let estimate_size = (pcm.len() as f32 * 1.25 + 7200.0) as usize;
    let mut out = Vec::<u8>::with_capacity(estimate_size);
    unsafe {
        out.set_len(estimate_size);

        let ctx = lame_init();
        lame_set_in_samplerate(ctx, SAMPLE_RATE as c_int);
        lame_set_out_samplerate(ctx, SAMPLE_RATE as c_int);
        lame_set_VBR(ctx, vbr_default);
        if lame_init_params(ctx) < 0 {
            panic!("Failed to init LAME parameters");
        }

        let mut mp3_bytes: c_int;
        let mut bytes_written = 0usize;

        mp3_bytes = lame_encode_buffer_interleaved(ctx,
                                                   pcm.as_mut_ptr() as *mut c_short,
                                                   (pcm.len() / CHANNELS as usize) as c_int,
                                                   out.as_mut_ptr() as *mut c_uchar,
                                                   out.len() as c_int
        );
        if mp3_bytes < 0 {
            panic!("lame encoding failed {mp3_bytes}");
        }
        bytes_written += mp3_bytes as usize;

        let remaining = &mut out[mp3_bytes as usize..];
        mp3_bytes = lame_encode_flush(ctx, remaining.as_mut_ptr() as *mut c_uchar, remaining.len() as c_int);
        if mp3_bytes < 0 {
            panic!("lame_encode_flush failed {mp3_bytes}");
        }
        bytes_written += mp3_bytes as usize;
        out.set_len(bytes_written);
        lame_close(ctx);
        return out;
    }
}

fn mono_to_stereo(pcm: &[i16]) -> Vec<i16> {
    let mut out = Vec::<i16>::with_capacity(pcm.len() * 2);
    unsafe { out.set_len(out.capacity()) };
    for (i, s) in pcm.iter().enumerate() {
        out[i * 2] = *s;
        out[i * 2 + 1] = *s;
    }
    return out;
}

fn mp3_decode_thread(data: Arc<Mutex<Vec<u8>>>, rx: Receiver<bool>, pcm_out: Sender<Vec<i16>>) {
    unsafe {
        let mut mp3dec = std::mem::zeroed();
        mp3dec_init(&mut mp3dec);
        //let mut debug = std::fs::File::create("reee").unwrap();
        let mut offset = 0usize;
        for finished in rx.iter() {
            let buf = data.lock().unwrap();

            let mut pcm = [0i16; MINIMP3_MAX_SAMPLES_PER_FRAME as usize];
            while buf.len() - offset >= (MINIMP3_MAX_SAMPLES_PER_FRAME * 8) as usize || finished {
                let slice = &buf[offset..];
                let mut info: mp3dec_frame_info_t = std::mem::zeroed();
                let samples = mp3dec_decode_frame(&mut mp3dec, slice.as_ptr(), slice.len() as _, pcm.as_mut_ptr(), &mut info);

                if samples > 0 {
                    let stereo = if info.channels == 1 {
                        mono_to_stereo(&pcm[..samples as usize])
                    } else if info.channels == 2 {
                        pcm[..(samples * 2) as usize].to_vec()
                    } else {
                        panic!("too many channels");
                    };
                    //debug.write_all(slice::from_raw_parts(stereo.as_ptr() as *const _, stereo.len() * 2)).unwrap();
                    pcm_out.send(stereo).unwrap();
                }
                if finished && offset >= buf.len() {
                    break
                }
                offset += info.frame_bytes as usize;
            }
        }
    }
}


async fn api(voice_id: &str, api_key: &str, mp3: Vec<u8>, out: Sender<Vec<i16>>, state: &Mutex<State>) -> Result<Vec<u8>, reqwest::Error> {
    let client = reqwest::Client::new();
    let url = format!("https://api.elevenlabs.io/v1/speech-to-speech/{voice_id}/stream?optimize_streaming_latency=4");

    let settings = serde_json::to_string(&json!({
        "similarity_boost": 0.5,
        "stability": 0.75,
        "style": 0,
        "use_speaker_boost": false
    })).unwrap();
    let form = multipart::Form::new()
        .part("audio", multipart::Part::bytes(mp3).file_name("file.mp3"))
        .part("model_id", multipart::Part::text("eleven_english_sts_v2"))
        .part("voice_settings", multipart::Part::text(settings));

    let request = client.post(url)
        //.header("accept", "audio/mpeg")
        .header("accept", "*/*")
        .header("xi-api-key", api_key)
        .header("Authorization", std::fs::read_to_string("./auth").unwrap().trim())
        .multipart(form);

    let start = Instant::now();
    let response = request.send().await?;
    if response.status().is_success() {
        println!("api took {}ms", start.elapsed().as_millis());
        *state.lock().unwrap() = State::Playing;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (decode_tx, decode_rx) = channel();
        let buffer_copy = buffer.clone();
        thread::spawn(move || {
            mp3_decode_thread(buffer_copy, decode_rx, out);
        });

        let mut copy = Vec::<u8>::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let data = &chunk?[..];
            buffer.lock().unwrap().extend_from_slice(data);
            decode_tx.send(false).unwrap();
            copy.extend_from_slice(data);
        }
        decode_tx.send(true).unwrap();
        return Ok(copy);
    } else {
        let body = response.text().await?;
        panic!("API failed {body}");
    }
}

static RT: Lazy<Runtime> = Lazy::new(||
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build().unwrap()
);

fn main() {
    let matches = clap::Command::new("autospeech2speech")
        .arg(Arg::new("voice-id").long("voice-id").value_name("VOICE_ID").help("The elevenlabs voice id").required(true))
        .arg(Arg::new("api-key").long("api-key").value_name("API_KEY").help("The elevenlabs xi-api-key").required(true))
        .arg(Arg::new("source").long("source").value_name("SOURCE").help("The pipewire source").required(true))
        .arg(Arg::new("sink").long("sink").value_name("SINK").help("The pipewire sink").required(true))
        .get_matches();

    let voice = matches.get_one::<String>("voice-id").unwrap();
    let key = matches.get_one::<String>("api-key").unwrap();
    let source = matches.get_one::<String>("source").unwrap().clone();
    let sink = matches.get_one::<String>("sink").unwrap().clone();

    let state = Arc::new(Mutex::new(State::Idle(None)));

    pipewire::init();
    let (tx, rx) = channel();
    let (pw_sender, pw_receiver) = pipewire::channel::channel();
    ctrlc::set_handler(move || {
        pw_sender.send(Terminate).unwrap();
    }).unwrap();
    let _pw_thread = thread::spawn(move || pw_playback_thread(&sink, rx, pw_receiver).unwrap());
    let audio_sender = tx;

    let mut input = Libinput::new_with_udev(Interface);
    input.udev_assign_seat("seat0").unwrap();
    let fd = unsafe { BorrowedFd::borrow_raw(input.as_raw_fd()) };
    let pollfd = PollFd::new(&fd, PollFlags::POLLIN);
    while poll(&mut [pollfd], -1).is_ok() {
        input.dispatch().unwrap();
        for event in &mut input {
            if let Event::Keyboard(kb_event) = event {
                let key_state = kb_event.key_state();
                let event_key = Key::new(kb_event.key() as u16);
                if event_key == Key::KEY_RIGHTCTRL && key_state == KeyState::Pressed {
                    if let State::Idle(Some(mp3)) = state.lock().unwrap().deref_mut() {
                        let mp3_copy: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(mp3.as_ref().clone()));
                        let output_copy = audio_sender.clone();
                        let (tx, rx) = channel();
                        tx.send(true).unwrap();
                        thread::spawn(move || {
                            mp3_decode_thread(mp3_copy, rx, output_copy);
                        });
                    }
                }
                if event_key == Key::KEY_RIGHTSHIFT {
                    if key_state == KeyState::Pressed && matches!(*state.lock().unwrap(), State::Idle(_)) {
                        let (mic_send, mic_receive) = channel();
                        let (pw_sender, pw_receiver) = pipewire::channel::channel();
                        let source = source.clone();
                        thread::spawn(move || pw_mic_thread(&source, mic_send, pw_receiver));

                        let output_copy = audio_sender.clone();
                        let state_copy = state.clone();
                        let voice = voice.clone();
                        let key = key.clone();
                        *state.lock().unwrap() = State::Recording(pw_sender);
                        thread::spawn(move || {
                            let mut pcm = Vec::<i16>::new();
                            for v in mic_receive.iter() {
                                pcm.extend_from_slice(&v);
                            }
                            let start = Instant::now();
                            let mp3 = encode_mp3(pcm.as_mut_slice());
                            println!("encode took {}ms", start.elapsed().as_millis());
                            *state_copy.lock().unwrap() = State::Generating;
                            let last_copy = RT.block_on(async {
                                return api(&voice, &key, mp3,output_copy, &state_copy).await.unwrap();
                            });
                            *state_copy.lock().unwrap() = State::Idle(Some(Arc::new(last_copy)));
                        });
                    }
                    if key_state == KeyState::Released {
                        if let State::Recording(sender) = state.lock().unwrap().deref_mut() {
                            let _ = sender.send(Terminate);
                        }
                    }
                }
            }
        }
    }
}
