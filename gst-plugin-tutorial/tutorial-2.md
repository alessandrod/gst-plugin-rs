# How to write GStreamer Elements in Rust Part 2: A raw audio sine wave source

In this part, a raw audio sine wave source element is going to be written. The final code can be found [here](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/blob/master/gst-plugin-tutorial/src/sinesrc.rs).

### Table of Contents

1.  [Boilerplate](#boilerplate)
2.  [Caps Negotiation](#caps-negotiation)
3.  [Query Handling](#query-handling)
4.  [Buffer Creation](#buffer-creation)
5.  [(Pseudo) Live Mode](#pseudo-live-mode)
6.  [Unlocking](#unlocking)
7.  [Seeking](#seeking)

### Boilerplate

The first part here will be all the boilerplate required to set up the element. You can safely [skip](#caps-negotiation) this if you remember all this from the [previous tutorial](tutorial-1.md).

Our sine wave element is going to produce raw audio, with a number of channels and any possible sample rate with both 32 bit and 64 bit floating point samples. It will produce a simple sine wave with a configurable frequency, volume/mute and number of samples per audio buffer. In addition it will be possible to configure the element in (pseudo) live mode, meaning that it will only produce data in real-time according to the pipeline clock. And it will be possible to seek to any time/sample position on our source element. It will basically be a more simply version of the [`audiotestsrc`](https://gstreamer.freedesktop.org/data/doc/gstreamer/head/gst-plugins-base-plugins/html/gst-plugins-base-plugins-audiotestsrc.html) element from gst-plugins-base.

So let's get started with all the boilerplate. This time our element will be based on the [`BaseSrc`](https://gstreamer.freedesktop.org/data/doc/gstreamer/head/gstreamer-libs/html/GstBaseSrc.html) base class instead of [`BaseTransform`](https://gstreamer.freedesktop.org/data/doc/gstreamer/head/gstreamer-libs/html/GstBaseTransform.html).

```rust
use glib;
use gst;
use gst::prelude::*;
use gst_base::prelude::*;
use gst_audio;

use byte_slice_cast::*;

use gst_plugin::properties::*;
use gst_plugin::object::*;
use gst_plugin::element::*;
use gst_plugin::base_src::*;

use std::{i32, u32};
use std::sync::Mutex;
use std::ops::Rem;

use num_traits::float::Float;
use num_traits::cast::NumCast;

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
static PROPERTIES: [Property; 5] = [
    Property::UInt(
        "samples-per-buffer",
        "Samples Per Buffer",
        "Number of samples per output buffer",
        (1, u32::MAX),
        DEFAULT_SAMPLES_PER_BUFFER,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "freq",
        "Frequency",
        "Frequency",
        (1, u32::MAX),
        DEFAULT_FREQ,
        PropertyMutability::ReadWrite,
    ),
    Property::Double(
        "volume",
        "Volume",
        "Output volume",
        (0.0, 10.0),
        DEFAULT_VOLUME,
        PropertyMutability::ReadWrite,
    ),
    Property::Boolean(
        "mute",
        "Mute",
        "Mute",
        DEFAULT_MUTE,
        PropertyMutability::ReadWrite,
    ),
    Property::Boolean(
        "is-live",
        "Is Live",
        "(Pseudo) live output",
        DEFAULT_IS_LIVE,
        PropertyMutability::ReadWrite,
    ),
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

// Struct containing all the element data
struct SineSrc {
    cat: gst::DebugCategory,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl SineSrc {
    // Called when a new instance is to be created
    fn new(element: &BaseSrc) -> Box<BaseSrcImpl<BaseSrc>> {
        // Initialize live-ness and notify the base class that
        // we'd like to operate in Time format
        element.set_live(DEFAULT_IS_LIVE);
        element.set_format(gst::Format::Time);

        Box::new(Self {
            cat: gst::DebugCategory::new(
                "rssinesrc",
                gst::DebugColorFlags::empty(),
                Some("Rust Sine Wave Source"),
            ),
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
        })
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
    fn class_init(klass: &mut BaseSrcClass) {
        klass.set_metadata(
            "Sine Wave Source",
            "Source/Audio",
            "Creates a sine wave",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

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
        );
        klass.add_pad_template(src_pad_template);

        // Install all our properties
        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl<BaseSrc> for SineSrc {
    // Called whenever a value of a property is changed. It can be called
    // at any time from any thread.
    fn set_property(&self, obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];
        let element = obj.clone().downcast::<BaseSrc>().unwrap();

        match *prop {
            Property::UInt("samples-per-buffer", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let samples_per_buffer = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: &element,
                    "Changing samples-per-buffer from {} to {}",
                    settings.samples_per_buffer,
                    samples_per_buffer
                );
                settings.samples_per_buffer = samples_per_buffer;
                drop(settings);

                let _ =
                    element.post_message(&gst::Message::new_latency().src(Some(&element)).build());
            }
            Property::UInt("freq", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let freq = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: &element,
                    "Changing freq from {} to {}",
                    settings.freq,
                    freq
                );
                settings.freq = freq;
            }
            Property::Double("volume", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let volume = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: &element,
                    "Changing volume from {} to {}",
                    settings.volume,
                    volume
                );
                settings.volume = volume;
            }
            Property::Boolean("mute", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let mute = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: &element,
                    "Changing mute from {} to {}",
                    settings.mute,
                    mute
                );
                settings.mute = mute;
            }
            Property::Boolean("is-live", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let is_live = value.get().unwrap();
                gst_info!(
                    self.cat,
                    obj: &element,
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
    fn get_property(&self, _obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::UInt("samples-per-buffer", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.samples_per_buffer.to_value())
            }
            Property::UInt("freq", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.freq.to_value())
            }
            Property::Double("volume", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.volume.to_value())
            }
            Property::Boolean("mute", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.mute.to_value())
            }
            Property::Boolean("is-live", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.is_live.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

// Virtual methods of gst::Element. We override none
impl ElementImpl<BaseSrc> for SineSrc { }

impl BaseSrcImpl<BaseSrc> for SineSrc {
    // Called when starting, so we can initialize all stream-related state to its defaults
    fn start(&self, element: &BaseSrc) -> bool {
        // Reset state
        *self.state.lock().unwrap() = Default::default();

        gst_info!(self.cat, obj: element, "Started");

        true
    }

    // Called when shutting down the element so we can release all stream-related state
    fn stop(&self, element: &BaseSrc) -> bool {
        // Reset state
        *self.state.lock().unwrap() = Default::default();

        gst_info!(self.cat, obj: element, "Stopped");

        true
    }
}

struct SineSrcStatic;

// The basic trait for registering the type: This returns a name for the type and registers the
// instance and class initializations functions with the type system, thus hooking everything
// together.
impl ImplTypeStatic<BaseSrc> for SineSrcStatic {
    fn get_name(&self) -> &str {
        "SineSrc"
    }

    fn new(&self, element: &BaseSrc) -> Box<BaseSrcImpl<BaseSrc>> {
        SineSrc::new(element)
    }

    fn class_init(&self, klass: &mut BaseSrcClass) {
        SineSrc::class_init(klass);
    }
}

// Registers the type for our element, and then registers in GStreamer under
// the name "sinesrc" for being able to instantiate it via e.g.
// gst::ElementFactory::make().
pub fn register(plugin: &gst::Plugin) {
    let type_ = register_type(SineSrcStatic);
    gst::Element::register(plugin, "rssinesrc", 0, type_);
}
```

If any of this needs explanation, please see the [previous](tutorial-1.md) and the comments in the code. The explanation for all the structs fields and what they're good for will follow in the next sections.

With all of the above and a small addition to `src/lib.rs` this should compile now.

```rust
mod sinesrc;
[...]

fn plugin_init(plugin: &gst::Plugin) -> bool {
    [...]
    sinesrc::register(plugin);
    true
}
```

Also a couple of new crates have to be added to `Cargo.toml` and `src/lib.rs`, but you best check the code in the [repository](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/tree/master/gst-plugin-tutorial) for details.

### Caps Negotiation

The first part that we have to implement, just like last time, is caps negotiation. We already notified the base class about any caps that we can potentially handle via the caps in the pad template in `class_init` but there are still two more steps of behaviour left that we have to implement.

First of all, we need to get notified whenever the caps that our source is configured for are changing. This will happen once in the very beginning and then whenever the pipeline topology or state changes and new caps would be more optimal for the new situation. This notification happens via the `BaseTransform::set_caps` virtual method.

```rust
    fn set_caps(&self, element: &BaseSrc, caps: &gst::Caps) -> bool {
        use std::f64::consts::PI;

        let info = match gst_audio::AudioInfo::from_caps(caps) {
            None => return false,
            Some(info) => info,
        };

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
            sample_offset: sample_offset,
            sample_stop: sample_stop,
            accumulator: accumulator,
        };

        drop(state);

        let _ = element.post_message(&gst::Message::new_latency().src(Some(element)).build());

        true
    }
```

In here we parse the caps into a [`AudioInfo`](https://slomo.pages.freedesktop.org/rustdocs/gstreamer/gstreamer_audio/struct.AudioInfo.html) and then store that in our internal state, while updating various fields. We tell the base class about the number of bytes each buffer is usually going to hold, and update our current sample position, the stop sample position (when a seek with stop position happens, we need to know when to stop) and our accumulator. This happens by scaling both positions by the old and new sample rate. If we don't have an old sample rate, we assume nanoseconds (this will make more sense once seeking is implemented). The scaling is done with the help of the [`muldiv`](https://crates.io/crates/muldiv) crate, which implements scaling of integer types by a fraction with protection against overflows by doing up to 128 bit integer arithmetic for intermediate values.

The accumulator is the updated based on the current phase of the sine wave at the current sample position.

As a last step we post a new `LATENCY` message on the bus whenever the sample rate has changed. Our latency (in live mode) is going to be the duration of a single buffer, but more about that later.

`BaseSrc` is by default already selecting possible caps for us, if there are multiple options. However these defaults might not be (and often are not) ideal and we should override the default behaviour slightly. This is done in the `BaseSrc::fixate` virtual method.

```rust
    fn fixate(&self, element: &BaseSrc, caps: gst::Caps) -> gst::Caps {
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
        element.parent_fixate(caps)
    }
```

Here we take the caps that are passed in, truncate them (i.e. remove all but the very first [`Structure`](https://slomo.pages.freedesktop.org/rustdocs/gstreamer/gstreamer/structure/struct.Structure.html)) and then manually fixate the sample rate to the closest value to 48kHz. By default, caps fixation would result in the lowest possible sample rate but this is usually not desired.

For good measure, we also fixate the number of channels to the closest value to 1, but this would already be the default behaviour anyway. And then chain up to the parent class' implementation of `fixate`, which for now basically does the same as `Caps::fixate()`. After this, the caps are fixated, i.e. there is only a single `Structure` left and all fields have concrete values (no ranges or sets).

### Query Handling

As our source element will work by generating a new audio buffer from a specific offset, and especially works in `Time` format, we want to notify downstream elements that we don't want to run in `Pull` mode, only in `Push` mode. In addition would prefer sequential reading. However we still allow seeking later. For a source that does not know about `Time`, e.g. a file source, the format would be configured as `Bytes`. Other values than `Time` and `Bytes` generally don't make any sense.

The main difference here is that otherwise the base class would ask us to produce data for arbitrary `Byte` offsets, and we would have to produce data for that. While possible in our case, it's a bit annoying and for other audio sources it's not easily possible at all.

Downstream elements will try to query this very information from us, so we now have to override the default query handling of `BaseSrc` and handle the `SCHEDULING` query differently. Later we will also handle other queries differently.

```rust
    fn query(&self, element: &BaseSrc, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        match query.view_mut() {
            // We only work in Push mode. In Pull mode, create() could be called with
            // arbitrary offsets and we would have to produce for that specific offset
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            _ => BaseSrcImplExt::parent_query(self, element, query),
        }
    }
```

To handle the `SCHEDULING` query specifically, we first have to match on a view (mutable because we want to modify the view) of the query check the type of the query. If it indeed is a scheduling query, we can set the `SEQUENTIAL` flag and specify that we handle only `Push` mode, then return `true` directly as we handled the query already.

In all other cases we fall back to the parent class' implementation of the `query` virtual method.

### Buffer Creation

Now we have everything in place for a working element, apart from the virtual method to actually generate the raw audio buffers with the sine wave. From a high-level `BaseSrc` works by calling the `create` virtual method over and over again to let the subclass produce a buffer until it returns an error or signals the end of the stream.

Let's first talk about how to generate the sine wave samples themselves. As we want to operate on 32 bit and 64 bit floating point numbers, we implement a generic function for generating samples and storing them in a mutable byte slice. This is done with the help of the [`num_traits`](https://crates.io/crates/num-traits) crate, which provides all kinds of useful traits for abstracting over numeric types. In our case we only need the [`Float`](https://docs.rs/num-traits/0.2.0/num_traits/float/trait.Float.html) and [`NumCast`](https://docs.rs/num-traits/0.2.0/num_traits/cast/trait.NumCast.html) traits.

Instead of writing a generic implementation with those traits, it would also be possible to do the same with a simple macro that generates a function for both types. Which approach is nicer is a matter of taste in the end, the compiler output should be equivalent for both cases.

```rust
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

        for chunk in data.chunks_mut(channels as usize) {
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
```

This function takes the mutable byte slice from our buffer as argument, as well as the current value of the accumulator and the relevant settings for generating the sine wave.

As a first step, we "cast" the byte slice to one of the target type (f32 or f64) with the help of the [`byte_slice_cast`](https://crates.io/crates/byte-slice-cast) crate. This ensures that alignment and sizes are all matching and returns a mutable slice of our target type if successful. In case of GStreamer, the buffer alignment is guaranteed to be big enough for our types here and we allocate the buffer of a correct size later.

Now we convert all the parameters to the types we will use later, and store them together with the current accumulator value in local variables. Then we iterate over the whole floating point number slice in chunks with all channels, and fill each channel with the current value of our sine wave.

The sine wave itself is calculated by `val = volume * sin(2 * PI * frequency * (i + accumulator) / rate)`, but we actually calculate it by simply increasing the accumulator by `2 * PI * frequency / rate` for every sample instead of doing the multiplication for each sample. We also make sure that the accumulator always stays between `0` and `2 * PI` to prevent any inaccuracies from floating point numbers to affect our produced samples.

Now that this is done, we need to implement the `BaseSrc::create` virtual method for actually allocating the buffer, setting timestamps and other metadata and it and calling our above function.

```rust
    fn create(
        &self,
        element: &BaseSrc,
        _offset: u64,
        _length: u32,
    ) -> Result<gst::Buffer, gst::FlowReturn> {
        // Keep a local copy of the values of all our properties at this very moment. This
        // ensures that the mutex is never locked for long and the application wouldn't
        // have to block until this function returns when getting/setting property values
        let settings = *self.settings.lock().unwrap();

        // Get a locked reference to our state, i.e. the input and output AudioInfo
        let mut state = self.state.lock().unwrap();
        let info = match state.info {
            None => {
                gst_element_error!(element, gst::CoreError::Negotiation, ["Have no caps yet"]);
                return Err(gst::FlowReturn::NotNegotiated);
            }
            Some(ref info) => info.clone(),
        };

        // If a stop position is set (from a seek), only produce samples up to that
        // point but at most samples_per_buffer samples per buffer
        let n_samples = if let Some(sample_stop) = state.sample_stop {
            if sample_stop <= state.sample_offset {
                gst_log!(self.cat, obj: element, "At EOS");
                return Err(gst::FlowReturn::Eos);
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

        gst_debug!(self.cat, obj: element, "Produced buffer {:?}", buffer);

        Ok(buffer)
    }
```

Just like last time, we start with creating a copy of our properties (settings) and keeping a mutex guard of the internal state around. If the internal state has no `AudioInfo` yet, we error out. This would mean that no caps were negotiated yet, which is something we can't handle and is not really possible in our case.

Next we calculate how many samples we have to generate. If a sample stop position was set by a seek event, we have to generate samples up to at most that point. Otherwise we create at most the number of samples per buffer that were set via the property. Then we allocate a buffer of the corresponding size, with the help of the `bpf` field of the `AudioInfo`, and then set its metadata and fill the samples.

The metadata that is set is the timestamp (PTS), and the duration. The duration is calculated from the difference of the following buffer's timestamp and the current buffer's. By this we ensure that rounding errors are not causing the next buffer's timestamp to have a different timestamp than the sum of the current's and its duration. While this would not be much of a problem in GStreamer (inaccurate and jitterish timestamps are handled just fine), we can prevent it here and do so.

Afterwards we call our previously defined function on the writably mapped buffer and fill it with the sample values.

With all this, the element should already work just fine in any GStreamer-based application, for example `gst-launch-1.0`. Don't forget to set the `GST_PLUGIN_PATH` environment variable correctly like last time. Before running this, make sure to turn down the volume of your speakers/headphones a bit.

```bash
export GST_PLUGIN_PATH=`pwd`/target/debug
gst-launch-1.0 rssinesrc freq=440 volume=0.9 ! audioconvert ! autoaudiosink
```

You should hear a 440Hz sine wave now.

### (Pseudo) Live Mode

Many audio (and video) sources can actually only produce data in real-time and data is produced according to some clock. So far our source element can produce data as fast as downstream is consuming data, but we optionally can change that. We simulate a live source here now by waiting on the pipeline clock, but with a real live source you would only ever be able to have the data in real-time without any need to wait on a clock. And usually that data is produced according to a different clock than the pipeline clock, in which case translation between the two clocks is needed but we ignore this aspect for now. For details check the [GStreamer documentation](https://gstreamer.freedesktop.org/documentation/application-development/advanced/clocks.html).

For working in live mode, we have to add a few different parts in various places. First of all, we implement waiting on the clock in the `create` function.

```rust
    fn create(...)
        [...]
        state.sample_offset += n_samples;
        drop(state);

        // If we're live, we are waiting until the time of the last sample in our buffer has
        // arrived. This is the very reason why we have to report that much latency.
        // A real live-source would of course only allow us to have the data available after
        // that latency, e.g. when capturing from a microphone, and no waiting from our side
        // would be necessary.
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

            let id = clock.new_single_shot_id(wait_until).unwrap();

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
        }

        gst_debug!(self.cat, obj: element, "Produced buffer {:?}", buffer);

        Ok(buffer)
    }
```

To be able to wait on the clock, we first of all need to calculate the clock time until when we want to wait. In our case that will be the clock time right after the end of the last sample in the buffer we just produced. Simply because you can't capture a sample before it was produced.

We calculate the running time from the PTS and duration of the buffer with the help of the currently configured segment and then add the base time of the element on this to get the clock time as result. Please check the [GStreamer documentation](https://gstreamer.freedesktop.org/documentation/application-development/advanced/clocks.html) for details, but in short the running time of a pipeline is the time since the start of the pipeline (or the last reset of the running time) and the running time of a buffer can be calculated from its PTS and the segment, which provides the information to translate between the two. The base time is the clock time when the pipeline went to the `Playing` state, so just an offset.

Next we wait and then return the buffer as before.

Now we also have to tell the base class that we're running in live mode now. This is done by calling `set_live(true)` on the base class before changing the element state from `Ready` to `Paused`. For this we override the `Element::change_state` virtual method.

```rust
impl ElementImpl<BaseSrc> for SineSrc {
    fn change_state(
        &self,
        element: &BaseSrc,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        // Configure live'ness once here just before starting the source
        match transition {
            gst::StateChange::ReadyToPaused => {
                element.set_live(self.settings.lock().unwrap().is_live);
            }
            _ => (),
        }

        element.parent_change_state(transition)
    }
}
```

And as a last step, we also need to notify downstream elements about our [latency](https://gstreamer.freedesktop.org/documentation/application-development/advanced/clocks.html#latency). Live elements always have to report their latency so that synchronization can work correctly. As the clock time of each buffer is equal to the time when it was created, all buffers would otherwise arrive late in the sinks (they would appear as if they should've been played already at the time when they were created). So all the sinks will have to compensate for the latency that it took from capturing to the sink, and they have to do that in a coordinated way (otherwise audio and video would be out of sync if both have different latencies). For this the pipeline is querying each sink for the latency on its own branch, and then configures a global latency on all sinks according to that.

This querying is done with the `LATENCY` query, which we will now also have to handle.

```rust
    fn query(&self, element: &BaseSrc, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        match query.view_mut() {
            // We only work in Push mode. In Pull mode, create() could be called with
            // arbitrary offsets and we would have to produce for that specific offset
            QueryView::Scheduling(ref mut q) => {
                [...]
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
```

The latency that we report is the duration of a single audio buffer, because we're simulating a real live source here. A real live source won't be able to output the buffer before the last sample of it is captured, and the difference between when the first and last sample were captured is exactly the latency that we add here. Other elements further downstream that introduce further latency would then add their own latency on top of this.

Inside the latency query we also signal that we are indeed a live source, and additionally how much buffering we can do (in our case, infinite) until data would be lost. The last part is important if e.g. the video branch has a higher latency, causing the audio sink to have to wait some additional time (so that audio and video stay in sync), which would then require the whole audio branch to buffer some data. As we have an artificial live source, we can always generate data for the next time but a real live source would only have a limited buffer and if no data is read and forwarded once that runs full, data would get lost.

You can test this again with e.g. `gst-launch-1.0` by setting the `is-live`property to true. It should write in the output now that the pipeline is live.

`audiotestsrc` element also does it via `get_times` virtual method. But as this is only really useful for pseudo live sources like this one, we decided to explain how waiting on the clock can be achieved correctly and even more important how that relates to the next section.

### Unlocking

With the addition of the live mode, the `create` function is now blocking and waiting on the clock for some time. This is suboptimal as for example a (flushing) seek would have to wait now until the clock waiting is done, or when shutting down the application would have to wait.

To prevent this, all waiting/blocking in GStreamer streaming threads should be interruptible/cancellable when requested. And for example the `ClockID` that we got from the clock for waiting can be cancelled by calling `unschedule()` on it. We only have to do it from the right place and keep it accessible. The right place is the `BaseSrc::unlock` virtual method.

```rust
struct ClockWait {
    clock_id: Option<gst::ClockId>,
    flushing: bool,
}

struct SineSrc {
    cat: gst::DebugCategory,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    clock_wait: Mutex<ClockWait>,
}

[...]

    fn unlock(&self, element: &BaseSrc) -> bool {
        // This should unblock the create() function ASAP, so we
        // just unschedule the clock it here, if any.
        gst_debug!(self.cat, obj: element, "Unlocking");
        let mut clock_wait = self.clock_wait.lock().unwrap();
        if let Some(clock_id) = clock_wait.clock_id.take() {
            clock_id.unschedule();
        }
        clock_wait.flushing = true;

        true
    }
```

We store the clock ID in our struct, together with a boolean to signal whether we're supposed to flush already or not. And then inside `unlock`unschedule the clock ID and set this boolean flag to true.

Once everything is unlocked, we need to reset things again so that data flow can happen in the future. This is done in the `unlock_stop` virtual method.

```rust
    fn unlock_stop(&self, element: &BaseSrc) -> bool {
        // This signals that unlocking is done, so we can reset
        // all values again.
        gst_debug!(self.cat, obj: element, "Unlock stop");
        let mut clock_wait = self.clock_wait.lock().unwrap();
        clock_wait.flushing = false;

        true
    }
```

To make sure that this struct is always initialized correctly, we also call `unlock` from `stop`, and `unlock_stop` from `start`.

Now as a last step, we need to actually make use of the new struct we added around the code where we wait for the clock.

```rust
            // Store the clock ID in our struct unless we're flushing anyway.
            // This allows to asynchronously cancel the waiting from unlock()
            // so that we immediately stop waiting on e.g. shutdown.
            let mut clock_wait = self.clock_wait.lock().unwrap();
            if clock_wait.flushing {
                gst_debug!(self.cat, obj: element, "Flushing");
                return Err(gst::FlowReturn::Flushing);
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
            if res == gst::ClockReturn::Unscheduled {
                gst_debug!(self.cat, obj: element, "Flushing");
                return Err(gst::FlowReturn::Flushing);
            }
```

The important part in this code is that we first have to check if we are already supposed to unlock, before even starting to wait. Otherwise we would start waiting without anybody ever being able to unlock. Then we need to store the clock id in the struct and make sure to drop the mutex guard so that the `unlock` function can take it again for unscheduling the clock ID. And once waiting is done, we need to remove the clock id from the struct again and in case of `ClockReturn::Unscheduled` we directly return `FlowReturn::Flushing` instead of the error.

Similarly when using other blocking APIs it is important that they are woken up in a similar way when `unlock` is called. Otherwise the application developer's and thus user experience will be far from ideal.

### Seeking

As a last feature we implement seeking on our source element. In our case that only means that we have to update the `sample_offset` and `sample_stop` fields accordingly, other sources might have to do more work than that.

Seeking is implemented in the `BaseSrc::do_seek` virtual method, and signalling whether we can actually seek in the `is_seekable` virtual method.

```rust
    fn is_seekable(&self, _element: &BaseSrc) -> bool {
        true
    }

    fn do_seek(&self, element: &BaseSrc, segment: &mut gst::Segment) -> bool {
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
                sample_offset: sample_offset,
                sample_stop: sample_stop,
                accumulator: accumulator,
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
                sample_offset: sample_offset,
                sample_stop: sample_stop,
                accumulator: accumulator,
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
```

Currently no support for reverse playback is implemented here, that is left as an exercise for the reader. So as a first step we check if the segment has a negative rate, in which case we just fail and return false.

Afterwards we again take a copy of the settings, keep a mutable mutex guard of our state and then start handling the actual seek.

If no caps are known yet, i.e. the `AudioInfo` is `None`, we assume a rate of 1 billion. That is, we just store the time in nanoseconds for now and let the `set_caps` function take care of that (which we already implemented accordingly) once the sample rate is known.

Then, if a `Time` seek is performed, we convert the segment start and stop position from time to sample offsets and save them. And then update the accumulator in a similar way as in the `set_caps` function. If a seek is in `Default` format (i.e. sample offsets for raw audio), we just have to store the values and update the accumulator but only do so if the sample rate is known already. A sample offset seek does not make any sense until the sample rate is known, so we just fail here to prevent unexpected surprises later.

Try the following pipeline for testing seeking. You should be able to seek the current time drawn over the video, and with the left/right cursor key you can seek. Also this shows that we create a quite nice sine wave.

```bash
gst-launch-1.0  rssinesrc  !  audioconvert  !  monoscope  !  timeoverlay  !  navseek  !  glimagesink
```

And with that all features are implemented in our sine wave raw audio source.