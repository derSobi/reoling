mod bridge;
mod connect_dialog;
mod video_view;

use bridge::{spawn_connection, AppEvent};
use connect_dialog::build_connect_dialog;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Button, HeaderBar, Label};
use std::rc::Rc;
use video_view::VideoView;

fn main() {
    gstreamer::init().expect("failed to initialize GStreamer");

    let app = Application::builder()
        .application_id("de.dersobi.reoling")
        .build();

    app.connect_activate(|app| {
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
        build_connect_dialog(&dialog_container, move |device_name, uid, username, password, channel_id| {
            status_label_for_dialog.set_text("Connecting...");
            let receiver = spawn_connection(uid, username, password, channel_id);
            let status_label = status_label_for_dialog.clone();
            let video_view = Rc::clone(&video_view_for_dialog);
            let header = header_for_dialog.clone();
            let dialog_widget = dialog_widget_for_hide.clone();
            glib::spawn_future_local(async move {
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
                            status_label
                                .set_text(&format!("Streaming ({} bytes/frame)", frame.data.len()));
                            video_view.push_frame(&frame);
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

    app.run();
}
