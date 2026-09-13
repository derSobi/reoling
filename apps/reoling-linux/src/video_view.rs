use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use gtk4::Picture;
use reolink_core::VideoFrame;

pub struct VideoView {
    picture: Picture,
    appsrc: AppSrc,
    _pipeline: gstreamer::Pipeline,
}

impl VideoView {
    /// Builds `appsrc ! h264parse ! decodebin ! gtk4paintablesink` and binds
    /// the sink's paintable to a fresh `gtk4::Picture`.
    pub fn new() -> Self {
        let pipeline = gstreamer::Pipeline::new();

        let appsrc = gstreamer::ElementFactory::make("appsrc")
            .build()
            .expect("appsrc element missing — install gstreamer1.0-plugins-base")
            .downcast::<AppSrc>()
            .expect("appsrc is always an AppSrc");
        appsrc.set_caps(Some(
            &gstreamer::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ));
        appsrc.set_is_live(true);
        appsrc.set_format(gstreamer::Format::Time);

        let h264parse = gstreamer::ElementFactory::make("h264parse")
            .build()
            .expect("h264parse element missing — install gstreamer1.0-plugins-bad");
        let decodebin = gstreamer::ElementFactory::make("decodebin")
            .build()
            .expect("decodebin element missing");
        let sink = gstreamer::ElementFactory::make("gtk4paintablesink")
            .build()
            .expect("gtk4paintablesink missing — install gstreamer1.0-plugins-good/gtk4 support");

        pipeline
            .add_many([appsrc.upcast_ref(), &h264parse, &decodebin, &sink])
            .expect("adding elements failed");
        appsrc.link(&h264parse).expect("linking appsrc->h264parse failed");
        h264parse.link(&decodebin).expect("linking h264parse->decodebin failed");

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
        let picture = Picture::new();
        picture.set_paintable(Some(&paintable));

        pipeline.set_state(gstreamer::State::Playing).expect("failed to start pipeline");

        Self { picture, appsrc, _pipeline: pipeline }
    }

    pub fn widget(&self) -> &Picture {
        &self.picture
    }

    /// Pushes one raw H.264 access unit into the pipeline. `microseconds`
    /// becomes the buffer's presentation timestamp.
    pub fn push_frame(&self, frame: &VideoFrame) {
        let mut buffer = gstreamer::Buffer::from_slice(frame.data.clone());
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gstreamer::ClockTime::from_useconds(frame.microseconds as u64));
        let _ = self.appsrc.push_buffer(buffer);
    }
}
