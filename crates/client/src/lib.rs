//! Browser client, compiled to WASM. All UI is built with web-sys directly.

pub mod audio;
pub mod capture;
pub mod dom;
pub mod scan;
pub mod views;
pub mod ws;

use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {
    // Test harnesses load this module without the app shell; do nothing there.
    let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("app"))
    else {
        return;
    };
    let view = views::call::CallView::mount(&root);
    std::mem::forget(view); // lives for the page
}
