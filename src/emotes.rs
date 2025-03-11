use gtk::prelude::*;
use gtk::{glib, gdk, gio, Box, Image, Label, Orientation, TextView, Widget, WrapMode};
use std::collections::HashMap;
use twitch_irc::message::PrivmsgMessage;
use chrono::Local;
use twitch_irc::message::RGBColor;
use std::fs;

pub fn get_emote_map(channel: &str) -> HashMap<String, String> {
    let mut emotes = HashMap::new();

    // Build the path to the channel's emote directory
    let home = dirs::home_dir().unwrap_or_default();
    let emote_dir = home.join(".config/admiral/emotes").join(channel);

    // Read the directory if it exists
    if let Ok(entries) = fs::read_dir(&emote_dir) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_file() {
                    if let Some(file_name) = entry.file_name().to_str() {
                        // Only process image files (you might want to add more extensions)
                        if file_name.ends_with(".png") || file_name.ends_with(".gif") {
                            // Remove the extension to get the emote name
                            if let Some(emote_name) = file_name.split('.').next() {
                                let relative_path = format!("~/.config/admiral/emotes/{}/{}", channel, file_name);
                                emotes.insert(emote_name.to_string(), relative_path);
                            }
                        }
                    }
                }
            }
        }
    }
    emotes
}

/// Converts an `RGBColor` to a CSS hex string like "#RRGGBB"
fn rgb_to_hex(color: &RGBColor) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b)
}

pub fn parse_message(msg: &PrivmsgMessage, emote_map: &HashMap<String, String>) -> Widget {
    let container = Box::new(Orientation::Vertical, 0);
    container.set_margin_top(4);
    container.set_margin_bottom(4);
    container.set_margin_start(6);
    container.set_margin_end(6);

    // Sender's name with color
    let sender_label = Label::new(Some(&msg.sender.name));
    sender_label.set_xalign(0.0);

    if let Some(color) = &msg.name_color {
        let color_hex = rgb_to_hex(color);
        sender_label.set_markup(&format!(
            "<span foreground=\"{}\"><b>{}</b></span> - {}",
            color_hex,
            glib::markup_escape_text(&msg.sender.name),
            &msg.server_timestamp.with_timezone(&Local).format("%-I:%M:%S %p").to_string()));
    } else {
        sender_label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&msg.sender.name)));
    }

    container.append(&sender_label);

    // Message row (single line of text + emotes)
    let message_box = Box::new(Orientation::Horizontal, 2);

    for word in msg.message_text.split_whitespace() {
        if let Some(path) = emote_map.get(word) {
            let file = gio::File::for_path(path);
            if let Ok(texture) = gdk::Texture::from_file(&file) {
                let image = Image::from_paintable(Some(&texture));
                image.set_pixel_size(24);
                message_box.append(&image);
            }
        } else {
            let label = Label::new(Some(word));
            message_box.append(&label);
        }
    }

    container.append(&message_box);
    container.upcast::<Widget>()
}
