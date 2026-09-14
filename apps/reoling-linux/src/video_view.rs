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

    /// Builds `appsrc ! <codec>parse ! <decoder chain> ! queue !
    /// gtk4paintablesink` for the given codec and binds the sink's
    /// paintable to `self.picture`. The decoder chain is built by hand
    /// (not `decodebin`) specifically to control what follows a hardware
    /// decoder — see `decoder_chain`'s own doc comment for why.
    fn build_pipeline(&self, video_type: VideoType) -> Pipeline {
        let (parse_name, media_type, hw_decoder_name, sw_decoder_name) = match video_type {
            VideoType::H264 => ("h264parse", "video/x-h264", "nvh264dec", "avdec_h264"),
            VideoType::H265 => ("h265parse", "video/x-h265", "nvh265dec", "avdec_h265"),
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

        let decode_chain = build_decoder_chain(hw_decoder_name, sw_decoder_name);

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
            .add_many([appsrc.upcast_ref(), &parse])
            .expect("adding appsrc/parse failed");
        pipeline.add_many(decode_chain.elements.iter()).expect("adding decoder chain failed");
        pipeline.add_many([&queue, &sink]).expect("adding queue/sink failed");

        appsrc.link(&parse).expect("linking appsrc->parse failed");
        let mut upstream = parse;
        for element in &decode_chain.elements {
            upstream.link(element).expect("linking decoder chain failed");
            upstream = element.clone();
        }
        upstream.link(&queue).expect("linking decoder chain->queue failed");
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

struct DecoderChain {
    elements: Vec<gstreamer::Element>,
}

/// Builds the decode step by hand instead of delegating to `decodebin`,
/// specifically so a hardware decoder's output gets bridged to something
/// `gtk4paintablesink` can actually consume. `decodebin` auto-selects the
/// highest-ranked decoder for a format — a hardware one (`nvh264dec`/
/// `nvh265dec` via NVDEC) on this class of system — but its output is
/// GPU-resident (`video/x-raw(memory:CUDAMemory)`), which the sink doesn't
/// accept directly, and `decodebin` never bridges the gap: the
/// `cudadownload`/`cudaconvert` elements that would (confirmed present via
/// `gst-inspect-1.0`) ship with `Rank::NONE`, which excludes them from
/// autoplugging — and promoting that rank was tried and disproven
/// (`decodebin` still skipped them). Every element used here
/// (`nvh264dec`/`nvh265dec`, `avdec_h264`/`avdec_h265`, `cudadownload`,
/// `videoconvert`) has a static "Always" src pad per its own
/// `gst-inspect-1.0` output, so this whole chain links immediately with no
/// dynamic pad-added juggling.
fn build_decoder_chain(hw_decoder_name: &str, sw_decoder_name: &str) -> DecoderChain {
    if let Ok(hw_decoder) = gstreamer::ElementFactory::make(hw_decoder_name).build() {
        let cudadownload = gstreamer::ElementFactory::make("cudadownload")
            .build()
            .expect("cudadownload element missing — install gstreamer1.0-plugins-bad with CUDA support");
        let videoconvert = gstreamer::ElementFactory::make("videoconvert")
            .build()
            .expect("videoconvert element missing — install gstreamer1.0-plugins-base");
        DecoderChain { elements: vec![hw_decoder, cudadownload, videoconvert] }
    } else {
        let sw_decoder = gstreamer::ElementFactory::make(sw_decoder_name)
            .build()
            .unwrap_or_else(|_| panic!("{sw_decoder_name} element missing — install gstreamer1.0-libav"));
        DecoderChain { elements: vec![sw_decoder] }
    }
}
