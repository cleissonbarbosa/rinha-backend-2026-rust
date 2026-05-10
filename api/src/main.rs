use std::env;
use std::path::PathBuf;

use rinha_rust_api::http::{serve, App};
use rinha_rust_api::ivf_index::ExactIndex;
use rinha_rust_api::vectorize::Normalization;

fn main() {
    let addr = env::var("API_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let resources_dir =
        PathBuf::from(env::var("RESOURCES_DIR").unwrap_or_else(|_| "resources".to_string()));

    let norm = Normalization::load(resources_dir.join("normalization.json"));
    let index = match ExactIndex::load(&resources_dir) {
        Ok(index) => index,
        Err(err) => {
            eprintln!("failed to load IVF index: {err}");
            std::process::exit(2);
        }
    };

    let mut app = App::new(norm, index);
    app.warmup();
    if let Err(err) = serve(&addr, app) {
        eprintln!("server stopped: {err}");
        std::process::exit(2);
    }
}
