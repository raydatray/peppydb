//! Write the Rust code for the API from a descriptor set.
//!
//! `protogen <descriptors.binpb> <out-dir>`: decode the
//! `FileDescriptorSet` that `buf build` wrote and generate prost messages
//! and tonic clients and servers into `<out-dir>`, one file per protobuf
//! package. `tools/gen-proto` runs it (plan 0026).

use std::path::PathBuf;
use std::process::ExitCode;

use prost::Message;
use prost_types::FileDescriptorSet;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [descriptors, out] = args.as_slice() else {
        eprintln!("usage: protogen <descriptors.binpb> <out-dir>");
        return ExitCode::from(2);
    };
    match generate(PathBuf::from(descriptors), PathBuf::from(out)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("protogen: {error}");
            ExitCode::FAILURE
        }
    }
}

fn generate(descriptors: PathBuf, out: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(&descriptors)?;
    let set = FileDescriptorSet::decode(bytes.as_slice())?;
    std::fs::create_dir_all(&out)?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .emit_rerun_if_changed(false)
        .out_dir(out)
        .compile_fds(set)?;
    Ok(())
}
