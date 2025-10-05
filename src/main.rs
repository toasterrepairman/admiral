use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, TabBar, TabView, TabPage, TabOverview};
use gtk::{ScrolledWindow, ListBox, Label, Entry, Button as GtkButton, Orientation, Box, Align, MenuButton, Popover, Stack};
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

mod auth;
mod emotes;
use crate::emotes::{get_emote_map, parse_message, cleanup_emote_cache, cleanup_media_file_cache};
use crate::auth::create_auth_window;

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

fn build_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Admiral")
        .default_width(700)
        .default_height(600)
        .build();

    let header = HeaderBar::builder()
        .show_title(false)
        .css_classes(["flat"])
        .build();

    // Create TabView and TabBar
    let tab_view = TabView::builder()
        .vexpand(true)
        .build();

    let tab_bar = TabBar::builder()
        .view(&tab_view)
        .autohide(false)
        .build();

    // Create TabOverview
    let tab_overview = TabOverview::builder()
        .view(&tab_view)
        .child(&tab_view)
        .build();

    // Login button
    let login_button = GtkButton::builder().label("Log In").build();

    let popover_box = Box::new(Orientation::Vertical, 5);
    popover_box.append(&login_button);

    let popover = Popover::builder()
        .child(&popover_box)
        .build();

    let menu_button = MenuButton::builder()
        .popover(&popover)
        .build();

    // Add tab button
    let add_tab_button = GtkButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add new tab")
        .build();

    // Overview button
    let overview_button = GtkButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("Tab overview")
        .build();

    header.pack_end(&menu_button);
    header.pack_end(&overview_button);
    header.pack_end(&add_tab_button);

    // Main content
    let content = Box::new(Orientation::Vertical, 0);
    content.append(&header);
    content.append(&tab_bar);
    content.append(&tab_overview);

    // Store all tabs
    let tabs: Arc<Mutex<HashMap<String, Arc<TabData>>>> = Arc::new(Mutex::new(HashMap::new()));

    // Login button
    login_button.connect_clicked(clone!(@strong app => move |_| {
        create_auth_window(&app);
    }));

    // Add tab button handler
    add_tab_button.connect_clicked(clone!(@strong tab_view, @strong tabs => move |_| {
        create_new_tab("New Tab", &tab_view, &tabs);
    }));

    // Overview button handler
    overview_button.connect_clicked(clone!(@strong tab_overview => move |_| {
        tab_overview.set_open(true);
    }));

    // Create initial tab
    create_new_tab("Tab 1", &tab_view, &tabs);

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
        cleanup_media_file_cache();
        println!("Cleaning cache...");
        glib::ControlFlow::Continue
    });

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

    // Create listbox
    let listbox = ListBox::builder().build();

    let scrolled_window = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&listbox)
        .build();

    // Create placeholder
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

    // Create stack
    let stack = Stack::builder()
        .vexpand(true)
        .hexpand(true)
        .build();

    stack.add_named(&placeholder_box, Some("placeholder"));
    stack.add_named(&scrolled_window, Some("chat"));
    stack.set_visible_child_name("placeholder");

    // Add components to tab content
    tab_content.append(&entry_box);
    tab_content.append(&stack);

    // Add to tab view
    let page = tab_view.append(&tab_content);
    page.set_title(label);

    // Create channels for this tab
    let (tx, rx) = mpsc::channel();
    let (error_tx, error_rx) = mpsc::channel();

    // Generate unique tab ID based on current number of tabs
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
    tabs.lock().unwrap().insert(tab_id, tab_data_arc.clone());

    // Connect button handler for this tab
    connect_button.connect_clicked(clone!(@strong tab_data_arc => move |_| {
        let channel_name = tab_data_arc.entry.text().to_string();

        if channel_name.is_empty() {
            // Disconnect if empty
            disconnect_tab(&tab_data_arc);
            return;
        }

        let current_state = tab_data_arc.connection_state.lock().unwrap().clone();
        match current_state {
            ConnectionState::Connected(_) => {
                disconnect_tab(&tab_data_arc);
                start_connection_for_tab(&channel_name, &tab_data_arc);
            },
            ConnectionState::Disconnected | ConnectionState::Connecting => {
                start_connection_for_tab(&channel_name, &tab_data_arc);
            }
        }
    }));

    // Entry activation handler
    entry.connect_activate(clone!(@strong connect_button => move |_| {
        connect_button.emit_clicked();
    }));

    // Switch to new tab
    tab_view.set_selected_page(&page);
}

fn start_connection_for_tab(
    channel: &str,
    tab_data: &Arc<TabData>
) {
    *tab_data.connection_state.lock().unwrap() = ConnectionState::Connecting;
    *tab_data.channel_name.lock().unwrap() = Some(channel.to_string());

    tab_data.listbox.remove_all();
    tab_data.stack.set_visible_child_name("chat");

    // Update tab title
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
                eprintln!("Failed to join channel: {}", e);
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

            while let Some(message) = incoming_messages.recv().await {
                if let twitch_irc::message::ServerMessage::Privmsg(msg) = message {
                    if tx.send(msg.clone()).is_err() {
                        eprintln!("Failed to send message");
                        break;
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

fn disconnect_tab(tab_data: &Arc<TabData>) {
    *tab_data.connection_state.lock().unwrap() = ConnectionState::Disconnected;
    tab_data.client_state.lock().unwrap().disconnect();
    tab_data.listbox.remove_all();
    tab_data.stack.set_visible_child_name("placeholder");
}
