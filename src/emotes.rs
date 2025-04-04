use gtk::prelude::*;
use gtk::{glib, gdk, gio, Box, Image, Label, Orientation, TextView, Widget, WrapMode};
use twitch_irc::message::PrivmsgMessage;
use chrono::Local;
use twitch_irc::message::RGBColor;
use std::fs;
use std::{collections::HashMap, fs::File, io::Write, path::Path, sync::Arc};
use reqwest::blocking::get;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Emote {
    name: String,
    url: String,
    local_path: String,
}

pub fn get_emote_map() -> HashMap<String, Emote> {
    let mut emotes = HashMap::new();

    emotes.insert(
        "Kappa".to_string(),
        Emote {
            name: "Kappa".to_string(),
            url: "https://example.com/kappa.png".to_string(),
            local_path: "~/.config/admiral/emotes/kappa.png".to_string(),
        },
    );

    emotes.insert(
        "PogChamp".to_string(),
        Emote {
            name: "PogChamp".to_string(),
            url: "https://example.com/pogchamp.png".to_string(),
            local_path: "~/.config/admiral/emotes/pog.png".to_string(),
        },
    );

    emotes
}

/// Converts an `RGBColor` to a CSS hex string like "#RRGGBB"
fn rgb_to_hex(color: &RGBColor) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b)
}

pub fn parse_message(msg: &PrivmsgMessage, emote_map: &HashMap<String, Emote>) -> Widget {
    // Sender's name with color
    let sender_label = Label::new(Some(&msg.sender.name));
    sender_label.set_xalign(0.0);

    if let Some(color) = &msg.name_color {
        let color_hex = rgb_to_hex(color);
        sender_label.set_markup(&format!(
            "<span foreground=\"{}\"><b>{}</b></span> - <i>{}</i>",
            color_hex,
            glib::markup_escape_text(&msg.sender.name),
            &msg.server_timestamp.with_timezone(&Local).format("%-I:%M:%S %p").to_string()));
    } else {
        sender_label.set_markup(&format!("<b>{}</b> - <i>{}</i>",
            &msg.sender.name,
            &msg.server_timestamp.with_timezone(&Local).format("%-I:%M:%S %p")));
    }

    let container = Box::new(Orientation::Vertical, 0);
    container.set_margin_top(4);
    container.set_margin_bottom(4);
    container.set_margin_start(6);
    container.set_margin_end(6);
    // Message row (single line of text + emotes)
    let message_box = Box::new(Orientation::Horizontal, 3);

    for word in msg.message_text.split_whitespace() {
        if let Some(emote) = emote_map.get(word) {
            let file = gio::File::for_path(&shellexpand::tilde(&emote.local_path).to_string());
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
    container.prepend(&sender_label);
    container.upcast::<Widget>()
}
