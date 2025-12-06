// main.rs

// Import the correct gio for webkit6
use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, TabBar, TabView, TabPage, TabOverview};
use gtk::{gdk, ScrolledWindow, Button, Entry, Button as GtkButton, Orientation, Box, Align, Stack, ListBoxRow, Popover};
use webkit6::WebView;
use webkit6::prelude::WebViewExt;
use std::sync::{Arc, Mutex};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};
use twitch_irc::login::StaticLoginCredentials;
use glib::clone;
use adw::gio::SimpleAction; // Use gio from adw to match versions
use std::collections::HashMap;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use tokio::runtime::Runtime;
use glib::source::idle_add_local;
use glib::ControlFlow;
use serde::Deserialize;
use serde::Serialize;
use shellexpand; // For path expansion
use std::fs; // For file operations
use std::path::Path; // For path handling
use std::io::{Read, Write}; // For reading/writing files
use toml; // For TOML serialization
use rlimit::{Resource};
use std::time::Instant;

mod auth; // Assuming these modules exist
mod emotes; // Assuming these modules exist
use crate::emotes::{MESSAGE_CSS, get_emote_map, parse_message_html, cleanup_emote_cache, cleanup_media_file_cache}; // Updated import

// Connection state management
#[derive(Debug, Clone)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected(String), // channel name
}

