use gtk::prelude::*;
use gtk::{glib, gdk, gio, Image, Label, Orientation, TextView, Widget, WrapMode};
// Import Box as GtkBox to avoid conflicts with std::boxed::Box
use gtk::Box as GtkBox;
use gtk::gdk_pixbuf::PixbufAnimation;
use gtk::MediaFile;
use gtk::Video;
use twitch_irc::message::PrivmsgMessage;
use chrono::Local;
use twitch_irc::message::RGBColor;
use std::fs;
use std::{collections::HashMap, fs::File, io::Write, path::Path, sync::Arc};
use reqwest::blocking::{get, Client, Response};
use std::sync::{Mutex, RwLock, mpsc};
use std::{path::{PathBuf}, thread, time::Duration, collections::HashSet};
use std::error::Error as StdError;
use serde::{Deserialize, Serialize};
use once_cell::sync::Lazy;

// Tracking which channels are currently being processed
static DOWNLOADING_CHANNELS: Lazy<RwLock<HashMap<String, bool>>> = Lazy::new(|| RwLock::new(HashMap::new()));

// Your existing Emote struct for local representation
#[derive(Debug, Clone)]
pub struct Emote {
    name: String,
    url: String, // This would be the fully constructed CDN URL
    local_path: String,
    is_gif: bool, // Or more generally, is_animated
}

// --- Updated 7TV API Response Structures ---

#[derive(Debug, Deserialize)]
struct SevenTVUserResponse { // Represents the top-level object from /v3/users/twitch/{id}
    // Based on the spec, the path to the active emote set for a user (e.g., a Twitch channel)
    // is often through user.connections[platform="twitch"].emote_set
    // Or, if you are fetching a specific emote set by ID, this structure might be simpler.
    // For now, assuming your existing UserResponse somehow gets to the EmoteSetModel:
    emote_set: Option<ApiEmoteSet>, // This should map to EmoteSetModel
}

#[derive(Debug, Deserialize)]
struct ApiEmoteSet { // Corresponds to EmoteSetModel in the 7TV API spec
    id: String,
    name: String,
    emotes: Vec<ApiActiveEmote>, // List of ActiveEmoteModel
    // Add other fields from EmoteSetModel if needed (e.g., flags, capacity)
}

#[derive(Debug, Deserialize)]
struct ApiActiveEmote { // Corresponds to ActiveEmoteModel
    id: String,
    name: String,        // Name of the emote
    data: Option<ApiEmoteData>, // This is the nested part containing host info
    // Add other fields from ActiveEmoteModel if needed (e.g., flags, timestamp)
}

#[derive(Debug, Deserialize)]
struct ApiEmoteData { // Corresponds to EmotePartialModel
    // animated: bool, // You can add this if you want to know from API if it's animated
    host: Option<ImageHost>, // The host field is inside data
    // You can also include other fields from EmotePartialModel like 'name', 'id' if they differ or are useful
}

#[derive(Debug, Deserialize, Clone)]
struct ImageHost { // Corresponds to ImageHost in the 7TV API spec
    url: String,           // Base URL for the emote, e.g., "//cdn.7tv.app/emote/EMOTE_ID"
    files: Vec<ImageFile>, // Available image files
}

#[derive(Debug, Deserialize, Clone)]
struct ImageFile { // Corresponds to the file objects within ImageHost.files
    name: String,        // Actual file name, e.g., "1x.webp", "animated.gif", "3x.png"
    // static_name: Option<String>, // The spec shows this, useful if you want to specifically target static versions.
    format: String,      // File format, e.g., "WEBP", "PNG", "GIF", "AVIF" (spec focuses on AVIF/WEBP but API might send others)
    width: u32,
    height: u32,
    // size: u32, // from spec
    // flags: u32, // from spec, EmoteFileFlagModel
}

// (Keep your DOWNLOADING_CHANNELS, get_emote_map, fetch_missing_emotes, find_best_image_file, is_image_file, rgb_to_hex, parse_message as they were,
//  unless find_best_image_file needs tweaks based on new format knowledge, but its current logic is likely fine)

/// Get emotes for a specific channel from the local filesystem
/// and synchronize with 7TV if needed
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

    // Start a background task to fetch missing emotes from 7TV if not already running
    fetch_missing_emotes(channel_id);

    emotes
}

fn fetch_missing_emotes(channel_id: &str) -> thread::JoinHandle<()> {
    let channel_id = channel_id.to_string();

    // Check if download is already in progress for this channel
    {
        let downloading = DOWNLOADING_CHANNELS.read().unwrap();
        if downloading.get(&channel_id).copied().unwrap_or(false) {
            // Download already in progress, no need to start again
            return thread::spawn(|| {}); // Return a dummy JoinHandle that does nothing
        }
    }

    // Mark channel as being processed
    {
        let mut downloading = DOWNLOADING_CHANNELS.write().unwrap();
        downloading.insert(channel_id.clone(), true);
    }

    // Start a new thread for downloading and return the join handle
    thread::spawn(move || {
        if let Err(e) = download_channel_emotes(&channel_id) {
            eprintln!("Failed to download emotes for channel {}: {:?}", channel_id, e);
        }

        let mut downloading = DOWNLOADING_CHANNELS.write().unwrap();
        downloading.insert(channel_id.clone(), false);
    }) // No semicolon here so the JoinHandle is returned
}

