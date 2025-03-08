use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, ActionRow };
use gtk::{ScrolledWindow, ListBox, ListBoxRow, Label, Entry, Button, Orientation, Box, Align, Adjustment};
use std::sync::{Arc, Mutex};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};
use twitch_irc::login::StaticLoginCredentials;
use tokio::sync::mpsc;
use tokio::task;
use glib::clone;

#[tokio::main]
async fn main() {
    let app = Application::builder()
        .application_id("com.example.TwitchChatViewer")
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
    let scroll_rules = Adjustment::new(0.0, 0.0, 100.0, 1.0, 10.0, 10.0);
    scroll_rules.set_value(scroll_rules.upper());

    let listbox = ListBox::builder()
        .build();

    let scrolled_window = ScrolledWindow::builder()
        .vexpand(true)
        .vadjustment(&scroll_rules)
        .hexpand(true)
        .halign(Align::Baseline)
        .child(&listbox)
        .build();

    content.append(&header);
    content.append(&scrolled_window);

    let message_list = Arc::new(Mutex::new(listbox.clone()));
    let (tx, mut rx) = mpsc::unbounded_channel();

    connect_button.connect_clicked(clone!(@strong message_list => move |_| {
        let channel = entry.text().to_string();
        let tx = tx.clone();
        let message_list = message_list.clone();

        task::spawn(async move {
            let config = ClientConfig::default();
            let (mut incoming_messages, client) = TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

            if let Err(e) = client.join(channel) {
                eprintln!("Failed to join channel: {}", e);
                return;
            }

            while let Some(message) = incoming_messages.recv().await {
                if let twitch_irc::message::ServerMessage::Privmsg(msg) = message {
                    let _ = tx.send(msg.clone());
                }
            }
        });
    }));

    glib::MainContext::default().spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            let message_list = message_list.clone();
            glib::MainContext::default().spawn_local(async move {
                // let row = ListBoxRow::builder().child(&Label::new(Some(&msg))).build();
                let row = ActionRow::builder()
                    .activatable(true)
                    .title(&msg.message_text)
                    .subtitle(format!("{} - {:?}", &msg.sender.name, &msg.server_timestamp))
                    .build();
                message_list.lock().unwrap().append(&row);
                // let upper = adjustment.upper(); // This is the bottom-most point
                // adjustment.set_value(upper); // Set the value to the bottom-most point
                // message_list.lock().unwrap().select_all();
            });
        }
    });

    let window = ApplicationWindow::builder()
                .application(app)
                .title("Chat")
                .default_width(500)
                .default_height(600)
                // add content to window
                .content(&content)
                .build();
    window.present();
}
