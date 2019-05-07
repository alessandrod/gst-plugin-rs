// Copyright (C) 2019 Alessandro Decina <alessandro.d@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::cmp;
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::sync::{Mutex, MutexGuard};

use crate::gst;
use crate::gst::prelude::*;
use crate::gst::subclass::prelude::*;
use crate::gst_base;
use glib;
use glib::subclass;

use nuts::{pes, ts};

use smallvec::SmallVec;

lazy_static! {
    static ref CAT: gst::DebugCategory = {
        gst::DebugCategory::new(
            "nutsdemux",
            gst::DebugColorFlags::empty(),
            Some("Rust TS demuxer"),
        )
    };
}

const PROGRAM_NOT_SET: i32 = -1;

static PROPERTIES: [subclass::Property; 1] = [subclass::Property("program-number", |name| {
    glib::ParamSpec::int(
        name,
        "Program Number",
        "Transport stream program to demux",
        -1,
        std::i32::MAX,
        PROGRAM_NOT_SET,
        glib::ParamFlags::READWRITE | glib::ParamFlags::CONSTRUCT,
    )
})];

#[derive(Debug, Clone, Copy)]
struct Settings {
    pub program_number: Option<u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            program_number: None,
        }
    }
}

type Events = SmallVec<[Event; 2]>;

#[derive(Debug)]
struct TsDemux {
    sinkpad: gst::Pad,
    adapter: Mutex<gst_base::UniqueAdapter>,
    flow_combiner: Mutex<gst_base::UniqueFlowCombiner>,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

#[derive(Debug)]
enum State {
    Stopped,
    Streaming(StreamingState),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    ConfigureProgram(u16),
    NewSegment(Vec<gst::Pad>, gst::Segment),
    Buffer(u16, gst::Buffer),
}

#[derive(Debug)]
struct StreamingState {
    parser: ts::Parser,
    resync: bool,
    pat: Option<ts::psi::PAT>,
    programs: HashMap<u16, Program>,
    streams: HashMap<u16, StreamData>,
    pcr_pid: u16,
    program_number: Option<u16>,
    segment: Option<DemuxSegment>,
    offset: u64,
    packet_size: Option<usize>,
    queue_data: bool,
    needed_timestamps: usize,
    first_pcr: gst::ClockTime,
    first_pcr_offset: u64,
    last_pcr: gst::ClockTime,
    last_pcr_offset: u64,
    parse_offset: u64,
    need_newsegment: bool,
}

#[derive(Debug)]
enum DemuxSegment {
    Upstream(gst::Segment),
    PullMode(gst::Segment),
}

#[derive(Debug, Eq, PartialEq)]
struct Program {
    number: u16,
    pmt: Option<ts::psi::PMT>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StreamData {
    pid: u16,
    pad: Option<gst::Pad>,
    caps: Option<gst::Caps>,
    discont: bool,
    first_ts: gst::ClockTime,
    buf_queue: Vec<gst::Buffer>,
}

impl State {
    fn streaming_state(&self) -> &StreamingState {
        match self {
            State::Streaming(sstate) => sstate,
            _ => panic!(),
        }
    }

    fn streaming_state_mut(&mut self) -> &mut StreamingState {
        match self {
            State::Streaming(ref mut sstate) => sstate,
            _ => panic!(),
        }
    }
}

impl ObjectSubclass for TsDemux {
    const NAME: &'static str = "NutsDemux";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));

        sinkpad.set_activate_function(|pad, parent| {
            TsDemux::catch_panic_pad_function(
                parent,
                || Err(gst_loggable_error!(CAT, "Panic activating sink pad")),
                |demux, element| demux.sink_activate(pad, element),
            )
        });

        sinkpad.set_activatemode_function(|pad, parent, mode, active| {
            TsDemux::catch_panic_pad_function(
                parent,
                || {
                    Err(gst_loggable_error!(
                        CAT,
                        "Panic activating sink pad with mode"
                    ))
                },
                |demux, element| demux.sink_activatemode(pad, element, mode, active),
            )
        });

        sinkpad.set_chain_function(|pad, parent, buffer| {
            TsDemux::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |demux, element| demux.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            TsDemux::catch_panic_pad_function(
                parent,
                || false,
                |demux, element| demux.sink_event(pad, element, event),
            )
        });

        TsDemux {
            sinkpad,
            state: Mutex::new(State::Stopped),
            settings: Mutex::new(Default::default()),
            adapter: Mutex::new(gst_base::UniqueAdapter::new()),
            flow_combiner: Mutex::new(gst_base::UniqueFlowCombiner::new()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "MPEG TS Demuxer",
            "Codec/Demuxer",
            "Demuxes MPEG Transport Streams",
            "Alessandro Decina <alessandro.d@gmail.com>",
        );

        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();

            caps.append(
                gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", &1i32)
                    .field("parsed", &false)
                    .build(),
            );
            caps.append(
                gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", &2i32)
                    .field("stream-format", &"adts")
                    .build(),
            );
            caps.append(
                gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", &4i32)
                    .field("stream-format", &"loas")
                    .build(),
            );
            caps.append(gst::Caps::builder("audio/a-ac3").build());
        }
        let audiosrc_pad_template = gst::PadTemplate::new(
            "audio",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(audiosrc_pad_template);

        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();
            caps.append(
                gst::Caps::builder("video/x-h264")
                    .field("stream-format", &"byte-stream")
                    .field("alignment", &"nal")
                    .build(),
            );
            caps.append(
                gst::Caps::builder("video/x-h265")
                    .field("stream-format", &"byte-stream")
                    .field("alignment", &"nal")
                    .build(),
            );
        }
        let videosrc_pad_template = gst::PadTemplate::new(
            "video",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(videosrc_pad_template);

        let caps = gst::Caps::builder("video/mpegts").build();
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);
        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for TsDemux {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("program-number", ..) => {
                let mut settings = self.settings.lock().unwrap();
                if let State::Stopped = *self.state.lock().unwrap() {
                    let number = value.get::<i32>().unwrap();
                    match number {
                        PROGRAM_NOT_SET => settings.program_number = None,
                        _ => settings.program_number = Some(number as u16),
                    }
                }
            }
            _ => unimplemented!(),
        };
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("program-number", ..) => {
                let settings = self.settings.lock().unwrap();
                let res = match settings.program_number {
                    Some(number) => (number as i32).to_value(),
                    None => PROGRAM_NOT_SET.to_value(),
                };
                Ok(res)
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
    }
}

impl ElementImpl for TsDemux {}

impl TsDemux {
    fn sink_activate(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
    ) -> Result<(), gst::LoggableError> {
        let mode = {
            let mut query = gst::Query::new_scheduling();
            if !pad.peer_query(&mut query) {
                gst_warning!(CAT, obj: element, "scheduling query faileon peer");
            }

            if query
                .has_scheduling_mode_with_flags(gst::PadMode::Pull, gst::SchedulingFlags::SEEKABLE)
            {
                gst_debug!(CAT, obj: pad, "Activating in Pull mode");
                gst::PadMode::Pull
            } else {
                gst_debug!(CAT, obj: pad, "Activating in Push mode");
                gst::PadMode::Push
            }
        };

        pad.activate_mode(mode, true)?;
        Ok(())
    }

