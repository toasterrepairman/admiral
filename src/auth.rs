use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar};
use gtk::{Box as GtkBox, Button, Entry, Label, Orientation};
use keyring::Entry as KeyringEntry;
use reqwest::Client;
use std::sync::Arc;
use open;
use glib::MainContext;

const CLIENT_ID: &str = "your_client_id";
const REDIRECT_URI: &str = "http://localhost:8080";

pub struct AuthWindow {
    window: ApplicationWindow,
    client: Arc<Client>,
    keyring: Arc<KeyringEntry>,
}

impl AuthWindow {
    pub fn new(app: &Application) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Twitch Login")
            .default_width(400)
            .default_height(200)
            .build();

        let keyring = Arc::new(KeyringEntry::new("your_app_name", "twitch_token").unwrap());
        let client = Arc::new(Client::new());

        Self {
            window,
            client,
            keyring,
        }
    }

    pub fn build_ui(&self) {
        // Create a header bar
        let header = HeaderBar::builder()
            .show_title(true)
            .css_classes(["flat"])
            .title_widget(&Label::new(Some("Twitch Login")))
            .build();

        // Main content with padding
        let content_box = GtkBox::new(Orientation::Vertical, 20);

        let login_button = Button::with_label("Login");
        login_button.set_margin_top(0);
        login_button.set_margin_bottom(10);
        login_button.set_margin_start(20);
        login_button.set_margin_end(20);

        let token_entry = Entry::new();
        token_entry.set_placeholder_text(Some("Access Token"));
        token_entry.set_margin_top(10);
        token_entry.set_margin_bottom(10);
        token_entry.set_margin_start(20);
        token_entry.set_margin_end(20);

        let save_button = Button::with_label("Save Token");
        save_button.set_margin_top(10);
        save_button.set_margin_bottom(20);
        save_button.set_margin_start(20);
        save_button.set_margin_end(20);

        content_box.append(&login_button);
        content_box.append(&token_entry);
        content_box.append(&save_button);

        // Create a root layout container
        let root_box = GtkBox::new(Orientation::Vertical, 0);
        root_box.append(&header);
        root_box.append(&content_box);

        // Set the root layout as the content
        self.window.set_content(Some(&root_box));

        // Clone necessary references for async callbacks
        let keyring = self.keyring.clone();

        // Open Twitch login URL
        login_button.connect_clicked(move |_| {
            let auth_url = format!(
                "https://id.twitch.tv/oauth2/authorize?client_id={}&redirect_uri={}&response_type=code&scope=chat:read+chat:edit",
                CLIENT_ID, REDIRECT_URI
            );
            if open::that(auth_url).is_err() {
                eprintln!("Failed to open browser");
            }
        });

        // Save access token
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
    }

    pub fn show(&self) {
        self.window.present(); // Correct way to show the window in GTK4 + Libadwaita
    }
}

pub fn create_auth_window(app: &Application) {
    println!("Creating Auth Window...");
    let auth_window = AuthWindow::new(app);
    auth_window.build_ui();
    auth_window.show();
}
