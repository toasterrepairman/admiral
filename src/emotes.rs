use gtk::prelude::*;
use gtk::{glib, gdk, gio, Image, Label, Orientation, Widget, ListBoxRow, MediaFile, Picture};
use gtk::Box as GtkBox;
use glib::prelude::*;
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
use std::rc::Rc;
use std::cell::RefCell;

// Global emote cache to prevent loading the same emote multiple times
static EMOTE_CACHE: Lazy<RwLock<HashMap<String, Arc<CachedEmote>>>> = Lazy::new(|| RwLock::new(HashMap::new()));

// Tracking which channels are currently being processed
static DOWNLOADING_CHANNELS: Lazy<RwLock<HashMap<String, bool>>> = Lazy::new(|| RwLock::new(HashMap::new()));

// Tracking the last time we fetched emotes for a channel
static LAST_FETCH_TIME: Lazy<RwLock<HashMap<String, Instant>>> = Lazy::new(|| RwLock::new(HashMap::new()));

// Cached emote data to prevent reloading
#[derive(Debug, Clone)]
struct CachedEmote {
    name: String,
    texture: Option<gdk::Texture>,
    is_gif: bool,
    path: String,
}

impl CachedEmote {
    fn new(name: String, path: String, is_gif: bool) -> Self {
        let texture = if Path::new(&path).exists() {
            gdk::Texture::from_filename(&path).ok()
        } else {
            None
        };

        Self {
            name,
            texture,
            is_gif,
            path,
        }
    }
}

// Your existing Emote struct for local representation
#[derive(Debug, Clone)]
pub struct Emote {
    name: String,
    url: String,
    local_path: String,
    is_gif: bool,
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

// Lightweight emote widget that reuses cached resources
struct EmoteWidget {
    widget: Picture,
    _media_file: Option<MediaFile>, // Keep reference to prevent cleanup
}

impl EmoteWidget {
    fn new(cached_emote: &Arc<CachedEmote>) -> Self {
        let picture = Picture::new();
        picture.set_size_request(-0, 28);
        picture.set_can_shrink(false);

        let mut media_file = None;

        if let Some(ref texture) = cached_emote.texture {
            if cached_emote.is_gif {
                // For GIFs, we need MediaFile for animation
                let mf = MediaFile::for_filename(&cached_emote.path);
                mf.set_loop(true);
                picture.set_paintable(Some(&mf));
                mf.play();
                media_file = Some(mf);
            } else {
                // For static images, use the texture directly
                picture.set_paintable(Some(texture));
            }
        }

        Self {
            widget: picture,
            _media_file: media_file,
        }
    }

    fn get_widget(&self) -> Widget {
        self.widget.clone().upcast::<Widget>()
    }
}

// Resource manager for cleanup
struct MessageResourceManager {
    emote_widgets: Vec<EmoteWidget>,
}

impl MessageResourceManager {
    fn new() -> Self {
        Self {
            emote_widgets: Vec::new(),
        }
    }

    fn add_emote_widget(&mut self, widget: EmoteWidget) {
        self.emote_widgets.push(widget);
    }

    fn cleanup(&mut self) {
        // Explicit cleanup of MediaFile resources
        for emote_widget in &mut self.emote_widgets {
            if let Some(ref media_file) = emote_widget._media_file {
                if media_file.is_playing() {
                    media_file.pause();
                }
                // Clear the paintable to free VRAM
                emote_widget.widget.set_paintable(None::<&gdk::Paintable>);
            }
        }
        self.emote_widgets.clear();
    }
}

// Get cached emote or load it
fn get_cached_emote(emote: &Emote) -> Arc<CachedEmote> {
    let cache_key = format!("{}:{}", emote.name, emote.local_path);

    {
        let cache = EMOTE_CACHE.read().unwrap();
        if let Some(cached) = cache.get(&cache_key) {
            return Arc::clone(cached);
        }
    }

    // Load emote into cache
    let cached_emote = Arc::new(CachedEmote::new(
        emote.name.clone(),
        emote.local_path.clone(),
        emote.is_gif,
    ));

    {
        let mut cache = EMOTE_CACHE.write().unwrap();
        cache.insert(cache_key, Arc::clone(&cached_emote));
    }

    cached_emote
}

// Periodic cache cleanup to prevent unbounded growth
pub fn cleanup_emote_cache() {
    let mut cache = EMOTE_CACHE.write().unwrap();

    // Remove entries where the file no longer exists
    cache.retain(|_, cached_emote| {
        Path::new(&cached_emote.path).exists()
    });

    // If cache is still too large, remove oldest entries
    if cache.len() > 1000 {
        let keys_to_remove: Vec<_> = cache.keys().take(cache.len() - 800).cloned().collect();
        for key in keys_to_remove {
            cache.remove(&key);
        }
    }
}

/// Synchronize with 7TV if needed
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
            if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                let is_gif = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("gif"))
                    .unwrap_or(false);
                let full_path = path.to_string_lossy().to_string();
                let emote = Emote {
                    name: file_stem.to_string(),
                    url: full_path.clone(),
                    local_path: full_path,
                    is_gif,
                };
                emotes.insert(file_stem.to_string(), emote);
            }
        }
    }

    fetch_missing_emotes(channel_id);
    emotes
}

