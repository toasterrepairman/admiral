use gtk::prelude::*;
use twitch_irc::message::PrivmsgMessage;
use chrono::Local;
use twitch_irc::message::RGBColor;
use std::fs;
use std::{collections::HashMap, fs::File, io::Write, path::Path, sync::Arc, time::{Duration, Instant}};
use reqwest::blocking::Client;
use std::sync::{Mutex, RwLock, mpsc};
use std::{path::{PathBuf}, thread, collections::HashSet};
use std::error::Error as StdError;
use serde::Deserialize;
use once_cell::sync::Lazy;
use regex::Regex;

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
static DOWNLOADING_CHANNELS: Lazy<RwLock<HashMap<String, bool>>> = Lazy::new(|| RwLock::new(HashMap::new()));
static LAST_FETCH_TIME: Lazy<RwLock<HashMap<String, Instant>>> = Lazy::new(|| RwLock::new(HashMap::new()));

#[derive(Debug, Clone)]
pub struct Emote {
    name: String,
    url: String,
    local_path: String,
    is_gif: bool,
}

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

// --- Cache Cleanup Functions ---
pub fn cleanup_emote_cache() {
    let mut last_fetch = LAST_FETCH_TIME.write().unwrap();
    let now = Instant::now();
    last_fetch.retain(|_, time| now.duration_since(*time) < Duration::from_secs(3600));
}

pub fn cleanup_media_file_cache() {
    glib::idle_add_local_once(|| {
        println!("WebKit handles animated emotes, focusing disk cleanup.");
    });
}

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

// --- NEW FUNCTION: Parse Message to HTML ---
pub fn parse_message_html(msg: &PrivmsgMessage, emote_map: &HashMap<String, Emote>) -> String {
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
    let words: Vec<&str> = msg.message_text.split_whitespace().collect();
    let mut html_parts = Vec::new();


    for word in words {
        if let Some(emote) = emote_map.get(word) {
            // It's an emote, add the <img> tag

            // Use the local_path directly - it's already a full file path
            let emote_path_escaped = emote.local_path
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;");

            let emote_name_escaped = glib::markup_escape_text(&emote.name);
            html_parts.push(format!(
                r#"<img src="file://{}" alt=":{}:" title=":{}:">"#,
                emote_path_escaped, emote_name_escaped, emote_name_escaped
            ));
        } else {
            // Not an emote, just add the word as escaped text
            html_parts.push(glib::markup_escape_text(word).to_string());
        }
    }

    let html_content = html_parts.join(" ");

    format!(
        r#"<div class="message-box"><div class="message-header">{}<span class="timestamp">{}</span></div><div class="message-content">{}</div></div>"#,
        sender_color_html,
        timestamp_escaped,
        html_content
    )
}
