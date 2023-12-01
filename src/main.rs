mod audio;

use std::ffi::{c_int, c_uchar, OsStr};
use input::{Event, Libinput, LibinputInterface};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::{fs::OpenOptionsExt, io::OwnedFd};
use std::path::Path;
use std::{slice, thread};
use std::cmp::min;
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender};
use std::time::Instant;
use clap::{Arg};
use input::event::keyboard::{KeyboardEventTrait, KeyState};
use nix::poll::{poll, PollFlags, PollFd};

use evdev::Key;

extern crate libc;
use libc::{c_char, c_short, O_RDONLY, O_RDWR, O_WRONLY};
use subprocess::{Exec, Popen, Redirection};

use futures_util::StreamExt;
use once_cell::sync::{Lazy};
use tokio::runtime::Runtime;

use reqwest::multipart;
use serde_json::json;

//use rusty_ffmpeg::ffi;
use lame_sys::*;

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

fn decode_mp3(mp3: &[u8]) -> Vec<u8> {
    let ffmpeg_cmd = OsStr::new("ffmpeg -hide_banner -loglevel error -f mp3 -i - -f s16le -ar 48000 -ac 2 -");
    let mut proc = Exec::shell(ffmpeg_cmd)
        .stdin(Redirection::Pipe)
        .stdout(Redirection::Pipe)
        .popen().unwrap();
    let mut stdin = std::mem::take(&mut proc.stdin).unwrap();
    return thread::scope(|s| {
        s.spawn(move || {
            stdin.write_all(mp3).unwrap();
        });
        return read_stdout_fully(&proc);
    });
}

fn u8_to_i16(mut vec: Vec<u8>) -> Vec<i16> {
    let cast = unsafe { Vec::from_raw_parts(vec.as_mut_ptr() as *mut i16, vec.len() / 2, vec.capacity() / 2) };
    std::mem::forget(vec);
    return cast;
}

