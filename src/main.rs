use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar};
use gtk::{ScrolledWindow, ListBox, ListBoxRow, Label, Entry, Button as GtkButton, Orientation, Box, Align, Widget, MenuButton, Popover, Stack};
use std::sync::{Arc, Mutex};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};
use twitch_irc::login::StaticLoginCredentials;
use tokio::sync::mpsc;
use tokio::task;
use glib::clone;
use chrono::Local;
use gio::SimpleAction;
use std::collections::HashMap;

mod auth;
mod emotes;
use crate::emotes::{get_emote_map, parse_message, cleanup_emote_cache};
use crate::auth::create_auth_window;

#[tokio::main]
async fn main() {
    let app = Application::builder()
        .application_id("Admiral")
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

    let message_list = Arc::new(Mutex::new(listbox.clone()));
    let stack_ref = Arc::new(Mutex::new(stack.clone()));
    let (tx, mut rx) = mpsc::channel(100);
    let (error_tx, mut error_rx) = mpsc::channel(10);
    let active_task: Arc<Mutex<Option<task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    connect_button.connect_clicked(clone!(@strong message_list, @strong active_task, @strong stack_ref => move |_| {
        // Abort any existing task
        if let Some(handle) = active_task.lock().unwrap().take() {
            handle.abort();
        }

        // Clear existing messages
        message_list.lock().unwrap().remove_all();

        // Switch to chat view
        stack_ref.lock().unwrap().set_visible_child_name("chat");

        let channel = entry.text().to_string();
        let tx = tx.clone();
        let error_tx = error_tx.clone();
        let message_list = message_list.clone();

        let new_handle = task::spawn(async move {
            let config = ClientConfig::default();
            let (mut incoming_messages, client) = TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

            if let Err(e) = client.join(channel) {
                eprintln!("Failed to join channel: {}", e);

                // Send error signal to main thread
                let _ = error_tx.send(()).await;

                return;
            }

            while let Some(message) = incoming_messages.recv().await {
                if let twitch_irc::message::ServerMessage::Privmsg(msg) = message {
                    if tx.try_send(msg.clone()).is_err() {
                        eprintln!("Dropped message due to full queue.");
                    }
                }
            }
        });

        *active_task.lock().unwrap() = Some(new_handle);
    }));

    // Message processing timer (100ms)
    glib::timeout_add_local(std::time::Duration::from_millis(100), clone!(@strong stack_ref => move || {
        // Handle connection errors
        while let Ok(_) = error_rx.try_recv() {
            stack_ref.lock().unwrap().set_visible_child_name("placeholder");
        }

        // Handle incoming messages
        while let Ok(msg) = rx.try_recv() {
            let channelid = &msg.channel_id;
            let emote_map = get_emote_map(&channelid);
            let row = parse_message(&msg, &emote_map);
            let mut list = message_list.lock().unwrap();
            list.prepend(&row);

            // Limit the number of displayed messages
            let max_messages = 100;
            let row_count = list.observe_children().n_items();
            if row_count > max_messages as u32 {
                if let Some(last_row) = list.last_child() {
                    list.remove(&last_row);
                }
            }
        }
        glib::ControlFlow::Continue
    }));

    // Emote cache cleanup timer (30 seconds)
    glib::timeout_add_local(std::time::Duration::from_secs(30), move || {
        cleanup_emote_cache();
        println!("Cleaning cache...");
        glib::ControlFlow::Continue
    });

    window.set_content(Some(&content));

    let quit_action = SimpleAction::new("quit", None);
    quit_action.connect_activate(glib::clone!(@weak window => move |_, _| {
        window.close();
    }));
    window.add_action(&quit_action);

    app.set_accels_for_action("win.quit", &["<Control>q"]);

    window.present();
}
