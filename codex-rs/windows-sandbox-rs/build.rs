use std::env;
use std::path::PathBuf;

const SETUP_BIN: &str = "codex-windows-sandbox-setup";
const SETUP_MANIFEST: &str = "codex-windows-sandbox-setup.manifest";

fn main() -> Result<(), String> {
    println!("cargo:rerun-if-changed={SETUP_MANIFEST}");

    // Multi-AI Code 定制：下面整段只服务于 `codex-windows-sandbox-setup` 这个
    // 可执行目标——给它嵌一份请求提权的 UAC 清单。该目标已经在本包的
    // Cargo.toml 里被注释掉（连同 `codex-command-runner`），因为
    // elevated 沙箱那一档整体不再提供。
    //
    // 目标没了之后 `cargo:rustc-link-arg-bin=<目标名>=...` 会直接让构建失败，
    // 报 "invalid instruction"——所以这里必须一起短路，光注释 [[bin]] 是不够的。
    // 恢复 elevated 时把这两行删掉即可。
    return Ok(());

    #[allow(unreachable_code)]
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| "CARGO_MANIFEST_DIR should be set for build scripts".to_string())?;
    let manifest_path = PathBuf::from(manifest_dir).join(SETUP_MANIFEST);
    let manifest_path = manifest_path.display();

    // Keep this scoped to the setup helper so Codex binaries that link the
    // library do not inherit any resource metadata from this package.
    match (
        env::var("CARGO_CFG_TARGET_ENV").as_deref(),
        env::var("CARGO_CFG_TARGET_ABI").as_deref(),
    ) {
        (Ok("msvc"), _) => {
            println!("cargo:rustc-link-arg-bin={SETUP_BIN}=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bin={SETUP_BIN}=/MANIFESTINPUT:{manifest_path}");
        }
        (Ok("gnu"), Ok("llvm")) => {
            println!("cargo:rustc-link-arg-bin={SETUP_BIN}=-Wl,-Xlink=/manifest:embed");
            println!(
                "cargo:rustc-link-arg-bin={SETUP_BIN}=-Wl,-Xlink=/manifestinput:{manifest_path}"
            );
        }
        _ => {}
    }

    Ok(())
}
