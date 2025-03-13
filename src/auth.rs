use adw::prelude::*;
use adw::{Application, MessageDialog};
use gtk::{Box as GtkBox, Button, Entry, Orientation};
use keyring::Entry as KeyringEntry;
use reqwest::Client;
use std::sync::Arc;
use open;
use glib::MainContext;

const CLIENT_ID: &str = "your_client_id";
const REDIRECT_URI: &str = "http://localhost:8080";

pub struct AuthDialog {
    dialog: MessageDialog,
    client: Arc<Client>,
    keyring: Arc<KeyringEntry>,
}

impl AuthDialog {
    pub fn new(app: &Application) -> Self {
        let dialog = MessageDialog::builder()
            .modal(true)
            .heading("Twitch Login")
            .width_request(350)
            .height_request(250)
            .application(app)
            .build();

        // Add dialog buttons
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("close", "Close");
        dialog.set_default_response(Some("close"));

        let keyring = Arc::new(KeyringEntry::new("your_app_name", "twitch_token").unwrap());
        let client = Arc::new(Client::new());

        Self {
            dialog,
            client,
            keyring,
        }
    }

    pub fn build_ui(&self) {
        // Main content box
        let content_box = GtkBox::builder()
            .orientation(Orientation::Vertical)
            .spacing(20)
            .margin_top(20)
            .margin_bottom(20)
            .margin_start(20)
            .margin_end(20)
            .build();

        // Login button
        let login_button = Button::builder()
            .label("Login with Twitch")
            .css_classes(["suggested-action"])
            .build();

        // Token entry
        let token_entry = Entry::builder()
            .placeholder_text("Access Token")
            .build();

        // Save button
        let save_button = Button::builder()
            .label("Save Token")
            .css_classes(["suggested-action"])
            .build();

        content_box.append(&login_button);
        content_box.append(&token_entry);
        content_box.append(&save_button);

        self.dialog.set_extra_child(Some(&content_box));

        // Clone necessary references for callbacks
        let keyring = self.keyring.clone();

        // Handle login button click
        login_button.connect_clicked(move |_| {
            let auth_url = format!(
                "https://id.twitch.tv/oauth2/authorize?client_id={}&redirect_uri={}&response_type=code&scope=chat:read+chat:edit",
                CLIENT_ID, REDIRECT_URI
            );
            if open::that(auth_url).is_err() {
                eprintln!("Failed to open browser");
            }
        });

        // Handle save button click
        save_button.connect_clicked(move |_| {
            let token = token_entry.text().to_string();
            if !token.is_empty() {
                let keyring = keyring.clone();
                MainContext::default().spawn_local(async move {
                    if keyring.set_password(&token).is_ok() {
                        println!("Token saved!");
                    } else {
                        eprintln!("Failed to save token");
                    }
                });
            }
        });

        // Handle dialog response
        self.dialog.connect_response(None, |dialog, response| {
            if response == "close" || response == "cancel" {
                dialog.close();
            }
        });
    }

    pub fn present(&self) {
        self.dialog.present();
    }
}

pub fn create_auth_dialog(app: &Application) {
    println!("Creating Auth Dialog...");
    let auth_dialog = AuthDialog::new(app);
    auth_dialog.build_ui();
    auth_dialog.present();
}
