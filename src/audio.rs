use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::{mem, slice};
use std::cmp::min;

use pipewire as pw;
use pw::{properties, spa};
use spa::pod::Pod;
use libspa_sys as spa_sys;
use pipewire::buffer::Buffer;
use pipewire::spa::format::{MediaSubtype, MediaType};
use pipewire::spa::param::format_utils;

struct AudioState {
    rx: Receiver<Vec<i16>>,
    prev_buffer: Option<(Vec<i16>, usize)>
}

pub const SAMPLE_RATE: u32 = 44100;
pub const CHANNELS: u32 = 2;
pub const CHAN_SIZE: usize = std::mem::size_of::<i16>();

#[derive(Debug)]
pub struct Terminate;

// TODO: get rid of this in next pipwire-rs release
fn get_requested(buf: &Buffer) -> u64 {
    let ptr = buf as *const Buffer;
    let troll = ptr as *const *const pipewire_sys::pw_buffer;
    return unsafe { (**troll).requested };
}

pub fn pw_playback_thread(target: &str, audio_receiver: Receiver<Vec<i16>>, pw_receiver: pipewire::channel::Receiver<Terminate>) -> Result<(), pw::Error> {
    let mainloop = pw::MainLoop::new().expect("Failed to create PipeWire Mainloop");
    let context = pw::Context::new(&mainloop).expect("Failed to create PipeWire Context");
    let core = context
        .connect(None)
        .expect("Failed to connect to PipeWire Core");

    let _receiver = pw_receiver.attach(&mainloop, {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let stream = pw::stream::Stream::new(
        &core,
        "Speech2Speech",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::TARGET_OBJECT => target
        },
    )?;
    let state = AudioState {
        rx: audio_receiver,
        prev_buffer: None
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .process(|stream, state| match stream.dequeue_buffer() {
            None => println!("No buffer received"),
            Some(mut buffer) => {
                let requested_samples = (get_requested(&buffer) * CHANNELS as u64) as usize;
                let datas = buffer.datas_mut();
                let stride = CHAN_SIZE * CHANNELS as usize;
                let data = &mut datas[0];

                let mut samples_written = 0;
                if let Some(out_slice) = data.data() {
                    let out_slice_i16 = unsafe { slice::from_raw_parts_mut(out_slice.as_mut_ptr() as *mut i16, out_slice.len() / 2)};
                    while samples_written < requested_samples {
                        if let Some((vec, idx)) = &mut state.prev_buffer {
                            let remaining = &vec[*idx..];

                            let len = min(vec.len() - *idx, requested_samples - samples_written);
                            out_slice_i16[samples_written..samples_written + len].copy_from_slice(&remaining[..len]);
                            samples_written += len;
                            *idx += len;
                            // we've written out all of the data from the cached vec, next loop iteration will recv for more
                            if *idx == vec.len() {
                                state.prev_buffer = None;
                            }
                        } else {
                            match state.rx.try_recv() {
                                Ok(vec) => {
                                    state.prev_buffer = Some((vec, 0));
                                },
                                Err(TryRecvError::Empty) => {
                                    // fill with silence
                                    out_slice_i16[samples_written..requested_samples].fill(0);
                                    break;
                                },
                                Err(TryRecvError::Disconnected) => {
                                    panic!("Channel disconnected?")
                                }
                            }
                        }
                    }
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                let frames = samples_written / CHANNELS as usize;
                *chunk.size_mut() = (stride * frames) as _;
            }
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS);

    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(spa::pod::Object {
                type_: spa_sys::SPA_TYPE_OBJECT_Format,
                id: spa_sys::SPA_PARAM_EnumFormat,
                properties: audio_info.into(),
            }),
        )
        .unwrap()
        .0
        .into_inner();

    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        spa::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    mainloop.run();

    return Ok(());
}

pub fn pw_mic_thread(target: &str, audio_sender: Sender<Vec<i16>>, pw_receiver: pipewire::channel::Receiver<Terminate>) {
    let mainloop = pw::MainLoop::new().expect("Failed to create PipeWire Mainloop");
    let context = pw::Context::new(&mainloop).expect("Failed to create PipeWire Context");
    let core = context
        .connect(None)
        .expect("Failed to connect to PipeWire Core");

    let _receiver = pw_receiver.attach(&mainloop, {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::TARGET_OBJECT => target
    };

    let stream = pw::stream::Stream::new(&core, "audio-capture", props).unwrap();

    struct UserData {
        format: spa::param::audio::AudioInfoRaw,
        sender: Sender<Vec<i16>>
    }
    let data = UserData {
        format: Default::default(),
        sender: audio_sender
    };


    let _listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(|_, id, user_data, param| {
            // NULL means to clear the format
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }

            let (media_type, media_subtype) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };

            // only accept raw audio
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }

            // call a helper function to parse the format for us.
            user_data
                .format
                .parse(param)
                .expect("Failed to parse param changed to AudioInfoRaw");

            println!(
                "capturing rate:{} channels:{}",
                user_data.format.rate(),
                user_data.format.channels()
            );
        })
        .process(|stream, user_data| match stream.dequeue_buffer() {
            None => println!("out of buffers"),
            Some(mut buffer) => {
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }

                let data = &mut datas[0];
                let n_channels = user_data.format.channels();
                assert_eq!(n_channels, CHANNELS);
                let n_samples = data.chunk().size() as usize / (mem::size_of::<i16>());

                if let Some(samples_buf) = data.data() {
                    let samples = unsafe { slice::from_raw_parts(samples_buf.as_ptr() as *const i16, n_samples)};
                    let mut out = Vec::<i16>::with_capacity(n_samples);
                    unsafe { out.set_len(out.capacity()) };
                    out.as_mut_slice().copy_from_slice(samples);
                    user_data.sender.send(out).unwrap();
                }
            }
        })
        .register().unwrap();

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_channels(CHANNELS); // 2 is the default but i want to guarantee it
    audio_info.set_rate(SAMPLE_RATE);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(obj),
        )
        .unwrap()
        .0
        .into_inner();

    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        spa::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ).unwrap();

    mainloop.run();
}
