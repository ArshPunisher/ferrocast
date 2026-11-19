//! On Windows: the program's icon and its manifest (runs as the user, no admin; sharp on
//! high-DPI screens; Windows 10/11 controls).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui");
    // Resources need Windows' resource compiler: only when building on Windows for Windows
    // (a check from another system skips them).
    let target_windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let host_windows = std::env::var("HOST").is_ok_and(|h| h.contains("windows"));
    if target_windows && host_windows {
        #[cfg(windows)]
        resources();
    }
}

#[cfg(windows)]
fn resources() {
    let mut res = tauri_winres::WindowsResource::new();
    res.set_icon_with_id("../desktop/icons/icon.ico", "1");
    res.set("ProductName", "Ferrocast");
    res.set("FileDescription", "Ferrocast Setup");
    res.set("CompanyName", "ClearCraft Labs");
    res.set("LegalCopyright", "Copyright 2026 ClearCraft Labs");
    res.set_manifest(MANIFEST);
    if let Err(e) = res.compile() {
        panic!("Windows resources: {e}");
    }
}

#[cfg(windows)]
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="ClearCraftLabs.Ferrocast.Setup" version="1.0.0.0"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;
