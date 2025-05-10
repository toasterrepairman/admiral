use gtk::prelude::*;
use gtk::{glib, gdk, gio, Box, Image, Label, Orientation, TextView, Widget, WrapMode};
use gtk::gdk_pixbuf::PixbufAnimation;  // Add for GIF animation support
use gtk::MediaFile;  // Add for video support
use gtk::Video;  // Add this for GTK4 Video widget
use twitch_irc::message::PrivmsgMessage;
use chrono::Local;
use twitch_irc::message::RGBColor;
use std::fs;
use std::{collections::HashMap, fs::File, io::Write, path::Path, sync::Arc};
use reqwest::blocking::get;
use std::sync::Mutex;
use std::{path::{PathBuf}};

#[derive(Debug, Clone)]
pub struct Emote {
    name: String,
    url: String,
    local_path: String,
    is_gif: bool,  // Add a field to track if the emote is a GIF
}
/// Get emotes for a specific channel from the local filesystem
///
/// This function scans the channel-specific emote directory and builds
/// a map of all available emotes. The directory structure is:
/// ~/.config/admiral/emotes/{channel_id}/
pub fn get_emote_map(channel_id: &str) -> HashMap<String, Emote> {
    let mut emotes = HashMap::new();

    // Build the path to the channel's emote directory
    let base_path = shellexpand::tilde("~/.config/admiral/emotes").to_string();
    let base_dir = Path::new(&base_path);
    let channel_path = base_dir.join(channel_id);

    // Ensure both base and channel directories exist
    if !base_dir.exists() {
        println!("Base emotes directory doesn't exist, creating it now");
        if let Err(e) = fs::create_dir_all(base_dir) {
            eprintln!("Failed to create base emote directory: {}", e);
            return emotes;
        }
    }

    // Ensure channel-specific directory exists
    if !channel_path.exists() {
        if let Err(e) = fs::create_dir_all(&channel_path) {
            eprintln!("Failed to create emote directory for channel {}: {}", channel_id, e);
            return emotes;
        }
    }

    // Scan the directory for emote files
    match fs::read_dir(&channel_path) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();

                // Skip directories
                if path.is_dir() {
                    continue;
                }

                if let Some(file_name) = path.file_stem().and_then(|s| s.to_str()) {
                    let is_gif = path.extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.to_lowercase() == "gif")
                        .unwrap_or(false);

                    // Create the emote entry
                    let emote = Emote {
                        name: file_name.to_string(),
                        url: String::new(), // Empty URL as we're loading from filesystem
                        local_path: path.to_string_lossy().to_string(),
                        is_gif,
                    };

                    emotes.insert(file_name.to_string(), emote);
                }
            }
        },
        Err(e) => {
            eprintln!("Failed to read emote directory for channel {}: {}", channel_id, e);
        }
    }

    // Future: Add code to check for missing common emotes and download them
    // fetch_missing_emotes(channel_id, &mut emotes).await;

    emotes
}

// Placeholder for future implementation
// This would download missing emotes from 7TV API
async fn fetch_missing_emotes(channel_id: &str, emotes: &mut HashMap<String, Emote>) {
    // TODO:
    // 1. Query 7TV API for channel emotes
    // 2. Compare with local emotes
    // 3. Download any missing emotes
    // 4. Add them to the HashMap
}

// Helper function to determine if a file is an image/gif
fn is_image_file(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" => true,
            _ => false,
        }
    } else {
        false
    }
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
            let expanded_path = shellexpand::tilde(&emote.local_path).to_string();
            let file = gio::File::for_path(&expanded_path);

            if emote.is_gif {
                // Load the gif using MediaFile
                let expanded_path = shellexpand::tilde(&emote.local_path).to_string();
                let media_file = gtk::MediaFile::for_filename(&expanded_path);

                media_file.play();
                media_file.set_loop(true);

                // Display as a Picture without controls
                let picture = gtk::Picture::new();
                picture.set_paintable(Some(&media_file));
                picture.set_size_request(32, 32);

                message_box.append(&picture);
            } else {
                // For regular images, continue using the Image widget
                if let Ok(texture) = gdk::Texture::from_file(&file) {
                    let image = Image::from_paintable(Some(&texture));
                    image.set_pixel_size(24);
                    message_box.append(&image);
                }
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
