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
use gio::SimpleAction;

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

    // connect entry to button press
    entry.connect_activate(clone!(@strong connect_button => move |_| {
        connect_button.emit_clicked();
    }));

    let listbox = ListBox::builder()
        .build();

    let scrolled_window = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .halign(Align::Baseline)
        .child(&listbox)
        .build();

    // composite window content
    content.append(&header);
    content.append(&scrolled_window);

    // message list channel
    let message_list = Arc::new(Mutex::new(listbox.clone()));
    let (tx, mut rx) = mpsc::channel(100);
    // task handler
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
                    if tx.try_send(msg.clone()).is_err() {
                        eprintln!("Dropped message due to full queue.");
                    }
                }
            }
        });

        // Update the active task to track the new task
        *active_task.lock().unwrap() = Some(new_handle);
    }));

    glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        while let Ok(msg) = rx.try_recv() {
            let row = ActionRow::builder()
                .title(msg.message_text.clone())
                .subtitle(format!("{} - {}", msg.sender.name, msg.server_timestamp.with_timezone(&Local).format("%-I:%M:%S %p")))
                .build();

            let avatar = Avatar::builder()
                .text(&msg.sender.name)
                .show_initials(true)
                .size(32)
                .build();

            row.add_prefix(&avatar);

            let mut list = message_list.lock().unwrap();
            list.prepend(&row);

            while list.first_child().is_some() && list.row_at_index(100).is_some() {
                if let Some(child) = list.last_child() {
                    if let Some(row) = child.downcast_ref::<ListBoxRow>() {
                        list.remove(row);
                    } else {
                        eprintln!("Warning: Encountered non-ListBoxRow widget in ListBox!");
                    }
                }
            }
        }
        glib::ControlFlow::Continue // Keep running
    });

    let window = ApplicationWindow::builder()
                .application(app)
                .title("Admiral")
                .default_width(500)
                .default_height(600)
                // add content to window
                .content(&content)
                .build();

    // Create a "quit" action
    let quit_action = SimpleAction::new("quit", None);
    quit_action.connect_activate(glib::clone!(@weak window => move |_, _| {
        window.close(); // Close the window instead of quitting the app
    }));
    window.add_action(&quit_action);

    // Set up the accelerator for "quit" (Ctrl+Q)
    app.set_accels_for_action("win.quit", &["<Control>q"]);

    window.present();
}
