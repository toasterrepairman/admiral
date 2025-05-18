
use gtk::prelude::*;
use gtk::{glib, gdk, gio, Image, Label, Orientation, Widget, ListBoxRow, MediaFile};
use gtk::Box as GtkBox;
use twitch_irc::message::PrivmsgMessage;
use chrono::Local;
use twitch_irc::message::RGBColor;
use std::fs;
use std::{collections::HashMap, fs::File, io::Write, path::Path, sync::Arc, time::{Duration, Instant},};
use reqwest::blocking::{Client, Response};
use std::sync::{Mutex, RwLock, mpsc};
use std::{path::{PathBuf}, thread, collections::HashSet};
use std::error::Error as StdError;
use serde::Deserialize;
use once_cell::sync::Lazy;
use regex::Regex;

// Tracking which channels are currently being processed
static DOWNLOADING_CHANNELS: Lazy<RwLock<HashMap<String, bool>>> = Lazy::new(|| RwLock::new(HashMap::new()));

// Tracking the last time we fetched emotes for a channel
static LAST_FETCH_TIME: Lazy<RwLock<HashMap<String, Instant>>> = Lazy::new(|| RwLock::new(HashMap::new()));

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
struct SevenTVUserResponse {
    emote_set: Option<ApiEmoteSet>,
}

#[derive(Debug, Deserialize)]
struct ApiEmoteSet {
    id: String,
    name: String,
    emotes: Vec<ApiActiveEmote>,
}

#[derive(Debug, Deserialize)]
struct ApiActiveEmote {
    id: String,
    name: String,
    data: Option<ApiEmoteData>,
}

#[derive(Debug, Deserialize)]
struct ApiEmoteData {
    host: Option<ImageHost>,
}

#[derive(Debug, Deserialize, Clone)]
struct ImageHost {
    url: String,
    files: Vec<ImageFile>,
}

#[derive(Debug, Deserialize, Clone)]
struct ImageFile {
    name: String,
    format: String,
}

// Define a trait for media resources to ensure proper cleanup
trait MediaResource {
    fn get_widget(&self) -> Widget;
    fn cleanup(&mut self);
}

// Implementation for GIF animations using MediaFile
struct GifMediaResource {
    media_file: gtk::MediaFile,
    picture: gtk::Picture,
}

impl GifMediaResource {
    fn new(path: &str) -> Self {
        let media_file = gtk::MediaFile::for_filename(path);
        let picture = gtk::Picture::new();

        picture.set_paintable(Some(&media_file));
        picture.set_size_request(-1, 28); // Consistent size for all emotes

        media_file.play();
        media_file.set_loop(true);

        Self { media_file, picture }
    }
}

impl MediaResource for GifMediaResource {
    fn get_widget(&self) -> Widget {
        self.picture.clone().upcast::<Widget>()
    }

    fn cleanup(&mut self) {
        self.media_file.pause();
        self.media_file.set_loop(false);
        self.media_file.set_file(None::<&gio::File>);
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.media_file.set_resource(None);
    }
}

// Implementation for static images
struct StaticImageResource {
    image: gtk::Image,
}

impl StaticImageResource {
    fn new(path: &str) -> Self {
        let image = gtk::Image::from_file(path);
        image.set_pixel_size(28); // Consistent size for all emotes
        Self { image }
    }
}

impl MediaResource for StaticImageResource {
    fn get_widget(&self) -> Widget {
        self.image.clone().upcast::<Widget>()
    }

    fn cleanup(&mut self) {
        // Static images don't need special cleanup beyond what GTK handles
    }
}

/// Get emotes for a specific channel from the local filesystem
/// and synchronize with 7TV if needed
pub fn get_emote_map(channel_id: &str) -> HashMap<String, Emote> {
    let mut emotes = HashMap::new();
    let base_path = shellexpand::tilde("~/.config/admiral/emotes").to_string();
    let channel_path = Path::new(&base_path).join(channel_id);

    if !channel_path.exists() {
        if let Err(e) = fs::create_dir_all(&channel_path) {
            eprintln!("Failed to create emote directory for channel {}: {}", channel_id, e);
            return emotes;
        }
    }

    if let Ok(entries) = fs::read_dir(&channel_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(file_name) = path.file_stem().and_then(|s| s.to_str()) {
                let is_gif = path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_lowercase() == "gif").unwrap_or(false);
                let emote = Emote {
                    name: file_name.to_string(),
                    url: String::new(),
                    local_path: path.to_string_lossy().to_string(),
                    is_gif,
                };
                emotes.insert(file_name.to_string(), emote);
            }
        }
    }

    fetch_missing_emotes(channel_id);
    emotes
}

