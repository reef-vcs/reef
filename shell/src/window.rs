//! No capability is granted to any origin, so the page gets no IPC into the
//! app (S-269).

use std::sync::{Mutex, PoisonError};

use reef_shell::{Command, Page};
use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};

const MAIN: &str = "main";

static ORIGIN: Mutex<Option<String>> = Mutex::new(None);

fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn allowed(url: &Url) -> bool {
    // Tauri serves the app's own pages at `tauri://localhost`, and at
    // `http://tauri.localhost` on Windows.
    if url.scheme() == "tauri" || url.host_str() == Some("tauri.localhost") {
        return true;
    }
    let pinned = ORIGIN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if pinned.as_deref() == Some(origin_of(url).as_str()) {
        return true;
    }
    if matches!(url.scheme(), "http" | "https") {
        super::open(url.as_str().as_ref());
    }
    false
}

fn offline(why: &str) -> Option<Url> {
    let mut url = Url::parse("tauri://localhost/offline.html").ok()?;
    url.set_fragment(Some(why));
    Some(url)
}

/// Shows the window at `page`; call on the main thread.
pub fn show(handle: &AppHandle, page: Page) {
    let url = match &page {
        Page::Session(url) => {
            *ORIGIN.lock().unwrap_or_else(PoisonError::into_inner) = Some(url.origin().to_owned());
            url.expose().parse::<Url>().ok()
        }
        Page::Navigate(url) => url.parse::<Url>().ok(),
        Page::Offline(why) => offline(why),
    };
    let Some(url) = url else {
        return tracing::warn!("a page the window cannot load");
    };
    if let Some(window) = handle.get_webview_window(MAIN) {
        let _ = window.navigate(url);
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let target = if url.scheme() == "tauri" {
        WebviewUrl::App(format!("offline.html#{}", url.fragment().unwrap_or_default()).into())
    } else {
        WebviewUrl::External(url)
    };
    let built = WebviewWindowBuilder::new(handle, MAIN, target)
        .title("Reef")
        .inner_size(1200.0, 820.0)
        .min_inner_size(480.0, 400.0)
        .incognito(true)
        .on_navigation(allowed)
        .on_new_window(|url, _| {
            if let Some(shell) = super::shell() {
                shell.send(Command::NewWindow(url.to_string()));
            }
            NewWindowResponse::Deny
        })
        .on_page_load(|_, payload| {
            if payload.event() == PageLoadEvent::Finished
                && let Some(shell) = super::shell()
            {
                let mut url = payload.url().clone();
                url.set_fragment(None);
                shell.send(Command::Visited(url.to_string()));
            }
        })
        .build();
    match built {
        Ok(window) => {
            #[cfg(target_os = "macos")]
            let _ = handle.set_activation_policy(tauri::ActivationPolicy::Regular);
            window.on_window_event(|event| {
                if matches!(event, WindowEvent::Destroyed)
                    && let Some(shell) = super::shell()
                {
                    shell.send(Command::WindowClosed);
                }
            });
            let accessory = handle.clone();
            window.on_window_event(move |event| {
                if matches!(event, WindowEvent::Destroyed) {
                    #[cfg(target_os = "macos")]
                    let _ = accessory.set_activation_policy(tauri::ActivationPolicy::Accessory);
                    #[cfg(not(target_os = "macos"))]
                    let _ = &accessory;
                }
            });
            let _ = window.set_focus();
        }
        Err(error) => tracing::warn!(%error, "the window did not open"),
    }
}
