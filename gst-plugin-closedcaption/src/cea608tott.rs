// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;

use super::cea608tott_ffi as ffi;
use atomic_refcell::AtomicRefCell;

#[derive(Copy, Clone, Debug)]
enum Format {
    Srt,
    Vtt,
    Raw,
}

struct State {
    format: Option<Format>,
    wrote_header: bool,
    caption_frame: CaptionFrame,
    previous_text: Option<(gst::ClockTime, String)>,
    index: u64,
}

impl Default for State {
    fn default() -> Self {
        State {
            format: None,
            wrote_header: false,
            caption_frame: CaptionFrame::default(),
            previous_text: None,
            index: 1,
        }
    }
}

struct Cea608ToTt {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,

    state: AtomicRefCell<State>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "cea608tott",
        gst::DebugColorFlags::empty(),
        Some("CEA-608 to TT Element"),
    );
}

impl Cea608ToTt {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        _element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.borrow_mut();
        let format = match state.format {
            Some(format) => format,
            None => {
                gst_error!(CAT, obj: pad, "Not negotiated yet");
                return Err(gst::FlowError::NotNegotiated);
            }
        };

        let buffer_pts = buffer.get_pts();
        if buffer_pts.is_none() {
            gst_error!(CAT, obj: pad, "Require timestamped buffers");
            return Err(gst::FlowError::Error);
        }
        let pts = (buffer_pts.unwrap() as f64) / 1_000_000_000.0;

        let data = buffer.map_readable().map_err(|_| {
            gst_error!(CAT, obj: pad, "Can't map buffer readable");

            gst::FlowError::Error
        })?;

        if data.len() < 2 {
            gst_error!(CAT, obj: pad, "Invalid closed caption packet size");

            return Ok(gst::FlowSuccess::Ok);
        }

        let previous_text = match state
            .caption_frame
            .decode((data[0] as u16) << 8 | data[1] as u16, pts)
        {
            Ok(Status::Ok) => return Ok(gst::FlowSuccess::Ok),
            Err(_) => {
                gst_error!(CAT, obj: pad, "Failed to decode closed caption packet");
                return Ok(gst::FlowSuccess::Ok);
            }
            Ok(Status::Clear) => {
                gst_debug!(CAT, obj: pad, "Clearing previous closed caption packet");
                state.previous_text.take()
            }
            Ok(Status::Ready) => {
                gst_debug!(CAT, obj: pad, "Have new closed caption packet");
                let text = match state.caption_frame.to_text() {
                    Ok(text) => text,
                    Err(_) => {
                        gst_error!(CAT, obj: pad, "Failed to convert caption frame to text");
                        return Ok(gst::FlowSuccess::Ok);
                    }
                };

                state.previous_text.replace((buffer_pts, text))
            }
        };

        let previous_text = match previous_text {
            Some(previous_text) => previous_text,
            None => {
                gst_debug!(CAT, obj: pad, "Have no previous text");
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let duration = if buffer_pts > previous_text.0 {
            buffer_pts - previous_text.0
        } else {
            0.into()
        };

        let (timestamp, text) = previous_text;

        let header_buffer = if !state.wrote_header {
            state.wrote_header = true;

            match format {
                Format::Vtt => Some(Self::create_vtt_header(timestamp)),
                Format::Srt | Format::Raw => None,
            }
        } else {
            None
        };

        let buffer = match format {
            Format::Vtt => Self::create_vtt_buffer(timestamp, duration, text),
            Format::Srt => Self::create_srt_buffer(timestamp, duration, state.index, text),
            Format::Raw => Self::create_raw_buffer(timestamp, duration, text),
        };
        state.index += 1;
        drop(state);

        if let Some(header_buffer) = header_buffer {
            self.srcpad.push(header_buffer)?;
        }

        self.srcpad.push(buffer)
    }

    fn create_vtt_header(timestamp: gst::ClockTime) -> gst::Buffer {
        use std::fmt::Write;

        let mut headers = String::new();
        writeln!(&mut headers, "WEBVTT\r").unwrap();
        writeln!(&mut headers, "\r").unwrap();

        let mut buffer = gst::Buffer::from_mut_slice(headers.into_bytes());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(timestamp);
        }

        buffer
    }

