use std::ffi::{OsStr, OsString};
use input::{Event, Libinput, LibinputInterface};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::{fs::OpenOptionsExt, io::OwnedFd};
use std::path::Path;
use std::{thread};
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use clap::{Arg};
use input::event::keyboard::{KeyboardEventTrait, KeyState};
use nix::poll::{poll, PollFlags, PollFd};

use evdev::Key;

extern crate libc;
use libc::{O_RDONLY, O_RDWR, O_WRONLY};
use subprocess::{Exec, Popen, Redirection};
use subprocess::unix::PopenExt;

use futures_util::StreamExt;
use once_cell::sync::{Lazy};
use tokio::runtime::Runtime;

use reqwest::multipart;
use serde_json::json;
use tokio::time::Instant;

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

#[derive(Debug)]
enum State {
    Idle(Option<Arc<Vec<u8>>>),
    Recording(Arc<InputProcs>),
    Generating,
    Playing
}

#[derive(Debug)]
struct InputProcs {
    rec: Popen,
    ffmpeg: Popen
}
fn mic_input(rec_cmd: &OsStr) -> Arc<InputProcs> {
    let mut ffmpeg = Exec::shell(OsStr::new("ffmpeg -hide_banner -loglevel error -f s16le -ar 48000 -ac 2 -i - -f mp3 -"))
        .stdout(Redirection::Pipe)
        .stdin(Redirection::Pipe)
        .popen().unwrap();
    let rec = Exec::shell(rec_cmd)
        .stdout(Redirection::File(std::mem::take(&mut ffmpeg.stdin).unwrap()))
        .popen().unwrap();
    return Arc::new(InputProcs{ rec, ffmpeg});
}

fn audio_commands(source: &str, sink: &str) -> (OsString, OsString) {
    (
        // --target easyeffects_source
        OsString::from(format!("pw-record --target {source} --rate=48000 --channels=2 -")),
        // --target LiveSynthSink
        OsString::from(format!("pw-cat --target {sink} --rate=48000 --channels=2 -p -"))
    )
}

async fn api(voice_id: &str, api_key: &str, mp3: Vec<u8>, mut out: &File, state: &Mutex<State>) -> Result<Vec<u8>, reqwest::Error> {
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
        let mut copy = Vec::<u8>::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let data = &chunk?[..];
            out.write_all(data).unwrap();
            copy.extend_from_slice(data);
        }
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
    let (rec_cmd, cat_cmd) = audio_commands(&source, &sink);
    let ffmpeg_convert = OsString::from(format!("ffmpeg -hide_banner -loglevel error -f mp3 -i - -f s16le -ar 48000 -ac 2 - | {}", cat_cmd.to_str().unwrap()));

    let output = Arc::new(Exec::shell(ffmpeg_convert)
        .stdin(Redirection::Pipe)
        .popen().unwrap()
    );

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
                        let output_copy = output.clone();
                        thread::spawn(move || {
                            output_copy.stdin.as_ref().unwrap().write_all(mp3_copy.deref()).unwrap();
                        });
                    }
                }
                if event_key == Key::KEY_RIGHTSHIFT {
                    if key_state == KeyState::Pressed && matches!(*state.lock().unwrap(), State::Idle(_)) {
                        let input = mic_input(&rec_cmd);

                        let input_copy = input.clone();
                        let output_copy = output.clone();
                        let state_copy = state.clone();
                        let voice = voice.clone();
                        let key = key.clone();
                        *state.lock().unwrap() = State::Recording(input);
                        thread::spawn(move || {
                            let mp3 = read_stdout_fully(&input_copy.ffmpeg);
                            *state_copy.lock().unwrap() = State::Generating;
                            let last_copy = RT.block_on(async {
                                return api(&voice, &key, mp3,output_copy.stdin.as_ref().unwrap(), &state_copy).await.unwrap();
                            });
                            *state_copy.lock().unwrap() = State::Idle(Some(Arc::new(last_copy)));
                        });
                    }
                    if key_state == KeyState::Released {
                        if let State::Recording(proc) = state.lock().unwrap().deref_mut() {
                            proc.rec.send_signal(libc::SIGTERM).unwrap();
                        }
                    }
                }
            }
        }
    }
    output.send_signal(libc::SIGTERM).unwrap();
}
