fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/favicon.ico");
        compile("resources/GeodataEditor.rc", "GeodataEditor");
    }
}

#[cfg(windows)]
fn compile(resource: &str, binary: &str) {
    println!("cargo:rerun-if-changed={resource}");
    embed_resource::compile_for(resource, [binary], embed_resource::NONE)
        .manifest_optional()
        .expect("compile Windows version resource");
}
