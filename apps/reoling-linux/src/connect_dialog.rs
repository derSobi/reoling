use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Entry, Grid, Label, Orientation};

fn labeled_row(grid: &Grid, row: i32, label_text: &str, entry: &Entry) {
    let label = Label::new(Some(label_text));
    label.set_halign(gtk4::Align::End);
    grid.attach(&label, 0, row, 1, 1);
    entry.set_hexpand(true);
    grid.attach(entry, 1, row, 1, 1);
}

/// Builds the "add device" form directly into `container` (rather than
/// creating and returning its own), so the caller can hold a clone of
/// `container` *before* this call to hide it later — e.g. once connected,
/// mirroring the common NVR-app pattern of a dedicated add/connect screen
/// that gives way to a live-view screen instead of both staying stacked on
/// top of each other permanently.
///
/// `on_connect` receives the device name (for display only — a nickname
/// the user chose, not sent anywhere), then uid/username/password/channel.
pub fn build_connect_dialog(
    container: &GtkBox,
    on_connect: impl Fn(String, String, String, String, u8) + 'static,
) {
    container.set_orientation(Orientation::Vertical);
    container.set_spacing(12);
    container.set_margin_top(16);
    container.set_margin_bottom(16);
    container.set_margin_start(16);
    container.set_margin_end(16);

    let heading = Label::new(Some("Connect to a Reolink device"));
    heading.add_css_class("title-2");
    heading.set_halign(gtk4::Align::Start);
    container.append(&heading);

    let grid = Grid::new();
    grid.set_row_spacing(8);
    grid.set_column_spacing(12);

    let name_entry = Entry::builder().placeholder_text("e.g. Front Door NVR").build();
    let uid_entry = Entry::builder().placeholder_text("e.g. 9527000EXAMPLE01").build();
    let user_entry = Entry::builder().placeholder_text("admin").build();
    let pass_entry = Entry::builder().visibility(false).build();
    let channel_entry = Entry::builder().text("0").build();

    labeled_row(&grid, 0, "Device name", &name_entry);
    labeled_row(&grid, 1, "UID", &uid_entry);
    labeled_row(&grid, 2, "Username", &user_entry);
    labeled_row(&grid, 3, "Password", &pass_entry);
    labeled_row(&grid, 4, "Channel", &channel_entry);
    container.append(&grid);

    let status_label = Label::new(None);
    status_label.set_halign(gtk4::Align::Start);
    container.append(&status_label);

    let connect_button = Button::with_label("Connect");
    connect_button.add_css_class("suggested-action");
    connect_button.set_halign(gtk4::Align::End);
    container.append(&connect_button);

    let name_entry_c = name_entry.clone();
    let uid_entry_c = uid_entry.clone();
    let user_entry_c = user_entry.clone();
    let pass_entry_c = pass_entry.clone();
    let channel_entry_c = channel_entry.clone();
    connect_button.connect_clicked(move |_| {
        let channel_id: u8 = channel_entry_c.text().parse().unwrap_or(0);
        on_connect(
            name_entry_c.text().to_string(),
            uid_entry_c.text().to_string(),
            user_entry_c.text().to_string(),
            pass_entry_c.text().to_string(),
            channel_id,
        );
    });
}
