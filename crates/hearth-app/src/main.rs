//! Hearth desktop app: GTK4 + libadwaita + VTE client for hearthd.

mod actions;
mod keymap;
mod palette;
mod win;

use adw::prelude::*;
use gtk::gio;

const HELP: &str = "usage: hearth-app [--host TARGET] [--session NAME|ID] [--remote-control]

  --host TARGET     local (default), ssh:user@host, tcp:ADDR:PORT, unix:PATH
  --session NAME    show this session (default: the first one; one is created if none exist)
  --remote-control  let `hearth api client.action` drive this window";

const CSS: &str = "
.hearth-pane-header { padding: 2px 8px; font-size: 0.85em; background: alpha(@window_fg_color, 0.05); }
.hearth-pane.focused .hearth-pane-header { background: alpha(@accent_bg_color, 0.25); }
.hearth-status.blocked { color: @warning_color; }
.hearth-status.error { color: @error_color; }
.hearth-status.working { color: @accent_color; }
.hearth-status.done { color: @success_color; }
.hearth-status.idle { color: alpha(@window_fg_color, 0.5); }
";

fn main() {
    let mut opts = win::Options { host: std::env::var("HEARTH_HOST").unwrap_or_else(|_| "local".into()), session: None, remote_control: false };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => opts.host = args.next().unwrap_or_else(|| usage()),
            "--session" => opts.session = Some(args.next().unwrap_or_else(|| usage())),
            "--remote-control" => opts.remote_control = true,
            "--version" => {
                println!("hearth-app {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return;
            }
            _ => usage(),
        }
    }
    if let Err(e) = win::target_for(&opts.host) {
        eprintln!("hearth-app: {e}");
        std::process::exit(2);
    }
    let app = adw::Application::builder().application_id("dev.hearth.Hearth").flags(gio::ApplicationFlags::NON_UNIQUE).build();
    let opts = std::cell::RefCell::new(Some(opts));
    app.connect_startup(|_| {
        let css = gtk::CssProvider::new();
        css.load_from_string(CSS);
        if let Some(d) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&d, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
    let focus = gio::SimpleAction::new("focus-block", Some(gtk::glib::VariantTy::STRING));
    focus.connect_activate(|_, v| {
        if let Some(t) = v.and_then(|v| v.get::<String>()) {
            win::focus_from_notification(&t);
        }
    });
    app.add_action(&focus);
    app.connect_activate(move |app| {
        if let Some(o) = opts.borrow_mut().take() {
            win::Win::new(app, o);
        }
    });
    // GTK must not see our own flags.
    app.run_with_args::<&str>(&[]);
}

fn usage() -> ! {
    eprintln!("{HELP}");
    std::process::exit(2)
}