const FETCH_COOLDOWN: Duration = Duration::from_secs(60 * 1); // 1 minute

fn fetch_missing_emotes(channel_id: &str) -> Option<thread::JoinHandle<()>> {
    let channel_id = channel_id.to_string();
    let now = Instant::now();

    {
        let downloading = DOWNLOADING_CHANNELS.read().unwrap();
        if downloading.get(&channel_id).copied().unwrap_or(false) {
            return None;
        }
    }

    {
        let last_fetch_read = LAST_FETCH_TIME.read().unwrap();
        if let Some(&last_fetch) = last_fetch_read.get(&channel_id) {
            if now.duration_since(last_fetch) < FETCH_COOLDOWN {
                return None; // Cooldown not yet elapsed
            }
        }
    }

    let base_path = shellexpand::tilde("~/.config/admiral/emotes").to_string();
    let channel_path = Path::new(&base_path).join(&channel_id);
    let mut existing_emotes: HashSet<String> = HashSet::new();

    if channel_path.exists() {
        if let Ok(entries) = fs::read_dir(&channel_path) {
            for entry in entries.flatten() {
                if let Some(file_name) = entry.path().file_stem().and_then(|s| s.to_str()) {
                    existing_emotes.insert(file_name.to_string());
                }
            }
        }
    }

    match get_remote_emote_names(&channel_id) {
        Ok(remote_emote_names) => {
            let missing_emotes: Vec<String> = remote_emote_names
                .iter()
                .filter(|name| !existing_emotes.contains(*name))
                .cloned()
                .collect();

            let mut last_fetch_write = LAST_FETCH_TIME.write().unwrap();
            last_fetch_write.insert(channel_id.clone(), now);

            if !missing_emotes.is_empty() {
                {
                    let mut downloading = DOWNLOADING_CHANNELS.write().unwrap();
                    downloading.insert(channel_id.clone(), true);
                }

                Some(thread::spawn(move || {
                    println!(
                        "Detected {} missing emotes for channel {}, starting download (cooldown: {:?})...",
                        missing_emotes.len(),
                        channel_id,
                        FETCH_COOLDOWN
                    );
                    if let Err(e) = download_channel_emotes(&channel_id) {
                        eprintln!("Failed to download emotes for channel {}: {:?}", channel_id, e);
                    }

                    let mut downloading = DOWNLOADING_CHANNELS.write().unwrap();
                    downloading.insert(channel_id.clone(), false);
                }))
            } else {
                println!("No missing emotes detected for channel {} (cooldown: {:?}).", channel_id, FETCH_COOLDOWN);
                None
            }
        }
        Err(e) => {
            eprintln!("Failed to fetch remote emote list for channel {}: {:?}", channel_id, e);
            let mut last_fetch_write = LAST_FETCH_TIME.write().unwrap();
            last_fetch_write.insert(channel_id.clone(), now);
            None
        }
    }
}

fn get_remote_emote_names(channel_id: &str) -> Result<HashSet<String>, Box<dyn StdError + Send + Sync>> {
    let client = Client::new();
    let twitch_lookup_url = format!("https://7tv.io/v3/users/twitch/{}", channel_id);
    let mut emote_names = HashSet::new();

    let response = client.get(&twitch_lookup_url).send()?;
    if !response.status().is_success() {
        return Err(format!("7TV API request failed with status {}", response.status()).into());
    }

    let user_response: Result<SevenTVUserResponse, reqwest::Error> = response.json();
    match user_response {
        Ok(user_data) => {
            if let Some(emote_set) = user_data.emote_set {
                for active_emote in emote_set.emotes {
                    emote_names.insert(active_emote.name);
                }
            }
        }
        Err(e) => {
            return Err(format!("Failed to parse 7TV API response: {}", e).into());
        }
    }

    Ok(emote_names)
}

