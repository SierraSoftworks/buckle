//! Buckle UI entry point. Lane 9 (UI) builds the application here.

use yew::prelude::*;

#[function_component(App)]
fn app() -> Html {
    html! {
        <main style="padding: 2rem">
            <h1>{ "Buckle" }</h1>
            <p>{ "The UI is being built." }</p>
        </main>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    wasm_logger::init(wasm_logger::Config::default());
    yew::Renderer::<App>::new().render();
}
