//! The file transport as an external plugin: speaks the device protocol on
//! stdio. Functionally identical to the built-in `file` transport; it exists
//! to exercise the plugin path end to end (`--transport file-plugin`).

#[path = "../file_transport.rs"]
mod file_transport;

fn main() {
    let mut transport = file_transport::FileTransport::default();
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    if let Err(e) = tock_proto::serve(stdin, stdout, &mut transport) {
        eprintln!("tock-transport-file-plugin: {e}");
        std::process::exit(1);
    }
}