const FETCH_COOLDOWN: Duration = Duration::from_secs(60 * 1);

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
                return None;
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
                        "Detected {} missing emotes for channel {}, starting download...",
                        missing_emotes.len(),
                        channel_id
                    );
                    if let Err(e) = download_channel_emotes(&channel_id) {
                        eprintln!("Failed to download emotes for channel {}: {:?}", channel_id, e);
                    }

                    let mut downloading = DOWNLOADING_CHANNELS.write().unwrap();
                    downloading.insert(channel_id.clone(), false);
                }))
            } else {
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

    const MAX_EMOTES_PER_BATCH: usize = 50;
    const BATCH_DELAY_MS: u64 = 500;
    const DOWNLOAD_DELAY_MS: u64 = 100;
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

    let mut existing_emotes: HashSet<String> = HashSet::new();
    if let Ok(entries) = fs::read_dir(&channel_path) {
        for entry in entries.flatten() {
            if let Some(file_name) = entry.path().file_stem().and_then(|s| s.to_str()) {
                existing_emotes.insert(file_name.to_string());
            }
        }
    }

    let total_emotes = api_emote_set.emotes.len();
    let emotes_to_process: Vec<_> = api_emote_set.emotes.into_iter()
        .filter(|e| !existing_emotes.contains(&e.name))
        .collect();
    let new_emotes = emotes_to_process.len();

    println!(
        "Channel {} has {} total emotes. Found {} new emotes to download.",
        channel_id, total_emotes, new_emotes
    );

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

                        let mut success = false;
                        for retry in 1..=MAX_RETRIES {
                            let download_result = (|| -> Result<(), Box<dyn StdError + Send + Sync>> {
                                let response = client.get(&emote_url).send()?;

                                if response.status().is_success() {
                                    let bytes = response.bytes()?;

                                    {
                                        let mut file_handle = File::create(&local_path)?;
                                        file_handle.write_all(&bytes)?;
                                    }

                                    success = true;
                                } else if response.status().as_u16() == 429 {
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
                                    if local_path.exists() {
                                        let _ = fs::remove_file(&local_path);
                                    }
                                }

                                thread::sleep(Duration::from_millis(DOWNLOAD_DELAY_MS * retry as u64));
                            } else if success {
                                break;
                            }
                        }

                        thread::sleep(Duration::from_millis(DOWNLOAD_DELAY_MS));
                    }
                }
            }
        }

        if batch_idx < batch_count - 1 {
            thread::sleep(Duration::from_millis(BATCH_DELAY_MS));
        }
    }

    println!("Finished processing all {} new emotes for channel {}", new_emotes, channel_id);
    Ok(())
}

fn find_best_image_file(files: &[ImageFile]) -> Option<&ImageFile> {
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format == "GIF") {
        return Some(file);
    }
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format == "PNG") {
        return Some(file);
    }
    files.first()
}

fn rgb_to_hex(color: &RGBColor) -> String {
    let mut r = color.r as f32 / 255.0;
    let mut g = color.g as f32 / 255.0;
    let mut b = color.b as f32 / 255.0;

    let luminance = 0.299 * r + 0.587 * g + 0.114 * b;

    if luminance < 0.3 {
        let boost = 0.3 / (luminance + 0.001);
        r *= boost;
        g *= boost;
        b *= boost;
    }

    let avg = (r + g + b) / 3.0;
    let vibrancy_limit = 0.7;
    r = avg + (r - avg) * vibrancy_limit;
    g = avg + (g - avg) * vibrancy_limit;
    b = avg + (b - avg) * vibrancy_limit;

    let r = (r.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (g.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (b.clamp(0.0, 1.0) * 255.0).round() as u8;

    format!("#{:02X}{:02X}{:02X}", r, g, b)
}

pub fn parse_message(msg: &PrivmsgMessage, emote_map: &HashMap<String, Emote>) -> Widget {
    let container = GtkBox::new(Orientation::Vertical, 2);
    container.set_margin_top(4);
    container.set_margin_bottom(4);
    container.set_margin_start(8);
    container.set_margin_end(8);
    container.add_css_class("message-box");

    let header_box = GtkBox::new(Orientation::Horizontal, 0);
    header_box.set_hexpand(true);

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

    let message_box = GtkBox::new(Orientation::Horizontal, 2);
    message_box.set_hexpand(true);
    message_box.set_valign(gtk::Align::Start);
    message_box.set_halign(gtk::Align::Start);
    message_box.add_css_class("message-content");

    container.append(&header_box);
    container.append(&message_box);

    let row = ListBoxRow::new();
    row.set_child(Some(&container));
    row.add_css_class("message-row");

    // Resource manager for this message
    let resource_manager = Rc::new(RefCell::new(MessageResourceManager::new()));
    let resource_manager_cleanup = Rc::clone(&resource_manager);

    let re = Regex::new(r"(\s+|\S+)").unwrap();
    let mut buffer = String::new();

    for cap in re.find_iter(&msg.message_text) {
        let word = cap.as_str();
        let word_trim = word.trim();

        if !word_trim.is_empty() && emote_map.contains_key(word_trim) {
            if !buffer.is_empty() {
                let label = Label::new(Some(&buffer));
                label.set_wrap(true);
                label.add_css_class("message-text");
                label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                label.set_xalign(0.0);
                message_box.append(&label);
                buffer.clear();
            }

            let emote = &emote_map[word_trim];
            let cached_emote = get_cached_emote(emote);

            if cached_emote.texture.is_some() {
                let emote_widget = EmoteWidget::new(&cached_emote);
                message_box.append(&emote_widget.get_widget());
                resource_manager.borrow_mut().add_emote_widget(emote_widget);
            } else {
                buffer.push_str(word);
            }
        } else {
            buffer.push_str(word);
        }
    }

    if !buffer.is_empty() {
        let label = Label::new(Some(&buffer));
        label.set_wrap(true);
        label.add_css_class("message-text");
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_xalign(0.0);
        message_box.append(&label);
    }

    // Cleanup resources when row is destroyed
    row.connect_destroy(move |_| {
        resource_manager_cleanup.borrow_mut().cleanup();
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
        .message-text {
            font-size: 12pt;
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
