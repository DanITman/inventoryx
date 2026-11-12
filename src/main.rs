// InventoryX — disk usage treemap for Unraid.
// A port of Disk Inventory X (Tjark Derlien) to Linux.
//
// This program is free software; you can redistribute it and/or modify it under the
// terms of the GNU General Public License as published by the Free Software Foundation;
// either version 3 of the License, or any later version.

mod api;
mod guard;
mod remove;
mod http;
mod index;
mod kinds;
mod layout;
mod persist;
mod platform;
mod scan;
mod stats;

use std::sync::Arc;

fn usage() -> ! {
    eprintln!("usage: inventoryx serve [--socket PATH | --listen ADDR:PORT] [--index-dir auto|off|PATH]\n       inventoryx scan PATH");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => {
            let mut listen = http::Listen::Tcp("127.0.0.1:7878".into());
            let mut store = api::Store::Off;
            let mut rest = args[1..].chunks(2);
            for pair in &mut rest {
                match (pair[0].as_str(), pair.get(1)) {
                    ("--socket", Some(p)) => listen = http::Listen::Unix(p.clone()),
                    ("--listen", Some(a)) => listen = http::Listen::Tcp(a.clone()),
                    ("--index-dir", Some(d)) => store = match d.as_str() {
                        "off" | "" => api::Store::Off,
                        "auto" => api::Store::Auto,
                        path if path.starts_with('/') => api::Store::Dir(path.into()),
                        _ => usage(),
                    },
                    _ => usage(),
                }
            }
            let platform = platform::Platform::detect();
            // On Unraid the program is reached only through the web UI's login.
            if platform.is_unraid() && !matches!(listen, http::Listen::Unix(_)) {
                eprintln!("inventoryx: on Unraid use --socket; a network port would bypass the login");
                std::process::exit(2);
            }
            let app = api::App::new(platform, store);
            app.load_saved();
            let handler = Arc::new(move |req| app.handle(req));
            if let Err(e) = http::serve(listen, handler) {
                eprintln!("inventoryx: {e}");
                std::process::exit(1);
            }
        }
        Some("scan") => {
            let Some(path) = args.get(1) else { usage() };
            let p = platform::Platform::detect();
            let Some(spec) = p.spec_for(&format!("path:{path}")) else { usage() };
            let progress = Arc::new(scan::Progress::default());
            match scan::start(spec, Arc::new(kinds::KindTable::new()), progress) {
                Ok(ix) => {
                    let r = &ix.nodes[0];
                    println!("{} files, {} nodes, {} bytes on disk, {} logical, {} unreadable, {:.2}s, index {} MB",
                             r.files, ix.nodes.len(), r.physical, r.logical, ix.unreadable,
                             ix.duration.as_secs_f64(), ix.memory_bytes() / 1_000_000);
                }
                Err(e) => { eprintln!("inventoryx: {e}"); std::process::exit(1); }
            }
        }
        _ => usage(),
    }
}
