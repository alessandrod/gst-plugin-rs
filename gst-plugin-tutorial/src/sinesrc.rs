// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_audio;
use gst_base;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use byte_slice_cast::*;

use std::ops::Rem;
use std::sync::Mutex;
use std::{i32, u32};

use num_traits::cast::NumCast;
use num_traits::float::Float;

// Default values of properties
const DEFAULT_SAMPLES_PER_BUFFER: u32 = 1024;
const DEFAULT_FREQ: u32 = 440;
const DEFAULT_VOLUME: f64 = 0.8;
const DEFAULT_MUTE: bool = false;
const DEFAULT_IS_LIVE: bool = false;

// Property value storage
#[derive(Debug, Clone, Copy)]
struct Settings {
    samples_per_buffer: u32,
    freq: u32,
    volume: f64,
    mute: bool,
    is_live: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            samples_per_buffer: DEFAULT_SAMPLES_PER_BUFFER,
            freq: DEFAULT_FREQ,
            volume: DEFAULT_VOLUME,
            mute: DEFAULT_MUTE,
            is_live: DEFAULT_IS_LIVE,
        }
    }
}

// Metadata for the properties
static PROPERTIES: [subclass::Property; 5] = [
    subclass::Property("samples-per-buffer", |name| {
        glib::ParamSpec::uint(
            name,
            "Samples Per Buffer",
            "Number of samples per output buffer",
            1,
            u32::MAX,
            DEFAULT_SAMPLES_PER_BUFFER,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("freq", |name| {
        glib::ParamSpec::uint(
            name,
            "Frequency",
            "Frequency",
            1,
            u32::MAX,
            DEFAULT_FREQ,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("volume", |name| {
        glib::ParamSpec::double(
            name,
            "Volume",
            "Output volume",
            0.0,
            10.0,
            DEFAULT_VOLUME,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("mute", |name| {
        glib::ParamSpec::boolean(
            name,
            "Mute",
            "Mute",
            DEFAULT_MUTE,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("is-live", |name| {
        glib::ParamSpec::boolean(
            name,
            "Is Live",
            "(Pseudo) live output",
            DEFAULT_IS_LIVE,
            glib::ParamFlags::READWRITE,
        )
    }),
];

// Stream-specific state, i.e. audio format configuration
// and sample offset
struct State {
    info: Option<gst_audio::AudioInfo>,
    sample_offset: u64,
    sample_stop: Option<u64>,
    accumulator: f64,
}

impl Default for State {
    fn default() -> State {
        State {
            info: None,
            sample_offset: 0,
            sample_stop: None,
            accumulator: 0.0,
        }
    }
}

struct ClockWait {
    clock_id: Option<gst::ClockId>,
    flushing: bool,
}

// Struct containing all the element data
struct SineSrc {
    cat: gst::DebugCategory,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    clock_wait: Mutex<ClockWait>,
}

impl SineSrc {
    fn process<F: Float + FromByteSlice>(
        data: &mut [u8],
        accumulator_ref: &mut f64,
        freq: u32,
        rate: u32,
        channels: u32,
        vol: f64,
    ) {
        use std::f64::consts::PI;

        // Reinterpret our byte-slice as a slice containing elements of the type
        // we're interested in. GStreamer requires for raw audio that the alignment
        // of memory is correct, so this will never ever fail unless there is an
        // actual bug elsewhere.
        let data = data.as_mut_slice_of::<F>().unwrap();

        // Convert all our parameters to the target type for calculations
        let vol: F = NumCast::from(vol).unwrap();
        let freq = freq as f64;
        let rate = rate as f64;
        let two_pi = 2.0 * PI;

        // We're carrying a accumulator with up to 2pi around instead of working
        // on the sample offset. High sample offsets cause too much inaccuracy when
        // converted to floating point numbers and then iterated over in 1-steps
        let mut accumulator = *accumulator_ref;
        let step = two_pi * freq / rate;

        for chunk in data.chunks_exact_mut(channels as usize) {
            let value = vol * F::sin(NumCast::from(accumulator).unwrap());
            for sample in chunk {
                *sample = value;
            }

            accumulator += step;
            if accumulator >= two_pi {
                accumulator -= two_pi;
            }
        }

        *accumulator_ref = accumulator;
    }
}

// This trait registers our type with the GObject object system and
// provides the entry points for creating a new instance and setting
// up the class data
impl ObjectSubclass for SineSrc {
    const NAME: &'static str = "RsSineSrc";
    type ParentType = gst_base::BaseSrc;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    // This macro provides some boilerplate.
    glib_object_subclass!();

    // Called when a new instance is to be created. We need to return an instance
    // of our struct here.
    fn new() -> Self {
        Self {
            cat: gst::DebugCategory::new(
                "rssinesrc",
                gst::DebugColorFlags::empty(),
                Some("Rust Sine Wave Source"),
            ),
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
            clock_wait: Mutex::new(ClockWait {
                clock_id: None,
                flushing: true,
            }),
        }
    }

    // Called exactly once when registering the type. Used for
    // setting up metadata for all instances, e.g. the name and
    // classification and the pad templates with their caps.
    //
    // Actual instances can create pads based on those pad templates
    // with a subset of the caps given here. In case of basesrc,
    // a "src" and "sink" pad template are required here and the base class
    // will automatically instantiate pads for them.
    //
    // Our element here can output f32 and f64
    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        // Set the element specific metadata. This information is what
        // is visible from gst-inspect-1.0 and can also be programatically
        // retrieved from the gst::Registry after initial registration
        // without having to load the plugin in memory.
        klass.set_metadata(
            "Sine Wave Source",
            "Source/Audio",
            "Creates a sine wave",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        // Create and add pad templates for our sink and source pad. These
        // are later used for actually creating the pads and beforehand
        // already provide information to GStreamer about all possible
        // pads that could exist for this type.

        // On the src pad, we can produce F32/F64 with any sample rate
        // and any number of channels
        let caps = gst::Caps::new_simple(
            "audio/x-raw",
            &[
                (
                    "format",
                    &gst::List::new(&[
                        &gst_audio::AUDIO_FORMAT_F32.to_string(),
                        &gst_audio::AUDIO_FORMAT_F64.to_string(),
                    ]),
                ),
                ("layout", &"interleaved"),
                ("rate", &gst::IntRange::<i32>::new(1, i32::MAX)),
                ("channels", &gst::IntRange::<i32>::new(1, i32::MAX)),
            ],
        );
        // The src pad template must be named "src" for basesrc
        // and specific a pad that is always there
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        // Install all our properties
        klass.install_properties(&PROPERTIES);
    }
}

// Implementation of glib::Object virtual methods
impl ObjectImpl for SineSrc {
    // This macro provides some boilerplate.
    glib_object_impl!();

    // Called right after construction of a new instance
    fn constructed(&self, obj: &glib::Object) {
        // Call the parent class' ::constructed() implementation first
        self.parent_constructed(obj);

        // Initialize live-ness and notify the base class that
        // we'd like to operate in Time format
        let basesrc = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();
        basesrc.set_live(DEFAULT_IS_LIVE);
        basesrc.set_format(gst::Format::Time);
    }

    // Called whenever a value of a property is changed. It can be called
    // at any time from any thread.
    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        let basesrc = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();

        match *prop {
            subclass::Property("samples-per-buffer", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let samples_per_buffer = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: basesrc,
                    "Changing samples-per-buffer from {} to {}",
                    settings.samples_per_buffer,
                    samples_per_buffer
                );
                settings.samples_per_buffer = samples_per_buffer;
                drop(settings);

                let _ =
                    basesrc.post_message(&gst::Message::new_latency().src(Some(basesrc)).build());
            }
            subclass::Property("freq", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let freq = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: basesrc,
                    "Changing freq from {} to {}",
                    settings.freq,
                    freq
                );
                settings.freq = freq;
            }
            subclass::Property("volume", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let volume = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: basesrc,
                    "Changing volume from {} to {}",
                    settings.volume,
                    volume
                );
                settings.volume = volume;
            }
            subclass::Property("mute", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let mute = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: basesrc,
                    "Changing mute from {} to {}",
                    settings.mute,
                    mute
                );
                settings.mute = mute;
            }
            subclass::Property("is-live", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let is_live = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: basesrc,
                    "Changing is-live from {} to {}",
                    settings.is_live,
                    is_live
                );
                settings.is_live = is_live;
            }
            _ => unimplemented!(),
        }
    }

    // Called whenever a value of a property is read. It can be called
    // at any time from any thread.
    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("samples-per-buffer", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.samples_per_buffer.to_value())
            }
            subclass::Property("freq", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.freq.to_value())
            }
            subclass::Property("volume", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.volume.to_value())
            }
            subclass::Property("mute", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.mute.to_value())
            }
            subclass::Property("is-live", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.is_live.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

// Implementation of gst::Element virtual methods
impl ElementImpl for SineSrc {
    // Called whenever the state of the element should be changed. This allows for
    // starting up the element, allocating/deallocating resources or shutting down
    // the element again.
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let basesrc = element.downcast_ref::<gst_base::BaseSrc>().unwrap();

        // Configure live'ness once here just before starting the source
        match transition {
            gst::StateChange::ReadyToPaused => {
                basesrc.set_live(self.settings.lock().unwrap().is_live);
            }
            _ => (),
        }

        // Call the parent class' implementation of ::change_state()
        self.parent_change_state(element, transition)
    }
}

// Implementation of gst_base::BaseSrc virtual methods
impl BaseSrcImpl for SineSrc {
    // Called whenever the input/output caps are changing, i.e. in the very beginning before data
    // flow happens and whenever the situation in the pipeline is changing. All buffers after this
    // call have the caps given here.
    //
    // We simply remember the resulting AudioInfo from the caps to be able to use this for knowing
    // the sample rate, etc. when creating buffers
    fn set_caps(
        &self,
        element: &gst_base::BaseSrc,
        caps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        use std::f64::consts::PI;

        let info = gst_audio::AudioInfo::from_caps(caps).ok_or_else(|| {
            gst_loggable_error!(self.cat, "Failed to build `AudioInfo` from caps {}", caps)
        })?;

        gst_debug!(self.cat, obj: element, "Configuring for caps {}", caps);

        element.set_blocksize(info.bpf() * (*self.settings.lock().unwrap()).samples_per_buffer);

        let settings = *self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();

        // If we have no caps yet, any old sample_offset and sample_stop will be
        // in nanoseconds
        let old_rate = match state.info {
            Some(ref info) => info.rate() as u64,
            None => gst::SECOND_VAL,
        };

        // Update sample offset and accumulator based on the previous values and the
        // sample rate change, if any
        let old_sample_offset = state.sample_offset;
        let sample_offset = old_sample_offset
            .mul_div_floor(info.rate() as u64, old_rate)
            .unwrap();

        let old_sample_stop = state.sample_stop;
        let sample_stop =
            old_sample_stop.map(|v| v.mul_div_floor(info.rate() as u64, old_rate).unwrap());

        let accumulator =
            (sample_offset as f64).rem(2.0 * PI * (settings.freq as f64) / (info.rate() as f64));

        *state = State {
            info: Some(info),
            sample_offset,
            sample_stop,
            accumulator,
        };

        drop(state);

        let _ = element.post_message(&gst::Message::new_latency().src(Some(element)).build());

        Ok(())
    }

    // Called when starting, so we can initialize all stream-related state to its defaults
    fn start(&self, element: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        // Reset state
        *self.state.lock().unwrap() = Default::default();
        self.unlock_stop(element)?;

        gst_info!(self.cat, obj: element, "Started");

        Ok(())
    }

    // Called when shutting down the element so we can release all stream-related state
    fn stop(&self, element: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        // Reset state
        *self.state.lock().unwrap() = Default::default();
        self.unlock(element)?;

        gst_info!(self.cat, obj: element, "Stopped");

        Ok(())
    }

    fn query(&self, element: &gst_base::BaseSrc, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        match query.view_mut() {
            // We only work in Push mode. In Pull mode, create() could be called with
            // arbitrary offsets and we would have to produce for that specific offset
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            // In Live mode we will have a latency equal to the number of samples in each buffer.
            // We can't output samples before they were produced, and the last sample of a buffer
            // is produced that much after the beginning, leading to this latency calculation
            QueryView::Latency(ref mut q) => {
                let settings = *self.settings.lock().unwrap();
                let state = self.state.lock().unwrap();

                if let Some(ref info) = state.info {
                    let latency = gst::SECOND
                        .mul_div_floor(settings.samples_per_buffer as u64, info.rate() as u64)
                        .unwrap();
                    gst_debug!(self.cat, obj: element, "Returning latency {}", latency);
                    q.set(settings.is_live, latency, gst::CLOCK_TIME_NONE);
                    true
                } else {
                    false
                }
            }
            _ => BaseSrcImplExt::parent_query(self, element, query),
        }
    }

    // Creates the audio buffers
    fn create(
        &self,
        element: &gst_base::BaseSrc,
        _offset: u64,
        _length: u32,
    ) -> Result<gst::Buffer, gst::FlowError> {
        // Keep a local copy of the values of all our properties at this very moment. This
        // ensures that the mutex is never locked for long and the application wouldn't
        // have to block until this function returns when getting/setting property values
        let settings = *self.settings.lock().unwrap();

        // Get a locked reference to our state, i.e. the input and output AudioInfo
        let mut state = self.state.lock().unwrap();
        let info = match state.info {
            None => {
                gst_element_error!(element, gst::CoreError::Negotiation, ["Have no caps yet"]);
                return Err(gst::FlowError::NotNegotiated);
            }
            Some(ref info) => info.clone(),
        };

        // If a stop position is set (from a seek), only produce samples up to that
        // point but at most samples_per_buffer samples per buffer
        let n_samples = if let Some(sample_stop) = state.sample_stop {
            if sample_stop <= state.sample_offset {
                gst_log!(self.cat, obj: element, "At EOS");
                return Err(gst::FlowError::Eos);
            }

            sample_stop - state.sample_offset
        } else {
            settings.samples_per_buffer as u64
        };

        // Allocate a new buffer of the required size, update the metadata with the
        // current timestamp and duration and then fill it according to the current
        // caps
        let mut buffer =
            gst::Buffer::with_size((n_samples as usize) * (info.bpf() as usize)).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();

            // Calculate the current timestamp (PTS) and the next one,
            // and calculate the duration from the difference instead of
            // simply the number of samples to prevent rounding errors
            let pts = state
                .sample_offset
                .mul_div_floor(gst::SECOND_VAL, info.rate() as u64)
                .unwrap()
                .into();
            let next_pts: gst::ClockTime = (state.sample_offset + n_samples)
                .mul_div_floor(gst::SECOND_VAL, info.rate() as u64)
                .unwrap()
                .into();
            buffer.set_pts(pts);
            buffer.set_duration(next_pts - pts);

            // Map the buffer writable and create the actual samples
            let mut map = buffer.map_writable().unwrap();
            let data = map.as_mut_slice();

            if info.format() == gst_audio::AUDIO_FORMAT_F32 {
                Self::process::<f32>(
                    data,
                    &mut state.accumulator,
                    settings.freq,
                    info.rate(),
                    info.channels(),
                    settings.volume,
                );
            } else {
                Self::process::<f64>(
                    data,
                    &mut state.accumulator,
                    settings.freq,
                    info.rate(),
                    info.channels(),
                    settings.volume,
                );
            }
        }
        state.sample_offset += n_samples;
        drop(state);

        // If we're live, we are waiting until the time of the last sample in our buffer has
        // arrived. This is the very reason why we have to report that much latency.
        // A real live-source would of course only allow us to have the data available after
        // that latency, e.g. when capturing from a microphone, and no waiting from our side
        // would be necessary..
        //
        // Waiting happens based on the pipeline clock, which means that a real live source
        // with its own clock would require various translations between the two clocks.
        // This is out of scope for the tutorial though.
        if element.is_live() {
            let clock = match element.get_clock() {
                None => return Ok(buffer),
                Some(clock) => clock,
            };

            let segment = element
                .get_segment()
                .downcast::<gst::format::Time>()
                .unwrap();
            let base_time = element.get_base_time();
            let running_time = segment.to_running_time(buffer.get_pts() + buffer.get_duration());

            // The last sample's clock time is the base time of the element plus the
            // running time of the last sample
            let wait_until = running_time + base_time;
            if wait_until.is_none() {
                return Ok(buffer);
            }

            // Store the clock ID in our struct unless we're flushing anyway.
            // This allows to asynchronously cancel the waiting from unlock()
            // so that we immediately stop waiting on e.g. shutdown.
            let mut clock_wait = self.clock_wait.lock().unwrap();
            if clock_wait.flushing {
                gst_debug!(self.cat, obj: element, "Flushing");
                return Err(gst::FlowError::Flushing);
            }

            let id = clock.new_single_shot_id(wait_until).unwrap();
            clock_wait.clock_id = Some(id.clone());
            drop(clock_wait);

            gst_log!(
                self.cat,
                obj: element,
                "Waiting until {}, now {}",
                wait_until,
                clock.get_time()
            );
            let (res, jitter) = id.wait();
            gst_log!(
                self.cat,
                obj: element,
                "Waited res {:?} jitter {}",
                res,
                jitter
            );
            self.clock_wait.lock().unwrap().clock_id.take();

            // If the clock ID was unscheduled, unlock() was called
            // and we should return Flushing immediately.
            if res == Err(gst::ClockError::Unscheduled) {
                gst_debug!(self.cat, obj: element, "Flushing");
                return Err(gst::FlowError::Flushing);
            }
        }

        gst_debug!(self.cat, obj: element, "Produced buffer {:?}", buffer);

        Ok(buffer)
    }

    fn fixate(&self, element: &gst_base::BaseSrc, caps: gst::Caps) -> gst::Caps {
        // Fixate the caps. BaseSrc will do some fixation for us, but
        // as we allow any rate between 1 and MAX it would fixate to 1. 1Hz
        // is generally not a useful sample rate.
        //
        // We fixate to the closest integer value to 48kHz that is possible
        // here, and for good measure also decide that the closest value to 1
        // channel is good.
        let mut caps = gst::Caps::truncate(caps);
        {
            let caps = caps.make_mut();
            let s = caps.get_mut_structure(0).unwrap();
            s.fixate_field_nearest_int("rate", 48_000);
            s.fixate_field_nearest_int("channels", 1);
        }

        // Let BaseSrc fixate anything else for us. We could've alternatively have
        // called Caps::fixate() here
        self.parent_fixate(element, caps)
    }

    fn is_seekable(&self, _element: &gst_base::BaseSrc) -> bool {
        true
    }

    fn do_seek(&self, element: &gst_base::BaseSrc, segment: &mut gst::Segment) -> bool {
        // Handle seeking here. For Time and Default (sample offset) seeks we can
        // do something and have to update our sample offset and accumulator accordingly.
        //
        // Also we should remember the stop time (so we can stop at that point), and if
        // reverse playback is requested. These values will all be used during buffer creation
        // and for calculating the timestamps, etc.

        if segment.get_rate() < 0.0 {
            gst_error!(self.cat, obj: element, "Reverse playback not supported");
            return false;
        }

        let settings = *self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();

        // We store sample_offset and sample_stop in nanoseconds if we
        // don't know any sample rate yet. It will be converted correctly
        // once a sample rate is known.
        let rate = match state.info {
            None => gst::SECOND_VAL,
            Some(ref info) => info.rate() as u64,
        };

        if let Some(segment) = segment.downcast_ref::<gst::format::Time>() {
            use std::f64::consts::PI;

            let sample_offset = segment
                .get_start()
                .unwrap()
                .mul_div_floor(rate, gst::SECOND_VAL)
                .unwrap();

            let sample_stop = segment
                .get_stop()
                .map(|v| v.mul_div_floor(rate, gst::SECOND_VAL).unwrap());

            let accumulator =
                (sample_offset as f64).rem(2.0 * PI * (settings.freq as f64) / (rate as f64));

            gst_debug!(
                self.cat,
                obj: element,
                "Seeked to {}-{:?} (accum: {}) for segment {:?}",
                sample_offset,
                sample_stop,
                accumulator,
                segment
            );

            *state = State {
                info: state.info.clone(),
                sample_offset,
                sample_stop,
                accumulator,
            };

            true
        } else if let Some(segment) = segment.downcast_ref::<gst::format::Default>() {
            use std::f64::consts::PI;

            if state.info.is_none() {
                gst_error!(
                    self.cat,
                    obj: element,
                    "Can only seek in Default format if sample rate is known"
                );
                return false;
            }

            let sample_offset = segment.get_start().unwrap();
            let sample_stop = segment.get_stop().0;

            let accumulator =
                (sample_offset as f64).rem(2.0 * PI * (settings.freq as f64) / (rate as f64));

            gst_debug!(
                self.cat,
                obj: element,
                "Seeked to {}-{:?} (accum: {}) for segment {:?}",
                sample_offset,
                sample_stop,
                accumulator,
                segment
            );

            *state = State {
                info: state.info.clone(),
                sample_offset,
                sample_stop,
                accumulator,
            };

            true
        } else {
            gst_error!(
                self.cat,
                obj: element,
                "Can't seek in format {:?}",
                segment.get_format()
            );

            false
        }
    }

    fn unlock(&self, element: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        // This should unblock the create() function ASAP, so we
        // just unschedule the clock it here, if any.
        gst_debug!(self.cat, obj: element, "Unlocking");
        let mut clock_wait = self.clock_wait.lock().unwrap();
        if let Some(clock_id) = clock_wait.clock_id.take() {
            clock_id.unschedule();
        }
        clock_wait.flushing = true;

        Ok(())
    }

    fn unlock_stop(&self, element: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        // This signals that unlocking is done, so we can reset
        // all values again.
        gst_debug!(self.cat, obj: element, "Unlock stop");
        let mut clock_wait = self.clock_wait.lock().unwrap();
        clock_wait.flushing = false;

        Ok(())
    }
}

// Registers the type for our element, and then registers in GStreamer under
// the name "sinesrc" for being able to instantiate it via e.g.
// gst::ElementFactory::make().
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rssinesrc",
        gst::Rank::None,
        SineSrc::get_type(),
    )
}
