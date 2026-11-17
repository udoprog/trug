use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
fn start() {}

#[wasm_bindgen]
pub fn answer() -> u32 {
    42
}