fn download_channel_emotes(channel_id: &str) -> Result<(), Box<dyn StdError + Send + Sync>> {
    println!("Fetching emotes for channel {} from 7TV", channel_id);

    let client = Client::new();
    let twitch_lookup_url = format!("https://7tv.io/v3/users/twitch/{}", channel_id);

    // Batch size for processing emotes - helps manage file handles
    const MAX_EMOTES_PER_BATCH: usize = 50;
    // Rate limit for API calls - helps avoid 429 errors
    const BATCH_DELAY_MS: u64 = 500;
    // Small delay between individual downloads within a batch
    const DOWNLOAD_DELAY_MS: u64 = 100;
    // Maximum number of retries for failed downloads
    const MAX_RETRIES: usize = 3;

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

    // Load existing emotes to prevent redownloading
    let mut existing_emotes: HashSet<String> = HashSet::new();
    if let Ok(entries) = fs::read_dir(&channel_path) {
        for entry in entries.flatten() {
            if let Some(file_name) = entry.path().file_stem().and_then(|s| s.to_str()) {
                existing_emotes.insert(file_name.to_string());
            }
        }
    }

    // Count total and new emotes
    let total_emotes = api_emote_set.emotes.len();
    let emotes_to_process: Vec<_> = api_emote_set.emotes.into_iter()
        .filter(|e| !existing_emotes.contains(&e.name))
        .collect();
    let new_emotes = emotes_to_process.len();

    println!(
        "Channel {} has {} total emotes. Found {} new emotes to download.",
        channel_id, total_emotes, new_emotes
    );

    // Process emotes in fixed-size batches to manage file handles
    let batch_count = (new_emotes + MAX_EMOTES_PER_BATCH - 1) / MAX_EMOTES_PER_BATCH;
    for (batch_idx, batch) in emotes_to_process.chunks(MAX_EMOTES_PER_BATCH).enumerate() {
        println!(
            "Processing batch {}/{} ({} emotes)",
            batch_idx + 1,
            batch_count,
            batch.len()
        );

        for (emote_idx, active_emote) in batch.iter().enumerate() {
            if let Some(emote_data) = &active_emote.data {
                if let Some(host_info) = &emote_data.host {
                    if host_info.url.trim().is_empty() {
                        continue;
                    }

                    let file_opt = find_best_image_file(&host_info.files);

                    if let Some(file_to_download) = file_opt {
                        let file_extension = file_to_download.format.to_lowercase();
                        let base_emote_url = host_info
                            .url
                            .trim_start_matches("https://")
                            .trim_start_matches("http://")
                            .trim_start_matches("//");
                        let emote_url = format!("https://{}/{}", base_emote_url, file_to_download.name);
                        let local_path = channel_path.join(format!("{}.{}", active_emote.name, file_extension));

                        println!(
                            "Downloading emote {}/{}: {} (URL: {})",
                            emote_idx + 1, batch.len(), active_emote.name, emote_url
                        );

                        // Try downloading with retries
                        let mut success = false;
                        for retry in 1..=MAX_RETRIES {
                            let download_result = (|| -> Result<(), Box<dyn StdError + Send + Sync>> {
                                let response = client.get(&emote_url).send()?;

                                if response.status().is_success() {
                                    let bytes = response.bytes()?;

                                    // Write file with explicit scope to ensure handle is closed
                                    {
                                        let mut file_handle = File::create(&local_path)?;
                                        file_handle.write_all(&bytes)?;
                                        // File handle is closed when it goes out of scope
                                    }

                                    println!("Successfully downloaded emote {} to {:?}", active_emote.name, local_path);
                                    success = true;
                                } else if response.status().as_u16() == 429 {
                                    // Rate limited, wait longer
                                    println!("Rate limited (429) when downloading {}. Retrying after delay...", active_emote.name);
                                    thread::sleep(Duration::from_millis(DOWNLOAD_DELAY_MS * 5));
                                } else {
                                    return Err(format!(
                                        "Failed to download emote image {} from {}: HTTP {}",
                                        active_emote.name, emote_url, response.status()
                                    ).into());
                                }

                                Ok(())
                            })();

                            if let Err(e) = download_result {
                                eprintln!("Error processing download for emote {} (attempt {}/{}): {:?}",
                                          active_emote.name, retry, MAX_RETRIES, e);

                                if retry == MAX_RETRIES {
                                    // Last attempt failed, let's clean up any partial download
                                    if local_path.exists() {
                                        let _ = fs::remove_file(&local_path);
                                    }
                                }

                                // Wait before retrying
                                thread::sleep(Duration::from_millis(DOWNLOAD_DELAY_MS * retry as u64));
                            } else if success {
                                break; // Successfully downloaded, no need to retry
                            }
                        }

                        // Small delay between downloads to avoid overwhelming the server
                        thread::sleep(Duration::from_millis(DOWNLOAD_DELAY_MS));
                    }
                }
            }
        }

        // Delay between batches to avoid rate limiting
        if batch_idx < batch_count - 1 {
            println!("Finished batch {}/{}. Waiting before next batch...", batch_idx + 1, batch_count);
            thread::sleep(Duration::from_millis(BATCH_DELAY_MS));
        }
    }

    println!("Finished processing all {} new emotes for channel {}", new_emotes, channel_id);
    Ok(())
}