// Consolidated HTML template for chat WebView
fn get_chat_html_template() -> &'static str {
    r#"
    <!DOCTYPE html>
    <html>
    <head>
      <style>
        html, body {
            margin: 0;
            padding: 0;
            height: 100%;
            overflow: hidden;
        }
        body {
            display: flex;
            flex-direction: column;
            font-family: sans-serif;
            background-color: rgba(0, 0, 0, 0.95); /* Solid background prevents overdraw */
            color: inherit;
            will-change: transform; /* Hint for GPU acceleration */
            transform: translateZ(0); /* Force compositing layer */
        }
        #chat-container {
            flex: 1;
            overflow-y: auto;
            padding: 8px;
            display: flex;
            flex-direction: column;
            contain: layout style paint; /* Optimize repaints */
        }
        .message-box {
            border: 1px solid rgba(153, 153, 153, 0.3);
            border-radius: 8px;
            padding: 8px;
            margin-bottom: 4px;
            background-color: rgba(255, 255, 255, 0.02);
            contain: layout style paint; /* Isolate repaints */
        }
        .message-header { display: flex; justify-content: space-between; }
        .sender { font-weight: bold; }
        .timestamp { color: rgba(170, 170, 170, 0.8); font-size: 0.8em; }
        .message-content {
            margin-top: 4px;
            word-wrap: break-word;
            line-height: 28px;
            font-weight: light;
        }
        .message-content img {
            height: 28px;
            width: auto;
            vertical-align: middle;
            display: inline-block;
            margin: 0 2px;
            max-height: 28px;
            max-width: none;
            pointer-events: auto;
            cursor: pointer;
            will-change: auto;
            backface-visibility: hidden;
            transition: transform 0.1s ease;
        }
        .message-content img:hover {
            transform: scale(1.1);
        }
        /* Emote popover styles */
        .emote-popover {
            position: fixed;
            background-color: rgba(30, 30, 30, 0.95);
            border: 1px solid rgba(255, 255, 255, 0.2);
            border-radius: 8px;
            padding: 12px;
            z-index: 10000;
            box-shadow: 0 4px 20px rgba(0, 0, 0, 0.3);
            min-width: 200px;
            max-width: 300px;
            pointer-events: auto;
        }
        .emote-popover img {
            width: 80px;
            height: 80px;
            display: block;
            margin: 0 auto 8px;
            object-fit: contain;
            border-radius: 4px;
            background-color: rgba(255, 255, 255, 0.1);
            padding: 4px;
        }
        .emote-popover-name {
            font-weight: bold;
            text-align: center;
            margin-bottom: 4px;
            font-size: 14px;
        }
        .emote-popover-url {
            font-size: 10px;
            color: rgba(255, 255, 255, 0.6);
            text-align: center;
            word-break: break-all;
            font-family: monospace;
        }
        .emote-popover-close {
            position: absolute;
            top: 4px;
            right: 4px;
            background: none;
            border: none;
            color: rgba(255, 255, 255, 0.6);
            cursor: pointer;
            font-size: 16px;
            width: 20px;
            height: 20px;
            border-radius: 50%;
            display: flex;
            align-items: center;
            justify-content: center;
        }
        .emote-popover-close:hover {
            background-color: rgba(255, 255, 255, 0.1);
            color: rgba(255, 255, 255, 0.9);
        }
        /* Buffer element for maintaining scroll position */
        .scroll-buffer {
            height: 1px;
            width: 100%;
            flex-shrink: 0;
        }
        @media (prefers-color-scheme: dark) {
            body { color: #ffffff; }
        }
        @media (prefers-color-scheme: light) {
            body { color: #000000; background-color: rgba(255, 255, 255, 0.95); }
            .message-box { background-color: rgba(0, 0, 0, 0.02); }
        }
      </style>
    </head>
    <body>
    <div id="chat-container">
      <div id="chat-body">
        <div class="scroll-buffer"></div> <!-- Initial buffer element -->
      </div>
    </div>
    <script>
      let isUserScrolling = false;
      let scrollTimeout = null;
      const chatContainer = document.getElementById('chat-container');
      const chatBody = document.getElementById('chat-body');
      const MAX_MESSAGES = 200; // Increased buffer size
      const CLEANUP_THRESHOLD = 300; // Cleanup only when significantly over limit
      let messageCount = 0;
      let messageQueue = [];
      let lastScrollHeight = 0;
      let lastScrollTop = 0;

      chatContainer.addEventListener('scroll', function() {
        const isAtBottom = chatContainer.scrollHeight - chatContainer.scrollTop <= chatContainer.clientHeight + 50;
        isUserScrolling = !isAtBottom;

        // Store scroll position for anchoring
        lastScrollTop = chatContainer.scrollTop;
        lastScrollHeight = chatContainer.scrollHeight;

        clearTimeout(scrollTimeout);
        scrollTimeout = setTimeout(() => {
          isUserScrolling = false;
          flushMessageQueue();
        }, 2000);
      });

      function maintainScrollPosition() {
        const currentScrollHeight = chatContainer.scrollHeight;
        const heightDiff = currentScrollHeight - lastScrollHeight;

        if (heightDiff > 0 && !isUserScrolling) {
          // Auto-scroll to bottom
          chatContainer.scrollTop = chatContainer.scrollHeight;
        }
      }

      function cleanupOldMessages() {
        const messages = chatBody.getElementsByClassName('message-box');
        messageCount = messages.length;

        if (messageCount > CLEANUP_THRESHOLD) {
          const toRemove = messageCount - MAX_MESSAGES;
          const currentScrollTop = chatContainer.scrollTop;
          const currentScrollHeight = chatContainer.scrollHeight;

          // Batch remove with requestAnimationFrame for smooth performance
          requestAnimationFrame(() => {
            for (let i = 0; i < toRemove; i++) {
              if (messages.length > 0 && messages[0].className !== 'scroll-buffer') {
                chatBody.removeChild(messages[0]);
              }
            }

            // Maintain relative scroll position
            const newScrollHeight = chatContainer.scrollHeight;
            const scrollRatio = currentScrollTop / currentScrollHeight;
            chatContainer.scrollTop = newScrollHeight * scrollRatio;
          });
        }
      }

      function flushMessageQueue() {
        if (messageQueue.length > 0) {
          const batchSize = Math.min(messageQueue.length, 20); // Process in smaller batches
          const fragment = document.createDocumentFragment();

          for (let i = 0; i < batchSize; i++) {
            const tempDiv = document.createElement('div');
            tempDiv.innerHTML = messageQueue[i];
            while (tempDiv.firstChild) {
              fragment.appendChild(tempDiv.firstChild);
            }
          }

          chatBody.appendChild(fragment);
          messageQueue.splice(0, batchSize);

          maintainScrollPosition();

          // Schedule cleanup if needed
          if (chatBody.getElementsByClassName('message-box').length > CLEANUP_THRESHOLD) {
            setTimeout(cleanupOldMessages, 100);
          }

          // Process more if queue still has items
          if (messageQueue.length > 0) {
            requestAnimationFrame(flushMessageQueue);
          }
        }
      }

      function appendMessages(htmlString) {
        if (isUserScrolling) {
          messageQueue.push(htmlString);
          if (messageQueue.length === 1) {
            requestAnimationFrame(flushMessageQueue);
          }
          return;
        }

        const tempDiv = document.createElement('div');
        tempDiv.innerHTML = htmlString;
        const fragment = document.createDocumentFragment();
        while (tempDiv.firstChild) {
          fragment.appendChild(tempDiv.firstChild);
        }

        chatBody.appendChild(fragment);
        maintainScrollPosition();

        // Schedule cleanup asynchronously
        const messages = chatBody.getElementsByClassName('message-box');
        if (messages.length > CLEANUP_THRESHOLD) {
          requestAnimationFrame(cleanupOldMessages);
        }
      }

      window.onload = function() {
        chatContainer.scrollTop = chatContainer.scrollHeight;
        lastScrollHeight = chatContainer.scrollHeight;
        setupEmotePopovers();
      };

      // Emote popover functionality
      let currentPopover = null;

      function setupEmotePopovers() {
        console.log('Setting up emote popovers');

        // Single click listener for everything
        document.addEventListener('click', function(event) {
          const target = event.target;
          console.log('Clicked element:', target.tagName, target.alt, target.src);

          // If clicking on an emote, show popover
          if (target.tagName === 'IMG' &&
              ((target.alt && target.alt.startsWith(':') && target.alt.endsWith(':')) ||
               (target.src && (target.src.includes('7tv.app') || target.src.includes('emote'))))) {
            console.log('Emote clicked!');
            event.preventDefault();
            event.stopPropagation();
            showEmotePopover(target);
            return;
          }

          // If clicking on close button, hide popover
          if (currentPopover && target.classList.contains('emote-popover-close')) {
            console.log('Close button clicked');
            event.preventDefault();
            event.stopPropagation();
            hideEmotePopover();
            return;
          }

          // If clicking outside popover, hide it
          if (currentPopover && !currentPopover.contains(target)) {
            console.log('Clicked outside popover');
            hideEmotePopover();
            return;
          }
        });

        // Close popover on escape key
        document.addEventListener('keydown', function(event) {
          if (event.key === 'Escape' && currentPopover) {
            console.log('Escape key pressed');
            hideEmotePopover();
          }
        });
      }

      function showEmotePopover(emoteImg) {
        console.log('Showing popover for emote:', emoteImg.src);

        // Hide existing popover if any
        hideEmotePopover();

        const emoteName = emoteImg.alt && emoteImg.alt.startsWith(':') && emoteImg.alt.endsWith(':')
          ? emoteImg.alt.substring(1, emoteImg.alt.length - 1)
          : 'Emote';
        const emoteUrl = emoteImg.src;

        console.log('Emote name:', emoteName, 'URL:', emoteUrl);

        // Create popover element
        const popover = document.createElement('div');
        popover.className = 'emote-popover';
        popover.style.display = 'block';
        popover.innerHTML = `
          <button class="emote-popover-close" title="Close">&times;</button>
          <img src="${emoteUrl}" alt="${emoteName}" />
          <div class="emote-popover-name">${emoteName}</div>
          <div class="emote-popover-url">${emoteUrl}</div>
        `;

        // Add close button functionality is now handled by the main event listener

        // Position popover near the clicked emote
        const rect = emoteImg.getBoundingClientRect();
        const popoverWidth = 250;
        const popoverHeight = 150;

        let left = rect.left + (rect.width / 2) - (popoverWidth / 2);
        let top = rect.bottom + 10;

        // Ensure popover stays within viewport
        if (left < 10) left = 10;
        if (left + popoverWidth > window.innerWidth - 10) {
          left = window.innerWidth - popoverWidth - 10;
        }
        if (top + popoverHeight > window.innerHeight - 10) {
          top = rect.top - popoverHeight - 10;
        }

        popover.style.left = left + 'px';
        popover.style.top = top + 'px';

        document.body.appendChild(popover);
        currentPopover = popover;

        console.log('Popover added to DOM');
      }

      function hideEmotePopover() {
        if (currentPopover) {
          console.log('Hiding popover');
          document.body.removeChild(currentPopover);
          currentPopover = null;
        }
      }
    </script>
    </body>
    </html>
    "#
}

fn get_chat_html_template_with_color(background_color: Option<&str>) -> String {
    let base_template = get_chat_html_template();

    if let Some(color) = background_color {
        let css_replacement = format!("            background-color: {}; /* Solid background prevents overdraw */", color);
        let light_css_replacement = format!("            body {{ background-color: {}; }}", color);
        let light_msg_css_replacement = format!("            .message-box {{ background-color: {}; }}", color);

        base_template
            .replace("            background-color: rgba(0, 0, 0, 0.95); /* Solid background prevents overdraw */", &css_replacement)
            .replace("            body { color: #000000; background-color: rgba(255, 255, 255, 0.95); }", &light_css_replacement)
            .replace("            .message-box { background-color: rgba(0, 0, 0, 0.02); }", &light_msg_css_replacement)
    } else {
        base_template.to_string()
    }
}

// Client state that needs to be shared and controlled
struct ClientState {
    client: Option<TwitchIRCClient<SecureTCPTransport, StaticLoginCredentials>>,
    runtime: Option<Runtime>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl ClientState {
    fn new() -> Self {
        Self {
            client: None,
            runtime: Some(Runtime::new().unwrap()),
            join_handle: None,
        }
    }
    fn disconnect(&mut self) {
        self.client = None;
        if let Some(handle) = self.join_handle.take() {
            handle.join().unwrap_or(());
        }
        if self.runtime.is_none() {
            self.runtime = Some(Runtime::new().unwrap());
        }
    }
}

// Favorites data structure with starred channels
#[derive(Deserialize, Serialize, Default)]
struct Favorites {
    channels: Vec<String>,
    starred: Vec<String>, // List of starred channels
    background_color: Option<String>, // Custom background color hex code
}

struct TabData {
    page: TabPage,
    webview: WebView,
    stack: Stack,
    entry: Entry,
    channel_name: Arc<Mutex<Option<String>>>,
    client_state: Arc<Mutex<ClientState>>,
    connection_state: Arc<Mutex<ConnectionState>>,
    tx: std::sync::mpsc::SyncSender<twitch_irc::message::PrivmsgMessage>,
    rx: Arc<Mutex<std::sync::mpsc::Receiver<twitch_irc::message::PrivmsgMessage>>>,
    error_tx: std::sync::mpsc::Sender<()>,
    error_rx: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
    last_js_execution: Arc<Mutex<Instant>>,
}


// In your main function, replace the rlimit code with:
fn main() {
    let app = Application::builder()
        .application_id("com.toasterrepair.Admiral")
        .build();

    // Add this code right after setting up the app, before app.connect_activate
    // Check and potentially increase the file descriptor limit
    if let Ok((soft_limit, hard_limit)) = rlimit::getrlimit(rlimit::Resource::NOFILE) {
        println!("Current file descriptor limit: soft={}, hard={}", soft_limit, hard_limit);
        let new_soft_limit = hard_limit.min(4096); // Increase to 4096 for WebKit's needs
        if new_soft_limit > soft_limit {
            if let Err(e) = rlimit::setrlimit(rlimit::Resource::NOFILE, new_soft_limit, hard_limit) {
                eprintln!("Failed to increase file descriptor soft limit: {}", e);
            } else {
                println!("Successfully increased file descriptor soft limit to {}", new_soft_limit);
            }
        }
    } else {
        eprintln!("Failed to get current file descriptor limits using rlimit crate.");
    }

    // Set environment variables to limit WebKit resource usage
    std::env::set_var("WEBKIT_FORCE_MONOSPACE_FONT", "1");
    std::env::set_var("WEBKIT_USE_SINGLE_WEB_PROCESS", "1");
    std::env::set_var("WEBKIT_FORCE_SANDBOX", "0"); // Disable sandbox to reduce fd usage
    app.connect_activate(build_ui);
    app.run();
}

// Favorites management functions (remain largely the same)
fn get_favorites_path() -> std::path::PathBuf {
    let config_dir = shellexpand::tilde("~/.config/admiral").into_owned();
    std::path::PathBuf::from(config_dir).join("favorites.toml")
}

fn load_favorites() -> Favorites {
    let path = get_favorites_path();
    if !path.exists() {
        let favorites = Favorites::default();
        save_favorites(&favorites);
        return favorites;
    }
    let mut file = fs::File::open(path).expect("Failed to open favorites file");
    let mut contents = String::new();
    file.read_to_string(&mut contents).expect("Failed to read favorites file");
    toml::from_str(&contents).unwrap_or_else(|_| {
        eprintln!("Failed to parse favorites file, using empty list");
        Favorites::default()
    })
}

fn save_favorites(favorites: &Favorites) {
    let path = get_favorites_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("Failed to create config directory");
    }
    let toml = toml::to_string(favorites).expect("Failed to serialize favorites");
    fs::write(path, toml).expect("Failed to write favorites file");
}

fn add_favorite(channel: &str) {
    let mut favorites = load_favorites();
    let channel_lower = channel.to_lowercase();
    if !favorites.channels.contains(&channel_lower) {
        favorites.channels.push(channel_lower);
        favorites.channels.sort();
        save_favorites(&favorites);
    }
}

fn remove_favorite(channel: &str) {
    let mut favorites = load_favorites();
    let channel_lower = channel.to_lowercase();
    favorites.channels.retain(|c| c != &channel_lower);
    favorites.starred.retain(|c| c != &channel_lower);
    save_favorites(&favorites);
}

fn toggle_star(channel: &str) {
    let mut favorites = load_favorites();
    let channel_lower = channel.to_lowercase();
    if favorites.starred.contains(&channel_lower) {
        favorites.starred.retain(|c| c != &channel_lower);
    } else {
        if favorites.channels.contains(&channel_lower) {
            favorites.starred.push(channel_lower);
            favorites.starred.sort();
        }
    }
    save_favorites(&favorites);
}

fn is_starred(channel: &str) -> bool {
    let favorites = load_favorites();
    favorites.starred.contains(&channel.to_lowercase())
}

fn get_background_color() -> Option<String> {
    let favorites = load_favorites();
    favorites.background_color
}

fn set_background_color(color: Option<&str>) {
    let mut favorites = load_favorites();
    favorites.background_color = color.map(|c| c.to_string());
    save_favorites(&favorites);
}

fn validate_hex_color(color: &str) -> bool {
    if color.len() != 7 || !color.starts_with('#') {
        return false;
    }
    color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn apply_background_color_to_tabs(
    tab_view: &TabView,
    tabs: &Arc<Mutex<HashMap<String, Arc<TabData>>>>,
    color: Option<&str>,
) {
    let tabs_map = tabs.lock().unwrap();
    for (_, tab_data) in tabs_map.iter() {
        // Update WebKit background color
        if let Some(color_hex) = color {
            // Parse hex color to RGBA
            if let Ok(rgb) = u32::from_str_radix(&color_hex[1..], 16) {
                let r = ((rgb >> 16) & 0xFF) as f32 / 255.0;
                let g = ((rgb >> 8) & 0xFF) as f32 / 255.0;
                let b = (rgb & 0xFF) as f32 / 255.0;
                let bg_color = gdk::RGBA::new(r, g, b, 0.95);
                tab_data.webview.set_background_color(&bg_color);

                // Update the CSS in the WebView
                let js_code = format!(
                    r#"
                    if (typeof updateBackgroundColor === 'function') {{
                        updateBackgroundColor('{}');
                    }} else {{
                        // Create the function if it doesn't exist
                        const style = document.createElement('style');
                        style.textContent = `
                            body {{ background-color: {} !important; }}
                            @media (prefers-color-scheme: light) {{
                                body {{ background-color: {} !important; }}
                            }}
                        `;
                        document.head.appendChild(style);

                        window.updateBackgroundColor = function(color) {{
                            document.body.style.backgroundColor = color + 'e6'; // Add 90% opacity
                        }};
                    }}
                    "#,
                    color_hex, color_hex, color_hex
                );

                tab_data.webview.evaluate_javascript(
                    &js_code,
                    None,
                    None,
                    None::<&adw::gio::Cancellable>,
                    move |result| {
                        if let Err(e) = result {
                            eprintln!("Error updating background color: {}", e);
                        }
                    },
                );
            }
        } else {
            // Reset to default background
            let bg_color = gdk::RGBA::new(0.0, 0.0, 0.0, 0.95);
            tab_data.webview.set_background_color(&bg_color);

            let js_code = r#"
            if (typeof updateBackgroundColor === 'function') {
                updateBackgroundColor('rgba(0, 0, 0, 0.95)');
            }
            "#;

            tab_data.webview.evaluate_javascript(
                js_code,
                None,
                None,
                None::<&adw::gio::Cancellable>,
                move |result| {
                    if let Err(e) = result {
                        eprintln!("Error resetting background color: {}", e);
                    }
                },
            );
        }
    }
}

fn load_and_display_favorites(
    list: &gtk::ListBox, // Use fully qualified name to avoid ambiguity
    favorites_entry: &Entry,
    favorites_list: &gtk::ListBox, // Use fully qualified name
    tab_view: &TabView,
    tabs: &Arc<Mutex<HashMap<String, Arc<TabData>>>>,
    web_context: &webkit6::WebContext,
) {
    list.remove_all();
    let favorites = load_favorites();
    let mut starred_channels = Vec::new();
    let mut regular_channels = Vec::new();
    for channel in &favorites.channels {
        if favorites.starred.contains(channel) {
            starred_channels.push(channel.clone());
        } else {
            regular_channels.push(channel.clone());
        }
    }
    if !starred_channels.is_empty() {
        for channel in &starred_channels {
            create_favorite_row(
                list,
                channel,
                true, // is_starred
                &tab_view,
                &tabs,
                &favorites_entry,
                &favorites_list,
                web_context,
            );
        }
    }
    if !regular_channels.is_empty() {
        for channel in &regular_channels {
            create_favorite_row(
                list,
                channel,
                false, // is_starred
                &tab_view,
                &tabs,
                &favorites_entry,
                &favorites_list,
                web_context,
            );
        }
    }
    if favorites.channels.is_empty() {
        // Create a status page style empty state
        let empty_row = ListBoxRow::new();
        empty_row.set_selectable(false);
        empty_row.set_activatable(false);
        let empty_box = Box::new(Orientation::Vertical, 12);
        empty_box.set_margin_top(24);
        empty_box.set_margin_bottom(24);
        empty_box.set_halign(Align::Center);
        let empty_label = gtk::Label::new(Some("No favorites yet"));
        empty_label.add_css_class("title-4");
        let subtitle_label = gtk::Label::new(Some("Add channels to get started"));
        subtitle_label.add_css_class("dim-label");
        empty_box.append(&empty_label);
        empty_box.append(&subtitle_label);
        empty_row.set_child(Some(&empty_box));
        list.append(&empty_row);
    }
}

fn create_favorite_row(
    list: &gtk::ListBox, // Use fully qualified name
    channel: &str,
    is_starred: bool,
    tab_view: &TabView,
    tabs: &Arc<Mutex<HashMap<String, Arc<TabData>>>>,
    favorites_entry: &Entry,
    favorites_list: &gtk::ListBox, // Use fully qualified name
    web_context: &webkit6::WebContext,
) {
    // Create ActionRow for a modern Libadwaita look
    let action_row = adw::ActionRow::builder()
        .title(channel)
        .activatable(true)
        .build();

    // Create suffix button box
    let suffix_box = Box::new(Orientation::Horizontal, 6);

    // Star button
    let star_icon = if is_starred { "starred-symbolic" } else { "non-starred-symbolic" };
    let star_tooltip = if is_starred { "Unstar channel" } else { "Star channel" };
    let star_button = Button::builder()
        .icon_name(star_icon)
        .tooltip_text(star_tooltip)
        .valign(gtk::Align::Center)
        .build();
    star_button.add_css_class("flat");

    // Trash button
    let trash_button = Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove from favorites")
        .valign(gtk::Align::Center)
        .build();
    trash_button.add_css_class("flat");

    suffix_box.append(&star_button);
    suffix_box.append(&trash_button);
    action_row.add_suffix(&suffix_box);

    // Handle row activation (clicking the row itself)
    let channel_clone = channel.to_string();
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();
    let web_context_clone = web_context.clone();
    action_row.connect_activated(move |_| {
        println!("Row clicked for channel: {}", channel_clone);
        create_new_tab(&channel_clone, &tab_view_clone, &tabs_clone, &web_context_clone);
        let tab_view_clone2 = tab_view_clone.clone();
        let tabs_clone2 = tabs_clone.clone();
        let channel_clone2 = channel_clone.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
            println!("Attempting to connect to channel: {}", channel_clone2);
            if let Some(selected_page) = tab_view_clone2.selected_page() {
                let tabs_guard = tabs_clone2.lock().unwrap();
                for (_, tab_data) in tabs_guard.iter() {
                    if tab_data.page == selected_page {
                        println!("Found matching tab, setting entry and connecting");
                        tab_data.entry.set_text(&channel_clone2);
                        let tab_data_clone = Arc::clone(tab_data);
                        let channel_for_connection = channel_clone2.clone();
                        start_connection_for_tab(&channel_for_connection, &tab_data_clone);
                        break;
                    }
                }
            } else {
                println!("No selected page found!");
            }
        });
    });

    // Handle star button click
    let channel_clone = channel.to_string();
    let favorites_list_clone = favorites_list.clone();
    let favorites_entry_clone = favorites_entry.clone();
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();
    let web_context_clone = web_context.clone();
    star_button.connect_clicked(move |_| {
        toggle_star(&channel_clone);
        load_and_display_favorites(
            &favorites_list_clone,
            &favorites_entry_clone,
            &favorites_list_clone,
            &tab_view_clone,
            &tabs_clone,
            &web_context_clone,
        );
    });

    // Handle trash button click
    let channel_clone = channel.to_string();
    let favorites_list_clone = favorites_list.clone();
    let favorites_entry_clone = favorites_entry.clone();
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();
    let web_context_clone = web_context.clone();
    trash_button.connect_clicked(move |_| {
        remove_favorite(&channel_clone);
        load_and_display_favorites(
            &favorites_list_clone,
            &favorites_entry_clone,
            &favorites_list_clone,
            &tab_view_clone,
            &tabs_clone,
            &web_context_clone,
        );
    });

    list.append(&action_row);
}