    fn split_time(time: gst::ClockTime) -> (u64, u8, u8, u16) {
        let time = time.unwrap();

        let mut s = time / 1_000_000_000;
        let mut m = s / 60;
        let h = m / 60;
        s %= 60;
        m %= 60;
        let ns = time % 1_000_000_000;

        (h as u64, m as u8, s as u8, (ns / 1_000_000) as u16)
    }

    fn create_vtt_buffer(
        timestamp: gst::ClockTime,
        duration: gst::ClockTime,
        text: String,
    ) -> gst::Buffer {
        use std::fmt::Write;

        let mut data = String::new();

        let (h1, m1, s1, ms1) = Self::split_time(timestamp);
        let (h2, m2, s2, ms2) = Self::split_time(timestamp + duration);

        writeln!(
            &mut data,
            "{:02}:{:02}:{:02}.{:03} --> {:02}:{:02}:{:02}.{:03}\r",
            h1, m1, s1, ms1, h2, m2, s2, ms2
        )
        .unwrap();
        writeln!(&mut data, "{}\r", text).unwrap();
        writeln!(&mut data, "\r").unwrap();

        let mut buffer = gst::Buffer::from_mut_slice(data.into_bytes());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(timestamp);
            buffer.set_duration(duration);
        }

        buffer
    }

    fn create_srt_buffer(
        timestamp: gst::ClockTime,
        duration: gst::ClockTime,
        index: u64,
        text: String,
    ) -> gst::Buffer {
        use std::fmt::Write;

        let mut data = String::new();

        let (h1, m1, s1, ms1) = Self::split_time(timestamp);
        let (h2, m2, s2, ms2) = Self::split_time(timestamp + duration);

        writeln!(&mut data, "{:02}\r", index).unwrap();
        writeln!(
            &mut data,
            "{}:{:02}:{:02},{:03} --> {:02}:{:02}:{:02},{:03}\r",
            h1, m1, s1, ms1, h2, m2, s2, ms2
        )
        .unwrap();
        writeln!(&mut data, "{}\r", text).unwrap();
        writeln!(&mut data, "\r").unwrap();

        let mut buffer = gst::Buffer::from_mut_slice(data.into_bytes());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(timestamp);
            buffer.set_duration(duration);
        }

        buffer
    }

    fn create_raw_buffer(
        timestamp: gst::ClockTime,
        duration: gst::ClockTime,
        text: String,
    ) -> gst::Buffer {
        let mut buffer = gst::Buffer::from_mut_slice(text.into_bytes());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(timestamp);
            buffer.set_duration(duration);
        }

        buffer
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Caps(..) => {
                let mut state = self.state.borrow_mut();

                if state.format.is_some() {
                    return true;
                }

                let mut downstream_caps = match self.srcpad.get_allowed_caps() {
                    None => self.srcpad.get_pad_template_caps().unwrap(),
                    Some(caps) => caps,
                };

                if downstream_caps.is_empty() {
                    gst_error!(CAT, obj: pad, "Empty downstream caps");
                    return false;
                }

                downstream_caps.fixate();

                gst_debug!(
                    CAT,
                    obj: pad,
                    "Negotiating for downstream caps {}",
                    downstream_caps
                );

                let s = downstream_caps.get_structure(0).unwrap();
                let new_caps = if s.get_name() == "application/x-subtitle-vtt" {
                    state.format = Some(Format::Vtt);
                    gst::Caps::builder("application/x-subtitle-vtt").build()
                } else if s.get_name() == "application/x-subtitle" {
                    state.format = Some(Format::Srt);
                    gst::Caps::builder("application/x-subtitle").build()
                } else if s.get_name() == "text/x-raw" {
                    state.format = Some(Format::Raw);
                    gst::Caps::builder("text/x-raw")
                        .field("format", &"utf8")
                        .build()
                } else {
                    unreachable!();
                };

                let new_event = gst::Event::new_caps(&new_caps).build();

                return self.srcpad.push_event(new_event);
            }
            EventView::FlushStop(..) => {
                let mut state = self.state.borrow_mut();
                state.caption_frame = CaptionFrame::default();
                state.previous_text = None;
            }
            EventView::Eos(..) => {
                let mut state = self.state.borrow_mut();
                if let Some((timestamp, text)) = state.previous_text.take() {
                    gst_debug!(CAT, obj: pad, "Outputting final text on EOS");

                    let format = state.format.unwrap();

                    let header_buffer = if !state.wrote_header {
                        state.wrote_header = true;

                        match format {
                            Format::Vtt => Some(Self::create_vtt_header(timestamp)),
                            Format::Srt | Format::Raw => None,
                        }
                    } else {
                        None
                    };

                    let buffer = match format {
                        Format::Vtt => Self::create_vtt_buffer(timestamp, 0.into(), text),
                        Format::Srt => {
                            Self::create_srt_buffer(timestamp, 0.into(), state.index, text)
                        }
                        Format::Raw => Self::create_raw_buffer(timestamp, 0.into(), text),
                    };
                    state.index += 1;
                    drop(state);

                    if let Some(header_buffer) = header_buffer {
                        let _ = self.srcpad.push(header_buffer);
                    }

                    let _ = self.srcpad.push(buffer);
                }
            }
            _ => (),
        }

        pad.event_default(Some(element), event)
    }
}