/// Find the best image file for an emote
fn find_best_image_file(files: &[ImageFile]) -> Option<&ImageFile> {
    // Prefer 1x since it saves space
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format == "WEBP") {
        return Some(file);
    }
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format == "GIF") {
        return Some(file);
    }
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format == "PNG") {
        return Some(file);
    }
    files.first()
}

/// Converts an `RGBColor` to a CSS hex string like "#RRGGBB"
fn rgb_to_hex(color: &RGBColor) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b)
}

pub fn parse_message(msg: &PrivmsgMessage, emote_map: &HashMap<String, Emote>) -> Widget {
    // Create the main container
    let container = GtkBox::new(Orientation::Vertical, 2);
    container.set_margin_top(4);
    container.set_margin_bottom(4);
    container.set_margin_start(8);
    container.set_margin_end(8);
    container.add_css_class("message-box");

    // Create header with username and timestamp
    let header_box = GtkBox::new(Orientation::Horizontal, 0);
    header_box.set_hexpand(true);

    // Set up username label with appropriate color
    let sender_label = Label::new(None);
    sender_label.set_xalign(0.0);
    sender_label.set_hexpand(true);

    if let Some(color) = &msg.name_color {
        let color_hex = rgb_to_hex(color);
        sender_label.set_markup(&format!(
            "<span foreground=\"{}\"><b>{}</b></span>",
            color_hex,
            glib::markup_escape_text(&msg.sender.name)
        ));
    } else {
        sender_label.set_markup(&format!(
            "<b>{}</b>",
            glib::markup_escape_text(&msg.sender.name)
        ));
    }

    // Set up timestamp
    let timestamp = Label::new(Some(
        &msg.server_timestamp
            .with_timezone(&Local)
            .format("%-I:%M:%S %p")
            .to_string(),
    ));
    timestamp.add_css_class("dim-label");
    timestamp.set_xalign(1.0);

    header_box.append(&sender_label);
    header_box.append(&timestamp);

    // Create message content box
    let message_box = GtkBox::new(Orientation::Horizontal, 2);
    message_box.set_hexpand(true);
    message_box.set_valign(gtk::Align::Start);
    message_box.set_halign(gtk::Align::Start);
    message_box.add_css_class("message-content");

    // Create header with username and timestamp
    let header_box = GtkBox::new(Orientation::Horizontal, 0);
    header_box.set_hexpand(true);

    // Set up username label with appropriate color
    let sender_label = Label::new(None);
    sender_label.set_xalign(0.0);
    sender_label.set_hexpand(true);

    if let Some(color) = &msg.name_color {
        let color_hex = rgb_to_hex(color);
        sender_label.set_markup(&format!(
            "<span foreground=\"{}\"><b>{}</b></span>",
            color_hex,
            glib::markup_escape_text(&msg.sender.name)
        ));
    } else {
        sender_label.set_markup(&format!(
            "<b>{}</b>",
            glib::markup_escape_text(&msg.sender.name)
        ));
    }

    // Set up timestamp
    let timestamp = Label::new(Some(
        &msg.server_timestamp
            .with_timezone(&Local)
            .format("%-I:%M:%S %p")
            .to_string(),
    ));
    timestamp.add_css_class("dim-label");
    timestamp.set_xalign(1.0);

    header_box.append(&sender_label);
    header_box.append(&timestamp);

    // Use regex to split message into tokens (words and whitespace)
    let re = Regex::new(r"(\s+|\S+)").unwrap();
    let mut buffer = String::new();

    let container = GtkBox::new(Orientation::Vertical, 2);
    container.set_margin_top(4);
    container.set_margin_bottom(4);
    container.set_margin_start(8);
    container.set_margin_end(8);
    container.add_css_class("message-box");

    container.append(&header_box);

    let message_box = GtkBox::new(Orientation::Horizontal, 2);
    message_box.set_hexpand(true);
    message_box.set_valign(gtk::Align::Start);
    message_box.set_halign(gtk::Align::Start);
    message_box.add_css_class("message-content");
    container.append(&message_box);

    let row = ListBoxRow::new();
    row.set_child(Some(&container));
    row.add_css_class("message-row");

    // Create a RefCell to hold the MediaResourceManager for this row
    let resource_manager = std::rc::Rc::new(std::cell::RefCell::new(MediaResourceManager::new()));
    let resource_manager_clone_for_destroy = std::rc::Rc::clone(&resource_manager);

    for cap in re.find_iter(&msg.message_text) {
        let word = cap.as_str();
        let word_trim = word.trim();

        // Check if this word is an emote
        if !word_trim.is_empty() && emote_map.contains_key(word_trim) {
            // Flush any accumulated text before adding the emote
            if !buffer.is_empty() {
                let label = Label::new(Some(&buffer));
                label.set_wrap(true);
                label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                label.set_xalign(0.0);
                message_box.append(&label);
                buffer.clear();
            }

            // Get emote details
            let emote = &emote_map[word_trim];
            let path = shellexpand::tilde(&emote.local_path).to_string();

            // Skip if file doesn't exist
            if !Path::new(&path).exists() {
                buffer.push_str(word);
                continue;
            }

            // Create appropriate widget based on whether it's a GIF or static image
            if emote.is_gif {
                let gif_resource = GifMediaResource::new(&path);
                message_box.append(&gif_resource.get_widget());
                resource_manager.borrow_mut().add_resource(gif_resource);
            } else {
                let static_resource = StaticImageResource::new(&path);
                message_box.append(&static_resource.get_widget());
                resource_manager.borrow_mut().add_resource(static_resource);
            }
        } else {
            // Accumulate regular text
            buffer.push_str(word);
        }
    }

    // Flush any remaining text
    if !buffer.is_empty() {
        let label = Label::new(Some(&buffer));
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_xalign(0.0);
        message_box.append(&label);
    }

    // Clean up all media resources when the row is destroyed
    row.connect_destroy(move |_| {
        let mut rm = resource_manager_clone_for_destroy.borrow_mut();
        rm.resources.iter_mut().for_each(|res| res.cleanup());
        rm.resources.clear();
        println!("Message row destroyed, media resources cleaned up.");
    });

    // Apply CSS styling
    let css_provider = gtk::CssProvider::new();
    css_provider.load_from_data(
        "
        .message-box {
            border: 1px solid alpha(#999, 0.3);
            border-radius: 8px;
            padding: 8px;
            background-color: alpha(#fff, 0.02);
        }
        .message-row {
            background-color: transparent;
        }
        .dim-label {
            color: alpha(#aaa, 0.8);
            font-size: 0.8em;
        }
        .message-content {
            padding-top: 4px;
        }
        ",
    );

    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    row.upcast::<Widget>()
}

struct MediaResourceManager {
    resources: Vec<Box<dyn MediaResource>>,
}

impl MediaResourceManager {
    fn new() -> Self {
        Self { resources: Vec::new() }
    }

    fn add_resource<T: MediaResource + 'static>(&mut self, resource: T) {
        self.resources.push(Box::new(resource));
    }
}