fn disconnect_tab_handler(tab_data: &Arc<TabData>) {
    println!("Disconnecting tab...");
    *tab_data.connection_state.lock().unwrap() = ConnectionState::Disconnected;
    tab_data.client_state.lock().unwrap().disconnect();

    // Load a data URI to clear content without fetching anything
    tab_data.webview.load_uri("about:blank");

    // Alternatively, reload empty HTML to force a fresh context
    tab_data.webview.load_html("<!DOCTYPE html><html><head></head><body></body></html>", None);

    tab_data.stack.set_visible_child_name("placeholder");
    tab_data.page.set_title("New Tab");
    *tab_data.channel_name.lock().unwrap() = None;

    // Drain message queue
    let rx = tab_data.rx.lock().unwrap();
    while rx.try_recv().is_ok() {
        // Discard messages
    }
    drop(rx);
}

fn build_ui(app: &Application) {
    // Create a shared WebContext to limit process creation and resource usage
    let web_context = webkit6::WebContext::new();
    web_context.set_automation_allowed(false);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Admiral")
        .default_width(700)
        .default_height(600)
        .build();

    // Allow window to resize to very small widths
    window.set_default_size(200, 450);
    window.set_size_request(120, 400); // Minimum size
    let css_provider = gtk::CssProvider::new();
    css_provider.load_from_string(MESSAGE_CSS); // Use load_from_string instead
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let tab_view = TabView::builder()
        .vexpand(true)
        .build();
    let tab_bar = TabBar::builder()
        .view(&tab_view)
        .autohide(true)
        .build();
    tab_bar.add_css_class("inline");

    let header = HeaderBar::builder()
        .build();

    let favorites_button = GtkButton::builder()
        .icon_name("non-starred-symbolic")
        .tooltip_text("Favorites")
        .build();

    let popover = Popover::builder()
        .autohide(true)
        .build();

    let popover_content = Box::new(Orientation::Vertical, 6);
    popover_content.set_margin_top(6);
    popover_content.set_margin_bottom(6);
    popover_content.set_margin_start(6);
    popover_content.set_margin_end(6);
    popover_content.set_width_request(300);

    // Background color setting
    let color_row = adw::ActionRow::builder()
        .title("Background Color")
        .subtitle("Enter hex code (e.g., #1a1a1a)")
        .build();

    let color_entry = Entry::builder()
        .placeholder_text("#000000")
        .max_length(7)
        .width_chars(8)
        .build();

    // Load current background color
    if let Some(color) = get_background_color() {
        color_entry.set_text(&color);
    }

    color_row.add_suffix(&color_entry);
    popover_content.append(&color_row);

    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(6);
    separator.set_margin_bottom(6);
    popover_content.append(&separator);

    let favorites_entry = Entry::builder()
        .placeholder_text("Add channel to favorites")
        .build();

    // Use icon-only button with primary style (GNOME-like)
    let add_favorite_button = GtkButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add to favorites")
        .build();
    add_favorite_button.add_css_class("circular");
    add_favorite_button.add_css_class("suggested-action");

    let favorites_entry_box = Box::new(Orientation::Horizontal, 6);
    favorites_entry_box.append(&favorites_entry);
    favorites_entry_box.append(&add_favorite_button);
    popover_content.append(&favorites_entry_box);

    let favorites_list = gtk::ListBox::builder() // Use fully qualified name
        .vexpand(true)
        .selection_mode(gtk::SelectionMode::None)
        .build();
    favorites_list.add_css_class("boxed-list");
    let favorites_scrolled = ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(200)
        .child(&favorites_list)
        .propagate_natural_height(true)
        .build();
    favorites_scrolled.set_margin_top(6);
    popover_content.append(&favorites_scrolled);

    popover.set_child(Some(&popover_content));

    let favorites_button_clone = favorites_button.clone();
    let popover_clone = popover.clone();
    favorites_button.connect_clicked(move |_| {
        popover_clone.set_parent(&favorites_button_clone);
        popover_clone.popup();
    });

    header.pack_start(&favorites_button);

    let add_tab_button = GtkButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add new tab")
        .build();

    let overview_button = GtkButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("Tab overview")
        .build();

    header.pack_end(&add_tab_button);
    header.pack_end(&overview_button);

    let tab_overview = TabOverview::builder()
        .view(&tab_view)
        .child(&tab_view)
        .enable_new_tab(false)
        .show_end_title_buttons(false)
        .build();

    let content = Box::new(Orientation::Vertical, 0);
    content.append(&header);
    content.append(&tab_bar);
    content.append(&tab_overview);

    let tabs: Arc<Mutex<HashMap<String, Arc<TabData>>>> = Arc::new(Mutex::new(HashMap::new()));

    add_tab_button.connect_clicked(clone!(
        #[strong]
        tab_view,
        #[strong]
        tabs,
        #[strong]
        web_context,
        move |_| {
            create_new_tab("New Tab", &tab_view, &tabs, &web_context);
        }
    ));

    let tabs_clone = tabs.clone();
    let favorites_list_clone = favorites_list.clone();
    let favorites_entry_clone = favorites_entry.clone();
    let tab_view_for_color = tab_view.clone();
    let tabs_for_color = tabs.clone();

    // Color entry change handler
    color_entry.connect_changed(clone!(
        #[strong]
        tab_view_for_color,
        #[strong]
        tabs_for_color,
        move |entry| {
            let color_text = entry.text().to_string();
            if color_text.is_empty() || color_text == "#" {
                set_background_color(None);
                apply_background_color_to_tabs(&tab_view_for_color, &tabs_for_color, None);
            } else if validate_hex_color(&color_text) {
                set_background_color(Some(&color_text));
                apply_background_color_to_tabs(&tab_view_for_color, &tabs_for_color, Some(&color_text));
            }
        }
    ));

    add_favorite_button.connect_clicked(clone!(
        #[strong]
        favorites_list_clone,
        #[strong]
        favorites_entry_clone,
        #[strong]
        tab_view,
        #[strong]
        tabs_clone,
        #[strong]
        web_context,
        move |_| {
            let channel = favorites_entry_clone.text().to_string();
            if !channel.is_empty() {
                add_favorite(&channel);
                favorites_entry_clone.set_text("");
                load_and_display_favorites(&favorites_list_clone, &favorites_entry_clone, &favorites_list_clone, &tab_view, &tabs_clone, &web_context);
            }
        }
    ));

    favorites_entry_clone.connect_activate(clone!(
        #[strong]
        add_favorite_button,
        move |_| {
            add_favorite_button.emit_clicked();
        }
    ));

    load_and_display_favorites(&favorites_list, &favorites_entry, &favorites_list, &tab_view, &tabs, &web_context);

    overview_button.connect_clicked(clone!(
        #[strong]
        tab_overview,
        move |_| {
            tab_overview.set_open(true);
        }
    ));

    create_new_tab("New Tab", &tab_view, &tabs, &web_context);

    // Apply any saved background color to existing tabs
    if let Some(color) = get_background_color() {
        apply_background_color_to_tabs(&tab_view, &tabs, Some(&color));
    }

    // Clear message queues when switching away from a tab
    let tabs_for_selection = tabs.clone();
    let tab_view_for_selection = tab_view.clone();
    tab_view.connect_selected_page_notify(move |_| {
        // When switching tabs, immediately drain all inactive tab queues
        if let Some(selected_page) = tab_view_for_selection.selected_page() {
            let tabs_map = tabs_for_selection.lock().unwrap();
            for (_, tab_data) in tabs_map.iter() {
                if tab_data.page != selected_page {
                    // Drain the entire queue for the inactive tab
                    let rx = tab_data.rx.lock().unwrap();
                    let mut drained = 0;
                    while let Ok(_) = rx.try_recv() {
                        drained += 1;
                        if drained > 200 {
                            break; // Safety limit
                        }
                    }
                    if drained > 0 {
                        println!("Switched tabs: drained {} queued messages from inactive channel", drained);
                    }
                    drop(rx);
                }
            }
        }
    });

    let tabs_for_close = tabs.clone();
    tab_view.connect_close_page(move |_tab_view, page| {
        println!("Tab close requested");
        let tabs_map = tabs_for_close.lock().unwrap();
        let mut tab_id_to_remove = None;
        for (tab_id, tab_data) in tabs_map.iter() {
            if &tab_data.page == page {
                println!("Found tab to disconnect: {}", tab_id);
                disconnect_tab_handler(tab_data);
                tab_id_to_remove = Some(tab_id.clone());
                break;
            }
        }
        drop(tabs_map);
        if let Some(tab_id) = tab_id_to_remove {
            tabs_for_close.lock().unwrap().remove(&tab_id);
            println!("Removed tab from HashMap: {}", tab_id);
        }
        glib::Propagation::Proceed
    });

    let tabs_clone = tabs.clone();
    let tab_view_for_processing = tab_view.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        let tabs_map = tabs_clone.lock().unwrap();

        const MAX_BATCH_SIZE: usize = 30; // Conservative batch size for better responsiveness
        const MAX_DRAIN_PER_TAB: usize = 50; // Limit draining to prevent blocking

        if let Some(selected_page) = tab_view_for_processing.selected_page() {
            // Process messages for ALL tabs, but only display for the active one
            for (_, tab_data) in tabs_map.iter() {
                let is_active_tab = tab_data.page == selected_page;

                if is_active_tab {
                    // Throttle JS execution to prevent overwhelming WebView
                    let last_execution = *tab_data.last_js_execution.lock().unwrap();
                    if last_execution.elapsed() < std::time::Duration::from_millis(30) {
                        continue;
                    }

                    let mut messages_to_process = Vec::new();
                    let rx = tab_data.rx.lock().unwrap();

                    // Collect messages up to batch size
                    while messages_to_process.len() < MAX_BATCH_SIZE {
                        match rx.try_recv() {
                            Ok(msg) => messages_to_process.push(msg),
                            Err(_) => break,
                        }
                    }
                    drop(rx);

                    if !messages_to_process.is_empty() {
                        let webview = tab_data.webview.clone();
                        let channel_id_for_closure = messages_to_process
                            .first()
                            .map(|msg| msg.channel_id.clone());
                        let last_js_execution = tab_data.last_js_execution.clone();

                        if let Some(channel_id_str) = channel_id_for_closure {
                            let emote_map = get_emote_map(&channel_id_str);
                            let mut html_content = String::new();
                            for msg in &messages_to_process {
                                html_content.push_str(&parse_message_html(msg, &emote_map));
                                html_content.push('\n');
                            }

                            let escaped_html = html_content
                                .replace('\\', "\\\\")
                                .replace('\'', "\\'")
                                .replace('\n', "\\n")
                                .replace('\r', "\\r");

                            let js_code = format!(
                                r#"if (typeof appendMessages === 'function') {{ appendMessages('{}'); }}"#,
                                escaped_html
                            );

                            webview.evaluate_javascript(
                                &js_code,
                                None,
                                None,
                                None::<&adw::gio::Cancellable>,
                                move |result| {
                                    match result {
                                        Ok(_) => {
                                            *last_js_execution.lock().unwrap() = Instant::now();
                                        }
                                        Err(e) => {
                                            eprintln!("Error running JS: {}", e);
                                        }
                                    }
                                },
                            );
                        }
                    }
                } else {
                    // For inactive tabs, aggressively drain the queue to prevent buildup
                    let rx = tab_data.rx.lock().unwrap();
                    let mut drained = 0;
                    while drained < MAX_DRAIN_PER_TAB {
                        match rx.try_recv() {
                            Ok(_) => drained += 1,
                            Err(_) => break,
                        }
                    }
                    drop(rx);
                }
            }
        } else {
            // No tab selected - drain all tabs
            for (_, tab_data) in tabs_map.iter() {
                let rx = tab_data.rx.lock().unwrap();
                let mut drained = 0;
                while drained < MAX_DRAIN_PER_TAB {
                    match rx.try_recv() {
                        Ok(_) => drained += 1,
                        Err(_) => break,
                    }
                }
                drop(rx);
            }
        }

        glib::ControlFlow::Continue
    });

    glib::timeout_add_local(std::time::Duration::from_secs(30), move || {
        cleanup_emote_cache();
        cleanup_media_file_cache();
        println!("Cleaning emote cache...");
        glib::ControlFlow::Continue
    });

    let new_tab_action = SimpleAction::new("new-tab", None);
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();
    let web_context_clone = web_context.clone();
    new_tab_action.connect_activate(move |_, _| {
        create_new_tab("New Tab", &tab_view_clone, &tabs_clone, &web_context_clone);
    });
    window.add_action(&new_tab_action);

    let close_tab_action = SimpleAction::new("close-tab", None);
    let tab_view_close = tab_view.clone();
    close_tab_action.connect_activate(move |_, _| {
        if let Some(selected_page) = tab_view_close.selected_page() {
            tab_view_close.close_page(&selected_page);
        }
    });
    window.add_action(&close_tab_action);

    app.set_accels_for_action("win.new-tab", &["<Control>t"]);
    app.set_accels_for_action("win.close-tab", &["<Control>w"]);

    window.set_content(Some(&content));

    let quit_action = SimpleAction::new("quit", None);
    let tabs_quit = tabs.clone();
    let window_quit = window.clone();
    quit_action.connect_activate(move |_, _| {
        println!("Quit action triggered");
        let tabs_map = tabs_quit.lock().unwrap();
        for (tab_id, tab_data) in tabs_map.iter() {
            println!("Disconnecting tab: {}", tab_id);
            disconnect_tab_handler(tab_data);
        }
        drop(tabs_map);
        tabs_quit.lock().unwrap().clear();
        println!("All tabs disconnected and cleared");

        // Close the window after all tabs are disconnected
        window_quit.close();
    });
    window.add_action(&quit_action);
    app.set_accels_for_action("win.quit", &["<Control>q"]);

    let tabs_for_window_close = tabs.clone();
    window.connect_close_request(move |_window| {
        println!("Window close button clicked");
        let tabs_map = tabs_for_window_close.lock().unwrap();
        for (tab_id, tab_data) in tabs_map.iter() {
            println!("Disconnecting tab on window close: {}", tab_id);
            disconnect_tab_handler(tab_data);
        }
        drop(tabs_map);
        tabs_for_window_close.lock().unwrap().clear();
        println!("All tabs disconnected on window close");
        glib::Propagation::Proceed
    });

    window.present();
}

