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
}

struct TabData {
    page: TabPage,
    webview: WebView,
    stack: Stack,
    entry: Entry,
    channel_name: Arc<Mutex<Option<String>>>,
    client_state: Arc<Mutex<ClientState>>,
    connection_state: Arc<Mutex<ConnectionState>>,
    tx: std::sync::mpsc::SyncSender<twitch_irc::message::PrivmsgMessage>, // Changed to SyncSender
    rx: Arc<Mutex<std::sync::mpsc::Receiver<twitch_irc::message::PrivmsgMessage>>>,
    error_tx: std::sync::mpsc::Sender<()>,
    error_rx: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
}

// In your main function, replace the rlimit code with:
fn main() {
    let app = Application::builder()
        .application_id("com.example.Admiral")
        .build();

    // Add this code right after setting up the app, before app.connect_activate
    // Check and potentially increase the file descriptor limit
    if let Ok((soft_limit, hard_limit)) = rlimit::getrlimit(rlimit::Resource::NOFILE) {
        println!("Current file descriptor limit: soft={}, hard={}", soft_limit, hard_limit);
        let new_soft_limit = hard_limit.min(1024);
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

fn load_and_display_favorites(
    list: &gtk::ListBox, // Use fully qualified name to avoid ambiguity
    favorites_entry: &Entry,
    favorites_list: &gtk::ListBox, // Use fully qualified name
    tab_view: &TabView,
    tabs: &Arc<Mutex<HashMap<String, Arc<TabData>>>>
) {
    // Implementation remains the same as before
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
        let header_row = ListBoxRow::new();
        let header_label = gtk::Label::new(Some("Starred Channels"));
        header_label.add_css_class("heading");
        header_row.set_child(Some(&header_label));
        header_row.set_selectable(false);
        list.append(&header_row);
        for channel in &starred_channels {
            create_favorite_row(
                list,
                channel,
                true, // is_starred
                &tab_view,
                &tabs,
                &favorites_entry,
                &favorites_list,
            );
        }
        let separator = gtk::Separator::new(Orientation::Horizontal);
        separator.set_margin_top(8);
        separator.set_margin_bottom(8);
        separator.set_sensitive(false);
        separator.set_opacity(0.4);
        list.append(&separator);
    }
    if !regular_channels.is_empty() {
        if !starred_channels.is_empty() {
            let header_row = ListBoxRow::new();
            let header_label = gtk::Label::new(Some("Favorites"));
            header_label.add_css_class("heading");
            header_row.set_child(Some(&header_label));
            header_row.set_selectable(false);
            list.append(&header_row);
        }
        for channel in &regular_channels {
            create_favorite_row(
                list,
                channel,
                false, // is_starred
                &tab_view,
                &tabs,
                &favorites_entry,
                &favorites_list,
            );
        }
    }
    if favorites.channels.is_empty() {
        let empty_row = ListBoxRow::new();
        let empty_label = gtk::Label::new(Some("No favorites yet"));
        empty_label.add_css_class("dim-label");
        empty_row.set_child(Some(&empty_label));
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
) {
    // Implementation remains the same as before
    let row = ListBoxRow::new();
    let content_box = Box::new(Orientation::Horizontal, 6);
    content_box.set_hexpand(true);
    let channel_label = gtk::Label::builder()
        .label(channel)
        .halign(Align::Start)
        .valign(gtk::Align::Center)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let star_icon = if is_starred { "starred-symbolic" } else { "non-starred-symbolic" };
    let star_tooltip = if is_starred { "Unstar channel" } else { "Star channel" };
    let star_button = Button::builder()
        .icon_name(star_icon)
        .tooltip_text(star_tooltip)
        .build();
    let trash_button = Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove from favorites")
        .build();
    content_box.append(&channel_label);
    let spacer = Box::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    content_box.append(&spacer);
    content_box.append(&star_button);
    content_box.append(&trash_button);
    row.set_child(Some(&content_box));
    row.set_selectable(true);
    row.set_activatable(true);
    let channel_clone = channel.to_string();
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();
    let gesture = gtk::GestureClick::new();
    gesture.connect_released(move |_, _, _, _| {
        println!("Row clicked for channel: {}", channel_clone);
        create_new_tab(&channel_clone, &tab_view_clone, &tabs_clone);
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
    row.add_controller(gesture);
    let channel_clone = channel.to_string();
    let favorites_list_clone = favorites_list.clone();
    let favorites_entry_clone = favorites_entry.clone();
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();
    star_button.connect_clicked(move |_| {
        toggle_star(&channel_clone);
        load_and_display_favorites(
            &favorites_list_clone,
            &favorites_entry_clone,
            &favorites_list_clone,
            &tab_view_clone,
            &tabs_clone,
        );
    });
    let channel_clone = channel.to_string();
    let favorites_list_clone = favorites_list.clone();
    let favorites_entry_clone = favorites_entry.clone();
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();
    trash_button.connect_clicked(move |_| {
        remove_favorite(&channel_clone);
        load_and_display_favorites(
            &favorites_list_clone,
            &favorites_entry_clone,
            &favorites_list_clone,
            &tab_view_clone,
            &tabs_clone,
        );
    });
    row.add_css_class("compact-row");
    list.append(&row);
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
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Admiral")
        .default_width(700)
        .default_height(600)
        .build();

    // CSS management (init once) - Apply to WebViews if needed via UserScript
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

    let favorites_entry = Entry::builder()
        .placeholder_text("Add channel to favorites")
        .build();

    let add_favorite_button = GtkButton::builder()
        .label("Add")
        .build();

    let favorites_entry_box = Box::new(Orientation::Horizontal, 6);
    favorites_entry_box.append(&favorites_entry);
    favorites_entry_box.append(&add_favorite_button);
    popover_content.append(&favorites_entry_box);

    let favorites_list = gtk::ListBox::builder() // Use fully qualified name
        .vexpand(true)
        .build();
    let favorites_scrolled = ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(200)
        .child(&favorites_list)
        .build();
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
        move |_| {
            create_new_tab("New Tab", &tab_view, &tabs);
        }
    ));

    let tabs_clone = tabs.clone();
    let favorites_list_clone = favorites_list.clone();
    let favorites_entry_clone = favorites_entry.clone();
    add_favorite_button.connect_clicked(clone!(
        #[strong]
        favorites_list_clone,
        #[strong]
        favorites_entry_clone,
        #[strong]
        tab_view,
        #[strong]
        tabs_clone,
        move |_| {
            let channel = favorites_entry_clone.text().to_string();
            if !channel.is_empty() {
                add_favorite(&channel);
                favorites_entry_clone.set_text("");
                load_and_display_favorites(&favorites_list_clone, &favorites_entry_clone, &favorites_list_clone, &tab_view, &tabs_clone);
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

    load_and_display_favorites(&favorites_list, &favorites_entry, &favorites_list, &tab_view, &tabs);

    overview_button.connect_clicked(clone!(
        #[strong]
        tab_overview,
        move |_| {
            tab_overview.set_open(true);
        }
    ));

    create_new_tab("New Tab", &tab_view, &tabs);

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
    glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        let tabs_map = tabs_clone.lock().unwrap();

        // Only process the currently selected tab
        if let Some(selected_page) = tab_view_for_processing.selected_page() {
            for (_, tab_data) in tabs_map.iter() {
                if tab_data.page != selected_page {
                    continue; // Skip inactive tabs
                }

                let mut messages_to_process = Vec::new();
                let rx = tab_data.rx.lock().unwrap();
                const MAX_BATCH_SIZE: usize = 20; // Reduce batch size

                loop {
                    if messages_to_process.len() >= MAX_BATCH_SIZE {
                        break;
                    }
                    match rx.try_recv() {
                        Ok(msg) => messages_to_process.push(msg),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break,
                    }
                }
                drop(rx);

                if !messages_to_process.is_empty() {
                    let webview = tab_data.webview.clone();
                    let channel_id_for_closure = messages_to_process
                        .first()
                        .map(|msg| msg.channel_id.clone());

                    if let Some(channel_id_str) = channel_id_for_closure {
                        idle_add_local(move || {
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

                            webview.evaluate_javascript(&js_code, None, None, None::<&adw::gio::Cancellable>, |result| {
                                if let Err(e) = result {
                                    eprintln!("Error running JS: {}", e);
                                }
                            });

                            ControlFlow::Break
                        });
                    }
                }
                break; // Only process one tab
            }
        } else {
            // If no tab is selected, drain messages from all tabs to prevent queue buildup
            for (_, tab_data) in tabs_map.iter() {
                let rx = tab_data.rx.lock().unwrap();
                let mut drained = 0;
                while rx.try_recv().is_ok() {
                    drained += 1;
                    if drained > 100 { break; } // Prevent infinite loop
                }
                if drained > 0 {
                    println!("Drained {} messages from inactive tab", drained);
                }
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
    new_tab_action.connect_activate(move |_, _| {
        create_new_tab("New Tab", &tab_view_clone, &tabs_clone);
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

    let tab_view_monitor = tab_view.clone();
    let tab_bar_monitor = tab_bar.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        let n_pages = tab_view_monitor.n_pages();
        glib::ControlFlow::Continue
    });

    window.present();
}

fn create_new_tab(
    label: &str,
    tab_view: &TabView,
    tabs: &Arc<Mutex<HashMap<String, Arc<TabData>>>>
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
    webview.set_vexpand(true);
    webview.set_hexpand(true);

    // Set WebView to use transparent background and match GTK theme
    webview.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));

    // Inject initial HTML and JavaScript with theme-aware styling
    let initial_html = r#"
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
            background-color: transparent;
            color: inherit;
        }
        #chat-container {
            flex: 1;
            overflow-y: auto;
            padding: 8px;
            display: flex;
            flex-direction: column;
        }
        .message-box {
            border: 1px solid rgba(153, 153, 153, 0.3);
            border-radius: 8px;
            padding: 8px;
            margin-bottom: 4px;
            background-color: rgba(255, 255, 255, 0.02);
        }
        .message-header { display: flex; justify-content: space-between; }
        .sender { font-weight: bold; }
        .timestamp { color: rgba(170, 170, 170, 0.8); font-size: 0.8em; }
        .message-content {
            margin-top: 4px;
            word-wrap: break-word;
            line-height: 28px;
        }
        .message-content img {
            height: 28px;
            width: auto;
            vertical-align: middle;
            display: inline-block;
            margin: 0 2px;
        }
        @media (prefers-color-scheme: dark) {
            body { color: #ffffff; }
        }
        @media (prefers-color-scheme: light) {
            body { color: #000000; }
            .message-box { background-color: rgba(0, 0, 0, 0.02); }
        }
      </style>
    </head>
    <body>
    <div id="chat-container">
      <div id="chat-body"></div>
    </div>
    <script>
      let isUserScrolling = false;
      let scrollTimeout = null;
      const chatContainer = document.getElementById('chat-container');
      const MAX_MESSAGES = 50; // Reduced from 100
      let messageCount = 0;
      let isProcessing = false;

      chatContainer.addEventListener('scroll', function() {
        const isAtBottom = chatContainer.scrollHeight - chatContainer.scrollTop <= chatContainer.clientHeight + 50;
        isUserScrolling = !isAtBottom;

        clearTimeout(scrollTimeout);
        scrollTimeout = setTimeout(() => {
          isUserScrolling = false;
        }, 2000);
      });

      function appendMessages(htmlString) {
        // Throttle if already processing
        if (isProcessing) {
          console.log("Skipping message batch - already processing");
          return;
        }

        isProcessing = true;

        try {
          var chatBody = document.getElementById('chat-body');
          var tempDiv = document.createElement('div');
          tempDiv.innerHTML = htmlString;

          // Use DocumentFragment for better performance
          var fragment = document.createDocumentFragment();
          while (tempDiv.firstChild) {
            fragment.appendChild(tempDiv.firstChild);
            messageCount++;
          }
          chatBody.appendChild(fragment);

          // Aggressive cleanup - remove old messages
          if (messageCount > MAX_MESSAGES) {
            var messages = chatBody.getElementsByClassName('message-box');
            var toRemove = messageCount - MAX_MESSAGES;
            for (var i = 0; i < toRemove && messages.length > 0; i++) {
              chatBody.removeChild(messages[0]);
              messageCount--;
            }
          }

          // Only auto-scroll if user isn't manually scrolling
          if (!isUserScrolling) {
            chatContainer.scrollTop = chatContainer.scrollHeight;
          }
        } finally {
          isProcessing = false;
        }
      }

      window.onload = function() {
        chatContainer.scrollTop = chatContainer.scrollHeight;
      };
    </script>
    </body>
    </html>
    "#;

    webview.load_html(initial_html, None);

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

    let (tx, rx) = mpsc::sync_channel(100); // This is correct - bounded channel
    let (error_tx, error_rx) = mpsc::channel();

    let tab_count = tabs.lock().unwrap().len();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let tab_id = format!("tab_{}_{}", timestamp, tab_count);
    let tab_data = TabData {
        page: page.clone(),
        webview: webview.clone(), // Store WebView
        stack: stack.clone(),
        entry: entry.clone(),
        channel_name: Arc::new(Mutex::new(None)),
        client_state: Arc::new(Mutex::new(ClientState::new())),
        connection_state: Arc::new(Mutex::new(ConnectionState::Disconnected)),
        tx,
        rx: Arc::new(Mutex::new(rx)),
        error_tx,
        error_rx: Arc::new(Mutex::new(error_rx)),
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
    // Clear WebView content and show chat view with theme-aware HTML
    let initial_html_with_js = r#"
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
            background-color: transparent;
            color: inherit;
        }
        #chat-container {
            flex: 1;
            overflow-y: auto;
            padding: 8px;
            display: flex;
            flex-direction: column;
        }
        .message-box {
            border: 1px solid rgba(153, 153, 153, 0.3);
            border-radius: 8px;
            padding: 8px;
            margin-bottom: 4px;
            background-color: rgba(255, 255, 255, 0.02);
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
        }
        @media (prefers-color-scheme: dark) {
            body { color: #ffffff; }
        }
        @media (prefers-color-scheme: light) {
            body { color: #000000; }
            .message-box { background-color: rgba(0, 0, 0, 0.02); }
        }
      </style>
    </head>
    <body>
    <div id="chat-container">
      <div id="chat-body"></div>
    </div>
    <script>
      let isUserScrolling = false;
      let scrollTimeout = null;
      const chatContainer = document.getElementById('chat-container');
      const MAX_MESSAGES = 100;

      // Detect if user is manually scrolling
      chatContainer.addEventListener('scroll', function() {
        const isAtBottom = chatContainer.scrollHeight - chatContainer.scrollTop <= chatContainer.clientHeight + 50;
        isUserScrolling = !isAtBottom;

        // Reset after 2 seconds of no scrolling
        clearTimeout(scrollTimeout);
        scrollTimeout = setTimeout(() => {
          isUserScrolling = false;
        }, 2000);
      });

      function appendMessages(htmlString) {
        var chatBody = document.getElementById('chat-body');
        var tempDiv = document.createElement('div');
        tempDiv.innerHTML = htmlString;

        while (tempDiv.firstChild) {
          chatBody.appendChild(tempDiv.firstChild);
        }

        // Remove old messages if we exceed the limit
        var messages = chatBody.getElementsByClassName('message-box');
        while (messages.length > MAX_MESSAGES) {
          chatBody.removeChild(messages[0]);
        }

        // Only auto-scroll if user isn't manually scrolling
        if (!isUserScrolling) {
          chatContainer.scrollTop = chatContainer.scrollHeight;
        }
      }

      // Initial scroll to bottom
      window.onload = function() {
        chatContainer.scrollTop = chatContainer.scrollHeight;
      };
    </script>
    </body>
    </html>
    "#;
    tab_data.webview.load_html(initial_html_with_js, None);
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
