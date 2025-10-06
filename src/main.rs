// main.rs

use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, TabBar, TabView, TabPage, TabOverview};
use gtk::{ScrolledWindow, Button, ListBox, Label, Entry, Button as GtkButton, Orientation, Box, Align, Stack, ListBoxRow, Popover};
use std::sync::{Arc, Mutex};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};
use twitch_irc::login::StaticLoginCredentials;
use glib::clone;
use gio::SimpleAction;
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

mod auth; // Assuming these modules exist
mod emotes; // Assuming these modules exist
use crate::emotes::{get_emote_map, parse_message, cleanup_emote_cache, cleanup_media_file_cache};

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
        // Note: Dropping the client handle might be enough for disconnection in twitch-irc
        // depending on how the library handles dropped handles. For explicit disconnection,
        // you might need a method on the client itself if available.
        // Here, we clear the client and join the thread.
        self.client = None;
        if let Some(handle) = self.join_handle.take() {
            handle.join().unwrap_or(());
        }
        // Recreate the runtime for potential reconnection
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

// Tab data structure
struct TabData {
    page: TabPage,
    listbox: ListBox,
    stack: Stack,
    entry: Entry,
    channel_name: Arc<Mutex<Option<String>>>,
    client_state: Arc<Mutex<ClientState>>,
    connection_state: Arc<Mutex<ConnectionState>>,
    tx: std::sync::mpsc::Sender<twitch_irc::message::PrivmsgMessage>,
    rx: Arc<Mutex<std::sync::mpsc::Receiver<twitch_irc::message::PrivmsgMessage>>>,
    error_tx: std::sync::mpsc::Sender<()>,
    error_rx: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
}

fn main() {
    let app = Application::builder()
        .application_id("com.example.Admiral")
        .build();
    app.connect_activate(build_ui);
    app.run();
}

// Favorites management functions
fn get_favorites_path() -> std::path::PathBuf {
    let config_dir = shellexpand::tilde("~/.config/admiral").into_owned();
    std::path::PathBuf::from(config_dir).join("favorites.toml")
}

fn load_favorites() -> Favorites {
    let path = get_favorites_path();
    if !path.exists() {
        // Create default file if it doesn't exist
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
    // Create parent directories if they don't exist
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
        favorites.channels.sort(); // Keep the list sorted
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
        // Remove from starred
        favorites.starred.retain(|c| c != &channel_lower);
    } else {
        // Add to starred if it's in the favorites list
        if favorites.channels.contains(&channel_lower) {
            favorites.starred.push(channel_lower);
            favorites.starred.sort(); // Keep starred sorted
        }
    }
    save_favorites(&favorites);
}

fn is_starred(channel: &str) -> bool {
    let favorites = load_favorites();
    favorites.starred.contains(&channel.to_lowercase())
}

