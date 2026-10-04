//! xssh desktop manager: hosts, sessions, port forwards, jobs, audit log and daemon control.
//! It is a client of the xssh daemon like the CLI; closing it never stops the daemon.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod model;
mod pages;
mod ui;

use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

fn main() {
    let backend = match backend::Backend::open() {
        Ok(b) => b,
        Err(e) => {
            let text = match &e.hint {
                Some(h) => format!(
                    "{}

{h}",
                    e.message
                ),
                None => e.message.clone(),
            };
            fatal(&text);
        }
    };
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::sync_system_appearance(None, cx);
        // Closing the window quits the app; the daemon keeps running on its own.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1240.), px(800.)), cx))),
            window_min_size: Some(size(px(900.), px(560.))),
            // Frameless: the app draws its own title bar (`TitleBar` in app.rs).
            titlebar: Some(TitlebarOptions {
                title: Some("xssh".into()),
                ..TitleBar::title_bar_options()
            }),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| app::MainView::new(backend, window, cx))).expect("open window");
        cx.activate(true);
    });
}

/// Report a startup error. Release builds have no console, so Windows gets a message box.
fn fatal(text: &str) -> ! {
    eprintln!("xssh-desktop: {text}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW};
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(text), wide("xssh"));
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_ICONERROR) };
    }
    std::process::exit(1);
}