    fn sink_activatemode(
        &self,
        _pad: &gst::Pad,
        element: &gst::Element,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            self.start(element, mode).map_err(|err| {
                element.post_error_message(&err);
                gst_loggable_error!(CAT, "Failed to start element with mode {:?}", mode)
            })?;

            if mode == gst::PadMode::Pull {
                self.start_task(element)?;
            }
        } else {
            if mode == gst::PadMode::Pull {
                let _ = self.sinkpad.stop_task();
            }

            self.stop(element).map_err(|err| {
                element.post_error_message(&err);
                gst_loggable_error!(CAT, "Failed to stop element")
            })?;
        }

        Ok(())
    }

    fn start_task(&self, element: &gst::Element) -> Result<(), gst::LoggableError> {
        let element_weak = element.downgrade();
        let pad_weak = self.sinkpad.downgrade();
        self.sinkpad
            .start_task(move || {
                let element = match element_weak.upgrade() {
                    Some(element) => element,
                    None => {
                        if let Some(pad) = pad_weak.upgrade() {
                            pad.pause_task().unwrap();
                        }
                        return;
                    }
                };

                let el = Self::from_instance(&element);
                el.loop_fn(&element);
            })
            .map_err(|_| gst_loggable_error!(CAT, "Failed to start pad task"))
    }

    fn start(&self, _element: &gst::Element, mode: gst::PadMode) -> Result<(), gst::ErrorMessage> {
        let mut sstate = StreamingState::new();
        if mode == gst::PadMode::Pull {
            sstate.segment = Some(DemuxSegment::PullMode(default_seek_segment()))
        }
        *self.state.lock().unwrap() = State::Streaming(sstate);

        Ok(())
    }

