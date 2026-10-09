//! Install the Chromium build that matches the bundled Playwright driver.
//!
//! This deliberately does **not** shell out to a `playwright` CLI on `PATH`:
//! that binary is frequently absent (it is an npm package, not part of this
//! crate) and, when it is present, it may install a browser build that does not
//! match the driver pinned in `Cargo.lock`. The crate's own installer always
//! matches the driver this binary was built against.

use playwright_rs::{install_browsers, install_browsers_with_deps};

#[tokio::main]
async fn main() {
    let with_deps = std::env::args().any(|arg| arg == "--with-deps");
    println!(
        "Installing the Playwright Chromium matching this crate's driver{}...",
        if with_deps {
            " plus system dependencies"
        } else {
            ""
        }
    );

    let result = if with_deps {
        install_browsers_with_deps(Some(&["chromium"])).await
    } else {
        install_browsers(Some(&["chromium"])).await
    };

    if let Err(error) = result {
        eprintln!("freechatcode: could not install Playwright Chromium: {error}");
        if !with_deps {
            eprintln!(
                "If it downloaded but will not start, the browser's system libraries are missing. \
                 Re-run with --with-deps (that path installs them under sudo), or install them yourself."
            );
        }
        eprintln!(
            "Containers and minimal images usually need the libraries; the browser itself is \
             shared with the host through ~/.cache/ms-playwright."
        );
        std::process::exit(1);
    }

    println!("Playwright Chromium installed.");
}
