// emotes.rs

use gtk::prelude::*;
use gtk::{glib, gdk, gio, Image, Label, Orientation, Widget, ListBoxRow, MediaFile, Picture, Popover, PopoverMenu, GestureClick};
use gtk::Box as GtkBox;
use glib::prelude::*;
use twitch_irc::message::PrivmsgMessage;
use chrono::Local;
use twitch_irc::message::RGBColor;
use std::fs;
use std::{collections::HashMap, fs::File, io::Write, path::Path, sync::Arc, time::{Duration, Instant}};
use reqwest::blocking::{Client, Response};
use std::sync::{Mutex, RwLock, mpsc};
use std::{path::{PathBuf}, thread, collections::HashSet};
use std::error::Error as StdError;
use serde::Deserialize;
use once_cell::sync::Lazy;
use regex::Regex;
use std::rc::Rc;
use std::cell::RefCell;

pub static MESSAGE_CSS: &str = "
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
    line-height: 28px; /* Consistent line height matching emote size */
}
.dim-label {
    color: alpha(#aaa, 0.8);
    font-size: 0.8em;
}
.message-content {
    padding-top: 4px;
}
.emote-popover-label {
    font-family: monospace;
    font-size: 11pt;
    padding: 4px 8px;
}
.flowbox-child {
    margin: 0;
    padding: 0;
}
";

// --- Global Caches and State ---
// Cache for static image textures only (PNGs)
static TEXTURE_CACHE: Lazy<RwLock<HashMap<String, Arc<gdk::Texture>>>> = Lazy::new(|| RwLock::new(HashMap::new()));

// Tracking which channels are currently being processed
static DOWNLOADING_CHANNELS: Lazy<RwLock<HashMap<String, bool>>> = Lazy::new(|| RwLock::new(HashMap::new()));

// Tracking the last time we fetched emotes for a channel
static LAST_FETCH_TIME: Lazy<RwLock<HashMap<String, Instant>>> = Lazy::new(|| RwLock::new(HashMap::new()));

// LRU cache for GIF MediaFiles with size limit
thread_local! {
    static GIF_MEDIA_CACHE: RefCell<LruMediaCache> = RefCell::new(LruMediaCache::new(30)); // Max 30 GIFs in memory
}

// Simple LRU cache for MediaFiles
struct LruMediaCache {
    cache: HashMap<String, (MediaFile, Instant)>,
    max_size: usize,
}

impl LruMediaCache {
    fn new(max_size: usize) -> Self {
        Self {
            cache: HashMap::new(),
            max_size,
        }
    }

    fn get(&mut self, key: &str) -> Option<MediaFile> {
        if let Some((media_file, _)) = self.cache.get(key) {
            // Clone the media_file before updating
            let media_file_clone = media_file.clone();
            // Update access time
            self.cache.insert(key.to_string(), (media_file_clone.clone(), Instant::now()));
            Some(media_file_clone)
        } else {
            None
        }
    }

    fn insert(&mut self, key: String, media_file: MediaFile) {
        // If at capacity, remove oldest entry
        if self.cache.len() >= self.max_size && !self.cache.contains_key(&key) {
            if let Some(oldest_key) = self.cache.iter()
                .min_by_key(|(_, (_, time))| time)
                .map(|(k, _)| k.clone())
            {
                if let Some((old_media, _)) = self.cache.remove(&oldest_key) {
                    // Clean up the old MediaFile
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if old_media.is_playing() {
                            old_media.pause();
                        }
                        old_media.set_file(None::<&gio::File>);
                    }));
                }
            }
        }

        self.cache.insert(key, (media_file, Instant::now()));
    }

    fn clear(&mut self) {
        // Clean up all MediaFiles
        for (_, (media_file, _)) in self.cache.drain() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if media_file.is_playing() {
                    media_file.pause();
                }
                media_file.set_file(None::<&gio::File>);
            }));
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

// --- Simplified Emote Widget ---
struct EmoteWidget {
    picture: Picture,
    popover: Popover,
    emote_name: String,
}

impl EmoteWidget {
    fn new(emote: &Emote) -> Self {
        let picture = Picture::new();
        picture.set_size_request(28, 28);
        picture.set_hexpand(false);
        picture.set_vexpand(false);
        picture.set_halign(gtk::Align::Start);
        picture.set_valign(gtk::Align::Start);

        if emote.is_gif {
            // For GIFs, use LRU cached MediaFile or create new one
            let media_file = GIF_MEDIA_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();

                // Try to get from cache
                if let Some(cached) = cache.get(&emote.local_path) {
                    cached
                } else {
                    // Create new MediaFile
                    let media_file = MediaFile::for_filename(&emote.local_path);
                    media_file.set_loop(true);
                    media_file.play();

                    // Add to cache
                    cache.insert(emote.local_path.clone(), media_file.clone());
                    media_file
                }
            });

            picture.set_paintable(Some(&media_file));
        } else {
            // For static images, use shared texture cache
            let texture = get_or_load_texture(&emote.local_path);
            if let Some(tex) = texture {
                // Dereference Arc to get &Texture
                picture.set_paintable(Some(&*tex));
            }
        }

        // Create popover with emote name
        let popover = Popover::new();
        popover.set_parent(&picture);
        popover.set_position(gtk::PositionType::Top);
        popover.set_autohide(true);
        let popover_label = Label::new(Some(&format!(":{}: ", emote.name)));
        popover_label.add_css_class("emote-popover-label");
        popover.set_child(Some(&popover_label));

        // Add click gesture to show popover
        let gesture = GestureClick::new();
        gesture.set_button(0);
        let popover_clone = popover.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            popover_clone.popup();
        });
        picture.add_controller(gesture);

        Self {
            picture,
            popover,
            emote_name: emote.name.clone(),
        }
    }

    fn get_widget(&self) -> Widget {
        self.picture.clone().upcast::<Widget>()
    }

    fn cleanup(&self) {
        // Unparent the popover
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.popover.unparent();
        }));
    }
}

