use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use gtk4::Picture;
use reolink_core::{VideoFrame, VideoType};
use std::cell::RefCell;

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

    /// Builds `appsrc ! <codec>parse ! <software decoder> ! queue !
    /// gtk4paintablesink` for the given codec and binds the sink's
    /// paintable to `self.picture`. Always uses the software decoder
    /// (`avdec_h264`/`avdec_h265`, from `gst-libav`) rather than
    /// `decodebin`'s default hardware choice (`nvh264dec`/`nvh265dec` via
    /// NVDEC on this class of system) — real hardware testing 2026-09-14
    /// found the hardware path fatally unstable (`CUDA call failed`,
    /// `Couldn't map picture` from `nvdecoder`, killing the whole pipeline
    /// with `Internal data stream error` roughly a minute into a session),
    /// on top of an interop gap with this sink that an explicit
    /// `cudadownload`+`videoconvert` bridge fixed but didn't make stable.
    /// See the `e21e1d5` commit for that hardware-decode version if this
    /// is worth revisiting once the crash itself is root-caused.
    fn build_pipeline(&self, video_type: VideoType) -> Pipeline {
        let (parse_name, media_type, decoder_name) = match video_type {
            VideoType::H264 => ("h264parse", "video/x-h264", "avdec_h264"),
            VideoType::H265 => ("h265parse", "video/x-h265", "avdec_h265"),
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
        // made every frame's QoS deadline meaningless regardless of `sync`.
        // `do-timestamp` makes appsrc stamp each buffer's PTS from the
        // pipeline clock at the moment it's pushed instead, so `sync: true`
        // (the sink's default) paces playback against real elapsed time
        // like any other live source. Confirmed against real hardware
        // 2026-09-14 alongside the queue below.
        appsrc.set_do_timestamp(true);

        let parse = gstreamer::ElementFactory::make(parse_name)
            .build()
            .unwrap_or_else(|_| panic!("{parse_name} element missing — install gstreamer1.0-plugins-bad"));
        let decoder = gstreamer::ElementFactory::make(decoder_name)
            .build()
            .unwrap_or_else(|_| panic!("{decoder_name} element missing — install gstreamer1.0-libav"));

        // Without a queue, GStreamer's live-pipeline latency calculation
        // has no buffering to work with and collapses to zero ("Pipeline
        // construction is invalid, please add queues" — confirmed via
        // GST_DEBUG=3 against real hardware 2026-09-14). With zero latency
        // budget, a frame's QoS deadline is "decoded and painted instantly,
        // no slack" — any real decode time at all makes it "late", so it
        // gets dropped (`Dropping frame due to QoS`, logged for nearly
        // every frame). `leaky=downstream` with a bounded time window
        // gives decode a real deadline to hit while still discarding
        // backlog (rather than stalling upstream) if painting itself ever
        // falls behind.
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
            .add_many([appsrc.upcast_ref(), &parse, &decoder, &queue, &sink])
            .expect("adding elements failed");
        appsrc.link(&parse).expect("linking appsrc->parse failed");
        parse.link(&decoder).expect("linking parse->decoder failed");
        decoder.link(&queue).expect("linking decoder->queue failed");
        queue.link(&sink).expect("linking queue->sink failed");

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
