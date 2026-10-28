//! Small web-sys helpers so the views stay readable.

use wasm_bindgen::convert::FromWasmAbi;
use wasm_bindgen::prelude::*;
use web_sys::{Document, Element, EventTarget, Window};

pub fn window() -> Window {
    web_sys::window().expect("no window")
}

pub fn document() -> Document {
    window().document().expect("no document")
}

/// Creates `<tag class="class">`.
pub fn el(tag: &str, class: &str) -> Element {
    let e = document().create_element(tag).expect("create_element");
    if !class.is_empty() {
        e.set_class_name(class);
    }
    e
}

/// Creates an element with text content.
pub fn el_text(tag: &str, class: &str, text: &str) -> Element {
    let e = el(tag, class);
    e.set_text_content(Some(text));
    e
}

pub fn set_text(e: &Element, text: &str) {
    e.set_text_content(Some(text));
}

pub fn append(parent: &Element, child: &Element) {
    parent.append_child(child).expect("append_child");
}

pub fn set_attr(e: &Element, name: &str, value: &str) {
    e.set_attribute(name, value).expect("set_attribute");
}

/// Toggles a CSS class.
pub fn set_class(e: &Element, class: &str, on: bool) {
    let list = e.class_list();
    let _ = if on { list.add_1(class) } else { list.remove_1(class) };
}

pub fn set_disabled(e: &Element, disabled: bool) {
    if disabled {
        set_attr(e, "disabled", "");
    } else {
        let _ = e.remove_attribute("disabled");
    }
}

/// Attaches an event listener for the lifetime of the page.
pub fn on<E>(target: &EventTarget, event: &str, f: impl FnMut(E) + 'static)
where
    E: FromWasmAbi + 'static,
{
    let cb = Closure::<dyn FnMut(E)>::new(f);
    target
        .add_event_listener_with_callback(event, cb.as_ref().unchecked_ref())
        .expect("add_event_listener");
    cb.forget();
}

pub fn log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}
