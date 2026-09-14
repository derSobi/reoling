mod bridge;
mod connect_dialog;
mod video_view;

use bridge::{spawn_connection, AppEvent, UidTransport};
use connect_dialog::build_connect_dialog;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Button, HeaderBar, Label};
use std::rc::Rc;
use std::time::Duration;
use video_view::VideoView;

const STATUS_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

fn main() {
    gstreamer::init().expect("failed to initialize GStreamer");

    // Debug-only, not shown anywhere in the UI: lets the TCP-preferring UID
    // connect path (the default — see `bridge::spawn_connection`) be A/B'd
    // against the plain UDP/P2P session on real hardware. Parsed by hand
    // and never handed to `app.run()` — GTK's own argv parser rejects
    // options it doesn't recognize.
    let uid_transport = if std::env::args().any(|a| a == "--prefer-udp") {
        UidTransport::Udp
    } else {
        UidTransport::PreferTcp
    };

    let app = Application::builder()
        .application_id("de.dersobi.reoling")
        .build();

    app.connect_activate(move |app| {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Reoling")
            .default_width(800)
            .default_height(600)
            .build();

        let header = HeaderBar::new();
        let fullscreen_button = Button::from_icon_name("view-fullscreen-symbolic");
        fullscreen_button.set_tooltip_text(Some("Toggle fullscreen"));
        header.pack_end(&fullscreen_button);
        window.set_titlebar(Some(&header));

        let window_for_fullscreen = window.clone();
        fullscreen_button.connect_clicked(move |button| {
            if window_for_fullscreen.is_fullscreen() {
                window_for_fullscreen.unfullscreen();
                button.set_icon_name("view-fullscreen-symbolic");
            } else {
                window_for_fullscreen.fullscreen();
                button.set_icon_name("view-restore-symbolic");
            }
        });

        // gtk4::Label is already a reference-counted GObject wrapper
        // (cloning it clones the handle, not the widget), so it can be
        // cloned directly into closures without an extra Rc. VideoView is a
        // plain Rust struct wrapping GStreamer elements, so it does need Rc
        // to be shared the same way.
        let status_label = Label::new(Some("Not connected"));
        let video_view = Rc::new(VideoView::new());

        let dialog_container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

        let status_label_for_dialog = status_label.clone();
        let video_view_for_dialog = Rc::clone(&video_view);
        let header_for_dialog = header.clone();
        let dialog_widget_for_hide = dialog_container.clone();
        build_connect_dialog(&dialog_container, move |device_name, target, username, password, channel_id, quality| {
            status_label_for_dialog.set_text("Connecting...");
            let receiver =
                spawn_connection(target, username, password, channel_id, quality, uid_transport);
            let status_label = status_label_for_dialog.clone();
            let video_view = Rc::clone(&video_view_for_dialog);
            let header = header_for_dialog.clone();
            let dialog_widget = dialog_widget_for_hide.clone();
            glib::spawn_future_local(async move {
                // Setting label text triggers GTK layout/redraw work on
                // this same main thread that also composites the video
                // texture — updating it on every single frame (dozens of
                // times a second) competed with that rendering for the
                // thread and made playback choppier the higher the frame
                // rate, confirmed against real hardware 2026-09-14 (worse
                // on the higher-bitrate main stream, but present on sub
                // stream too, just less noticeable). Throttled to once a
                // second; the byte count was diagnostic-only anyway.
                let mut last_status_update = std::time::Instant::now() - STATUS_UPDATE_INTERVAL;
                while let Ok(event) = receiver.recv().await {
                    match event {
                        AppEvent::LoggedIn(_info) => {
                            status_label.set_text("Logged in, starting video...");
                            // The form has done its job; give the live view
                            // the room, the same way the official app moves
                            // from an "add device" screen to a live-view one.
                            dialog_widget.set_visible(false);
                            let title = if device_name.is_empty() {
                                "Connected".to_string()
                            } else {
                                device_name.clone()
                            };
                            header.set_title_widget(Some(&Label::new(Some(&title))));
                        }
                        AppEvent::Frame(frame) => {
                            if last_status_update.elapsed() >= STATUS_UPDATE_INTERVAL {
                                status_label.set_text(&format!(
                                    "Streaming ({} bytes/frame)",
                                    frame.data.len()
                                ));
                                last_status_update = std::time::Instant::now();
                            }
                            video_view.push_frame(&frame);
                            // Frames often arrive in network bursts —
                            // `receiver.recv().await` resolving immediately
                            // for an already-queued event doesn't
                            // necessarily hand control back to the GLib
                            // main loop first, so a burst can run this
                            // whole loop body several times back to back
                            // with no chance for anything else on this
                            // thread (including gtk4paintablesink's own
                            // scheduled repaint) to run in between.
                            // Confirmed against real hardware 2026-09-14:
                            // GST_DEBUG showed the pipeline's QoS
                            // `earliest_time` reference frozen for a
                            // whole burst of dropped frames, then jumping
                            // forward by as much as ~1.8s in one step —
                            // the sink simply wasn't getting scheduled to
                            // paint during that stretch, playback visibly
                            // dropping to ~1fps with big jumps. A
                            // zero-duration timeout future still goes
                            // through the main loop's normal dispatch, so
                            // it lets any pending paint get serviced
                            // before this loop resumes.
                            glib::timeout_future(std::time::Duration::ZERO).await;
                        }
                        AppEvent::Failed(reason) => {
                            status_label.set_text(&format!("Error: {reason}"))
                        }
                    }
                }
            });
        });

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        root.append(&dialog_container);
        root.append(&status_label);
        root.append(video_view.widget());
        window.set_child(Some(&root));
        window.present();
    });

    // `run_with_args::<&str>(&[])` rather than `run()`: GTK's own argv
    // parser doesn't know `--prefer-udp` (parsed by hand above) and would
    // reject it as an invalid option.
    app.run_with_args::<&str>(&[]);
}