fn download_channel_emotes(channel_id: &str) -> Result<(), Box<dyn StdError + Send + Sync>> {
    println!("Fetching emotes for channel {} from 7TV", channel_id);

    let client = Client::new();
    let twitch_lookup_url = format!("https://7tv.io/v3/users/twitch/{}", channel_id);

    let response = client.get(&twitch_lookup_url).send()?;
    if !response.status().is_success() {
        return Err(format!("7TV API request failed with status {}", response.status()).into());
    }

    let user_response: SevenTVUserResponse = response.json()?;

    let api_emote_set = match user_response.emote_set {
        Some(set) => set,
        None => {
            println!("No emote set found for channel {} in 7TV user response.", channel_id);
            return Ok(());
        }
    };

    let base_path = shellexpand::tilde("~/.config/admiral/emotes").to_string();
    let channel_path = Path::new(&base_path).join(channel_id);

    if !channel_path.exists() {
        fs::create_dir_all(&channel_path)?;
    }

    let mut existing_emotes: HashSet<String> = HashSet::new();
    if let Ok(entries) = fs::read_dir(&channel_path) {
        for entry in entries.flatten() {
            if let Some(file_name) = entry.path().file_stem().and_then(|s| s.to_str()) {
                existing_emotes.insert(file_name.to_string());
            }
        }
    }

    for active_emote in api_emote_set.emotes {
        if let Some(emote_data) = &active_emote.data {
            if let Some(host_info) = &emote_data.host {
                if host_info.url.trim().is_empty() {
                    eprintln!(
                        "Emote {} (ID: {}) has host data, but the host URL is empty. Skipping.",
                        active_emote.name, active_emote.id
                    );
                    continue;
                }

                let file_opt = find_best_image_file(&host_info.files);

                if let Some(file_to_download) = file_opt {
                    let file_extension = file_to_download.format.to_lowercase();
                    let local_file_name = format!("{}", active_emote.name); // Use name without extension for checking

                    // Efficiently check if the emote already exists
                    if existing_emotes.contains(&local_file_name) {
                        continue;
                    }

                    let base_emote_url = host_info
                        .url
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_start_matches("//");
                    let emote_url = format!("https://{}/{}", base_emote_url, file_to_download.name);
                    let local_path = channel_path.join(format!("{}.{}", active_emote.name, file_extension));

                    println!(
                        "Downloading emote {} (URL: {}) for channel {}",
                        active_emote.name, emote_url, channel_id
                    );

                    let download_result = (|| -> Result<(), Box<dyn StdError + Send + Sync>> {
                        let response = client.get(&emote_url).send()?;
                        if response.status().is_success() {
                            let bytes = response.bytes()?;
                            let mut file_handle = File::create(&local_path)?;
                            file_handle.write_all(&bytes)?;
                            println!("Successfully downloaded emote {} to {:?}", active_emote.name, local_path);
                        } else {
                            eprintln!(
                                "Failed to download emote image {} from {}: HTTP {}",
                                active_emote.name, emote_url, response.status()
                            );
                        }
                        Ok(())
                    })();

                    if let Err(e) = download_result {
                        eprintln!("Error processing download for emote {}: {:?}", active_emote.name, e);
                    }

                    thread::sleep(Duration::from_millis(100)); // Rate limiting
                } else {
                    eprintln!(
                        "No suitable image file found by find_best_image_file for emote {} (ID: {}) in channel {}. Files available: {:?}",
                        active_emote.name, active_emote.id, channel_id, host_info.files
                    );
                }
            } else {
                eprintln!(
                    "Emote {} (ID: {}) has a 'data' object, but is missing 'host' information. Skipping.",
                    active_emote.name, active_emote.id
                );
            }
        } else {
            eprintln!(
                "Emote {} (ID: {}) is missing the 'data' field which contains host information. Skipping.",
                active_emote.name, active_emote.id
            );
        }
    }

    println!("Finished processing emotes for channel {}", channel_id);
    Ok(())
}

/// Find the best image file for an emote
fn find_best_image_file(files: &[ImageFile]) -> Option<&ImageFile> {
    // Prefer files in this order: 3x WebP, 3x GIF, 3x PNG
    // Start with PNG 3x
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format == "PNG") {
        return Some(file);
    }

    // Then GIF 3x
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format == "GIF") {
        return Some(file);
    }

    // If nothing specific found, just return the first file
    files.first()
}

/// Helper function to determine if a file is an image/gif
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

    let container = GtkBox::new(Orientation::Vertical, 0);
    container.set_margin_top(4);
    container.set_margin_bottom(4);
    container.set_margin_start(6);
    container.set_margin_end(6);
    // Message row (single line of text + emotes)
    let message_box = GtkBox::new(Orientation::Horizontal, 3);

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
                picture.set_size_request(-1, 32);

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
