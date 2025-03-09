use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, ActionRow, Avatar};
use gtk::{ScrolledWindow, ListBox, ListBoxRow, Label, Entry, Button, Orientation, Box, Align, Adjustment};
use std::sync::{Arc, Mutex};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};
use twitch_irc::login::StaticLoginCredentials;
use tokio::sync::mpsc;
use tokio::task;
use glib::clone;
use chrono::{TimeZone, NaiveDateTime, Utc, Local};

#[tokio::main]
async fn main() {
    let app = Application::builder()
        .application_id("com.toaster.Admiral")
        .build();

    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &Application) {
    let header = HeaderBar::builder()
        .show_title(true)
        .css_classes(["flat"])
        .build();
    let entry = Entry::builder().placeholder_text("Enter channel name").build();
    let connect_button = Button::builder().label("Connect").build();
    // Combine the content in a box
    let content = Box::new(Orientation::Vertical, 0);

    header.pack_start(&entry);
    header.pack_end(&connect_button);

    let listbox = ListBox::builder()
        .build();

    let scrolled_window = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .halign(Align::Baseline)
        .child(&listbox)
        .build();

    content.append(&header);
    content.append(&scrolled_window);

    let message_list = Arc::new(Mutex::new(listbox.clone()));
    let (tx, mut rx) = mpsc::channel(10);

    let active_task: Arc<Mutex<Option<task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    connect_button.connect_clicked(clone!(@strong message_list, @strong active_task => move |_| {
        // Kill any existing task
        if let Some(handle) = active_task.lock().unwrap().take() {
            // Optionally await the task to finish or just cancel it
            handle.abort(); // If the task is cancellable
        }

        message_list.lock().unwrap().remove_all();
        let channel = entry.text().to_string();
        let tx = tx.clone();
        let message_list = message_list.clone();

        // Spawn a new task
        let new_handle = task::spawn(async move {
            let config = ClientConfig::default();
            let (mut incoming_messages, client) = TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

            if let Err(e) = client.join(channel) {
                eprintln!("Failed to join channel: {}", e);
                return;
            }

            while let Some(message) = incoming_messages.recv().await {
                if let twitch_irc::message::ServerMessage::Privmsg(msg) = message {
                    // Attempt to send the message. If it fails, it will be dropped.
                    let _ = tx.try_send(msg.clone());
                }
            }
        });

        // Update the active task to track the new task
        *active_task.lock().unwrap() = Some(new_handle);
    }));


    glib::MainContext::default().spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            // Build message row outside of a new spawn_local
            let row = ActionRow::builder()
                .activatable(true)
                .title(format!("{}", &msg.message_text))
                .subtitle(format!("{} - {}",
                    &msg.sender.name,
                    &msg.server_timestamp.with_timezone(&Local).format("%-I:%M:%S %p").to_string()))
                .build();

            // Create avatar
            let avatar = Avatar::builder()
                .text(&msg.sender.name)
                .show_initials(true)
                .size(32)
                .build();

            row.add_prefix(&avatar);

            // Single lock for both operations
            if let Ok(mut list) = message_list.lock() {
                // Add message
                list.prepend(&row);

                // Cull oldest messages in the same lock
                if let Some(old_mess) = list.row_at_index(100) {
                    list.remove(&old_mess);
                }
            }
        }
    });


    let window = ApplicationWindow::builder()
                .application(app)
                .title("Admiral")
                .default_width(500)
                .default_height(600)
                // add content to window
                .content(&content)
                .build();

    window.present();
}
