// emotes.rs

use gtk::prelude::*; // For glib::markup_escape_text
use twitch_irc::message::PrivmsgMessage; // Import the message struct
use chrono::Local;
use twitch_irc::message::RGBColor;
use std::{collections::HashMap, sync::Arc, time::{Duration, Instant}};
use reqwest::blocking::Client; // Blocking client for background threads
use std::sync::{Mutex, RwLock, mpsc};
use std::{thread, collections::HashSet};
use std::error::Error as StdError;
use serde::Deserialize;
use once_cell::sync::Lazy;

pub static MESSAGE_CSS: &str = "
.message-box {
    border: 1px solid alpha(#999, 0.3);
    border-radius: 8px;
    padding: 8px;
    margin-bottom: 4px;
    background-color: alpha(#fff, 0.02);
    display: block;
    overflow: hidden;
    box-sizing: border-box;
}
.message-row {
    background-color: transparent;
}
.message-header {
    display: block;
    margin-bottom: 4px;
}
.message-text {
    font-size: 12pt;
    line-height: 28px; /* Consistent line height matching emote size */
    display: inline;
}
.message-text img {
    display: inline-block;
    vertical-align: middle;
    height: 28px;
    width: auto;
    margin: 0 2px;
    max-height: 28px;
    max-width: 28px;
    pointer-events: auto;
    cursor: pointer;
    will-change: auto;
    backface-visibility: hidden;
    transition: transform 0.1s ease;
}

.message-text img:hover {
    transform: scale(1.1);
}
.dim-label {
    color: alpha(#aaa, 0.8);
    font-size: 0.8em;
}
.message-content {
    padding-top: 4px;
    display: block;
    word-wrap: break-word;
    overflow-wrap: break-word;
}
.sender {
    font-weight: bold;
}
.timestamp {
    color: alpha(#aaa, 0.8);
    font-size: 0.8em;
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

// --- Global State for Emote Maps and Fetching ---
static EMOTE_MAPS: Lazy<RwLock<HashMap<String, HashMap<String, String>>>> = Lazy::new(|| RwLock::new(HashMap::new())); // channel_id -> {emote_name -> remote_url}
static DOWNLOADING_CHANNELS: Lazy<RwLock<HashMap<String, bool>>> = Lazy::new(|| RwLock::new(HashMap::new()));
static LAST_FETCH_TIME: Lazy<RwLock<HashMap<String, Instant>>> = Lazy::new(|| RwLock::new(HashMap::new()));

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
    url: String, // Base URL for the host (e.g., cdn.7tv.app)
    files: Vec<ImageFile>,
}

#[derive(Debug, Deserialize, Clone)]
struct ImageFile {
    name: String, // Filename (e.g., 1x.webp)
    format: String, // Format (e.g., "WEBP", "PNG", "GIF")
}

pub fn cleanup_emote_cache() {
    let mut last_fetch = LAST_FETCH_TIME.write().unwrap();
    let now = Instant::now();

    // Collect channels to remove
    let channels_to_remove: Vec<String> = last_fetch
        .iter()
        .filter_map(|(channel_id, time)| {
            if now.duration_since(*time) >= Duration::from_secs(3600) {
                Some(channel_id.clone())
            } else {
                None
            }
        })
        .collect();

    // Remove from all caches
    for channel_id in channels_to_remove {
        last_fetch.remove(&channel_id);
        EMOTE_MAPS.write().unwrap().remove(&channel_id);
        DOWNLOADING_CHANNELS.write().unwrap().remove(&channel_id);
        println!("Removed emote data for inactive channel: {}", channel_id);
    }

    println!("Cleaned up cache, {} channels remaining.", last_fetch.len());
}

pub fn cleanup_media_file_cache() {
    // No local files to clean now.
    glib::idle_add_local_once(|| {
        println!("No local emote cache to clean.");
    });
}

// --- Emote Map Retrieval (Uses Remote URLs) ---
pub fn get_emote_map(channel_id: &str) -> HashMap<String, String> { // Return map of emote_name -> remote_url
    // Check if map already exists in memory
    {
        let maps_read = EMOTE_MAPS.read().unwrap();
        if let Some(map) = maps_read.get(channel_id) {
            return map.clone(); // Return the existing map
        }
    }

    // If not in memory, trigger background fetch and return empty map for now
    // The calling function (parse_message_html) will likely call again shortly after fetch completes.
    fetch_missing_emotes(channel_id);

    // Return an empty map if not yet fetched
    HashMap::new()
}

const FETCH_COOLDOWN: Duration = Duration::from_secs(60 * 1); // 1 minute

// --- Background Emote Fetching (Updates In-Memory Map) ---
fn fetch_missing_emotes(channel_id: &str) -> Option<thread::JoinHandle<()>> {
    let channel_id = channel_id.to_string(); // Clone for thread
    let now = Instant::now();

    // Check if download is already in progress
    {
        let downloading = DOWNLOADING_CHANNELS.read().unwrap();
        if downloading.get(&channel_id).copied().unwrap_or(false) {
            return None;
        }
    }

    // Check if cooldown period has passed
    {
        let last_fetch_read = LAST_FETCH_TIME.read().unwrap();
        if let Some(&last_fetch) = last_fetch_read.get(&channel_id) {
            if now.duration_since(last_fetch) < FETCH_COOLDOWN {
                return None;
            }
        }
    }

    // Check in-memory map again just before starting fetch (double-check)
    {
        let maps_read = EMOTE_MAPS.read().unwrap();
        if maps_read.contains_key(&channel_id) {
             // Update fetch time anyway
             let mut last_fetch_write = LAST_FETCH_TIME.write().unwrap();
             last_fetch_write.insert(channel_id.clone(), now);
             return None;
        }
    }

    // Clone channel_id for the thread
    let channel_id_clone = channel_id.clone();
    let handle = thread::spawn(move || {
        match download_emote_urls(&channel_id_clone) { // Fetch remote URLs
            Ok(remote_emote_map) => {
                // Store the fetched map in the global in-memory cache
                let mut maps_write = EMOTE_MAPS.write().unwrap();
                maps_write.insert(channel_id_clone.clone(), remote_emote_map);
            }
            Err(e) => {
                eprintln!("Failed to fetch emote URLs for channel_id {}: {:?}", channel_id_clone, e);
            }
        }
        // Mark download as finished
        let mut downloading = DOWNLOADING_CHANNELS.write().unwrap();
        downloading.insert(channel_id_clone.clone(), false);
        // Update fetch time
        let mut last_fetch_write = LAST_FETCH_TIME.write().unwrap();
        last_fetch_write.insert(channel_id_clone.clone(), now);
    });

    // Mark download as in progress
    {
        let mut downloading = DOWNLOADING_CHANNELS.write().unwrap();
        downloading.insert(channel_id.clone(), true);
    }

    Some(handle)
}

// --- Download Logic (Fetches Remote URLs) ---
fn download_emote_urls(channel_id: &str) -> Result<HashMap<String, String>, Box<dyn StdError + Send + Sync>> { // Return map of name -> remote URL
    let client = Client::new();
    let twitch_lookup_url = format!("https://7tv.io/v3/users/twitch/{}", channel_id);
    const MAX_RETRIES: usize = 3;

    let mut success = false;
    let mut response_text = String::new();
    for retry in 1..=MAX_RETRIES {
        let response = client.get(&twitch_lookup_url).send()?;
        if response.status().is_success() {
            response_text = response.text()?;
            success = true;
            break;
        } else if response.status().as_u16() == 429 {
            thread::sleep(Duration::from_secs(2 * retry as u64)); // Exponential backoff
        } else {
            return Err(format!("7TV API request failed with status {}: {}", response.status(), response.text().unwrap_or_else(|_| "No error body".to_string())).into());
        }
    }

    if !success {
        return Err(format!("Failed to fetch 7TV API response for channel_id {} after {} retries.", channel_id, MAX_RETRIES).into());
    }

    let user_response: SevenTVUserResponse = serde_json::from_str(&response_text)?;

    let mut remote_emote_map = HashMap::new();

    if let Some(api_emote_set) = user_response.emote_set {
        for active_emote in api_emote_set.emotes {
            if let Some(emote_data) = &active_emote.data {
                if let Some(host_info) = &emote_data.host {
                    if host_info.url.trim().is_empty() {
                        continue;
                    }
                    let file_opt = find_best_image_file(&host_info.files);
                    if let Some(file_to_use) = file_opt {
                        // Construct the full URL for the specific file
                        let base_emote_url = host_info.url.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("//");
                        let emote_remote_url = format!("https://{}/{}", base_emote_url, file_to_use.name);
                        remote_emote_map.insert(active_emote.name, emote_remote_url);
                    }
                }
            }
        }
    }

    Ok(remote_emote_map)
}

// --- Helper Functions ---
fn find_best_image_file(files: &[ImageFile]) -> Option<&ImageFile> {
    // Prioritize 1x versions, then prefer GIF for animation, then PNG for quality, then first available
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format.eq_ignore_ascii_case("gif")) {
        return Some(file);
    }
    if let Some(file) = files.iter().find(|f| f.name.contains("1x") && f.format.eq_ignore_ascii_case("png")) {
        return Some(file);
    }
    if let Some(file) = files.iter().find(|f| f.name.contains("1x")) {
         return Some(file);
    }
    // If no 1x found, look for any GIF
     if let Some(file) = files.iter().find(|f| f.format.eq_ignore_ascii_case("gif")) {
        return Some(file);
    }
    // Otherwise, take the first one (could prioritize PNG over others)
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

// --- Parse Message to HTML (Updated to use remote URLs) ---
pub fn parse_message_html(msg: &PrivmsgMessage, emote_map: &HashMap<String, String>) -> String { // emote_map is now name -> remote_url
    let sender_name_escaped = glib::markup_escape_text(&msg.sender.name);
    let timestamp = msg.server_timestamp
        .with_timezone(&Local)
        .format("%-I:%M:%S %p")
        .to_string();
    let timestamp_escaped = glib::markup_escape_text(&timestamp);

    let sender_color_html = if let Some(color) = &msg.name_color {
        let color_hex = rgb_to_hex(color);
        format!(r#"<span class="sender" style="color: {};">{}</span>"#, color_hex, sender_name_escaped)
    } else {
        format!(r#"<span class="sender">{}</span>"#, sender_name_escaped)
    };

    // Process message text to replace emotes with <img> tags
    let mut html_content = String::with_capacity(msg.message_text.len() * 2);
    let words = msg.message_text.split_whitespace();
    let mut first = true;

    for word in words {
        if !first {
            html_content.push(' ');
        }
        first = false;

        if let Some(remote_url) = emote_map.get(word) {
            // It's an emote, add the <img> tag with the remote URL
            let emote_name_escaped = glib::markup_escape_text(word);
            let remote_url_escaped = glib::markup_escape_text(remote_url);
            html_content.push_str(r#"<img src=""#);
            html_content.push_str(&remote_url_escaped);
            html_content.push_str(r#"" alt=":"#);
            html_content.push_str(&emote_name_escaped);
            html_content.push_str(r#":" title="Click to view emote details" loading="lazy" crossorigin="anonymous"/>"#);
        } else {
            // Not an emote, just add the word as escaped text
            html_content.push_str(&glib::markup_escape_text(word));
        }
    }

    format!(
        r#"<div class="message-box"><div class="message-header">{} <span class="timestamp">{}</span></div><div class="message-content"><span class="message-text">{}</span></div></div>"#,
        sender_color_html,
        timestamp_escaped,
        html_content
    )
}
