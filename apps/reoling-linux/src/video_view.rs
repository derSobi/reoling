use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use gtk4::Picture;
use reolink_core::{VideoFrame, VideoType};
use std::cell::{Cell, RefCell};

/// How far into the pipeline's running time the very first frame is
/// scheduled to display — the jitter buffer. Frames arrive from the
/// camera over UDP/P2P (or even TCP under real network jitter) with
/// uneven spacing even though the camera itself captures at a steady
/// cadence (`frame.microseconds` advances by a near-constant ~40ms at
/// 25fps — confirmed against real hardware 2026-09-14, `gap_since_last`
/// in `probe_login_video`'s own output bounces between ~0ms and ~85ms for
/// a camera clock that never deviates from ~40ms). Without slack, the
/// player has no room to smooth that back out before display.
const PLAYOUT_DELAY: gstreamer::ClockTime = gstreamer::ClockTime::from_mseconds(500);

struct Pipeline {
    appsrc: AppSrc,
    _pipeline: gstreamer::Pipeline,
    // The camera's own capture clock (`frame.microseconds`) mapped onto
    // this pipeline's running time, established from the first frame —
    // see `push_frame`'s doc comment for why this replaces `do-timestamp`.
    first_camera_us: Cell<Option<u32>>,
    first_pipeline_pts: Cell<Option<gstreamer::ClockTime>>,
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
        // No `do-timestamp` here — see `push_frame`'s doc comment for why
        // PTS is instead computed manually from the camera's own capture
        // clock plus a playout delay.
        // appsrc's own default internal buffer (`max-bytes`) is 200KB —
        // at the real main-stream bitrate (confirmed via the camera's own
        // GetEnc API: 8192 kbit/s, ~1MB/s) that's under 200ms of data,
        // far short of `PLAYOUT_DELAY` + the queue's own capacity below.
        // Raised well past what a few seconds of main-stream data needs
        // so appsrc itself is never the bottleneck holding frames back.
        appsrc.set_property("max-bytes", 16 * 1024 * 1024u64);

        let parse = gstreamer::ElementFactory::make(parse_name)
            .build()
            .unwrap_or_else(|_| panic!("{parse_name} element missing — install gstreamer1.0-plugins-bad"));
        let decoder = gstreamer::ElementFactory::make(decoder_name)
            .build()
            .unwrap_or_else(|_| panic!("{decoder_name} element missing — install gstreamer1.0-libav"));

        // Without a queue, GStreamer's live-pipeline latency calculation
        // has no buffering to work with and collapses to zero ("Pipeline
        // construction is invalid, please add queues" — confirmed via
        // GST_DEBUG=3 against real hardware 2026-09-14). Its capacity must
        // stay comfortably above `PLAYOUT_DELAY`: buffers now carry PTS
        // spaced at the camera's real ~40ms cadence (see `push_frame`), so
        // a burst of several frames arriving close together in real time
        // still spans real *virtual* time once queued. A tighter cap here
        // (500ms was tried) makes `leaky=downstream` fire on ordinary
        // bursts and discard whichever buffers are closest to their
        // display time — confirmed against real hardware 2026-09-14: the
        // manual-PTS fix above made playback *worse* (~1fps, large jumps)
        // until this was widened, because most of every burst was being
        // leaked before it ever reached the sink.
        let queue = gstreamer::ElementFactory::make("queue")
            .property("max-size-time", 3_000_000_000u64) // 3s
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

        Pipeline {
            appsrc,
            _pipeline: pipeline,
            first_camera_us: Cell::new(None),
            first_pipeline_pts: Cell::new(None),
        }
    }

    /// Pushes one raw access unit into the pipeline. PTS is computed from
    /// the camera's own capture clock (`frame.microseconds`), not from
    /// arrival time. Real hardware testing 2026-09-14 (see
    /// `.plans/reoling-video-stutter-diagnosis.md`) found that
    /// `appsrc.set_do_timestamp(true)` — stamping each buffer's PTS at the
    /// moment it's pushed — turns network arrival jitter directly into
    /// playback jitter: the camera captures at a steady ~40ms cadence, but
    /// frames arrive unevenly (bursts, gaps up to ~85ms in the same
    /// capture-steady session), and with `sync: true` the sink replayed
    /// exactly that uneven arrival pattern. Mapping the camera's own clock
    /// onto the pipeline's running time (anchored at the first frame, plus
    /// `PLAYOUT_DELAY` of slack to absorb jitter) reconstructs the
    /// camera's steady cadence instead.
    pub fn push_frame(&self, frame: &VideoFrame) {
        if self.pipeline.borrow().is_none() {
            let pipeline = self.build_pipeline(frame.video_type);
            *self.pipeline.borrow_mut() = Some(pipeline);
        }

        let pipeline_ref = self.pipeline.borrow();
        let p = pipeline_ref.as_ref().unwrap();

        let first_camera_us = p.first_camera_us.get().unwrap_or_else(|| {
            p.first_camera_us.set(Some(frame.microseconds));
            frame.microseconds
        });
        let first_pipeline_pts = p.first_pipeline_pts.get().unwrap_or_else(|| {
            // Not `p._pipeline.current_running_time()`: right after
            // `set_state(Playing)` the state change is still async, so
            // the pipeline's own clock/base-time may not be established
            // yet and this can read back `None`/stale — a real,
            // confirmed-plausible source of the first frames' PTS being
            // anchored wrong for the rest of the session (`first_pipeline_pts`
            // is cached once, from this single read). The pipeline was
            // just created; its running time at this point is 0 by
            // construction, no query needed.
            let pts = PLAYOUT_DELAY;
            p.first_pipeline_pts.set(Some(pts));
            pts
        });

        // `frame.microseconds` is a u32 device-uptime counter that wraps
        // every ~71.58 minutes; `wrapping_sub` keeps the delta correct
        // across a single wrap. A session running long enough to wrap
        // *twice* relative to its own first frame would need a proper
        // timestamp extender, not implemented here yet.
        let camera_delta = gstreamer::ClockTime::from_useconds(
            frame.microseconds.wrapping_sub(first_camera_us) as u64,
        );
        let pts = first_pipeline_pts + camera_delta;

        let mut buffer = gstreamer::Buffer::from_slice(frame.data.clone());
        buffer.get_mut().unwrap().set_pts(pts);

        let _ = p.appsrc.push_buffer(buffer);
    }
}
