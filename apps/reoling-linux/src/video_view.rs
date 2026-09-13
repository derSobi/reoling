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

        let parse = gstreamer::ElementFactory::make(parse_name)
            .build()
            .unwrap_or_else(|_| panic!("{parse_name} element missing — install gstreamer1.0-plugins-bad"));
        let decodebin = gstreamer::ElementFactory::make("decodebin")
            .build()
            .expect("decodebin element missing");
        let sink = gstreamer::ElementFactory::make("gtk4paintablesink")
            .build()
            .expect("gtk4paintablesink missing — install gstreamer1.0-plugins-good/gtk4 support");
        // `microseconds` in each VideoFrame is the camera's own device-uptime
        // counter, not wall-clock time — it has no relationship to this
        // pipeline's clock/base-time. With the default `sync: true`, the
        // sink schedules each buffer's display against that meaningless
        // PTS, which can defer rendering indefinitely (buffers accepted by
        // appsrc, decoded, but never actually painted). Disable clock sync
        // so decoded frames are shown as soon as they're ready, same as any
        // live-camera-viewer pipeline with a foreign timestamp source.
        sink.set_property("sync", false);

        pipeline
            .add_many([appsrc.upcast_ref(), &parse, &decodebin, &sink])
            .expect("adding elements failed");
        appsrc.link(&parse).expect("linking appsrc->parse failed");
        parse.link(&decodebin).expect("linking parse->decodebin failed");

        // decodebin exposes its output pad only once it knows the format, so
        // link decodebin->sink lazily.
        let sink_clone = sink.clone();
        decodebin.connect_pad_added(move |_element, pad| {
            let sink_pad = sink_clone.static_pad("sink").expect("sink always has a sink pad");
            if !sink_pad.is_linked() {
                let _ = pad.link(&sink_pad);
            }
        });

        let paintable = sink.property::<gtk4::gdk::Paintable>("paintable");
        self.picture.set_paintable(Some(&paintable));

        pipeline.set_state(gstreamer::State::Playing).expect("failed to start pipeline");

        Pipeline { appsrc, _pipeline: pipeline }
    }

    /// Pushes one raw access unit into the pipeline. `microseconds` becomes
    /// the buffer's presentation timestamp.
    pub fn push_frame(&self, frame: &VideoFrame) {
        if self.pipeline.borrow().is_none() {
            let pipeline = self.build_pipeline(frame.video_type);
            *self.pipeline.borrow_mut() = Some(pipeline);
        }

        let mut buffer = gstreamer::Buffer::from_slice(frame.data.clone());
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gstreamer::ClockTime::from_useconds(frame.microseconds as u64));

        let pipeline_ref = self.pipeline.borrow();
        let _ = pipeline_ref.as_ref().unwrap().appsrc.push_buffer(buffer);
    }
}