    fn stop(&self, _element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State::Stopped;
        self.adapter.lock().unwrap().clear();

        let mut flow_combiner = self.flow_combiner.lock().unwrap();
        flow_combiner.reset();

        Ok(())
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use crate::gst::EventView;

        gst_debug!(CAT, obj: pad, "Received sink event {:?}", event);
        match event.view() {
            EventView::Caps(e) => {
                let caps = e.get_caps();
                let s = caps.get_structure(0).unwrap();
                let packet_size = s.get::<i32>("packetsize").unwrap_or(188) as usize;
                let mut state = self.state.lock().unwrap();
                let mut sstate = state.streaming_state_mut();
                sstate.packet_size = Some(packet_size);
                sstate.parser = ts::Parser::with_packet_size(packet_size);
                gst_info!(CAT, obj: element, "configured packet size: {}", packet_size);
                true
            }
            EventView::Segment(s) => {
                let mut state = self.state.lock().unwrap();
                if let State::Streaming(ref mut sstate) = *state {
                    sstate.segment = Some(DemuxSegment::Upstream(s.get_segment().clone()));
                    return true;
                }
                pad.event_default(Some(element), event)
            }
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                self.flush(&mut state);
                drop(state);
                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use crate::gst::EventView;

        gst_debug!(CAT, obj: pad, "Received src event {:?}", event);
        match event.view() {
            EventView::Seek(e) => {
                if self.sinkpad.push_event(event.clone()) {
                    gst_info!(CAT, obj: pad, "here {:?}", event);
                    return true;
                }

                if self.sinkpad.get_mode() == gst::PadMode::Pull {
                    self.perform_seek(e, element)
                } else {
                    let (rate, flags, start_type, start, stop_type, stop) = e.get();

                    let start = match gst::ClockTime::try_from(start) {
                        Ok(start) => start,
                        Err(_) => {
                            gst_error!(CAT, obj: element, "seek has invalid format");
                            return false;
                        }
                    };

                    let stop = match gst::ClockTime::try_from(stop) {
                        Ok(stop) => stop,
                        Err(_) => {
                            gst_error!(CAT, obj: element, "seek has invalid format");
                            return false;
                        }
                    };

                    let mut state = self.state.lock().unwrap();
                    if let State::Streaming(ref mut sstate) = *state {
                        let start = gst::GenericFormattedValue::Bytes(gst::format::Bytes(
                            if start_type == gst::SeekType::Set && start != gst::CLOCK_TIME_NONE {
                                sstate.ts_to_offset(start)
                            } else {
                                None
                            },
                        ));
                        let stop = gst::GenericFormattedValue::Bytes(gst::format::Bytes(
                            if stop_type == gst::SeekType::Set && stop != gst::CLOCK_TIME_NONE {
                                sstate.ts_to_offset(stop)
                            } else {
                                None
                            },
                        ));
                        let new_seek =
                            gst::Event::new_seek(rate, flags, start_type, start, stop_type, stop)
                                .build();
                        drop(state);
                        return self.sinkpad.push_event(new_seek);
                    }

                    false
                }
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, element: &gst::Element, query: &mut gst::QueryRef) -> bool {
        use crate::gst::QueryView;

        gst_debug!(CAT, obj: pad, "Received src query {:?}", query);
        match query.view_mut() {
            QueryView::Position(ref mut q) => {
                if self.sinkpad.peer_query(q.get_mut_query()) {
                    return true;
                }

                let fmt = q.get_format();
                if fmt != gst::Format::Time {
                    return false;
                }

                if let State::Streaming(ref sstate) = *self.state.lock().unwrap() {
                    if let Some(DemuxSegment::PullMode(s)) = &sstate.segment {
                        gst_info!(
                            CAT,
                            obj: element,
                            "returning position {:?}",
                            s.get_position()
                        );
                        q.set(s.get_position());
                        return true;
                    }
                }

                false
            }
            QueryView::Duration(ref mut q) => {
                if self.sinkpad.peer_query(q.get_mut_query()) {
                    return true;
                }

                if let State::Streaming(ref sstate) = *self.state.lock().unwrap() {
                    if let Some(duration) = self
                        .query_upstream_size()
                        .and_then(|size| sstate.offset_to_ts(size))
                    {
                        q.set(duration);
                        return true;
                    }
                }

                false
            }
            QueryView::Latency(ref mut q) => {
                if self.sinkpad.peer_query(q.get_mut_query()) {
                    let (live, mut min, mut max) = q.get_result();
                    min += 700 * gst::MSECOND;
                    if max != gst::CLOCK_TIME_NONE {
                        max += 700 * gst::MSECOND;
                    }
                    q.set(live, min, max);
                    return true;
                }

                false
            }
            QueryView::Seeking(ref mut q) => {
                if self.sinkpad.peer_query(q.get_mut_query()) {
                    return true;
                }

                if q.get_format() == gst::Format::Time {
                    if let State::Streaming(ref sstate) = *self.state.lock().unwrap() {
                        if let Some(duration) = self
                            .query_upstream_size()
                            .and_then(|size| sstate.offset_to_ts(size))
                        {
                            q.set(true, gst::ClockTime::from_seconds(0), duration);
                            return true;
                        }
                    }
                }

                false
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn perform_seek(&self, event: gst::event::Seek, element: &gst::Element) -> bool {
        gst_info!(CAT, obj: element, "seeking {:?}", event);

        let (rate, flags, start_type, start, stop_type, stop) = event.get();

        let start = match gst::ClockTime::try_from(start) {
            Ok(start) => start,
            Err(_) => {
                gst_error!(CAT, obj: element, "seek has invalid format");
                return false;
            }
        };

        let stop = match gst::ClockTime::try_from(stop) {
            Ok(stop) => stop,
            Err(_) => {
                gst_error!(CAT, obj: element, "seek has invalid format");
                return false;
            }
        };

        if start_type == gst::SeekType::End || stop_type == gst::SeekType::End {
            gst_error!(CAT, obj: element, "Relative seeks are not supported");
            return false;
        }

        let mut state = self.state.lock().unwrap();
        let sstate = state.streaming_state();

        if sstate.first_pcr == gst::CLOCK_TIME_NONE || sstate.last_pcr == gst::CLOCK_TIME_NONE {
            gst_info!(CAT, obj: element, "no PCRs yet");
            return false;
        }

        let srcpads = sstate.stream_pads();
        if flags.contains(gst::SeekFlags::FLUSH) {
            gst_info!(CAT, obj: element, "flushing upstream");
            let event = gst::Event::new_flush_start().build();
            self.sinkpad.push_event(event.clone());
            gst_info!(CAT, obj: element, "flushing downstream");
            for pad in &srcpads {
                pad.push_event(event.clone());
            }
        }

        drop(state);
        gst_info!(CAT, obj: element, "pausing task");
        self.sinkpad.pause_task().unwrap();
        state = self.state.lock().unwrap();
        let sstate = state.streaming_state_mut();
        let mut segment = match &sstate.segment {
            Some(DemuxSegment::PullMode(segment)) => segment.clone(),
            _ => unreachable!(),
        };
        let first_pcr = sstate.first_pcr;
        if flags.contains(gst::SeekFlags::FLUSH) {
            drop(state);
            gst_info!(CAT, obj: element, "sending flush-stop upstream");
            let event = gst::Event::new_flush_stop(true).build();
            self.sinkpad.push_event(event.clone());
            gst_info!(CAT, obj: element, "sending flush-stop downstream");
            for pad in &srcpads {
                pad.push_event(event.clone());
            }
            state = self.state.lock().unwrap();
            self.flush(&mut state);
        }
        let sstate = state.streaming_state_mut();
        let time_seg = segment.downcast_mut::<gst::ClockTime>().unwrap();
        time_seg.do_seek(rate, flags, start_type, start, stop_type, stop);
        let target = time_seg.get_position();
        gst_info!(CAT, obj: element, "seek target {}", first_pcr + target);
        let target = cmp::max(gst::ClockTime::from_seconds(0), target - 500 * gst::MSECOND);
        sstate.segment = Some(DemuxSegment::PullMode(segment));
        sstate.offset = sstate.ts_to_offset(target).unwrap();
        drop(state);

        match self.start_task(element) {
            Err(error) => {
                error.log();
                false
            }
            _ => true,
        }
    }

    fn pull_range(
        &self,
        element: &gst::Element,
        offset: u64,
        size: u32,
    ) -> Result<Option<gst::Buffer>, gst::FlowError> {
        match self.sinkpad.pull_range(offset, size as u32) {
            Ok(buffer) => Ok(Some(buffer)),
            Err(gst::FlowError::Eos) => Ok(None),
            Err(gst::FlowError::Flushing) => {
                gst_debug!(CAT, obj: &self.sinkpad, "Pausing after pulling buffer, reason: flushing");

                self.sinkpad.pause_task().unwrap();
                Err(gst::FlowError::Flushing)
            }
            Err(flow) => {
                gst_error!(CAT, obj: &self.sinkpad, "Failed to pull, reason: {:?}", flow);

                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Streaming stopped, failed to pull buffer"]
                );

                self.sinkpad.pause_task().unwrap();
                Err(flow)
            }
        }
    }

    fn query_upstream_size(&self) -> Option<u64> {
        let mut q = gst::query::Query::new_duration(gst::Format::Bytes);
        if !self.sinkpad.peer_query(&mut q) {
            return None;
        }

        match gst::format::Bytes::try_from(q.get_result()) {
            Ok(gst::format::Bytes(b)) => b,
            _ => None,
        }
    }

    fn loop_fn(&self, element: &gst::Element) {
        let _ = self.do_loop(element);
    }

    fn do_loop(&self, element: &gst::Element) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let mut sstate = state.streaming_state_mut();

        let packet_size = sstate.packet_size.unwrap_or(188);
        let offset = sstate.offset;
        sstate.parse_offset = offset;
        drop(state);

        let size = packet_size * 1000;
        let buffer = self.pull_range(element, offset, size as u32)?;
        match self.handle_buffer(&self.sinkpad, element, buffer) {
            Ok(s) => Ok(s),
            Err(flow) => {
                match flow {
                    gst::FlowError::Flushing => {
                        gst_debug!(CAT, obj: element, "Pausing after flow {:?}", flow);
                    }
                    gst::FlowError::Eos => {
                        self.push_eos(element);

                        gst_debug!(CAT, obj: element, "Pausing after flow {:?}", flow);
                    }
                    _ => {
                        self.push_eos(element);

                        gst_error!(CAT, obj: element, "Pausing after flow {:?}", flow);

                        gst_element_error!(
                            element,
                            gst::StreamError::Failed,
                            ["Streaming stopped, reason: {:?}", flow]
                        );
                    }
                }

                self.sinkpad.pause_task().unwrap();
                Err(flow)
            }
        }
    }

    fn push_eos(&self, element: &gst::Element) {
        let state = self.state.lock().unwrap();
        let sstate = state.streaming_state();

        let pads: Vec<gst::Pad> = sstate
            .streams
            .values()
            .filter_map(|sd| sd.pad.clone())
            .collect();

        gst_info!(CAT, obj: element, "sending eos");

        drop(state);

        if pads.is_empty() {
            gst_element_error!(element, gst::StreamError::Failed, ["no supported streams"]);

            return;
        }

        for pad in pads {
            pad.push_event(gst::Event::new_eos().build());
        }
    }

    fn flush(&self, state: &mut MutexGuard<State>) {
        self.adapter.lock().unwrap().clear();
        self.flow_combiner.lock().unwrap().reset();
        if let State::Streaming(ref mut sstate) = **state {
            // FIXME: reset parser and offset
            sstate.segment = None;
            sstate.queue_data = true;
            sstate.need_newsegment = true;
            sstate.needed_timestamps = 0;
            for (_, stream) in sstate.streams.iter_mut() {
                stream.buf_queue.clear();
                stream.first_ts = gst::CLOCK_TIME_NONE;
                stream.discont = true;
                if stream.pad.is_some() {
                    sstate.needed_timestamps += 1;
                }
            }
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let mut sstate = state.streaming_state_mut();
        sstate.parse_offset = buffer.get_offset();
        drop(state);

        self.handle_buffer(pad, element, Some(buffer))
    }

    fn handle_buffer(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_trace!(CAT, obj: pad, "Handling buffer {:?}", buffer);
        let mut state = self.state.lock().unwrap();
        let mut sstate = state.streaming_state_mut();
        let mut adapter = self.adapter.lock().unwrap();
        let have_leftovers = adapter.available() > 0;

        let eos = if let Some(buffer) = buffer {
            if buffer.get_flags() & gst::BufferFlags::DISCONT == gst::BufferFlags::DISCONT {
                gst_info!(CAT, obj: element, "discont buffer");
                for (_, stream) in sstate.streams.iter_mut() {
                    stream.discont = true;
                }
            }

            sstate.offset += buffer.get_size() as u64;
            adapter.push(buffer);
            false
        } else {
            true
        };

        if sstate.packet_size.is_none() {
            let available = adapter.available();
            let map = adapter.map(available).unwrap();
            let size = ts::discover_packet_size(&*map).ok_or_else(|| {
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Invalid transport stream"]
                );

                gst::FlowError::Error
            })?;
            gst_info!(CAT, obj: element, "discovered packet size: {}", size);
            sstate.packet_size = Some(size);
            sstate.parser = ts::Parser::with_packet_size(size);
            drop(state);
            let consumed = self.parse_data(element, &*map)?;
            drop(map);
            adapter.flush(consumed);
            return Ok(gst::FlowSuccess::Ok);
        }

        let packet_size = sstate.packet_size.unwrap();
        drop(state);

        let mut available = adapter.available();
        if have_leftovers && available >= packet_size {
            let map = adapter.map(packet_size).unwrap();
            let consumed = self.parse_data(element, &*map)?;
            drop(map);
            adapter.flush(consumed);
            available = adapter.available();
        }

        if available >= packet_size {
            let map = adapter.map(available).unwrap();
            let consumed = self.parse_data(element, &*map)?;
            drop(map);
            adapter.flush(consumed);
        }

        if eos {
            gst_info!(CAT, obj: element, "eos");
            Err(gst::FlowError::Eos)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn parse_data(&self, element: &gst::Element, mut data: &[u8]) -> Result<usize, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let mut sstate = state.streaming_state_mut();
        let packet_size = sstate.packet_size.unwrap();
        let offset = sstate.parse_offset;
        while data.len() >= packet_size {
            let (consumed, events) = self.parse_packet(element, &mut sstate, &data)?;
            sstate.parse_offset += consumed as u64;
            data = &data[consumed..];
            if !events.is_empty() {
                drop(state);

                self.handle_events(element, events)?;

                state = self.state.lock().unwrap();
                sstate = state.streaming_state_mut();
            }
            if consumed == 0 {
                break;
            }
        }

        Ok((sstate.parse_offset - offset) as usize)
    }

    fn parse_packet(
        &self,
        element: &gst::Element,
        sstate: &mut StreamingState,
        data: &[u8],
    ) -> Result<(usize, Events), gst::FlowError> {
        if sstate.resync {
            return self
                .resync(element, sstate, data)
                .map(|skip| (skip, Events::new()));
        }

        let mut pending_events = Events::new();
        if sstate.need_newsegment
            && sstate.needed_timestamps == 0
            && sstate.first_pcr != gst::CLOCK_TIME_NONE
            && sstate.last_pcr != gst::CLOCK_TIME_NONE
        {
            sstate.need_newsegment = false;
            sstate.queue_data = false;

            let segment = sstate.new_segment(element);
            pending_events.push(Event::NewSegment(sstate.stream_pads(), segment));

            for (pid, stream) in sstate.streams.iter_mut() {
                if stream.pad.is_some() {
                    for buffer in stream.buf_queue.drain(0..) {
                        pending_events.push(Event::Buffer(*pid, buffer))
                    }
                }
            }
        }

        let ret = match sstate.parser.parse(data) {
            Err(ts::ParserError::Incomplete(_)) => unreachable!(),
            Err(ts::ParserError::Corrupt) => {
                gst_warning!(CAT, obj: element, "corrupt packet");
                sstate.resync = true;
                self.resync(element, sstate, data)
                    .map(|skip| (skip, Events::new()))
            }
            Err(ts::ParserError::LostSync) => {
                gst_warning!(CAT, obj: element, "lost sync");
                sstate.resync = true;
                self.resync(element, sstate, data)
                    .map(|skip| (skip, Events::new()))
            }
            Ok((next_input, (packet, ts_data))) => {
                let consumed = data.len() - next_input.len();
                match sstate.handle_packet(element, consumed, &packet, &ts_data) {
                    Ok(events) => Ok((consumed, events)),
                    Err(err) => {
                        element.post_error_message(&err);
                        Err(gst::FlowError::Error)
                    }
                }
            }
        };
        ret.map(|(usize, events)| {
            pending_events.extend(events);
            (usize, pending_events)
        })
    }

    fn resync(
        &self,
        element: &gst::Element,
        sstate: &mut StreamingState,
        data: &[u8],
    ) -> Result<usize, gst::FlowError> {
        if data.len() < sstate.packet_size.unwrap_or(188) * 100 {
            gst_trace!(CAT, obj: element, "synching, need more data");
            return Ok(0);
        }

        let skip = match sstate.parser.sync(data) {
            Some(sync_point) => data.len() - sync_point.len(),
            None => {
                gst_error!(CAT, obj: element, "can't find sync");
                return Err(gst::FlowError::Error);
            }
        };
        sstate.resync = false;

        Ok(skip)
    }

    fn handle_events(&self, element: &gst::Element, events: Events) -> Result<(), gst::FlowError> {
        use Event::*;

        for event in events {
            match event {
                ConfigureProgram(program_number) => self.configure_program(element, program_number),
                NewSegment(pads, segment) => self.send_new_segment(element, pads, &segment),
                Buffer(pid, buffer) => self.send_buffer(element, pid, buffer),
            }
            .map_err(|(error_message, err)| {
                if let Some(e) = error_message {
                    element.post_error_message(&e);
                }
                err
            })?
        }

        Ok(())
    }

    fn configure_program(
        &self,
        element: &gst::Element,
        program_number: u16,
    ) -> Result<(), (Option<gst::ErrorMessage>, gst::FlowError)> {
        let mut state = self.state.lock().unwrap();
        let sstate = state.streaming_state_mut();

        let number = self
            .settings
            .lock()
            .unwrap()
            .program_number
            .or(sstate.program_number);
        if let Some(number) = number {
            if number != program_number {
                return Ok(());
            }
        }
        let new_program = sstate
            .program_number
            .map(|n| n != program_number)
            .unwrap_or(true);
        if new_program {}
        sstate.program_number = Some(program_number);

        let program = sstate.programs.get_mut(&program_number).unwrap();
        let pmt = program.pmt.as_ref().unwrap();
        sstate.pcr_pid = pmt.pcr_pid;

        let prev_streams: HashSet<u16> = sstate.streams.keys().cloned().collect();
        let next_streams: HashSet<u16> = pmt.streams.keys().cloned().collect();
        let to_add: HashSet<u16> = next_streams.difference(&prev_streams).cloned().collect();
        let to_remove: HashSet<u16> = prev_streams.difference(&next_streams).cloned().collect();

        let mut pads_to_add: Vec<(gst::Pad, gst::Caps)> = Vec::new();
        sstate.streams.extend(to_add.iter().cloned().map(|pid| {
            let stream = pmt.streams.get(&pid).unwrap();
            let mut stream_data = StreamData {
                pid,
                pad: None,
                caps: None,
                discont: true,
                first_ts: gst::CLOCK_TIME_NONE,
                buf_queue: Vec::new(),
            };
            if let Some((pad, caps)) = self.create_srcpad(program_number, element, stream) {
                pads_to_add.push((pad.clone(), caps.clone()));
                stream_data.pad = Some(pad);
                stream_data.caps = Some(caps);
            }

            (pid, stream_data)
        }));

        let pads_to_remove: Vec<_> = to_remove
            .iter()
            .filter_map(|pid| sstate.streams.remove(pid).unwrap().pad)
            .collect();

        sstate.queue_data = true;

        gst_info!(CAT, obj: element, "Configured program {}", program_number,);

        if sstate.streams.values().all(|s| s.pad.is_none()) {
            return Err((
                Some(gst_error_msg!(
                    gst::StreamError::Demux,
                    ["no supported streams"]
                )),
                gst::FlowError::Error,
            ));
        }

        for (pad, caps) in &pads_to_add {
            self.setup_srcpad(element, pad, caps);
        }

        for pad in &pads_to_remove {
            self.remove_srcpad(element, pad);
        }

        sstate.needed_timestamps = sstate
            .streams
            .iter()
            .filter(|(_, s)| s.pad.is_some())
            .count();

        drop(state);

        element.no_more_pads();

        Ok(())
    }

    fn create_srcpad(
        &self,
        program_number: u16,
        element: &gst::Element,
        stream: &ts::psi::Stream,
    ) -> Option<(gst::Pad, gst::Caps)> {
        let (template, caps) = match stream.stream_type {
            0x02 => {
                let caps = gst::Caps::new_simple(
                    "video/mpeg",
                    &[("mpegversion", &2), ("systemstream", &false)],
                );
                (&"video", caps)
            }
            0x03 | 0x04 => {
                let caps = gst::Caps::new_simple("audio/mpeg", &[("mpegversion", &1)]);
                (&"audio", caps)
            }
            0x0F => {
                let caps = gst::Caps::new_simple(
                    "audio/mpeg",
                    &[("mpegversion", &2), ("stream-format", &"adts")],
                );
                (&"audio", caps)
            }
            0x84 => {
                let caps = gst::Caps::new_simple("audio/x-ac3", &[]);
                (&"audio", caps)
            }
            0x24 => {
                let caps =
                    gst::Caps::new_simple("video/x-h265", &[("stream-format", &"byte-stream")]);
                (&"video", caps)
            }
            0x1B => {
                let caps = gst::Caps::new_simple(
                    "video/x-h264",
                    &[("stream-format", &"byte-stream"), ("alignment", &"nal")],
                );
                (&"video", caps)
            }
            _ => {
                gst_info!(
                    CAT,
                    obj: element,
                    "unsupported stream type: {:#02x}",
                    stream.stream_type
                );
                return None;
            }
        };

        let name = format!("pad_{}_{}", program_number, stream.pid);
        if element.get_static_pad(&name).is_some() {
            panic!("pad already exists: {}", name)
        }

        let templ = element
            .get_element_class()
            .get_pad_template(template)
            .unwrap();
        let pad = gst::Pad::new_from_template(&templ, Some(&name));
        pad.set_event_function(|pad, parent, event| {
            TsDemux::catch_panic_pad_function(
                parent,
                || false,
                |demux, element| demux.src_event(pad, element, event),
            )
        });
        pad.set_query_function(|pad, parent, query| {
            TsDemux::catch_panic_pad_function(
                parent,
                || false,
                |demux, element| demux.src_query(pad, element, query),
            )
        });

        Some((pad, caps))
    }

    fn setup_srcpad(&self, element: &gst::Element, pad: &gst::Pad, caps: &gst::Caps) {
        let name = &pad.get_name();
        gst_info!(CAT, obj: element, "setting up pad {} caps {}", name, caps);

        pad.set_active(true).unwrap();

        let full_stream_id = pad.create_stream_id(element, Some(name)).unwrap();
        let group_id = if let Some(evt) = self
            .sinkpad
            .get_sticky_event(gst::EventType::StreamStart, 0)
        {
            if let gst::EventView::StreamStart(evt) = evt.view() {
                let group_id = evt.get_group_id();
                Some(group_id)
            } else {
                None
            }
        } else {
            None
        };
        // FIXME: rework stream-start code
        let mut event = gst::Event::new_stream_start(&full_stream_id);
        if let Some(id) = group_id {
            event = event.group_id(id);
        }
        pad.push_event(event.build());
        pad.push_event(gst::Event::new_caps(&caps).build());

        self.flow_combiner.lock().unwrap().add_pad(pad);
        element.add_pad(pad).unwrap();
    }

    fn remove_srcpad(&self, element: &gst::Element, pad: &gst::Pad) {
        gst_info!(CAT, obj: element, "removing pad {}", pad.get_name());
        self.flow_combiner.lock().unwrap().remove_pad(pad);
        pad.push_event(gst::Event::new_eos().build());
        pad.set_active(false).unwrap();
        element.remove_pad(pad).unwrap();
    }

    fn send_new_segment(
        &self,
        element: &gst::Element,
        pads: Vec<gst::Pad>,
        segment: &gst::Segment,
    ) -> Result<(), (Option<gst::ErrorMessage>, gst::FlowError)> {
        let event = gst::Event::new_segment(segment).build();
        gst_info!(CAT, obj: element, "sending new segment {:?}", segment);
        for pad in pads {
            pad.push_event(event.clone());
        }

        Ok(())
    }

    fn send_buffer(
        &self,
        element: &gst::Element,
        pid: u16,
        mut buffer: gst::Buffer,
    ) -> Result<(), (Option<gst::ErrorMessage>, gst::FlowError)> {
        let mut state = self.state.lock().unwrap();
        let sstate = state.streaming_state_mut();
        let mut stream = sstate.streams.get_mut(&pid).unwrap();

        let pad = stream.pad.clone().unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            if stream.discont {
                buffer.set_flags(gst::BufferFlags::DISCONT);
                stream.discont = false;
            } else {
                buffer.set_flags(buffer.get_flags() & !gst::BufferFlags::DISCONT);
            }
            if let Some(DemuxSegment::PullMode(ref mut segment)) = &mut sstate.segment {
                let position = gst::ClockTime::try_from(segment.get_position())
                    .unwrap_or(gst::CLOCK_TIME_NONE);
                let dts = buffer.get_dts();
                let pts = buffer.get_pts();
                for ts in [pts, dts].iter().cloned() {
                    if ts != gst::CLOCK_TIME_NONE
                        && ts >= sstate.first_pcr
                        && (position == gst::CLOCK_TIME_NONE || ts > position)
                    {
                        segment.set_position(ts - sstate.first_pcr);
                    }
                }
            }
        }

        drop(state);

        let res = pad.push(buffer);

        self.flow_combiner
            .lock()
            .unwrap()
            .update_pad_flow(&pad, res)
            .map_err(|err| (None, err))?;

        Ok(())
    }
}

impl StreamingState {
    fn new() -> StreamingState {
        StreamingState {
            parser: ts::Parser::new(),
            pat: None,
            programs: HashMap::new(),
            streams: HashMap::new(),
            pcr_pid: 0,
            program_number: None,
            resync: false,
            offset: 0,
            queue_data: true,
            needed_timestamps: 0,
            packet_size: None,
            first_pcr: gst::CLOCK_TIME_NONE,
            first_pcr_offset: 0,
            last_pcr: gst::CLOCK_TIME_NONE,
            last_pcr_offset: 0,
            parse_offset: 0,
            need_newsegment: true,
            segment: None,
        }
    }

    fn stream_pads(&self) -> Vec<gst::Pad> {
        let pads: Vec<gst::Pad> = self
            .streams
            .values()
            .filter_map(|sd| sd.pad.clone())
            .collect();

        pads
    }

    fn new_segment(&self, element: &gst::Element) -> gst::Segment {
        match &self.segment {
            Some(DemuxSegment::Upstream(segment)) => {
                if segment.get_format() == gst::Format::Time {
                    gst_info!(CAT, obj: element, "using upstream segment as is");
                    segment.clone()
                } else {
                    gst_info!(
                        CAT,
                        obj: element,
                        "translating upstream bytes segment to time {:?}",
                        segment
                    );
                    self.bytes_segment_to_time(
                        segment.downcast_ref::<gst::format::Bytes>().unwrap(),
                    )
                }
            }
            Some(DemuxSegment::PullMode(segment)) => self.rebase_segment(segment, self.first_pcr),
            None => unreachable!(),
        }
    }

    fn rebase_segment(&self, segment: &gst::Segment, first_ts: gst::ClockTime) -> gst::Segment {
        let mut segment = segment.clone();
        let seg = segment.downcast_mut::<gst::ClockTime>().unwrap();
        seg.set_start(first_ts + seg.get_start());
        let stop = seg.get_stop();
        if stop != gst::CLOCK_TIME_NONE {
            seg.set_stop(first_ts + stop);
        }
        seg.set_position(gst::CLOCK_TIME_NONE);

        segment
    }

    fn offset_to_ts(&self, offset: u64) -> Option<gst::ClockTime> {
        if self.first_pcr != gst::CLOCK_TIME_NONE
            && self.last_pcr != gst::CLOCK_TIME_NONE
            && self.last_pcr > self.first_pcr
        {
            if let gst::ClockTime(Some(pcr_diff)) = self.last_pcr - self.first_pcr {
                let offset_diff = self.last_pcr_offset - self.first_pcr_offset;
                return Some(gst::ClockTime(Some(offset * pcr_diff / offset_diff)));
            }
        }
        None
    }

    fn ts_to_offset(&self, ts: gst::ClockTime) -> Option<u64> {
        if ts == gst::CLOCK_TIME_NONE
            || self.first_pcr == gst::CLOCK_TIME_NONE
            || self.last_pcr == gst::CLOCK_TIME_NONE
        {
            return None;
        }
        let ts = ts.0.unwrap();
        if let gst::ClockTime(Some(pcr_diff)) = self.last_pcr - self.first_pcr {
            let offset_diff = self.last_pcr_offset - self.first_pcr_offset;
            let offset = self.first_pcr_offset + ts * offset_diff / pcr_diff;
            return Some(offset);
        }
        None
    }

    fn bytes_segment_to_time(
        &self,
        bytes_seg: &gst::FormattedSegment<gst::format::Bytes>,
    ) -> gst::Segment {
        let mut time_seg = gst::Segment::new();
        time_seg.set_format(gst::Format::Time);

        if let gst::format::Bytes(Some(start)) = bytes_seg.get_start() {
            let start = self.offset_to_ts(start).unwrap();
            time_seg.set_start(start + self.first_pcr);
            time_seg.set_time(start);
        }
        if let gst::format::Bytes(Some(stop)) = bytes_seg.get_stop() {
            let stop = self.offset_to_ts(stop).unwrap();
            time_seg.set_stop(stop + self.first_pcr);
        }

        time_seg
    }

    fn handle_packet(
        &mut self,
        element: &gst::Element,
        size: usize,
        packet: &ts::Packet,
        data: &ts::Data,
    ) -> Result<Events, gst::ErrorMessage> {
        use ts::psi::Section::{PAT, PMT};
        use ts::Data::*;

        Ok(match data {
            PSI(section) => match section {
                PAT(ref pat) if pat.is_complete() => self.handle_pat(element)?,
                PMT(ref pmt) if pmt.is_complete() => {
                    self.handle_pmt(element, pmt.program_number)?
                }
                _ => Events::new(),
            },
            PES(pes_packet, payload) => {
                self.handle_data(element, size, packet, Some(pes_packet), &payload)?
            }
            Data(payload) => self.handle_data(element, size, packet, None, payload)?,
        })
    }

    fn handle_pat(&mut self, element: &gst::Element) -> Result<Events, gst::ErrorMessage> {
        let events = Events::new();
        let pat = self.parser.pat().unwrap();
        let (to_add, to_remove): (Vec<u16>, Vec<u16>) = match &self.pat {
            Some(current_pat) => {
                if current_pat.version_number == pat.version_number {
                    return Ok(events);
                }

                let previous_programs = current_pat.programs();
                let next_programs = pat.programs();
                let to_remove = previous_programs
                    .difference(&next_programs)
                    .cloned()
                    .collect();
                let to_add = next_programs
                    .difference(&previous_programs)
                    .cloned()
                    .collect();
                (to_add, to_remove)
            }
            None => (pat.programs().iter().cloned().collect(), Vec::new()),
        };
        self.pat = Some(pat.clone());

        gst_debug!(CAT, obj: element, "Complete PAT {:?}", self.pat);

        to_remove.iter().for_each(|program_number| {
            self.programs.remove(&program_number);
        });

        self.programs.extend(
            to_add
                .iter()
                .cloned()
                .map(|number| (number, Program { number, pmt: None })),
        );

        Ok(events)
    }

    fn handle_pmt(
        &mut self,
        element: &gst::Element,
        program_number: u16,
    ) -> Result<Events, gst::ErrorMessage> {
        let events = Events::new();
        let program = self
            .programs
            .get_mut(&program_number)
            .expect("got PMT without PAT");

        let pmt = self.parser.get_pmt(program_number).unwrap();
        if let Some(current_pmt) = &program.pmt {
            if current_pmt.version_number == pmt.version_number {
                return Ok(events);
            }
        }

        gst_debug!(CAT, obj: element, "complete PMT: {:?}", pmt,);
        program.pmt = Some(pmt.clone());

        let mut events = Events::new();
        events.push(Event::ConfigureProgram(program_number));
        Ok(events)
    }

    fn handle_data(
        &mut self,
        element: &gst::Element,
        size: usize,
        packet: &ts::Packet,
        pes_packet: Option<&pes::Packet>,
        data: &[u8],
    ) -> Result<Events, gst::ErrorMessage> {
        let mut events = Events::new();

        let mut pcr_ts = gst::CLOCK_TIME_NONE;
        if packet.pid == self.pcr_pid {
            if let Some(pcr) = pcr(&packet) {
                let offset = self.parse_offset + size as u64;
                gst_trace!(
                    CAT,
                    obj: element,
                    "PCR {} offset {} time {}",
                    packet.pid,
                    offset,
                    pcr
                );
                if self.first_pcr == gst::CLOCK_TIME_NONE {
                    gst_info!(CAT, obj: element, "first pcr {} offset {}", pcr, offset);
                    self.first_pcr = pcr;
                    self.first_pcr_offset = offset;
                } else if self.last_pcr == gst::CLOCK_TIME_NONE || pcr > self.last_pcr {
                    self.last_pcr = pcr;
                    self.last_pcr_offset = offset;
                }
                pcr_ts = pcr;
            }
        }
        let stream = match self.streams.get_mut(&packet.pid) {
            Some(stream) => stream,
            _ => return Ok(events),
        };

        if stream.pad.is_none()
            || (self.queue_data && stream.buf_queue.is_empty() && pes_packet.is_none())
        {
            return Ok(events);
        }

        let mut pts = gst::CLOCK_TIME_NONE;
        let mut dts = gst::CLOCK_TIME_NONE;
        if let Some(pes_packet) = pes_packet {
            pts = gst::ClockTime(
                pes_packet
                    .header
                    .as_ref()
                    .and_then(|h| h.pts)
                    .map(|pts| pts * 100_000 / 9),
            );
            dts = gst::ClockTime(
                pes_packet
                    .header
                    .as_ref()
                    .and_then(|h| h.dts)
                    .map(|dts| dts * 100_000 / 9),
            );
        }
        let mut buffer = gst::Buffer::with_size(data.len()).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.copy_from_slice(0, data).unwrap();
            buffer.set_pts(pts);
            buffer.set_dts(dts);
        }

        if self.queue_data {
            stream.buf_queue.push(buffer);
            if stream.first_ts == gst::CLOCK_TIME_NONE {
                assert!(self.needed_timestamps > 0);
                for ts in [pcr_ts, pts, dts].iter().cloned() {
                    if ts != gst::CLOCK_TIME_NONE {
                        gst_info!(CAT, obj: element, "stream: {} first_ts: {}", stream.pid, ts);
                        stream.first_ts = ts;
                        self.needed_timestamps -= 1;
                        break;
                    }
                }
            }
        } else {
            let event = Event::Buffer(stream.pid, buffer);
            events.push(event);
        }
        Ok(events)
    }
}

fn default_seek_segment() -> gst::Segment {
    let mut segment: gst::FormattedSegment<gst::ClockTime> = gst::FormattedSegment::new();
    segment.set_start(0);
    segment.set_position(0);
    segment.set_time(0);
    segment.upcast()
}

fn pcr(packet: &ts::Packet) -> Option<gst::ClockTime> {
    packet
        .adaptation_field()
        .and_then(|af| af.program_clock_reference.as_ref())
        .map(|pcr| {
            let pcr = pcr.base * 300 + pcr.extension as u64;
            gst::ClockTime(Some(pcr * 1000 / 27))
        })
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "nutsdemux",
        gst::Rank::Primary + 100,
        TsDemux::get_type(),
    )
}
