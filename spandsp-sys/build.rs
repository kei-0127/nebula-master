use bindgen;

fn main() {
    println!("cargo:rustc-link-lib=spandsp");
    println!("cargo:rerun-if-changed=wrapper.h");
    // let bindings = bindgen::Builder::default()
    //     // The input header we would like to generate
    //     // bindings for.
    //     .header("wrapper.h")
    //     // Tell cargo to invalidate the built crate whenever any of the
    //     // included header files changed.
    //     .parse_callbacks(Box::new(bindgen::CargoCallbacks))
    //     // Finish the builder and generate the bindings.
    //     .generate()
    //     // Unwrap the Result and panic on failure.
    //     .expect("Unable to generate bindings");

    // bindings
    //     .write_to_file("./src/lib.rs")
    //     .expect("Couldn't write bindings!");
}
