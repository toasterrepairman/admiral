use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar};
use gtk::{ScrolledWindow, ListBox, ListBoxRow, Label, Entry, Button as GtkButton, Orientation, Box, Align, Widget, MenuButton, Popover, Stack};
use std::sync::{Arc, Mutex};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};
use twitch_irc::login::StaticLoginCredentials;
use glib::clone;
use chrono::Local;
use gio::SimpleAction;
use std::collections::HashMap;
use std::sync::mpsc::{self, TryRecvError, Receiver};
use std::thread;
use tokio::runtime::Runtime;
use glib::source::idle_add_local;
use glib::ControlFlow;
use glib::ControlFlow::Continue;

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
        // Drop the client to disconnect
        self.client = None;

        // Join the thread if it exists
        if let Some(handle) = self.join_handle.take() {
            handle.join().unwrap_or(());
        }

        // Recreate runtime if needed
        if self.runtime.is_none() {
            self.runtime = Some(Runtime::new().unwrap());
        }
    }
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
        .default_width(500)
        .default_height(600)
        .build();

    let header = HeaderBar::builder()
        .show_title(true)
        .css_classes(["flat"])
        .build();

    let entry = Entry::builder().placeholder_text("Enter channel name").build();
    let connect_button = GtkButton::builder().label("Connect").build();
    let login_button = GtkButton::builder().label("Log In").build();

    let popover_box = Box::new(Orientation::Vertical, 5);
    popover_box.append(&connect_button);
    popover_box.append(&login_button);

    let popover = Popover::builder()
        .child(&popover_box)
        .build();

    let menu_button = MenuButton::builder()
        .popover(&popover)
        .build();

    let content = Box::new(Orientation::Vertical, 0);

    header.pack_start(&entry);
    header.pack_end(&menu_button);

    entry.connect_activate(clone!(@strong connect_button => move |_| {
        connect_button.emit_clicked();
    }));

    login_button.connect_clicked(clone!(@strong app => move |_| {
        create_auth_window(&app);
    }));

    let listbox = ListBox::builder()
        .build();

    let scrolled_window = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(false)
        .halign(Align::Baseline)
        .child(&listbox)
        .build();

    // Create placeholder window
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

    // Create stack to switch between placeholder and chat
    let stack = Stack::builder()
        .vexpand(true)
        .hexpand(true)
        .build();

    stack.add_named(&placeholder_box, Some("placeholder"));
    stack.add_named(&scrolled_window, Some("chat"));
    stack.set_visible_child_name("placeholder");

    content.append(&header);
    content.append(&stack);

    // Shared state
    let message_list = Arc::new(Mutex::new(listbox.clone()));
    let stack_ref = Arc::new(Mutex::new(stack.clone()));
    let connection_state = Arc::new(Mutex::new(ConnectionState::Disconnected));
    let client_state = Arc::new(Mutex::new(ClientState::new()));
    let (tx, rx) = mpsc::channel();
    let (error_tx, error_rx) = mpsc::channel();

    // Connect button handler
    connect_button.connect_clicked(clone!(@strong message_list, @strong client_state, @strong stack_ref, @strong connection_state, @strong entry => move |_| {
        let current_channel = entry.text().to_string();

        if current_channel.is_empty() {
            // If entry is empty, disconnect and show placeholder
            disconnect(&client_state, &connection_state, &message_list, &stack_ref, true);
        } else {
            // Get current state
            let current_state = connection_state.lock().unwrap().clone();

            match current_state {
                ConnectionState::Connected(_) => {
                    // Disconnect if currently connected
                    disconnect(&client_state, &connection_state, &message_list, &stack_ref, false);
                    // Start new connection after disconnect
                    start_connection(
                        &current_channel,
                        &message_list,
                        &client_state,
                        &stack_ref,
                        &connection_state,
                        tx.clone(),
                        error_tx.clone()
                    );
                },
                ConnectionState::Disconnected | ConnectionState::Connecting => {
                    // Start new connection
                    start_connection(
                        &current_channel,
                        &message_list,
                        &client_state,
                        &stack_ref,
                        &connection_state,
                        tx.clone(),
                        error_tx.clone()
                    );
                }
            }
        }
    }));

    // Message processing timer (100ms)
    let client_state_clone = client_state.clone();
    let connection_state_clone = connection_state.clone();
    let message_list_clone = message_list.clone();
    let stack_ref_clone = stack_ref.clone();

    glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        // Handle connection errors
        loop {
            match error_rx.try_recv() {
                Ok(_) => {
                    disconnect(&client_state_clone, &connection_state_clone, &message_list_clone, &stack_ref_clone, true);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    let channelid = msg.channel_id.clone();
                    let emote_map = get_emote_map(&channelid);
                    let message_list = message_list_clone.clone();

                    // Schedule all GTK operations on the main (UI) thread
                    idle_add_local(move || {
                        let row = parse_message(&msg, &emote_map);
                        let list = message_list.lock().unwrap();

                        list.prepend(&row);

                        // Limit the number of displayed messages
                        let max_messages = 100;
                        let row_count = list.observe_children().n_items();
                        if row_count > max_messages as u32 {
                            if let Some(last_row) = list.last_child() {
                                list.remove(&last_row);
                            }
                        }

                        ControlFlow::Break
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        glib::ControlFlow::Continue
    });

    // Emote cache cleanup timer (30 seconds)
    glib::timeout_add_local(std::time::Duration::from_secs(30), move || {
        cleanup_emote_cache();
        cleanup_media_file_cache();
        println!("Cleaning cache...");
        glib::ControlFlow::Continue
    });

    window.set_content(Some(&content));

    let quit_action = SimpleAction::new("quit", None);
    let window_clone = window.clone();
    let client_state_quit = client_state.clone();
    let connection_state_quit = connection_state.clone();
    let message_list_quit = message_list.clone();
    let stack_ref_quit = stack_ref.clone();

    quit_action.connect_activate(move |_, _| {
        // Clean up before quitting
        disconnect(&client_state_quit, &connection_state_quit, &message_list_quit, &stack_ref_quit, true);
        window_clone.close();
    });
    window.add_action(&quit_action);

    app.set_accels_for_action("win.quit", &["<Control>q"]);

    window.present();
}

fn start_connection(
    channel: &str,
    message_list: &Arc<Mutex<ListBox>>,
    client_state: &Arc<Mutex<ClientState>>,
    stack_ref: &Arc<Mutex<Stack>>,
    connection_state: &Arc<Mutex<ConnectionState>>,
    tx: std::sync::mpsc::Sender<twitch_irc::message::PrivmsgMessage>,
    error_tx: std::sync::mpsc::Sender<()>
) {
    // Update connection state
    *connection_state.lock().unwrap() = ConnectionState::Connecting;

    // Clear existing messages
    message_list.lock().unwrap().remove_all();

    // Switch to chat view
    stack_ref.lock().unwrap().set_visible_child_name("chat");

    let channel = channel.to_string();
    let message_list = message_list.clone();
    let connection_state = connection_state.clone();
    let client_state_thread = client_state.clone();  // Clone for thread usage
    let client_state_store = client_state.clone();   // Clone for storing handle

    // Create new client and runtime
    let mut state = client_state.lock().unwrap();
    let runtime = state.runtime.take().unwrap();
    drop(state); // Release the lock before spawning thread

    let handle = thread::spawn(move || {
        runtime.block_on(async move {
            let config = ClientConfig::default();
            let (mut incoming_messages, client) = TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

            if let Err(e) = client.join(channel.clone()) {
                eprintln!("Failed to join channel: {}", e);
                let _ = error_tx.send(());
                return;
            }

            // Update client state with the new client
            {
                let mut state = client_state_thread.lock().unwrap();
                state.client = Some(client);
            }

            // Update connection state after successful join
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

            // Connection was closed, update state
            {
                let mut state = connection_state.lock().unwrap();
                if matches!(*state, ConnectionState::Connected(ref c) if c == &channel) {
                    *state = ConnectionState::Disconnected;
                }
            }
        });
    });

    // Store the join handle
    {
        let mut state = client_state_store.lock().unwrap();
        state.join_handle = Some(handle);
    }
}

fn disconnect(
    client_state: &Arc<Mutex<ClientState>>,
    connection_state: &Arc<Mutex<ConnectionState>>,
    message_list: &Arc<Mutex<ListBox>>,
    stack_ref: &Arc<Mutex<Stack>>,
    show_placeholder: bool
) {
    // Update connection state
    *connection_state.lock().unwrap() = ConnectionState::Disconnected;

    // Disconnect the client
    client_state.lock().unwrap().disconnect();

    // Clear messages
    message_list.lock().unwrap().remove_all();

    // Switch to placeholder view only if requested
    if show_placeholder {
        stack_ref.lock().unwrap().set_visible_child_name("placeholder");
    }
}
