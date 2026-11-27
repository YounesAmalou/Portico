//! Windows: embeds the app's icon, version and manifest in the executable.
//! Elsewhere, and when checking for Windows from another system (which has no
//! resource compiler), it does nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/portico.ico");
    println!("cargo:rerun-if-env-changed=PORTICO_TRAY_VERSION");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows::embed();
    }
}

#[cfg(windows)]
mod windows {
    /// Per-monitor DPI awareness for a sharp icon and menu, and the themed common controls.
    const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="Portico.Tray" version="1.0.0.0"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

    pub fn embed() {
        let version = std::env::var("PORTICO_TRAY_VERSION").unwrap_or_else(|_| "dev".into());
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/portico.ico")
            .set_manifest(MANIFEST)
            .set("ProductName", "Portico")
            .set("FileDescription", "Portico tray app")
            .set("ProductVersion", &version)
            .set("FileVersion", &version);
        if let Err(error) = resource.compile() {
            panic!("cannot embed the Windows resources: {error}");
        }
    }
}
