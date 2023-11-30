use std::sync::mpsc::{Receiver, TryRecvError};
use std::{slice};
use std::cmp::min;

use pipewire as pw;
use pw::{properties, spa};
use spa::pod::Pod;
use libspa_sys as spa_sys;
use pipewire::buffer::Buffer;

struct AudioState {
    rx: Receiver<Vec<i16>>,
    prev_buffer: Option<(Vec<i16>, usize)>
}

pub const DEFAULT_RATE: u32 = 48000;
pub const DEFAULT_CHANNELS: u32 = 2;
pub const CHAN_SIZE: usize = std::mem::size_of::<i16>();

pub struct Terminate;

// TODO: get rid of this in next pipwire-rs release
fn get_requested(buf: &Buffer) -> u64 {
    let ptr = buf as *const Buffer;
    let troll = ptr as *const *const pipewire_sys::pw_buffer;
    return unsafe { (**troll).requested };
}

pub fn pw_thread(audio_receiver: Receiver<Vec<i16>>, pw_receiver: pipewire::channel::Receiver<Terminate>) -> Result<(), pw::Error> {
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
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_CATEGORY => "Playback",
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
                let mut requested_samples = get_requested(&buffer) as usize * 2;
                let datas = buffer.datas_mut();
                let stride = CHAN_SIZE * DEFAULT_CHANNELS as usize;
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
                                    out_slice_i16[samples_written..].fill(0);
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
                let frames = samples_written / 2;
                *chunk.size_mut() = (stride * frames) as _;
            }
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(DEFAULT_RATE);
    audio_info.set_channels(DEFAULT_CHANNELS);

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
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
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();

    return Ok(());
}