fn create_new_tab(
    label: &str,
    tab_view: &TabView,
    tabs: &Arc<Mutex<HashMap<String, Arc<TabData>>>>,
    web_context: &webkit6::WebContext
) {
    let tab_content = Box::new(Orientation::Vertical, 0);

    let entry_box = Box::new(Orientation::Horizontal, 6);
    entry_box.set_margin_top(6);
    entry_box.set_margin_bottom(6);
    entry_box.set_margin_start(6);
    entry_box.set_margin_end(6);
    let entry = Entry::builder()
        .placeholder_text("Enter channel name")
        .hexpand(true)
        .build();
    let connect_button = GtkButton::builder()
        .label("Connect")
        .build();
    entry_box.append(&entry);
    entry_box.append(&connect_button);

    // Create WebView for chat display
    let webview = WebView::new();
    // Note: WebKitGTK doesn't provide a direct way to set context on individual views
    // The shared context will be used automatically when creating WebViews in the same process
    webview.set_vexpand(true);
    webview.set_hexpand(true);

    // Set webview background to saved color or default
    let bg_color = if let Some(color_hex) = get_background_color() {
        // Parse hex color to RGBA
        if let Ok(rgb) = u32::from_str_radix(&color_hex[1..], 16) {
            let r = ((rgb >> 16) & 0xFF) as f32 / 255.0;
            let g = ((rgb >> 8) & 0xFF) as f32 / 255.0;
            let b = (rgb & 0xFF) as f32 / 255.0;
            gdk::RGBA::new(r, g, b, 0.95)
        } else {
            gdk::RGBA::new(0.0, 0.0, 0.0, 0.95) // Fallback to black
        }
    } else {
        gdk::RGBA::new(0.0, 0.0, 0.0, 0.95) // Default black
    };
    webview.set_background_color(&bg_color);

    // Configure WebView for aggressive resource management
    let settings = webkit6::Settings::new();
    settings.set_enable_write_console_messages_to_stdout(true);
    settings.set_javascript_can_open_windows_automatically(false);
    settings.set_enable_page_cache(false);
    settings.set_enable_webgl(false);
    settings.set_enable_smooth_scrolling(false);
    settings.set_enable_media_stream(false);
    settings.set_enable_dns_prefetching(false);
    settings.set_hardware_acceleration_policy(webkit6::HardwareAccelerationPolicy::Never);
    settings.set_enable_media(false); // Disable media to prevent resource issues
    settings.set_enable_developer_extras(false);
    settings.set_enable_javascript(true); // Keep JS for chat functionality
    settings.set_enable_caret_browsing(false);
    settings.set_enable_html5_database(false);
    settings.set_enable_html5_local_storage(false);
    settings.set_enable_webaudio(false);

    webview.set_settings(&settings);

    // Set up a context menu handler to prevent right-click resource usage
    webview.connect_context_menu(move |_webview, context_menu, _event| {
        // Prevent context menu to avoid additional resource usage
        context_menu.remove_all();
        true // Consume the event
    });

    // Inject initial HTML and JavaScript with custom background color
    let html_template = get_chat_html_template_with_color(get_background_color().as_deref());
    webview.load_html(&html_template, None);

    let scrolled_window = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&webview) // Use WebView instead of ScrolledWindow(ListBox)
        .build();

    let placeholder_box = Box::new(Orientation::Vertical, 12);
    placeholder_box.set_valign(Align::Center);
    placeholder_box.set_halign(Align::Center);
    placeholder_box.set_margin_top(60);
    placeholder_box.set_margin_bottom(60);
    placeholder_box.set_margin_start(40);
    placeholder_box.set_margin_end(40);
    let main_label = gtk::Label::new(Some("Choose a channel"));
    main_label.set_css_classes(&["title-1"]);
    main_label.set_halign(Align::Center);
    let subtitle_label = gtk::Label::new(Some("Type a channel name in the entry above"));
    subtitle_label.set_css_classes(&["dim-label"]);
    subtitle_label.set_halign(Align::Center);
    placeholder_box.append(&main_label);
    placeholder_box.append(&subtitle_label);

    let stack = Stack::builder()
        .vexpand(true)
        .hexpand(true)
        .build();
    stack.add_named(&placeholder_box, Some("placeholder"));
    stack.add_named(&scrolled_window, Some("chat")); // Show WebView in chat view
    stack.set_visible_child_name("placeholder");

    tab_content.append(&entry_box);
    tab_content.append(&stack);

    let page = tab_view.append(&tab_content);
    page.set_title(label);

    let (tx, rx) = mpsc::sync_channel(100); // Reduced capacity - we drain background tabs aggressively
    let (error_tx, error_rx) = mpsc::channel();

    let tab_count = tabs.lock().unwrap().len();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let tab_id = format!("tab_{}_{}", timestamp, tab_count);
    let tab_data = TabData {
        page: page.clone(),
        webview: webview.clone(),
        stack: stack.clone(),
        entry: entry.clone(),
        channel_name: Arc::new(Mutex::new(None)),
        client_state: Arc::new(Mutex::new(ClientState::new())),
        connection_state: Arc::new(Mutex::new(ConnectionState::Disconnected)),
        tx,
        rx: Arc::new(Mutex::new(rx)),
        error_tx,
        error_rx: Arc::new(Mutex::new(error_rx)),
        last_js_execution: Arc::new(Mutex::new(Instant::now())),
    };
    let tab_data_arc = Arc::new(tab_data);
    tabs.lock().unwrap().insert(tab_id.clone(), tab_data_arc.clone());
    println!("Created new tab with id: {}", tab_id);

    connect_button.connect_clicked(clone!(
        #[strong]
        tab_data_arc,
        move |_| {
            let channel_name = tab_data_arc.entry.text().to_string();
            if channel_name.is_empty() {
                disconnect_tab_handler(&tab_data_arc);
                return;
            }
            let current_state = tab_data_arc.connection_state.lock().unwrap().clone();
            match current_state {
                ConnectionState::Connected(_) => {
                    disconnect_tab_handler(&tab_data_arc);
                    start_connection_for_tab(&channel_name, &tab_data_arc);
                },
                ConnectionState::Disconnected | ConnectionState::Connecting => {
                    start_connection_for_tab(&channel_name, &tab_data_arc);
                }
            }
        }
    ));

    entry.connect_activate(clone!(
        #[strong]
        connect_button,
        move |_| {
            connect_button.emit_clicked();
        }
    ));

    tab_view.set_selected_page(&page);
}

