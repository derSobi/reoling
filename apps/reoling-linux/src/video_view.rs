use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use gtk4::Picture;
use reolink_core::{VideoFrame, VideoType};
use std::cell::RefCell;

/// `decodebin` autoplugs the highest-ranked decoder for a format, which on
/// this class of system is a hardware one (`nvh264dec`/`nvh265dec` via
/// NVDEC). Its output is GPU-resident (`video/x-raw(memory:CUDAMemory)`),
/// which `gtk4paintablesink` doesn't accept directly (`GST_CAPS ... not
/// accepted` in `GST_DEBUG`), and with it in the loop `gtk4paintablesink`
/// falls further and further behind the decoder's own output rate for the
/// life of the session (`Dropping frame due to QoS` on the large majority
/// of frames) — visibly choppy playback, confirmed against real hardware
/// 2026-09-14. Promoting the rank of `cudadownload`/`cudaconvert` (present
/// on this system per `gst-inspect-1.0`, but shipped with `Rank::NONE`,
/// which excludes them from autoplugging) was tried as a fix and
/// **disproven** — `decodebin` still didn't insert either one and the
/// exact same caps warning and QoS drops persisted; whatever `decodebin`
/// needs to autoplug a bridge here, rank alone isn't it. Disabling the
/// hardware decoders outright is the only fix confirmed to actually work.
/// At the resolution this app requests by default (the sub-stream)
/// software decoding has CPU headroom to spare; the 4K main stream still
/// struggles in software and remains a known follow-up. Call once, before
/// building any pipeline.
pub fn disable_hardware_video_decoders() {
    for factory_name in ["nvh264dec", "nvh265dec", "nvh264sldec", "nvh265sldec", "vah264dec", "vah265dec"] {
        if let Some(factory) = gstreamer::ElementFactory::find(factory_name) {
            factory.set_rank(gstreamer::Rank::NONE);
        }
    }
}

struct Pipeline {
    appsrc: AppSrc,
    _pipeline: gstreamer::Pipeline,
}

pub struct VideoView {
    picture: Picture,
    // The codec (H.264 vs H.265) is only known once the first frame
    // arrives, so the pipeline is built lazily on first `push_frame`
    // rather than in `new`.
    pipeline: RefCell<Option<Pipeline>>,
}

impl VideoView {
    pub fn new() -> Self {
        Self { picture: Picture::new(), pipeline: RefCell::new(None) }
    }

    pub fn widget(&self) -> &Picture {
        &self.picture
    }

    /// Builds `appsrc ! <codec>parse ! decodebin ! gtk4paintablesink` for
    /// the given codec and binds the sink's paintable to `self.picture`.
    fn build_pipeline(&self, video_type: VideoType) -> Pipeline {
        let (parse_name, media_type) = match video_type {
            VideoType::H264 => ("h264parse", "video/x-h264"),
            VideoType::H265 => ("h265parse", "video/x-h265"),
        };

        let pipeline = gstreamer::Pipeline::new();

        let appsrc = gstreamer::ElementFactory::make("appsrc")
            .build()
            .expect("appsrc element missing — install gstreamer1.0-plugins-base")
            .downcast::<AppSrc>()
            .expect("appsrc is always an AppSrc");
        appsrc.set_caps(Some(
            &gstreamer::Caps::builder(media_type)
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ));
        appsrc.set_is_live(true);
        appsrc.set_format(gstreamer::Format::Time);
        // `frame.microseconds` is the camera's own device-uptime counter,
        // unrelated to this pipeline's clock/base-time — using it as PTS
        // (the previous approach) meant `sync: true` on the sink could
        // defer rendering indefinitely, and disabling sync entirely (also
        // tried) let the decoder flood `gtk4paintablesink` with frames
        // faster than it could paint them ("Have too many pending frames"
        // in GST_DEBUG, confirmed against real hardware 2026-09-14 — the
        // visible stutter was this, not the network transport a prior
        // investigation this same session had already fixed separately).
        // `do-timestamp` makes appsrc stamp each buffer's PTS from the
        // pipeline clock at the moment it's pushed instead, so the default
        // `sync: true` paces playback against real elapsed time like any
        // other live source.
        appsrc.set_do_timestamp(true);

        let parse = gstreamer::ElementFactory::make(parse_name)
            .build()
            .unwrap_or_else(|_| panic!("{parse_name} element missing — install gstreamer1.0-plugins-bad"));
        let decodebin = gstreamer::ElementFactory::make("decodebin")
            .build()
            .expect("decodebin element missing");
        // Without a queue, GStreamer's live-pipeline latency calculation
        // has no buffering to work with and collapses to zero (it says so
        // itself: "Pipeline construction is invalid, please add queues" —
        // confirmed via GST_DEBUG=3 against real hardware 2026-09-14).
        // With zero latency budget, a frame's QoS deadline is "decoded and
        // painted instantly, no slack" — any real decode time at all
        // makes it "late", so it gets dropped (`Dropping frame due to
        // QoS`, logged for nearly every frame). `leaky=downstream` with a
        // bounded time window gives decode a real deadline to hit while
        // still discarding backlog (rather than stalling upstream) if
        // painting itself ever falls behind.
        let queue = gstreamer::ElementFactory::make("queue")
            .property("max-size-time", 500_000_000u64) // 500ms
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .property_from_str("leaky", "downstream")
            .build()
            .expect("queue element missing — install gstreamer1.0-plugins-base");
        let sink = gstreamer::ElementFactory::make("gtk4paintablesink")
            .build()
            .expect("gtk4paintablesink missing — install gstreamer1.0-plugins-good/gtk4 support");

        pipeline
            .add_many([appsrc.upcast_ref(), &parse, &decodebin, &queue, &sink])
            .expect("adding elements failed");
        appsrc.link(&parse).expect("linking appsrc->parse failed");
        parse.link(&decodebin).expect("linking parse->decodebin failed");
        queue.link(&sink).expect("linking queue->sink failed");

        // decodebin exposes its output pad only once it knows the format, so
        // link decodebin->queue lazily.
        let queue_clone = queue.clone();
        decodebin.connect_pad_added(move |_element, pad| {
            let queue_pad = queue_clone.static_pad("sink").expect("queue always has a sink pad");
            if !queue_pad.is_linked() {
                let _ = pad.link(&queue_pad);
            }
        });

        let paintable = sink.property::<gtk4::gdk::Paintable>("paintable");
        self.picture.set_paintable(Some(&paintable));

        pipeline.set_state(gstreamer::State::Playing).expect("failed to start pipeline");

        Pipeline { appsrc, _pipeline: pipeline }
    }

    /// Pushes one raw access unit into the pipeline. PTS is assigned by
    /// `do-timestamp` (see `build_pipeline`), not from `frame.microseconds`.
    pub fn push_frame(&self, frame: &VideoFrame) {
        if self.pipeline.borrow().is_none() {
            let pipeline = self.build_pipeline(frame.video_type);
            *self.pipeline.borrow_mut() = Some(pipeline);
        }

        let buffer = gstreamer::Buffer::from_slice(frame.data.clone());
        let pipeline_ref = self.pipeline.borrow();
        let _ = pipeline_ref.as_ref().unwrap().appsrc.push_buffer(buffer);
    }
}
