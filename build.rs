fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon/fono8.ico");
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/app-icon/fono8.ico");
        resource.set("ProductName", "Fono8");
        resource.set("FileDescription", "Fono8 - music player");
        resource.set("CompanyName", "OPEN8");
        resource.set("LegalCopyright", "MIT License");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=Windows resources not embedded: {error}");
        }
    }
}