impl ObjectSubclass for Cea608ToTt {
    const NAME: &'static str = "Cea608ToTt";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));

        sinkpad.set_chain_function(|pad, parent, buffer| {
            Cea608ToTt::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |this, element| this.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            Cea608ToTt::catch_panic_pad_function(
                parent,
                || false,
                |this, element| this.sink_event(pad, element, event),
            )
        });

        sinkpad.use_fixed_caps();
        srcpad.use_fixed_caps();

        Self {
            srcpad,
            sinkpad,
            state: AtomicRefCell::new(State::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "CEA-608 to TT",
            "Generic",
            "Converts CEA-608 Closed Captions to SRT/VTT timed text",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();

            // WebVTT
            let s = gst::Structure::builder("application/x-subtitle-vtt").build();
            caps.append_structure(s);

            // SRT
            let s = gst::Structure::builder("application/x-subtitle").build();
            caps.append_structure(s);

            // Raw timed text
            let s = gst::Structure::builder("text/x-raw")
                .field("format", &"utf8")
                .build();
            caps.append_structure(s);
        }

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        let caps = gst::Caps::builder("closedcaption/x-cea-608")
            .field("format", &"raw")
            .build();

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);
    }
}

impl ObjectImpl for Cea608ToTt {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for Cea608ToTt {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
            }
            _ => (),
        }

        let ret = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
            }
            _ => (),
        }

        Ok(ret)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "cea608tott",
        gst::Rank::None,
        Cea608ToTt::get_type(),
    )
}

use std::mem;

#[derive(Copy, Clone, Debug)]
enum Status {
    Ok,
    Ready,
    Clear,
}

#[derive(Copy, Clone, Debug)]
struct Error;

struct CaptionFrame(ffi::caption_frame_t);

unsafe impl Send for CaptionFrame {}
unsafe impl Sync for CaptionFrame {}

impl CaptionFrame {
    fn new() -> Self {
        unsafe {
            let mut frame = mem::MaybeUninit::uninit();
            ffi::caption_frame_init(frame.as_mut_ptr());
            Self(frame.assume_init())
        }
    }

    fn decode(&mut self, cc_data: u16, timestamp: f64) -> Result<Status, Error> {
        unsafe {
            let res = ffi::caption_frame_decode(&mut self.0, cc_data, timestamp);
            match res {
                ffi::libcaption_stauts_t_LIBCAPTION_OK => Ok(Status::Ok),
                ffi::libcaption_stauts_t_LIBCAPTION_READY => Ok(Status::Ready),
                ffi::libcaption_stauts_t_LIBCAPTION_CLEAR => Ok(Status::Clear),
                _ => Err(Error),
            }
        }
    }

    fn to_text(&self) -> Result<String, Error> {
        unsafe {
            let mut data = Vec::with_capacity(ffi::CAPTION_FRAME_TEXT_BYTES as usize);

            let len =
                ffi::caption_frame_to_text(&self.0 as *const _ as *mut _, data.as_ptr() as *mut _);
            data.set_len(len as usize);

            String::from_utf8(data).map_err(|_| Error)
        }
    }
}

impl Default for CaptionFrame {
    fn default() -> Self {
        Self::new()
    }
}