fn encode_mp3_simple(pcm: &mut [i16]) -> Vec<u8> {
    let estimate_size = (pcm.len() as f32 * 1.25 + 7200.0) as usize;
    let mut out = Vec::<u8>::with_capacity(estimate_size);
    unsafe {
        out.set_len(estimate_size);

        let ctx = lame_init();
        lame_set_in_samplerate(ctx, 48000);
        lame_set_out_samplerate(ctx, 48000);
        lame_set_VBR(ctx, vbr_default);
        if lame_init_params(ctx) < 0 {
            panic!("Failed to init LAME parameters");
        }

        // worst case estimate according to lame.h
        let mut mp3_bytes: c_int = 0;
        let mut bytes_written = 0usize;
        let mut pcm_processed = 0;

        mp3_bytes = lame_encode_buffer_interleaved(ctx,
                                                   pcm[pcm_processed * 2..].as_mut_ptr() as *mut c_short,
                                                   (pcm.len() / 2) as c_int,
                                                   out.as_mut_ptr() as *mut c_uchar,
                                                   out.len() as c_int
        );
        if mp3_bytes < 0 {
            panic!("lame encoding failed {mp3_bytes}");
        }
        bytes_written += mp3_bytes as usize;

        let mut remaining = &mut out[mp3_bytes as usize..];
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

fn encode_mp3(pcm: &mut [i16]) -> Vec<u8> {
    let mut out = Vec::<u8>::new();
    unsafe {
        let ctx = lame_init();
        lame_set_in_samplerate(ctx, 48000);
        lame_set_out_samplerate(ctx, 48000);
        lame_set_VBR(ctx, vbr_default);
        if lame_init_params(ctx) < 0 {
            panic!("Failed to init LAME parameters");
        }
        const SAMPLES_PER_CHUK: usize = 8192;
        // worst case estimate according to lame.h
        const MP3_BUF_SIZE: usize = (SAMPLES_PER_CHUK as f32 * 1.25 + 7200.0) as usize;
        let mut mp3buf: [u8; MP3_BUF_SIZE] = [0; MP3_BUF_SIZE];
        let mut mp3_bytes: c_int = 0;
        let mut pcm_processed = 0;
        loop {
            let frames_to_process = min(SAMPLES_PER_CHUK, (pcm.len() / 2) - pcm_processed);

            mp3_bytes = lame_encode_buffer_interleaved(ctx,
                                                      pcm[pcm_processed * 2..].as_mut_ptr() as *mut c_short,
                                                       frames_to_process as c_int,
                                                      mp3buf.as_mut_ptr() as *mut c_uchar,
                                                      mp3buf.len() as c_int
            );
            if mp3_bytes < 0 {
                panic!("lame encoding failed {mp3_bytes}");
            }

            out.extend_from_slice(&mp3buf[..mp3_bytes as usize]);
            pcm_processed += frames_to_process;
            if pcm_processed >= (pcm.len() / 2) {
                break;
            }
        }
        mp3_bytes = lame_encode_flush(ctx, mp3buf.as_mut_ptr() as *mut c_uchar, mp3buf.len() as c_int);
        out.extend_from_slice(&mp3buf[..mp3_bytes as usize]);
        lame_close(ctx);
        return out;
    }
}

async fn api(voice_id: &str, api_key: &str, mp3: Vec<u8>, out: &Sender<Vec<i16>>, state: &Mutex<State>) -> Result<Vec<u8>, reqwest::Error> {
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
        /*let ctx;
        unsafe {
            ctx = lame_init();
            lame_set_in_samplerate(ctx, 48000);
            lame_set_out_samplerate(ctx, 48000);
            lame_set_decode_only(ctx, 1);
            if lame_init_params(ctx) < 0 {
                panic!("Failed to init LAME parameters");
            }
        }*/
        let mut convert_proc = Exec::shell("ffmpeg -hide_banner -loglevel error -f mp3 -i - -f s16le -ar 48000 -ac 2 -")
            .stdin(Redirection::Pipe)
            .stdout(Redirection::Pipe)
            .popen().unwrap();

        let mut stdout = std::mem::take(&mut convert_proc.stdout).unwrap();
        let out_copy = out.clone();
        let mut debug = File::create("reee").unwrap();

        let reader_thread = thread::spawn(move || {
            loop {
                let mut vec = Vec::with_capacity(1024);
                let slice = unsafe { slice::from_raw_parts_mut(vec.as_mut_ptr(), vec.capacity()) };
                match stdout.read(slice) {
                    Ok(n) => {
                        if n <= 0 {
                            return;
                        }
                        unsafe { vec.set_len(n) };
                        debug.write_all(vec.as_slice()).unwrap();
                        out_copy.send(u8_to_i16(vec)).unwrap();
                    },
                    Err(e) => panic!("{}", e)
                }
            }
        });

        let mut copy = Vec::<u8>::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let data = &chunk?[..];
            let mut stdin = convert_proc.stdin.as_ref().unwrap();
            stdin.write_all(data).unwrap();
            copy.extend_from_slice(data);
        }
        std::mem::take(&mut convert_proc.stdin);
        reader_thread.join().unwrap();
        return Ok(copy);
    } else {
        let body = response.text().await?;
        panic!("API failed {body}");
    }
}

fn read_stdout_fully(proc: &Popen) -> Vec<u8> {
    let mut out = Vec::new();
    proc.stdout.as_ref().unwrap().read_to_end(&mut out).unwrap();
    return out;
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
    let source = matches.get_one::<String>("source").unwrap();
    let sink = matches.get_one::<String>("sink").unwrap();

    let state = Arc::new(Mutex::new(State::Idle(None)));

    pipewire::init();
    let (tx, rx) = channel();
    let (pw_sender, pw_receiver) = pipewire::channel::channel();
    ctrlc::set_handler(move || {
        pw_sender.send(Terminate).unwrap();
    }).unwrap();
    let _pw_thread = thread::spawn(move || pw_playback_thread(rx, pw_receiver).unwrap());
    let audio_sender = Arc::new(tx);

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
                        let mp3_copy = mp3.clone();
                        let output_copy = audio_sender.clone();
                        thread::spawn(move || {
                            let decoded = u8_to_i16(decode_mp3(mp3_copy.deref()));
                            output_copy.send(decoded).unwrap();
                        });
                    }
                }
                if event_key == Key::KEY_RIGHTSHIFT {
                    if key_state == KeyState::Pressed && matches!(*state.lock().unwrap(), State::Idle(_)) {
                        let (mic_send, mic_receive) = channel();
                        let (pw_sender, pw_receiver) = pipewire::channel::channel();
                        thread::spawn(move || pw_mic_thread(mic_send, pw_receiver));

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
                            let mp3 = encode_mp3_simple(pcm.as_mut_slice());
                            println!("encode took {}ms", start.elapsed().as_millis());
                            *state_copy.lock().unwrap() = State::Generating;
                            let last_copy = RT.block_on(async {
                                return api(&voice, &key, mp3,output_copy.deref(), &state_copy).await.unwrap();
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