// Get or load a texture from cache
fn get_or_load_texture(path: &str) -> Option<Arc<gdk::Texture>> {
    // Check cache first
    {
        let cache = TEXTURE_CACHE.read().unwrap();
        if let Some(texture) = cache.get(path) {
            return Some(Arc::clone(texture));
        }
    }

    // Load texture if file exists
    if !Path::new(path).exists() {
        return None;
    }

    let texture = gdk::Texture::from_filename(path).ok()?;
    let arc_texture = Arc::new(texture);

    // Store in cache
    {
        let mut cache = TEXTURE_CACHE.write().unwrap();
        cache.insert(path.to_string(), Arc::clone(&arc_texture));
    }

    Some(arc_texture)
}

// --- Cache Cleanup Functions ---
pub fn cleanup_emote_cache() {
    // Clean up texture cache
    let mut texture_cache = TEXTURE_CACHE.write().unwrap();
    texture_cache.retain(|path, _| Path::new(path).exists());

    // If cache is still too large, remove half
    if texture_cache.len() > 500 {
        let keys_to_remove: Vec<_> = texture_cache.keys().take(texture_cache.len() / 2).cloned().collect();
        for key in keys_to_remove {
            texture_cache.remove(&key);
        }
    }

    // Clean up LAST_FETCH_TIME
    let mut last_fetch = LAST_FETCH_TIME.write().unwrap();
    let now = Instant::now();
    last_fetch.retain(|_, time| now.duration_since(*time) < Duration::from_secs(3600)); // Keep entries for 1 hour
}

pub fn cleanup_media_file_cache() {
    glib::idle_add_local_once(|| {
        GIF_MEDIA_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();

            // Remove entries that haven't been accessed in 60 seconds
            let now = Instant::now();
            let keys_to_remove: Vec<_> = cache.cache.iter()
                .filter(|(_, (_, time))| now.duration_since(*time) > Duration::from_secs(60))
                .map(|(k, _)| k.clone())
                .collect();

            for key in keys_to_remove {
                if let Some((media_file, _)) = cache.cache.remove(&key) {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if media_file.is_playing() {
                            media_file.pause();
                        }
                        media_file.set_file(None::<&gio::File>);
                    }));
                }
            }
        });
    });
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

// --- Simplified Message Resource Manager ---
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
        for widget in self.emote_widgets.drain(..) {
            widget.cleanup();
        }
    }
}

// --- Message Parsing and Widget Creation ---
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

    // Use a FlowBox for the message content
    let message_flowbox = gtk::FlowBox::new();
    message_flowbox.set_hexpand(true);
    message_flowbox.set_valign(gtk::Align::Start);
    message_flowbox.set_halign(gtk::Align::Start);
    message_flowbox.set_row_spacing(0);
    message_flowbox.set_column_spacing(0);
    message_flowbox.set_max_children_per_line(100);
    message_flowbox.set_homogeneous(false);
    message_flowbox.add_css_class("message-content");

    container.append(&header_box);
    container.append(&message_flowbox);

    let row = ListBoxRow::new();
    row.set_child(Some(&container));
    row.add_css_class("message-row");

    // Resource manager for this message
    let resource_manager = Rc::new(RefCell::new(MessageResourceManager::new()));
    let resource_manager_cleanup = Rc::clone(&resource_manager);

    // Process message text
    let re = Regex::new(r"(\S+|\s+)").unwrap();
    let mut current_chunk = String::new();

    for cap in re.find_iter(&msg.message_text) {
        let segment = cap.as_str();

        if segment.trim().is_empty() {
            current_chunk.push_str(segment);
            continue;
        }

        let is_emote = !segment.trim().is_empty() && emote_map.contains_key(segment.trim());

        if is_emote {
            if !current_chunk.is_empty() {
                let label = Label::new(Some(&current_chunk));
                label.set_wrap(true);
                label.add_css_class("message-text");
                label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                label.set_xalign(0.0);
                label.set_halign(gtk::Align::Start);
                label.set_valign(gtk::Align::Center);
                label.set_justify(gtk::Justification::Left);
                message_flowbox.append(&label);
                current_chunk.clear();
            }

            let emote = &emote_map[segment.trim()];
            let emote_widget = EmoteWidget::new(emote);
            message_flowbox.append(&emote_widget.get_widget());
            resource_manager.borrow_mut().add_emote_widget(emote_widget);
        } else {
            current_chunk.push_str(segment);
        }
    }

    if !current_chunk.is_empty() {
        let label = Label::new(Some(&current_chunk));
        label.set_wrap(true);
        label.add_css_class("message-text");
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_xalign(0.0);
        label.set_halign(gtk::Align::Start);
        label.set_valign(gtk::Align::Center);
        label.set_justify(gtk::Justification::Left);
        message_flowbox.append(&label);
    }

    // Cleanup resources when row is destroyed
    row.connect_destroy(move |_| {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let resource_manager_to_cleanup = resource_manager_cleanup.clone();
            glib::idle_add_local_once(move || {
                resource_manager_to_cleanup.borrow_mut().cleanup();
            });
        }));
    });

    row.upcast::<Widget>()
}
