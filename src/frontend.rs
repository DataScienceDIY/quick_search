#![allow(non_snake_case)]

use dioxus::prelude::*;


// define a component that renders a div with the text "Hello, world!"
pub fn App() -> Element {
    let mut count = use_signal(|| 0);

    rsx! {
        h1 { "High-Five counter: {count}" }
        button { onclick: move |_| count += 1, "Up high!" }
        button { onclick: move |_| count -= 1, "Down low!" }
    }
}