fn start_connection_for_tab(
    channel: &str,
    tab_data: &Arc<TabData>
) {
    *tab_data.connection_state.lock().unwrap() = ConnectionState::Connecting;
    *tab_data.channel_name.lock().unwrap() = Some(channel.to_string());
    // Clear WebView content and show chat view with custom background color
    let html_template = get_chat_html_template_with_color(get_background_color().as_deref());
    tab_data.webview.load_html(&html_template, None);
    tab_data.stack.set_visible_child_name("chat");
    tab_data.page.set_title(channel);

    let channel = channel.to_string();
    let connection_state = tab_data.connection_state.clone();
    let client_state_thread = tab_data.client_state.clone();
    let client_state_store = tab_data.client_state.clone();
    let tx = tab_data.tx.clone();
    let error_tx = tab_data.error_tx.clone();

    let mut state = tab_data.client_state.lock().unwrap();
    let runtime = state.runtime.take().unwrap();
    drop(state);

    let handle = thread::spawn(move || {
        runtime.block_on(async move {
            let config = ClientConfig::default();
            let (mut incoming_messages, client) = TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

            if let Err(e) = client.join(channel.clone()) {
                eprintln!("Failed to join channel '{}': {}", channel, e);
                let _ = error_tx.send(());
                return;
            }

            {
                let mut state = client_state_thread.lock().unwrap();
                state.client = Some(client);
            }

            {
                let mut state = connection_state.lock().unwrap();
                *state = ConnectionState::Connected(channel.clone());
            }

            // Around line 1010-1020 in the async block
            while let Some(message) = incoming_messages.recv().await {
                if let twitch_irc::message::ServerMessage::Privmsg(msg) = message {
                    // SyncSender will block if channel is full, preventing unbounded growth
                    match tx.send(msg.clone()) {
                        Ok(_) => {},
                        Err(e) => {
                            eprintln!("Failed to send message to UI thread: {}", e);
                            break;
                        }
                    }
                }
            }

            {
                let mut state = connection_state.lock().unwrap();
                if matches!(*state, ConnectionState::Connected(ref c) if c == &channel) {
                    *state = ConnectionState::Disconnected;
                }
            }
        });
    });

    {
        let mut state = client_state_store.lock().unwrap();
        state.join_handle = Some(handle);
    }
}
