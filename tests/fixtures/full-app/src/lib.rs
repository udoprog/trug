use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = "export function show(t) { document.getElementById('out').textContent = t; }")]
extern "C" {
    fn show(text: &str);
}

#[wasm_bindgen(start)]
fn start() {
    show("wasm ran 42");
}
