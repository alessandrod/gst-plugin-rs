// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate glib;
use glib::prelude::*;
extern crate gstreamer as gst;
extern crate gstreamer_check as gst_check;
extern crate gstreamer_video as gst_video;

extern crate gstrav1e;

fn init() {
    use std::sync::{Once, ONCE_INIT};
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrav1e::plugin_register_static().expect("rav1e test");
    });
}

#[test]
fn test_encode_i420() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::I420, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i420_10() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::I42010le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i420_12() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::I42012le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_y42b() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::Y42b, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i422_10() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::I42210le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_y422_12() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::I42212le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_y444() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::Y444, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i444_10() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::Y44410le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i444_12() {
    init();

    let video_info = gst_video::VideoInfo::new(gst_video::VideoFormat::Y44412le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

fn test_encode(video_info: &gst_video::VideoInfo) {
    let mut h = gst_check::Harness::new("rav1enc");
    {
        let rav1enc = h.get_element().unwrap();
        rav1enc.set_property("speed-preset", &10u32).unwrap();
        rav1enc.set_property("tile-cols-log2", &1u32).unwrap();
        rav1enc.set_property("tile-rows-log2", &1u32).unwrap();
    }
    h.play();
    h.set_src_caps(video_info.to_caps().unwrap());

    let buffer = {
        let buffer = gst::Buffer::with_size(video_info.size()).unwrap();
        let mut vframe = gst_video::VideoFrame::from_buffer_writable(buffer, &video_info).unwrap();

        for v in vframe.plane_data_mut(0).unwrap() {
            *v = 0;
        }

        match video_info.format_info().depth()[0] {
            8 => {
                for v in vframe.plane_data_mut(1).unwrap() {
                    *v = 128;
                }

                for v in vframe.plane_data_mut(2).unwrap() {
                    *v = 128;
                }
            }
            10 => {
                for v in vframe.plane_data_mut(1).unwrap().chunks_exact_mut(2) {
                    v[0] = 0;
                    v[1] = 2;
                }

                for v in vframe.plane_data_mut(2).unwrap().chunks_exact_mut(2) {
                    v[0] = 0;
                    v[1] = 2;
                }
            }
            12 => {
                for v in vframe.plane_data_mut(1).unwrap().chunks_exact_mut(2) {
                    v[0] = 0;
                    v[1] = 8;
                }

                for v in vframe.plane_data_mut(2).unwrap().chunks_exact_mut(2) {
                    v[0] = 0;
                    v[1] = 8;
                }
            }
            _ => unreachable!(),
        }

        vframe.into_buffer()
    };

    for _ in 0..5 {
        h.push(buffer.clone()).unwrap();
    }
    h.push_event(gst::Event::new_eos().build());

    for i in 0..5 {
        let buffer = h.pull().unwrap();
        if i == 0 {
            assert!(!buffer.get_flags().contains(gst::BufferFlags::DELTA_UNIT))
        }
    }
}
