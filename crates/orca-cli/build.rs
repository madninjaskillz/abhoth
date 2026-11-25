//! Windows: the mascot icon and version info in `abhoth.exe` and `abhoth-desktop.exe`
//! (PORTING.md D117). Built whenever the target is Windows: with rc.exe on a Windows host, and
//! with llvm-rc when cross-compiling from Linux (cargo-xwin in CI's `package` job). A Linux
//! target never compiles or links a resource.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows::embed();
    }
}

mod windows {
    use std::path::PathBuf;

    /// "1.2.3-rc.1" -> (1, 2, 3): the numeric part of the package version.
    fn version_numbers(version: &str) -> [u16; 3] {
        let core = version.split(['-', '+']).next().unwrap_or("");
        let mut out = [0u16; 3];
        for (slot, part) in out.iter_mut().zip(core.split('.')) {
            *slot = part.parse().unwrap_or(0);
        }
        out
    }

    pub fn embed() {
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
        let icon = manifest.join("../../assets/abhoth/icons/abhoth.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let [major, minor, patch] = version_numbers(&version);
        let icon_path = icon.display().to_string().replace('\\', "\\\\");
        let rc = format!(
            r#"1 ICON "{icon_path}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Abhoth"
      VALUE "FileDescription", "Abhoth"
      VALUE "FileVersion", "{version}"
      VALUE "ProductName", "Abhoth"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
        );
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("out dir")).join("abhoth.rc");
        std::fs::write(&out, rc).expect("writing abhoth.rc");
        embed_resource::compile(&out, embed_resource::NONE)
            .manifest_optional()
            .expect("compiling abhoth.rc");
    }
}