fn load_and_display_favorites(
    list: &ListBox,
    favorites_entry: &Entry,
    favorites_list: &ListBox,
    tab_view: &TabView,
    tabs: &Arc<Mutex<HashMap<String, Arc<TabData>>>>
) {
    // Clear existing items
    list.remove_all();
    let favorites = load_favorites();
    // Separate starred and non-starred channels
    let mut starred_channels = Vec::new();
    let mut regular_channels = Vec::new();
    for channel in &favorites.channels {
        if favorites.starred.contains(channel) {
            starred_channels.push(channel.clone());
        } else {
            regular_channels.push(channel.clone());
        }
    }
    // Display starred channels first
    if !starred_channels.is_empty() {
        // Add a header for starred channels
        let header_row = ListBoxRow::new();
        let header_label = Label::new(Some("Starred Channels"));
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
        // Replace the separator with a proper horizontal separator
        let separator = gtk::Separator::new(Orientation::Horizontal);
        separator.set_margin_top(8);
        separator.set_margin_bottom(8);
        separator.set_sensitive(false);
        separator.set_opacity(0.4); // Optional: add slight visibility
        list.append(&separator);
    }
    // Display regular channels
    if !regular_channels.is_empty() {
        // Add a header for regular channels if there are starred channels
        if !starred_channels.is_empty() {
            let header_row = ListBoxRow::new();
            let header_label = Label::new(Some("Favorites"));
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
    // Check if there are no favorites at all
    if favorites.channels.is_empty() {
        let empty_row = ListBoxRow::new();
        let empty_label = Label::new(Some("No favorites yet"));
        empty_label.add_css_class("dim-label");
        empty_row.set_child(Some(&empty_label));
        list.append(&empty_row);
    }
}

fn create_favorite_row(
    list: &ListBox,
    channel: &str,
    is_starred: bool,
    tab_view: &TabView,
    tabs: &Arc<Mutex<HashMap<String, Arc<TabData>>>>,
    favorites_entry: &Entry,
    favorites_list: &ListBox,
) {
    let row = ListBoxRow::new();
    let content_box = Box::new(Orientation::Horizontal, 6);
    content_box.set_hexpand(true);

    // Channel name label
    let channel_label = Label::builder()
        .label(channel)
        .halign(Align::Start)
        .valign(Align::Center)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    // Star button
    let star_icon = if is_starred { "starred-symbolic" } else { "non-starred-symbolic" };
    let star_tooltip = if is_starred { "Unstar channel" } else { "Star channel" };
    let star_button = Button::builder()
        .icon_name(star_icon)
        .tooltip_text(star_tooltip)
        .build();

    // Trash button
    let trash_button = Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove from favorites")
        .build();

    // 👉 RIGHT-ALIGN BUTTONS: Add label, then a spacer, then buttons
    content_box.append(&channel_label);
    // Spacer that expands to push buttons to the right
    let spacer = Box::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    content_box.append(&spacer);
    // Buttons (now right-aligned)
    content_box.append(&star_button);
    content_box.append(&trash_button);

    row.set_child(Some(&content_box));
    row.set_selectable(true);
    row.set_activatable(true); // Ensure it's activatable

    // ===== ROW CLICK: Open tab and connect =====
    let channel_clone = channel.to_string();
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();

    // Create a gesture click handler instead of using activate
    let gesture = gtk::GestureClick::new();
    gesture.connect_released(move |_, _, _, _| {
        println!("Row clicked for channel: {}", channel_clone);
        create_new_tab(&channel_clone, &tab_view_clone, &tabs_clone);
        // Small delay to ensure tab is fully created
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

    // ===== STAR BUTTON =====
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

    // ===== TRASH BUTTON =====
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

fn build_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Admiral")
        .default_width(700)
        .default_height(600)
        .build();

    // Create TabView and TabBar
    let tab_view = TabView::builder()
        .vexpand(true)
        .build();
    let tab_bar = TabBar::builder()
        .view(&tab_view)
        .autohide(true)
        .build();
    tab_bar.add_css_class("inline");

    // Create HeaderBar
    let header = HeaderBar::builder()
        .build();

    // === FAVORITES POPOVER IMPLEMENTATION ===
    // Create favorites button (placed in HeaderBar, left-justified)
    let favorites_button = GtkButton::builder()
        .icon_name("non-starred-symbolic")
        .tooltip_text("Favorites")
        .build();

    // Create popover content
    let popover = Popover::builder()
        .autohide(true)
        .build();

    // Create content for the popover
    let popover_content = Box::new(Orientation::Vertical, 6);
    popover_content.set_margin_top(6);
    popover_content.set_margin_bottom(6);
    popover_content.set_margin_start(6);
    popover_content.set_margin_end(6);

    // Entry for adding new favorites
    let favorites_entry = Entry::builder()
        .placeholder_text("Add channel to favorites")
        .build();

    // Button to add the channel
    let add_favorite_button = GtkButton::builder()
        .label("Add")
        .build();

    let favorites_entry_box = Box::new(Orientation::Horizontal, 6);
    favorites_entry_box.append(&favorites_entry);
    favorites_entry_box.append(&add_favorite_button);
    popover_content.append(&favorites_entry_box);

    // Scrolled window for favorites list
    let favorites_list = ListBox::builder()
        .vexpand(true)
        .build();
    let favorites_scrolled = ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(200)
        .child(&favorites_list)
        .build();
    popover_content.append(&favorites_scrolled);

    popover.set_child(Some(&popover_content));

    // Store clones for the button and popover
    let favorites_button_clone = favorites_button.clone();
    let popover_clone = popover.clone();

    // Connect the button to show the popover
    favorites_button.connect_clicked(move |_| {
        // This is the critical fix: set the parent before showing
        popover_clone.set_parent(&favorites_button_clone);
        popover_clone.popup();
    });

    // Add the favorites button to the start of the header bar (left side)
    header.pack_start(&favorites_button);

    // === END FAVORITES POPOVER IMPLEMENTATION ===

    // Add tab button (placed in HeaderBar)
    let add_tab_button = GtkButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add new tab")
        .build();

    // Overview button (placed in HeaderBar)
    let overview_button = GtkButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("Tab overview")
        .build();

    // Pack buttons into the HeaderBar
    header.pack_end(&add_tab_button);
    header.pack_end(&overview_button);

    // Create TabOverview
    let tab_overview = TabOverview::builder()
        .view(&tab_view)
        .child(&tab_view)
        .enable_new_tab(false)
        .show_end_title_buttons(false)
        .build();

    // Main content container
    let content = Box::new(Orientation::Vertical, 0);
    content.append(&header);
    content.append(&tab_bar);
    content.append(&tab_overview);

    // Store all tabs
    let tabs: Arc<Mutex<HashMap<String, Arc<TabData>>>> = Arc::new(Mutex::new(HashMap::new()));

    // Add tab button handler
    add_tab_button.connect_clicked(clone!(@strong tab_view, @strong tabs => move |_| {
        create_new_tab("New Tab", &tab_view, &tabs);
    }));

    // Favorites button handlers
    let tabs_clone = tabs.clone();
    let favorites_list_clone = favorites_list.clone();
    let favorites_entry_clone = favorites_entry.clone();
    add_favorite_button.connect_clicked(clone!(@strong favorites_list_clone, @strong favorites_entry_clone, @strong tab_view, @strong tabs_clone => move |_| {
        let channel = favorites_entry_clone.text().to_string();
        if !channel.is_empty() {
            add_favorite(&channel);
            favorites_entry_clone.set_text("");
            load_and_display_favorites(&favorites_list_clone, &favorites_entry_clone, &favorites_list_clone, &tab_view, &tabs_clone);
        }
    }));

    // Connect the entry to the add button (Enter key)
    favorites_entry_clone.connect_activate(clone!(@strong add_favorite_button => move |_| {
        add_favorite_button.emit_clicked();
    }));

    // Load initial favorites
    load_and_display_favorites(&favorites_list, &favorites_entry, &favorites_list, &tab_view, &tabs);

    // Overview button handler
    overview_button.connect_clicked(clone!(@strong tab_overview => move |_| {
        tab_overview.set_open(true);
    }));

    // Create initial tab
    create_new_tab("New Tab", &tab_view, &tabs);

    // Message processing timer
    let tabs_clone = tabs.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        let tabs_map = tabs_clone.lock().unwrap();
        for (_, tab_data) in tabs_map.iter() {
            // Handle errors
            let error_rx = tab_data.error_rx.lock().unwrap();
            loop {
                match error_rx.try_recv() {
                    Ok(_) => {
                        drop(error_rx);
                        disconnect_tab(tab_data);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            // Handle messages
            let rx = tab_data.rx.lock().unwrap();
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        let channelid = msg.channel_id.clone();
                        let emote_map = get_emote_map(&channelid);
                        let listbox = tab_data.listbox.clone();
                        idle_add_local(move || {
                            let row = parse_message(&msg, &emote_map);
                            listbox.prepend(&row);
                            let max_messages = 100;
                            let row_count = listbox.observe_children().n_items();
                            if row_count > max_messages as u32 {
                                if let Some(last_row) = listbox.last_child() {
                                    listbox.remove(&last_row);
                                }
                            }
                            ControlFlow::Break
                        });
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }
        glib::ControlFlow::Continue
    });

    // Emote cache cleanup timer
    glib::timeout_add_local(std::time::Duration::from_secs(30), move || {
        cleanup_emote_cache();
        // MediaFile cleanup is now handled by the resource manager per message.
        // This call remains if there's a need for a global check, but it's less critical.
        // cleanup_media_file_cache();
        println!("Cleaning emote cache...");
        glib::ControlFlow::Continue
    });

    // Action for creating a new tab (Ctrl+T)
    let new_tab_action = SimpleAction::new("new-tab", None);
    let tab_view_clone = tab_view.clone();
    let tabs_clone = tabs.clone();
    new_tab_action.connect_activate(move |_, _| {
        create_new_tab("New Tab", &tab_view_clone, &tabs_clone);
    });
    window.add_action(&new_tab_action);

    // Action for closing the selected tab (Ctrl+W)
    let close_tab_action = SimpleAction::new("close-tab", None);
    let tab_view_close = tab_view.clone();
    let tabs_close = tabs.clone();
    close_tab_action.connect_activate(move |_, _| {
        if let Some(selected_page) = tab_view_close.selected_page() {
            tab_view_close.close_page(&selected_page);
        }
    });
    window.add_action(&close_tab_action);

    // Set the accelerators
    app.set_accels_for_action("win.new-tab", &["<Control>t"]);
    app.set_accels_for_action("win.close-tab", &["<Control>w"]);

    window.set_content(Some(&content));

    // Quit action
    let quit_action = SimpleAction::new("quit", None);
    let window_clone = window.clone();
    let tabs_quit = tabs.clone();
    quit_action.connect_activate(move |_, _| {
        let tabs_map = tabs_quit.lock().unwrap();
        for (_, tab_data) in tabs_map.iter() {
            disconnect_tab(tab_data);
        }
        window_clone.close();
    });
    window.add_action(&quit_action);
    app.set_accels_for_action("win.quit", &["<Control>q"]);

    // Start monitoring tab count to control tab bar visibility
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
    // Create the tab content container
    let tab_content = Box::new(Orientation::Vertical, 0);

    // Create entry and connect button for this tab
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

    // Create listbox for chat messages
    let listbox = ListBox::builder().build();
    let scrolled_window = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&listbox)
        .build();

    // Create placeholder content for when not connected
    let placeholder_box = Box::new(Orientation::Vertical, 12);
    placeholder_box.set_valign(Align::Center);
    placeholder_box.set_halign(Align::Center);
    placeholder_box.set_margin_top(60);
    placeholder_box.set_margin_bottom(60);
    placeholder_box.set_margin_start(40);
    placeholder_box.set_margin_end(40);

    let main_label = Label::new(Some("Choose a channel"));
    main_label.set_css_classes(&["title-1"]);
    main_label.set_halign(Align::Center);

    let subtitle_label = Label::new(Some("Type a channel name in the entry above"));
    subtitle_label.set_css_classes(&["dim-label"]);
    subtitle_label.set_halign(Align::Center);

    placeholder_box.append(&main_label);
    placeholder_box.append(&subtitle_label);

    // Create stack to switch between placeholder and chat view
    let stack = Stack::builder()
        .vexpand(true)
        .hexpand(true)
        .build();
    stack.add_named(&placeholder_box, Some("placeholder"));
    stack.add_named(&scrolled_window, Some("chat"));
    stack.set_visible_child_name("placeholder");

    // Add components to the tab's content area
    tab_content.append(&entry_box);
    tab_content.append(&stack);

    // Append the new content area as a page to the TabView
    let page = tab_view.append(&tab_content);
    page.set_title(label);

    // Create channels for this specific tab to receive messages/errors
    let (tx, rx) = mpsc::channel();
    let (error_tx, error_rx) = mpsc::channel();

    // Generate a unique identifier for this tab
    let tab_count = tabs.lock().unwrap().len();
    let tab_id = format!("tab_{}", tab_count);

    let tab_data = TabData {
        page: page.clone(),
        listbox: listbox.clone(),
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
    tabs.lock().unwrap().insert(tab_id, tab_data_arc.clone()); // Store the tab data

    // Connect the connect/disconnect button for this specific tab
    connect_button.connect_clicked(clone!(@strong tab_data_arc => move |_| {
        let channel_name = tab_data_arc.entry.text().to_string();
        if channel_name.is_empty() {
            // If entry is empty, disconnect the current tab
            disconnect_tab(&tab_data_arc);
            return;
        }
        let current_state = tab_data_arc.connection_state.lock().unwrap().clone();
        match current_state {
            ConnectionState::Connected(_) => {
                // If already connected, disconnect first, then connect to the new channel
                disconnect_tab(&tab_data_arc);
                start_connection_for_tab(&channel_name, &tab_data_arc);
            },
            ConnectionState::Disconnected | ConnectionState::Connecting => {
                // If disconnected or connecting, try to connect
                start_connection_for_tab(&channel_name, &tab_data_arc);
            }
        }
    }));

    // Allow pressing Enter in the entry to trigger the connect button
    entry.connect_activate(clone!(@strong connect_button => move |_| {
        connect_button.emit_clicked();
    }));

    // Switch to the newly created tab
    tab_view.set_selected_page(&page);
}

fn start_connection_for_tab(
    channel: &str,
    tab_data: &Arc<TabData>
) {
    // Update connection state to Connecting
    *tab_data.connection_state.lock().unwrap() = ConnectionState::Connecting;

    // Store the channel name being connected to
    *tab_data.channel_name.lock().unwrap() = Some(channel.to_string());

    // Clear previous messages and show the chat view
    tab_data.listbox.remove_all();
    tab_data.stack.set_visible_child_name("chat");

    // Update the tab's title to reflect the connected channel
    tab_data.page.set_title(channel);

    // Extract necessary data for the thread
    let channel = channel.to_string();
    let connection_state = tab_data.connection_state.clone();
    let client_state_thread = tab_data.client_state.clone();
    let client_state_store = tab_data.client_state.clone();
    let tx = tab_data.tx.clone();
    let error_tx = tab_data.error_tx.clone();

    // Take the runtime from the stored state to move into the thread
    let mut state = tab_data.client_state.lock().unwrap();
    let runtime = state.runtime.take().unwrap();
    drop(state);

    // Spawn the connection thread
    let handle = thread::spawn(move || {
        runtime.block_on(async move {
            let config = ClientConfig::default();
            let (mut incoming_messages, client) = TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

            // Attempt to join the specified channel
            if let Err(e) = client.join(channel.clone()) {
                eprintln!("Failed to join channel '{}': {}", channel, e);
                let _ = error_tx.send(()); // Signal an error to the main thread
                return;
            }

            // Store the client handle so it can be managed (e.g., disconnected later)
            {
                let mut state = client_state_thread.lock().unwrap();
                state.client = Some(client);
            }

            // Update connection state to Connected
            {
                let mut state = connection_state.lock().unwrap();
                *state = ConnectionState::Connected(channel.clone());
            }

            // Main message loop
            while let Some(message) = incoming_messages.recv().await {
                if let twitch_irc::message::ServerMessage::Privmsg(msg) = message {
                    // Send the received message to the main thread for UI update
                    if tx.send(msg.clone()).is_err() {
                        eprintln!("Failed to send message to UI thread, channel might be closed");
                        break; // Exit the loop if sending fails
                    }
                }
            }

            // Update connection state back to Disconnected when the loop ends (e.g., due to error or disconnection)
            {
                let mut state = connection_state.lock().unwrap();
                if matches!(*state, ConnectionState::Connected(ref c) if c == &channel) {
                    *state = ConnectionState::Disconnected;
                }
            }
        });
    });

    // Store the join handle so we can wait for it later if needed (e.g., on app quit)
    {
        let mut state = client_state_store.lock().unwrap();
        state.join_handle = Some(handle);
    }
}

fn disconnect_tab(tab_data: &Arc<TabData>) {
    // Update the tab's connection state
    *tab_data.connection_state.lock().unwrap() = ConnectionState::Disconnected;

    // Trigger disconnection logic in the client state
    tab_data.client_state.lock().unwrap().disconnect();

    // Clear the chat display and show the placeholder
    tab_data.listbox.remove_all();
    tab_data.stack.set_visible_child_name("placeholder");

    // Reset the tab title
    tab_data.page.set_title("New Tab"); // Or some default title
}
